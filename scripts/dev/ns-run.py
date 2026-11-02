#!/usr/bin/env python3
"""ns-run: run a command on a Namespace instance, on this checkout's tree.

mcp-server's copy of contextstream's scripts/dev/ns-run.py (at its commit 5a7ad0ec1), so the release's gate and builds
run on Namespace instead of Depot. The shared parts (SHARE, COMMIT, TREE, the instance's life) are the same, so keep them in step; the
differences are the default --repo (mcp-server, cache volume nsrun-mcp-server-v1) and the runner's root (ns-runner.sh).
Where this text says depot-run, read scripts/dev/ns-run.

scripts/dev/depot-run --where namespace calls this with the merge base and the patch it made:
  ns-run.py --base SHA --patch FILE [--cpus N] [--timeout MIN] [--label TEXT] [--repo NAME] [--cold] [--artifact PATH]...
            [--kind agent|release] [--cache-tag TAG] [--artifacts-dir DIR] [--overflow-after S] [--budget-guard]
            [--result FILE] -- CMD...
In the instance's tree, HEAD is the base commit itself (no history), so `git rev-parse HEAD` gives the base and `git status`
shows only your changes. --input LOCAL:/data/PATH copies a file into the run (for example a stored verdict).
COMPILE CACHE: Rust compiles go through sccache on Namespace's HTTP cache: the instance may use one cache
(--sccache-cache NAME; default nsrun-agents, and none for --kind release until the release's verdict key allows it, since
sccache needs CARGO_INCREMENTAL unset), and the runner sets it up under the instance's own identity. A token that can't
grant that cache gets the run without it.
The release's validate calls it directly with --kind release (base = the target commit, --patch /dev/null): its instances
are labelled nsrun-release instead of nsrun, so they neither count toward nor wait for depot-run's share (SHARE below);
only Namespace's own limit applies to them. --cache-tag picks another cache volume (default nsrun-<repo>-v1), and
--artifacts-dir where the artifacts land (default .depot-run-artifacts/<instance>).

One instance per run, created through the Namespace Compute API with the token in NSC_TOKEN, or in the file
NSC_TOKEN_FILE names (default ~/.config/nsc/token.json); never printed:
  - shape from --cpus (4, 8, 16 or 32 vCPU with 2 GB each; 64 runs as 32, the largest shape the workspace allows),
  - a CACHE volume shared by every run (tag nsrun-<repo>-v1, /cache): the Ubuntu root with the toolchains, the cargo
    home and target dir, sccache, npm cache, and a git tree at the merge base the last run used. A run sends only the
    diff from that base to yours (or the whole tree on a cache miss) plus your patch,
  - a key made for this run only (native SSH, ssh <instance>@ssh.<cell>.namespace.so),
  - workload_permissions narrowed to {tenant: get}: code in the run cannot create instances or read the registry,
  - a main container that exits 0 when the run is finished, so the cache volume commits only after a run that ended.
The command runs as uid 1001 (like a Depot runner) in /work, the tree; its output streams here, and its exit status
is the verdict. The instance is always destroyed at the end.

OVERFLOW (docs/runbooks/depot-run.md, "Overflow on Bitrise"; depot-run passes these for an agent's run that can go to
Bitrise): --overflow-after S gives the run up, with nothing left on Namespace, once Namespace has kept it waiting S
seconds for capacity (the workspace answering 429, or depot-run's share full); --budget-guard gives it up before
anything is made when the workspace's unit-minutes this billing period are ahead of a straight line to the plan's
allowance (ns_budget.py; a cached read of Namespace's usage). Either way the exit status is 8, and depot-run runs the
same command on Bitrise. Release jobs take neither.

TREE: the runner checks, before the command, that HEAD is the base commit itself with its own tree, and that base plus
your patch is the tree computed here (both printed in the run's header). A cached tree it can't trust gets replaced by
the whole tree, sent once more; if even that fails, nothing runs and the exit status is 7, with no verdict. The cache
volume is kept only when the runner put the tree back exactly at the base and checked it.

LABELS (job 4): an agent's instance carries, beside nsrun and nsrun-repo, nsrun-devbox (the agentbox env, DEVBOX; the
pool's claim log names the agent that claimed it), nsrun-agent (CONTEXTSTREAM_AGENT_NAME or DEPOT_RUN_AGENT, when set)
and nsrun-tree (the tree it runs: the base plus the patch), so usage can be told apart by agent and a tree validated twice
shows. --result FILE gets {"run": instance id, "where": "namespace", "base", "rc": exit status} at the end.

Exit status: 0 the command passed; 1 it failed (or timed out); 3 your changes did not apply to the merge base;
5 Namespace refused the token; 7 Namespace could not run it (no capacity in time, no SSH, the runner failed, no exact
tree of the base);
8 the run goes to Bitrise instead (OVERFLOW).

Knobs (environment): NSRUN_ENDPOINT (the Compute API), NSRUN_CACHE_GB, NSRUN_CAPACITY_WAIT_S (how long to wait for
capacity, default 1800); the tests also set NSRUN_CAPACITY_RETRY_S, NSRUN_SSH_ATTEMPTS, NSRUN_SSH_RETRY_S, NSRUN_POLL_S.
"""
import argparse, datetime, json, os, random, re, shlex, shutil, signal, subprocess, sys, tempfile, time, urllib.error, urllib.request, uuid

