#!/usr/bin/env python3
"""depot-run's LANES (job 6, coordinator 2026-11-02 15:38Z): run a command on an agents' lane host, the same tree as on
Namespace (the merge base with origin/main plus your patch), through the host's serve over SSH. Runbook:
docs/runbooks/lane-host.md; protocol: serve.py `run`.

  lanes-run.py --hosts "lanes@HOST ..." --key FILE --base SHA --patch FILE --family rust|web|scripts --timeout MIN
               [--label TEXT] [--scratch-target] [--verdict-entry FILE] [--fresh] [--artifact PATH]...
               [--artifacts-dir DIR] --result FILE -- CMD...

The hosts are tried in order. Each gets one request, `ssh lanes@HOST run`, with a tar on stdin: request.json and
patch.diff. The host checks that the base is on its main, takes a free slot of the family, puts the base plus the patch in
the slot's worktree, checks the tree is exactly the one computed here, runs the command and streams its output. Its last
line is `LANES-REPORT {json}`, which this client reads and doesn't print; the command's exit status is the report's
"exit", so a command that itself exits 75 is never taken for "busy".

With no report, a host's 75 (busy, off, not ready), an SSH failure (255), 70 (serve's own error) or 2 (a request it
refused) all mean: try the next host. 3 means the patch doesn't apply to the base: no host would take it, so that is the
verdict.

BASE BUNDLE (coordinator 2026-11-02 17:2xZ, plan B: a lane host has no GitHub credential): when the host's mirror lacks
the base, its 75 carries `LANES-NEED-BASE {"id", "base", "head"}` (head: the mirror's main). If that head is an ancestor
of the base here, the same request goes to the same host once more with a thin bundle of main from the head
(`git bundle create base.bundle refs/remotes/origin/main ^HEAD_OF_HOST`, a tar member, "bundle" in request.json); the
host verifies it, fetches it and runs. A head this checkout doesn't have, or one not under the base: the next host.

REPOS (owner 2026-11-02 18:3xZ: Namespace only for overflow and coflow): this file is the same in contextstream,
contextcode and mcp-server; --repo names the host's mirror and slots (sent as "repo", except for contextstream), --family
the slot family. Only contextstream's --validate sends a verdict key, and only then is nsrun_verdicts.py loaded.

Exit status: the command's, when a host ran it (or reused a passing verdict for exactly this key); 3 when the patch
doesn't apply; 75 when no host ran it (depot-run then runs it on Namespace). --result gets
{"ran", "run", "where": "lanes", "host", "base", "rc", "reused"}, and with no run "why" the last host passed it over:
busy, unreachable, no-base (it lacked the base and took no bundle), too-big (the patch) or other (serve's 70 or 2).

Host keys are pinned (scripts/dev/lanes-known-hosts, StrictHostKeyChecking=yes); the key is the agents' serve key, forced
on the host to serve, so it can do nothing but ask for a run.
"""
import argparse, importlib.util, io, json, os, re, secrets, shlex, signal, subprocess, sys, tarfile, tempfile, threading, time

HERE = os.path.dirname(os.path.abspath(__file__))
REPORT = b"LANES-REPORT "
NEED_BASE = b"LANES-NEED-BASE "
KNOWN_HOSTS = os.environ.get("DEPOT_RUN_LANES_KNOWN_HOSTS", os.path.join(HERE, "lanes-known-hosts"))
CONNECT_S = int(os.environ.get("DEPOT_RUN_LANES_CONNECT_S", "10"))
MAX_PATCH = 48 * 1024 * 1024   # serve refuses a request over 64 MB


def say(msg):
    print("lanes: " + msg, file=sys.stderr, flush=True)


def expected_tree(base, patch_path):
    """TREE: base plus the patch, as the host must find it (git apply --cached in a scratch index)."""
    fd, index = tempfile.mkstemp(prefix="lanes-index-")
    os.close(fd)
    env = {**os.environ, "GIT_INDEX_FILE": index}
    try:
        os.unlink(index)
        subprocess.run(["git", "read-tree", base], env=env, check=True, capture_output=True)
        if os.path.getsize(patch_path):
            subprocess.run(["git", "apply", "--cached", "--binary", patch_path], env=env, check=True, capture_output=True)
        return subprocess.run(["git", "write-tree"], env=env, check=True, capture_output=True, text=True).stdout.strip()
    finally:
        if os.path.exists(index):
            os.unlink(index)