HERE = os.path.dirname(os.path.abspath(__file__))
ENDPOINT = os.environ.get("NSRUN_ENDPOINT", "https://us.compute.namespaceapis.com")
SVC = "namespace.cloud.compute.v1beta.ComputeService/"
SHAPES = {4: (4, 8192), 8: (8, 16384), 16: (16, 32768), 32: (32, 65536), 64: (32, 65536)}
BUSYBOX = "public.ecr.aws/docker/library/busybox:1.36@sha256:73aaf090f3d85aa34ee199857f03fa3a95c8ede2ffd4cc2cdb5b94e566b11662"
FINISH_PORT = 47123
CACHE_GB = int(os.environ.get("NSRUN_CACHE_GB", "120"))  # Namespace caps a cache volume at 128,000 MB
CAPACITY_WAIT_S = float(os.environ.get("NSRUN_CAPACITY_WAIT_S", "1800"))
CAPACITY_RETRY_S = float(os.environ.get("NSRUN_CAPACITY_RETRY_S", "15"))
SSH_ATTEMPTS = int(os.environ.get("NSRUN_SSH_ATTEMPTS", "40"))
SSH_RETRY_S = float(os.environ.get("NSRUN_SSH_RETRY_S", "3"))
POLL_S = float(os.environ.get("NSRUN_POLL_S", "2"))
# What depot-run gives a run unless --timeout says otherwise (TIMEOUT, job 4): said when a run times out.
TIMEOUT_DEFAULTS = "45 min for a command, 90 for --validate"
READY_WAIT_S = float(os.environ.get("NSRUN_READY_WAIT_S", "300"))
# SHARE: the Business plan runs 160 Linux vCPU at once for the whole workspace (2026-11-01; Team had 64). The
# release's builder and validate keep 96 of them: on 2026-11-01 they used 96 at once (validation 80, a builder 16) and
# Namespace refused new instances (429). depot-run's instances (label nsrun) together keep within NSRUN_AGENT_CPU, so a
# release job doesn't wait for an agent.
AGENT_CPU = int(os.environ.get("NSRUN_AGENT_CPU", "64"))
ENDED = ("DESTROYED", "DESTROYING", "ERROR")
LIVE = ("PENDING", "CREATING", "CREATED", "STARTING", "RUNNING")
END_WAIT_S = float(os.environ.get("NSRUN_END_WAIT_S", "60"))
# COMMIT: the last generation committed wins, on every node it reaches. A run that started on an empty cache (a node that
# had no generation yet) commits only when its command passed and left at least this much, so a failed or small cold run
# cannot replace a warmer cache (seen 2026-11-01: a validate that failed early replaced a 20 GB generation with 4.8 GB).
SEED_MIN_MB = int(float(os.environ.get("NSRUN_SEED_MIN_GB", "15")) * 1024)


def say(msg):
    print("depot-run: " + msg, file=sys.stderr, flush=True)


def token():
    """NSC_TOKEN (the bearer token itself, for CI jobs), else the file NSC_TOKEN_FILE names. Never printed."""
    if os.environ.get("NSC_TOKEN"):
        return os.environ["NSC_TOKEN"].strip()
    path = os.environ.get("NSC_TOKEN_FILE") or os.path.expanduser("~/.config/nsc/token.json")
    try:
        with open(path) as f:
            return json.load(f)["bearer_token"]
    except (OSError, ValueError, KeyError):
        say("no Namespace token at %s (NSC_TOKEN_FILE): agentbox envs claimed after the token reached golden have it; "
            "claim a new env, or run on Depot with --where depot" % path)
        sys.exit(5)


def api(method, body, timeout=120):
    """A ComputeService call; post() for another service (the budget guard's UsageService)."""
    return post(SVC + method, body, timeout)


def post(path, body, timeout=120):
    req = urllib.request.Request(ENDPOINT + "/" + path, data=json.dumps(body).encode(), method="POST",
                                 headers={"Authorization": "Bearer " + token(), "Content-Type": "application/json"})
    try:
        with urllib.request.urlopen(req, timeout=timeout) as r:
            raw = r.read()
            return r.status, (json.loads(raw) if raw else {})
    except urllib.error.HTTPError as e:
        raw = e.read()
        try:
            return e.code, json.loads(raw)
        except ValueError:
            return e.code, {"message": raw.decode(errors="replace")[:300]}
    except (urllib.error.URLError, OSError) as e:
        return 0, {"message": str(e)}


def created_key(row):
    """Sort key for an RFC 3339 time with any number of fraction digits, then the instance id."""
    day, _, frac = row.get("createdAt", "").rstrip("Z").partition(".")
    return (day, int((frac + "000000000")[:9]) if frac.isdigit() else 0, row.get("instanceId", ""))


def agents_in_use():
    """depot-run's instances that have not ended, oldest first, as (instance id, vCPU); None when the token may not list."""
    st, out = api("ListInstances", {"labelFilter": [{"name": "nsrun", "op": "EXIST"}], "maxEntries": 500})
    if st != 200:
        return None
    rows = sorted((i for i in out.get("instances") or [] if i.get("status") not in ENDED), key=created_key)
    return [(i.get("instanceId", ""), int((i.get("shape") or {}).get("virtualCpu") or 0)) for i in rows]


def git(*args, **kw):
    return subprocess.run(["git", *args], check=True, capture_output=True, **kw).stdout


def expected_tree(base, patch):
    """TREE: the tree of base plus the patch, as the runner must find it (git apply --cached in a scratch index)."""
    with tempfile.TemporaryDirectory() as d:
        env = dict(os.environ, GIT_INDEX_FILE=os.path.join(d, "index"))
        g = ["git", "-c", "apply.whitespace=nowarn", "-c", "core.autocrlf=false"]
        subprocess.run(g + ["read-tree", base], env=env, check=True, capture_output=True)
        if os.path.getsize(patch):
            subprocess.run(g + ["apply", "--cached", "--binary", patch], env=env, check=True, capture_output=True)
        return subprocess.run(g + ["write-tree"], env=env, check=True, capture_output=True, text=True).stdout.strip()


def label_value(text):
    """A label value Namespace takes: letters, digits, '.', '_' and '-', at most 63."""
    return re.sub(r"[^A-Za-z0-9._-]", "-", text)[:63].strip("-._") or "unknown"


def agent_labels(tree):
    """LABELS: who ran it (the agentbox env, and the agent's name when known) and the tree it runs."""
    labels = [{"name": "nsrun-devbox", "value": label_value(os.environ.get("DEVBOX", "") or "none")}]
    agent = os.environ.get("CONTEXTSTREAM_AGENT_NAME") or os.environ.get("DEPOT_RUN_AGENT") or ""
    if agent:
        labels.append({"name": "nsrun-agent", "value": label_value(agent)})
    return labels + [{"name": "nsrun-tree", "value": tree}]