def verdicts():
    spec = importlib.util.spec_from_file_location("nsrun_verdicts", os.path.join(HERE, "nsrun_verdicts.py"))
    mod = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(mod)
    return mod


def agent_name():
    env = os.environ.get("DEVBOX", "")
    name = os.environ.get("CONTEXTSTREAM_AGENT_NAME") or os.environ.get("DEPOT_RUN_AGENT") or ""
    return "/".join(x for x in (env, name) if x)[:120]


def payload(request, patch, bundle=None):
    buf = io.BytesIO()
    members = [("request.json", json.dumps(request, sort_keys=True).encode() + b"\n"), ("patch.diff", patch)]
    if bundle is not None:
        members.append(("base.bundle", bundle))
    with tarfile.open(fileobj=buf, mode="w", format=tarfile.PAX_FORMAT) as t:
        for name, data in members:
            info = tarfile.TarInfo(name)
            info.size, info.mode, info.mtime = len(data), 0o644, int(time.time())
            t.addfile(info, io.BytesIO(data))
    return buf.getvalue()


def ssh_argv(host, key, *words):
    return ["ssh", "-i", key, "-o", "IdentitiesOnly=yes", "-o", "BatchMode=yes", "-o", "StrictHostKeyChecking=yes",
            "-o", "UserKnownHostsFile=" + KNOWN_HOSTS, "-o", "ConnectTimeout=%d" % CONNECT_S,
            "-o", "ServerAliveInterval=30", "-o", "ServerAliveCountMax=4", "-o", "LogLevel=ERROR", host, *words]


class Run:
    """One request to one host: stream stdout (minus the report line), stderr as it comes; the report and exit status."""

    def __init__(self, argv, data, guard_s):
        self.proc = subprocess.Popen(argv, stdin=subprocess.PIPE, stdout=subprocess.PIPE, start_new_session=True)
        self.report = None
        self.need = None
        self.printed = False
        threading.Thread(target=self.feed, args=(data,), daemon=True).start()
        self.timer = threading.Timer(guard_s, self.stop)
        self.timer.daemon = True
        self.timer.start()

    def feed(self, data):
        try:
            self.proc.stdin.write(data)
        except (BrokenPipeError, OSError):
            pass
        finally:
            try:
                self.proc.stdin.close()
            except OSError:
                pass

    def stop(self, *_):
        # The connection closing is serve's SIGHUP: the run's processes are killed and the slot freed.
        try:
            os.killpg(self.proc.pid, signal.SIGTERM)
        except OSError:
            pass

    def wait(self):
        out = sys.stdout.buffer
        for line in iter(self.proc.stdout.readline, b""):
            if line.startswith(REPORT):
                try:
                    self.report = json.loads(line[len(REPORT):])
                except ValueError:
                    self.report = None
                continue
            if line.startswith(NEED_BASE):
                try:
                    self.need = json.loads(line[len(NEED_BASE):])
                except ValueError:
                    self.need = None
                continue
            out.write(line)
            out.flush()
            self.printed = True
        rc = self.proc.wait()
        self.timer.cancel()
        return rc


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--hosts", required=True)
    ap.add_argument("--key", required=True)
    ap.add_argument("--base", required=True)
    ap.add_argument("--patch", required=True)
    ap.add_argument("--family", required=True, help="the host's slot family: rust, web, scripts (contextstream), gate, ...")
    ap.add_argument("--repo", default="contextstream", help="REPOS: whose mirror and slots (contextstream, contextcode, mcp-server)")
    ap.add_argument("--timeout", type=int, required=True, help="minutes")
    ap.add_argument("--label", default="")
    ap.add_argument("--scratch-target", action="store_true")
    ap.add_argument("--verdict-entry", default="")
    ap.add_argument("--fresh", action="store_true")
    ap.add_argument("--artifact", action="append", default=[])
    ap.add_argument("--artifacts-dir", default="")
    ap.add_argument("--result", default="")
    ap.add_argument("cmd", nargs=argparse.REMAINDER)
    a = ap.parse_args()
    cmd = a.cmd[1:] if a.cmd[:1] == ["--"] else a.cmd
    if not cmd:
        ap.error("no command")

    def result(**kw):
        if a.result:
            with open(a.result, "w") as f:
                json.dump({"where": "lanes", "base": a.base, **kw}, f)

    if not re.fullmatch(r"[a-z][a-z0-9-]{0,31}", a.family) or not re.fullmatch(r"[a-z][a-z0-9-]{0,63}", a.repo):
        ap.error("--family and --repo are short lowercase names")
    with open(a.patch, "rb") as f:
        patch = f.read()
    if len(patch) > MAX_PATCH:
        say("your patch is %d MB, more than a lane host takes: not on lanes" % (len(patch) >> 20))
        result(ran=False, rc=75, why="too-big")
        return 75
    try:
        tree = expected_tree(a.base, a.patch)
    except subprocess.CalledProcessError:
        say("your changes did not apply to %s" % a.base[:12])
        result(ran=False, rc=3)
        return 3
    run_id = "lanes-" + secrets.token_hex(8)
    entry, key = None, None
    if a.verdict_entry:
        with open(a.verdict_entry) as f:
            ki = json.load(f)
        key = ki["key"]
        # SHARED verdicts: the host keeps this after a passing command, filling in when it finished and how long it took.
        entry = {"schema": ki["inputs"]["schema"], "key": key, "inputs": ki["inputs"], "rc": 0, "run": run_id,
                 "where": "lanes", "base": a.base, "runner": "lanes-serve", "finished_at": "__FINISHED_AT__",
                 "duration_s": "__DURATION_S__"}
    request = {"v": 1, "id": run_id, "base": a.base, "tree": tree, "family": a.family,
               "command": shlex.join(cmd), "timeout_s": a.timeout * 60, "agent": agent_name(),
               "label": a.label[:200], "verdict_key": key, "verdict_entry": entry, "fresh": bool(a.fresh),
               "scratch_target": bool(a.scratch_target), "artifacts": a.artifact}
    if a.repo != "contextstream":   # REPOS: contextstream's requests stay as the host first knew them
        request["repo"] = a.repo
    data = payload(request, patch)
    guard_s = a.timeout * 60 + 600   # serve ends the run at the timeout; this only guards a host that went silent

    hosts = [h for h in re.split(r"[\s,]+", a.hosts) if h]
    attempts = [(h, None) for h in hosts]
    why_not = "other"   # OVERFLOW: why the last host passed the run over (depot-run labels the Namespace run with it)
    while attempts:
        host, mode = attempts.pop(0)
        refresh = mode == "fresh"
        if refresh:   # the host offered a kept verdict that doesn't hold: the same host, a fresh run
            request["fresh"] = True
        if isinstance(mode, bytes):   # BASE BUNDLE: the same request, with main from the host's head
            request["bundle"] = "base.bundle"
            data = payload(request, patch, mode)
            say("sending %s main up to your base (a %d KB bundle)" % (host, len(mode) >> 10))
        else:
            request.pop("bundle", None)
            data = payload(request, patch)
        say("on lane host %s (%s): base %s plus %d bytes of patch" % (host, a.family, a.base[:12], len(patch)))
        run = Run(ssh_argv(host, a.key, "run"), data, guard_s)
        stop = lambda signum, _f: (run.stop(), sys.exit(130))
        signal.signal(signal.SIGINT, stop)
        signal.signal(signal.SIGTERM, stop)
        rc = run.wait()
        rep = run.report
        if rep is not None and rep.get("id") == run_id:
            got = rep.get("exit")
            got = got if isinstance(got, int) else 1
            reused = bool(rep.get("reused"))
            if reused and not (got == 0 and key and verdicts().valid_shared(rep.get("verdict"), key)):
                say("%s offered a kept verdict that doesn't hold for this key: running it fresh" % host)
                if not refresh:
                    attempts.insert(0, (host, "fresh"))
                continue
            if reused:
                say("REUSED, no new run: lane run %s on %s passed this exact tree (%s), steps %s and validate code at %s. "
                    "--fresh runs it again." % (rep["verdict"]["run"], host, tree[:12],
                                                 ",".join(rep["verdict"]["inputs"].get("steps", [])),
                                                 rep["verdict"]["finished_at"]))
            else:
                say("%s run %s %s in %ss (slot %s, %s)" % (host, run_id, "finished OK" if got == 0 else "FAILED (exit %d)" % got,
                                                             rep.get("seconds", "?"), rep.get("slot", "?"),
                                                             "warm" if rep.get("warm") else "cold"))
                if a.artifact:
                    fetch_artifacts(host, a.key, run_id, a.artifacts_dir or os.path.join(".depot-run-artifacts", run_id))
            result(ran=True, run=rep["verdict"]["run"] if reused and got == 0 else run_id, host=host, rc=got, reused=reused)
            return got
        need = run.need
        if rc == 75 and isinstance(need, dict) and need.get("id") == run_id and need.get("base") == a.base:
            why_not = "no-base"
            if isinstance(mode, bytes):
                say("%s still lacks %s after the bundle" % (host, a.base[:12]))
            else:
                bundle, why = base_bundle(a.base, need.get("head"))
                if bundle is not None:
                    attempts.insert(0, (host, bundle))
                    continue
                say("%s doesn't have your base %s, and no bundle for it: %s" % (host, a.base[:12], why))
            continue
        if rc == 3:
            say("your changes did not apply to %s (%s)" % (a.base[:12], host))
            result(ran=False, rc=3)
            return 3
        why = {75: "busy or not ready", 255: "unreachable", 70: "serve's own error", 2: "refused the request"}.get(
            rc, "ended with %d and no report" % rc)
        why_not = {75: "busy", 255: "unreachable"}.get(rc, "other")
        # 75 and 2: nothing ran (what came out is serve's own status line). Otherwise a run may have started and stopped.
        unfinished = run.printed and rc not in (75, 2)
        say("%s: %s%s" % (host, why, "; its output above is from a run that didn't finish" if unfinished else ""))
    result(ran=False, rc=75, why=why_not)
    return 75