def main():
    ap = argparse.ArgumentParser(add_help=False)
    ap.add_argument("--base", required=True)
    ap.add_argument("--patch", required=True)
    ap.add_argument("--cpus", type=int, default=16)
    # ARM64: an arm64 instance (machineArch), on a cache volume of its own (the root and the toolchains are per arch).
    ap.add_argument("--arch", choices=("amd64", "arm64"), default="amd64")
    ap.add_argument("--timeout", type=int, default=90)
    ap.add_argument("--label", default="")
    ap.add_argument("--repo", default="mcp-server")
    ap.add_argument("--lanes-state", default="",
                    help="OVERFLOW count: why this agent run is on Namespace and not a lane (scripts/dev/ns-run; label nsrun-lanes)")
    ap.add_argument("--cold", action="store_true")
    ap.add_argument("--artifact", action="append", default=[])
    ap.add_argument("--print", dest="print_only", action="store_true")
    ap.add_argument("--kind", choices=("agent", "release"), default="agent")
    ap.add_argument("--cache-tag", default="")
    ap.add_argument("--artifacts-dir", default="")
    ap.add_argument("--input", action="append", default=[], help="LOCAL:/data/PATH, copied into the run before the command")
    ap.add_argument("--overflow-after", type=float, default=None,
                    help="give the run up to Bitrise (exit 8) after waiting this many seconds for capacity")
    ap.add_argument("--budget-guard", action="store_true",
                    help="give the run up to Bitrise (exit 8) when Namespace's use this period is ahead of the allowance's pace")
    ap.add_argument("--sccache-cache", default=os.environ.get("NSRUN_SCCACHE_CACHE", ""),
                    help="the Namespace HTTP cache sccache uses (default nsrun-agents; none for --kind release; none: off)")
    ap.add_argument("--result", default="", help="write the run's id and exit status here (JSON) at the end")
    ap.add_argument("cmd", nargs=argparse.REMAINDER)
    a = ap.parse_args()
    cmd = a.cmd[1:] if a.cmd[:1] == ["--"] else a.cmd
    if not cmd:
        say("no command given"); return 2
    if a.cpus not in SHAPES:
        say("--cpus must be 4, 8, 16, 32 or 64"); return 2
    for spec in a.input:
        local, _, dest = spec.partition(":")
        if not os.path.isfile(local) or not dest.startswith("/data/") or ".." in dest.split("/") or "\n" in dest:
            say("--input takes LOCAL:/data/PATH with an existing file (got %r)" % spec); return 2
    cpus, mem = SHAPES[a.cpus]
    if a.cpus == 64:
        say("64 CPUs runs as 32 on Namespace (the largest shape the workspace allows is 32x64)")
    if a.sccache_cache in ("none", "off", "-"):
        a.sccache_cache = ""
    elif not a.sccache_cache:
        # Release runs opt in (--sccache-cache nsrun-release): sccache unsets CARGO_INCREMENTAL, part of the release's
        # verdict key (validate.py VERDICT_ENV).
        a.sccache_cache = "nsrun-agents" if a.kind == "agent" else ""
    nonce = uuid.uuid4().hex[:12]
    tag = a.cache_tag or ("nsrun-%s-v1" % a.repo if a.arch == "amd64" else "nsrun-%s-%s-v1" % (a.repo, a.arch))
    if a.print_only:
        print(json.dumps({"shape": "%dx%d" % (cpus, mem // 1024), "cache_tag": None if a.cold else tag,
                          "timeout_min": a.timeout, "base": a.base, "cmd": cmd}, indent=1))
        return 0
    if a.kind == "release":
        a.overflow_after, a.budget_guard = None, False   # release jobs are never routed away
    if a.budget_guard:
        sys.path.insert(0, HERE)
        import ns_budget
        if ns_budget.enabled():
            ahead, text, fresh = ns_budget.verdict(post)
            if ahead:
                say(text + ": this run goes to Bitrise (the budget guard)")
                return 8
            if fresh:
                say(text)

    work = tempfile.mkdtemp(prefix="nsrun-")
    inst = {"id": None, "ended": False, "ssh": None}

    def destroy(reason):
        """The last resort: a destroyed instance's cache volume is kept as it stands (seen 2026-11-01), like a commit."""
        if inst["id"] and not inst["ended"]:
            api("DestroyInstance", {"instanceId": inst["id"], "reason": reason})
            inst["ended"] = True

    def end(keep, reason):
        """End the instance through its main container: "keep" makes it exit 0, which commits the cache volume as the
        next runs' starting point; "drop" makes it exit 1, which abandons the volume. Wait for the instance to end;
        destroy it when it can't be told or doesn't end."""
        if not inst["id"] or inst["ended"]:
            return
        told = False
        for _ in range(3 if inst["ssh"] else 0):
            try:
                told = subprocess.run(inst["ssh"] + ["bash -c 'echo %s > /dev/tcp/127.0.0.1/%d'" % ("keep" if keep else "drop",
                                                                                                   FINISH_PORT)],
                                      capture_output=True, stdin=subprocess.DEVNULL, timeout=60).returncode == 0
            except subprocess.TimeoutExpired:
                told = False
            if told:
                break
            time.sleep(SSH_RETRY_S)
        t = time.time()
        while told and time.time() - t < END_WAIT_S:
            st, out = api("DescribeInstance", {"instanceId": inst["id"]})
            if st == 200 and (out.get("metadata") or {}).get("status") not in LIVE:
                inst["ended"] = True
                return
            time.sleep(POLL_S)
        destroy(reason)

    def on_signal(signum, _frame):
        end(False, "depot-run interrupted")
        shutil.rmtree(work, ignore_errors=True)
        sys.exit(130)

    signal.signal(signal.SIGINT, on_signal)
    signal.signal(signal.SIGTERM, on_signal)
    try:
        rc = run(a, cmd, cpus, mem, nonce, tag, work, inst, destroy, end)
        if a.result:
            with open(a.result, "w") as f:
                json.dump({"run": inst["id"], "where": "namespace", "base": a.base, "rc": rc}, f)
        return rc
    finally:
        end(False, "depot-run finished")
        shutil.rmtree(work, ignore_errors=True)


def run(a, cmd, cpus, mem, nonce, tag, work, inst, destroy, end):
    def overflow(t0, why):
        """OVERFLOW: True, said in one line, once capacity has been waited for --overflow-after seconds."""
        waited = time.time() - t0
        if a.overflow_after is None or waited < a.overflow_after:
            return False
        say("%s for %.0fs (its overflow limit is %gs): this run goes to Bitrise" % (why, waited, a.overflow_after))
        return True

    # TREE: the tree the runner must reach; LABELS: the instance carries it. A patch that doesn't apply makes no instance.
    try:
        want_tree = expected_tree(a.base, a.patch)
    except subprocess.CalledProcessError:
        say("your changes did not apply to %s" % a.base[:12]); return 3
    key = os.path.join(work, "key")
    subprocess.run(["ssh-keygen", "-q", "-t", "ed25519", "-N", "", "-C", "nsrun-" + nonce, "-f", key], check=True)
    deadline = (datetime.datetime.now(datetime.timezone.utc) + datetime.timedelta(minutes=a.timeout + 20)).strftime("%Y-%m-%dT%H:%M:%SZ")
    body = {
        "shape": {"virtualCpu": cpus, "memoryMegabytes": mem, "machineArch": a.arch, "os": "linux"},
        "documentedPurpose": ("depot-run (namespace): " + (a.label or " ".join(cmd)))[:200],
        "deadline": deadline, "placement": ["continent:us"],
        "labels": [{"name": "nsrun" if a.kind == "agent" else "nsrun-release", "value": nonce},
                   {"name": "nsrun-repo", "value": a.repo}] + (agent_labels(want_tree) if a.kind == "agent" else [])
                  + ([{"name": "nsrun-lanes", "value": label_value(a.lanes_state)}]
                     if a.kind == "agent" and a.lanes_state else []),
        "experimental": {"authorizedSshKeys": [open(key + ".pub").read().strip()]},
        "workloadPermissions": [{"resourceType": "tenant", "resourceId": "", "actions": ["get"]}]
                               + ([{"resourceType": "cache/httpcache", "resourceId": a.sccache_cache, "actions": ["ensure", "read", "write"]}]
                                  if a.sccache_cache else []),
        "containers": [{"name": "main", "network": "HOST", "workloadType": "JOB", "imageRef": BUSYBOX,
                        # COMMIT: exit 0 (the volume becomes the next runs' starting point) only when told "keep".
                        "entrypoint": ["/bin/sh", "-c", 'c=$(timeout %d nc -l -p %d); [ "$c" = keep ] && exit 0; exit 1'
                                       % ((a.timeout + 15) * 60, FINISH_PORT)]}],
    }
    if not a.cold:
        body["volumes"] = [{"tag": tag, "mountPoint": "/cache", "sizeMb": CACHE_GB * 1024, "persistencyKind": "CACHE"}]
    t0 = time.time()
    noted, early_ends = set(), 0
    while True:
        # SHARE: take a shape that fits in what depot-run's other instances leave of AGENT_CPU: the one asked for, else
        # 16x32, else wait. A token that may not list instances skips this, and Namespace's own limit decides (429).
        rows = agents_in_use() if a.kind == "agent" else None
        if rows is not None:
            in_use = sum(c for _, c in rows)
            if in_use + cpus > AGENT_CPU:
                if cpus > 16 and in_use + 16 <= AGENT_CPU:
                    say("%d vCPU would take depot-run past its share (%d of %d vCPU in use; release builds keep the rest): "
                        "running as 16x32" % (cpus, in_use, AGENT_CPU))
                    cpus, mem = SHAPES[16]
                elif time.time() - t0 >= CAPACITY_WAIT_S:
                    say("depot-run's share stayed full (%d of %d vCPU in use) for %.0f min" % (in_use, AGENT_CPU, CAPACITY_WAIT_S / 60))
                    return 7
                else:
                    if overflow(t0, "depot-run's share of Namespace stayed full (%d of %d vCPU in use)" % (in_use, AGENT_CPU)):
                        return 8
                    if "share" not in noted:
                        say("depot-run's share of Namespace is full (%d of %d vCPU in use; release builds keep the rest): "
                            "waiting" % (in_use, AGENT_CPU))
                        noted.add("share")
                    time.sleep(CAPACITY_RETRY_S)
                    continue
        body["shape"] = {"virtualCpu": cpus, "memoryMegabytes": mem, "machineArch": a.arch, "os": "linux"}
        st, out = api("CreateInstance", body)
        if st == 403 and a.sccache_cache:
            # A token can't grant what it doesn't hold: without cache/httpcache on this cache, run without the compile cache.
            say("the token can't give the instance the compile cache (cache/httpcache %s); running without it" % a.sccache_cache)
            a.sccache_cache = ""
            body["workloadPermissions"] = [p for p in body["workloadPermissions"] if p["resourceType"] != "cache/httpcache"]
            continue
        if st in (401, 403):
            say("Namespace refused the token (HTTP %d): %s" % (st, out.get("message", "")[:200])); return 5
        if st == 429 and overflow(t0, "Namespace's workspace stayed full (HTTP 429)"):
            return 8
        if st == 429 and time.time() - t0 < CAPACITY_WAIT_S:
            if "tenant" not in noted:
                say("Namespace concurrency is full for the workspace; waiting for capacity")
                noted.add("tenant")
            time.sleep(CAPACITY_RETRY_S)
            continue
        if st != 200:
            say("Namespace could not create the instance (HTTP %d): %s" % (st, out.get("message", "")[:300])); return 7
        md = out["metadata"]
        inst["id"], inst["ended"], inst["ssh"] = md["instanceId"], False, None
        # Two runs that checked the share at the same moment: the newer one gives way (below) and tries again.
        rows = agents_in_use() if a.kind == "agent" else None
        raced = False
        if rows is not None and inst["id"] in [i for i, _ in rows]:
            raced = sum(c for i, c in rows[:[i for i, _ in rows].index(inst["id"]) + 1]) > AGENT_CPU
        # Ready means started (ready_at); Namespace sometimes ends an instance before that (seen 2026-11-01 with a cache
        # volume): make another.
        state, t1 = "slow", time.time()
        while time.time() - t1 < READY_WAIT_S:
            st, got = api("DescribeInstance", {"instanceId": inst["id"]})
            gm = (got.get("metadata") or {}) if st == 200 else {}
            if gm.get("readyAt"):
                state = "ready"
                break
            if gm.get("status") in ENDED:
                state = gm["status"]
                break
            time.sleep(min(POLL_S, 1))
        if state in ENDED:
            # Seen right after a run on the same cache volume committed: two in a row, then a third came up. Pause a
            # little longer each time.
            early_ends += 1
            destroy("depot-run: ended before it was ready")
            if early_ends >= 5:
                say("Namespace ended %d instances before they were ready" % early_ends); return 7
            say("Namespace ended instance %s before it was ready (%s); creating another" % (inst["id"], state))
            time.sleep(min(CAPACITY_RETRY_S, 5) * early_ends)
            continue
        cell = md["ingressDomain"].split(".")[0]
        inst["ssh"] = ["ssh", "-i", key, "-o", "IdentitiesOnly=yes", "-o", "BatchMode=yes", "-o", "StrictHostKeyChecking=no",
                       "-o", "UserKnownHostsFile=/dev/null", "-o", "LogLevel=ERROR", "-o", "ConnectTimeout=20",
                       "-o", "ServerAliveInterval=30", "-o", "ServerAliveCountMax=6",
                       "%s@ssh.%s.namespace.so" % (inst["id"], cell)]
        # The key is enrolled a few seconds after creation; the runner script goes up first, alone on stdin.
        runner = os.path.join(HERE, "ns-runner.sh")
        for attempt in range(SSH_ATTEMPTS):
            with open(runner, "rb") as f:
                r = subprocess.run(inst["ssh"] + ["mkdir -p /tmp/nsrun && cat > /tmp/nsrun/ns-runner.sh && "
                                                  "bash /tmp/nsrun/ns-runner.sh probe"], stdin=f, capture_output=True, text=True)
            if r.returncode == 0:
                break
            time.sleep(SSH_RETRY_S)
        else:
            say("could not reach the instance over SSH: %s" % (r.stderr.strip()[-200:])); return 7
        if raced:
            say("another run took depot-run's share at the same moment; trying again")
            end(False, "depot-run: over its share")
            time.sleep(random.uniform(1, 5) * min(CAPACITY_RETRY_S, 1))
            continue
        break
    ssh = inst["ssh"]
    say("Namespace instance %s (%dx%d, %s%s); %s" % (inst["id"], cpus, mem // 1024, cell, "" if a.cold else ", cache " + tag,
                                                    out.get("instanceUrl", "")))
    probe = dict(l.split("=", 1) for l in r.stdout.split() if "=" in l)
    say("ready in %.0fs; cache: tree %s, rootfs %s, %s" % (time.time() - t0, probe.get("tree_base", "none")[:12],
                                                          probe.get("rootfs"), probe.get("cache_used")))
    # Payload: the diff from the cached tree's base to ours when we have that commit, else the whole tree.
    pay = os.path.join(work, "in")
    os.makedirs(pay)
    have = probe.get("tree_base", "none")
    use_delta = have not in ("none", "") and subprocess.run(["git", "cat-file", "-e", have + "^{commit}"],
                                                           capture_output=True).returncode == 0

    def tree_payload(delta):
        for name in ("delta.diff", "from", "tree.tar.zst"):
            if os.path.exists(os.path.join(pay, name)):
                os.remove(os.path.join(pay, name))
        if delta:
            with open(os.path.join(pay, "delta.diff"), "wb") as f:
                subprocess.run(["git", "diff", "--binary", have, a.base], stdout=f, check=True)
            open(os.path.join(pay, "from"), "w").write(have + "\n")
        else:
            with open(os.path.join(pay, "tree.tar.zst"), "wb") as f:
                gz = subprocess.Popen(["git", "archive", "--format=tar", a.base], stdout=subprocess.PIPE)
                subprocess.run(["zstd", "-3", "-T0", "-q", "-c"], stdin=gz.stdout, stdout=f, check=True)
                gz.wait()

    tree_payload(use_delta)
    open(os.path.join(pay, "expect_tree"), "w").write(want_tree + "\n")
    shutil.copy(a.patch, os.path.join(pay, "patch.diff"))
    open(os.path.join(pay, "base"), "w").write(a.base + "\n")
    open(os.path.join(pay, "sccache_cache"), "w").write(a.sccache_cache + "\n" if a.sccache_cache else "")
    site = re.match(r"[a-z]+", cell)   # the instance's site (iad3-a -> iad): sccache talks to the cache next to it
    open(os.path.join(pay, "sccache_site"), "w").write(site.group(0) + "\n" if site else "")
    with open(os.path.join(pay, "base.commit"), "wb") as f:   # the runner rebuilds the base commit's own object from this
        subprocess.run(["git", "cat-file", "commit", a.base], stdout=f, check=True)
    if a.input:
        os.makedirs(os.path.join(pay, "inputs"))
        with open(os.path.join(pay, "inputs.list"), "w") as listing:
            for n, spec in enumerate(a.input):
                local, _, dest = spec.partition(":")
                shutil.copy(local, os.path.join(pay, "inputs", str(n)))
                listing.write("%d %s\n" % (n, dest))
    open(os.path.join(pay, "cmd"), "w").write(" ".join(shlex.quote(c) for c in cmd) + "\n")
    open(os.path.join(pay, "timeout_s"), "w").write(str(a.timeout * 60) + "\n")
    open(os.path.join(pay, "artifacts"), "w").write("".join(p + "\n" for p in a.artifact))   # one per line; globs allowed
    saw_bad_tree = False
    while True:
        size = sum(os.path.getsize(os.path.join(pay, n)) for n in os.listdir(pay))
        say("sending %s (%.1f MB) and your patch (%d bytes)" % ("a delta from " + have[:12] if use_delta else "the whole tree",
                                                                size / 1e6, os.path.getsize(a.patch)))
        tar = subprocess.Popen(["tar", "-C", pay, "-cf", "-", "."], stdout=subprocess.PIPE)
        r = subprocess.run(ssh + ["rm -rf /tmp/nsrun/in && mkdir -p /tmp/nsrun/in && tar -x -f - -C /tmp/nsrun/in && "
                                  "bash /tmp/nsrun/ns-runner.sh run"], stdin=tar.stdout)
        tar.wait()
        rc = r.returncode
        # The verdict's kind comes from the runner's status file, not from the exit code a command could also use.
        got = subprocess.run(ssh + ["bash /tmp/nsrun/ns-runner.sh status"], capture_output=True, text=True, stdin=subprocess.DEVNULL)
        status = (got.stdout.split() or ["infra"]) if got.returncode == 0 else ["infra"]
        kind = status[0]
        if kind != "tree":
            break
        # TREE: the runner found no exact tree of the base; nothing ran. Once, the whole tree; then no verdict.
        saw_bad_tree = True
        if not use_delta:
            say("Namespace could not set up the exact tree of %s, even from the whole tree: nothing ran, no verdict" % a.base[:12])
            end(False, "depot-run: no exact tree")
            return 7
        say("the cached tree on %s could not give %s exactly; sending the whole tree" % (inst["id"], a.base[:12]))
        use_delta = False
        tree_payload(False)
    if kind in ("cmd", "timeout") and len(status) > 1 and status[1].lstrip("-").isdigit():
        rc = int(status[1])
    used_mb = int(status[2]) if len(status) > 2 and status[2].isdigit() else 0
    if a.artifact and kind in ("cmd", "timeout"):
        dest = a.artifacts_dir or os.path.join(".depot-run-artifacts", inst["id"])
        os.makedirs(dest, exist_ok=True)
        got = subprocess.run(ssh + ["bash /tmp/nsrun/ns-runner.sh artifacts"], capture_output=True, stdin=subprocess.DEVNULL)
        if got.stdout:
            subprocess.run(["tar", "-x", "-f", "-", "-C", dest], input=got.stdout)
            say("artifacts in " + dest)
    # A run that ended keeps its cache volume (COMMIT), unless it started on an empty cache and failed or left it
    # small; a runner that failed keeps nothing. TREE: and only when the runner put the tree back exactly at the base and
    # checked it ("ok"), in a run whose cached tree was trusted (a run that had to replace it keeps nothing).
    started_warm = probe.get("rootfs", "none") != "none" and have not in ("none", "")
    tree_ok = kind == "patch" or (len(status) > 3 and status[3] == "ok")
    commit = kind in ("cmd", "timeout", "patch") and tree_ok and not saw_bad_tree and (
        a.cold or started_warm or (kind == "cmd" and rc == 0 and used_mb >= SEED_MIN_MB))
    if kind in ("cmd", "timeout") and not tree_ok:
        say("not keeping this run's cache: its tree was not put back exactly at %s" % a.base[:12])
    elif saw_bad_tree and kind in ("cmd", "timeout", "patch"):
        say("not keeping this run's cache: the tree it found was not %s" % have[:12])
    if kind in ("cmd", "timeout", "patch") and not commit and tree_ok and not saw_bad_tree:
        say("not keeping this run's cache: it started empty and %s (%d MB); a warmer one may exist"
            % ("failed" if kind != "cmd" or rc != 0 else "left it small", used_mb))
    end(commit, "depot-run: run over")
    total = time.time() - t0
    if kind == "patch":
        say("your changes did not apply to %s" % a.base[:12]); return 3
    if kind == "infra":
        say("Namespace could not run this (the runner failed; ssh exit %d)" % r.returncode); return 7
    if kind == "timeout":
        say("Namespace run %s timed out after %d min; give it longer with depot-run --timeout MIN (the default is %s)"
            % (inst["id"], a.timeout, TIMEOUT_DEFAULTS)); return 1
    if rc == 0:
        say("Namespace run %s finished OK in %.0fs" % (inst["id"], total)); return 0
    say("Namespace run %s FAILED (exit %d) after %.0fs" % (inst["id"], rc, total)); return 1


if __name__ == "__main__":
    sys.exit(main())