def base_bundle(base, head):
    """BASE BUNDLE: main from the host's head, thin (its one prerequisite is that head); None when it can't be made here."""
    git = lambda *a: subprocess.run(["git", *a], capture_output=True)
    if not (isinstance(head, str) and re.fullmatch(r"[0-9a-f]{40}", head)):
        return None, "the host named no head"
    if git("cat-file", "-e", head + "^{commit}").returncode != 0:
        return None, "its main (%s) isn't in this checkout" % head[:12]
    if git("merge-base", "--is-ancestor", head, base).returncode != 0:
        return None, "its main (%s) isn't under your base" % head[:12]
    fd, path = tempfile.mkstemp(suffix=".bundle")
    os.close(fd)
    try:
        made = git("bundle", "create", path, "refs/remotes/origin/main", "^" + head)
        if made.returncode != 0:
            return None, "git bundle failed: %s" % made.stderr.decode(errors="replace").strip()[-200:]
        with open(path, "rb") as f:
            data = f.read()
    finally:
        os.unlink(path)
    if len(data) > MAX_PATCH:
        return None, "the bundle is %d MB, more than a lane host takes" % (len(data) >> 20)
    return data, ""


def fetch_artifacts(host, key, run_id, dest):
    got = subprocess.run(ssh_argv(host, key, "artifacts", run_id), capture_output=True, stdin=subprocess.DEVNULL)
    if got.returncode == 0 and got.stdout:
        os.makedirs(dest, exist_ok=True)
        subprocess.run(["tar", "-x", "-f", "-", "-C", dest], input=got.stdout)
        say("artifacts in " + dest)
    else:
        say("no artifacts came back from %s" % host)


if __name__ == "__main__":
    sys.exit(main())
