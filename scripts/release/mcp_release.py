#!/usr/bin/env python3
"""The MCP server release, run by Depot CI (.depot/workflows/release.yml). No person takes part.

  plan          What this run does. The version is the one package.json, Cargo.toml and server.json name at the
                pushed commit (a "prepare MCP X.Y.Z" commit raises it). A version whose every public channel is
                done ships nothing. Once tag vX.Y.Z exists, its commit is the release, whatever main holds since.
                Once R2 holds the complete bundle, a re-run publishes those bytes and builds nothing.
  macho-check   Check a darwin binary built on Linux: architecture, linked libraries, minimum macOS, signature.
  assemble      SBOM, version.json and checksums.txt for the six binaries (release_contract.py rules).
  fetch-bundle  Download the bundle a previous run put on R2 and verify it against the tag.
  publish       In this order, each step a no-op when already done with the same bytes: the annotated tag, R2
                mcp/vX.Y.Z then mcp/latest, a GitHub draft with the nine assets, npm, the MCP Registry, and the
                GitHub release published as Latest. A dry run packs and validates but publishes nothing.
  verify        Every public channel from the outside, as a user reaches it.
  await-hosted  Wait until the hosted gateway serves the version. The private deployment pins and rolls it on
                its own after it sees the published release; this repository never dispatches it.

Credentials arrive as environment variables from Depot CI secrets that only main's release workflow receives:
MCP_RELEASE_GITHUB_TOKEN (tag, GitHub release, MCP Registry login), MCP_RELEASE_NPM_TOKEN,
MCP_RELEASE_R2_BUCKET_URL, MCP_RELEASE_R2_ACCESS_KEY_ID and MCP_RELEASE_R2_SECRET_ACCESS_KEY. None is ever
printed, written to a file or put on a command line.
"""
from __future__ import annotations

import argparse
import base64
import datetime
import hashlib
import hmac
import importlib.util
import json
import os
import re
import shutil
import struct
import subprocess
import sys
import tarfile
import tempfile
import time
import urllib.error
import urllib.parse
import urllib.request
from pathlib import Path

sys.dont_write_bytecode = True
ROOT = Path(__file__).resolve().parent.parent.parent
REPO = "contextstream/mcp-server"
GITHUB_API = "https://api.github.com"
GITHUB_UPLOADS = "https://uploads.github.com"
NPM_PACKAGE = "@contextstream/mcp-server"
NPM_REGISTRY = "https://registry.npmjs.org"
MCP_REGISTRY = "https://registry.modelcontextprotocol.io"
R2_PUBLIC = "https://pub-68429b9f7857416c9484b75bf1887b96.r2.dev/mcp"
HOSTED_HEALTH = "https://mcp.contextstream.io/health"
TAGGER = ("ContextStream MCP release", "noreply@contextstream.io")

SYFT = ("https://github.com/anchore/syft/releases/download/v1.51.1/syft_1.51.1_linux_amd64.tar.gz",
        "8fcb33017a0dc1058298c923c436d19dfa68ae93968e0b423248542e3afb9fc3")
MCP_PUBLISHER = ("https://github.com/modelcontextprotocol/registry/releases/download/v1.8.1/mcp-publisher_linux_amd64.tar.gz",
                 "a06c9096dcb9727c13555b6be26c7effa707b01f06a4c561ba7a3635443cf2cc")

# What a darwin binary may link. A binary built on a Mac links exactly these; libiconv carries no symbol the
# server uses, so a linker that drops it is fine.
DARWIN_DYLIBS = {
    "/System/Library/Frameworks/CoreFoundation.framework/Versions/A/CoreFoundation": True,
    "/System/Library/Frameworks/CoreServices.framework/Versions/A/CoreServices": True,
    "/System/Library/Frameworks/SystemConfiguration.framework/Versions/A/SystemConfiguration": True,
    "/usr/lib/libSystem.B.dylib": True,
    "/usr/lib/libiconv.2.dylib": False,
}
DARWIN_ARCH = {"contextstream-mcp-darwin-arm64": (0x0100000C, (11, 0)), "contextstream-mcp-darwin-x64": (0x01000007, (10, 12))}

SHA = re.compile(r"[0-9a-f]{40}")
SECRET_NAMES = ("MCP_RELEASE_GITHUB_TOKEN", "MCP_RELEASE_NPM_TOKEN", "MCP_RELEASE_R2_BUCKET_URL",
                "MCP_RELEASE_R2_ACCESS_KEY_ID", "MCP_RELEASE_R2_SECRET_ACCESS_KEY")


def _load_contract():
    spec = importlib.util.spec_from_file_location("release_contract", ROOT / ".github/scripts/release_contract.py")
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


contract = _load_contract()
PAYLOAD = tuple(contract.PAYLOAD_FILES)
BINARIES = tuple(contract.RELEASE_ARTIFACTS)
# R2 upload order: version.json last, so its presence means the bundle under mcp/vX.Y.Z is complete.
UPLOAD_ORDER = tuple(sorted(PAYLOAD, key=lambda name: (name == "version.json", name == "checksums.txt", name)))


class Refused(Exception):
    """A fixed reason without a credential."""


def require(condition, message):
    if not condition:
        raise Refused(message)


def say(**fields):
    print(json.dumps(fields, sort_keys=True), flush=True)


def secret(name):
    value = os.environ.get(name, "").strip()
    require(value, name + " is not set (a Depot CI secret for main's release workflow)")
    return value


def sha256_bytes(data):
    return hashlib.sha256(data).hexdigest()


def sha256_file(path):
    digest = hashlib.sha256()
    with open(path, "rb") as handle:
        for chunk in iter(lambda: handle.read(1 << 20), b""):
            digest.update(chunk)
    return digest.hexdigest()


# -- HTTP --------------------------------------------------------------------------------------------------------------


def http(method, url, data=None, headers=None, attempts=4, timeout=120):
    """(status, body bytes). Retries connection errors and 5xx; returns 4xx to the caller. Never logs headers."""
    headers = dict(headers or {})
    headers.setdefault("User-Agent", "contextstream-mcp-release")
    last = None
    for attempt in range(attempts):
        request = urllib.request.Request(url, data=data, method=method, headers=headers)
        try:
            with urllib.request.urlopen(request, timeout=timeout) as response:
                return response.status, response.read()
        except urllib.error.HTTPError as error:
            body = error.read()
            if error.code < 500 and error.code != 429:
                return error.code, body
            last = "HTTP %d" % error.code
        except (urllib.error.URLError, TimeoutError, ConnectionError) as error:
            last = type(error).__name__
        time.sleep(3 * (attempt + 1))
    raise Refused("%s %s failed: %s" % (method, url.split("?")[0], last))


def get_json(url, headers=None):
    status, body = http("GET", url, headers=headers)
    return status, (json.loads(body) if status == 200 else None)


def github_headers(token=None):
    headers = {"Accept": "application/vnd.github+json", "X-GitHub-Api-Version": "2022-11-28"}
    token = token if token is not None else os.environ.get("MCP_RELEASE_GITHUB_TOKEN") or os.environ.get("GITHUB_READ_TOKEN")
    if token:
        headers["Authorization"] = "Bearer " + token.strip()
    return headers


# -- Public state ------------------------------------------------------------------------------------------------------


def npm_document():
    status, doc = get_json(NPM_REGISTRY + "/" + urllib.parse.quote(NPM_PACKAGE, safe="@"))
    require(status == 200, "npm answered HTTP %d for %s" % (status, NPM_PACKAGE))
    return doc


def registry_url(version):
    return MCP_REGISTRY + "/v0.1/servers/" + urllib.parse.quote("io.github.contextstream/mcp-server", safe="") + "/versions/" + version


def github_release(tag, headers):
    """The release for `tag`, drafts included (the tags endpoint hides drafts), or None."""
    status, releases = get_json(GITHUB_API + "/repos/" + REPO + "/releases?per_page=30", headers)
    require(status == 200, "GitHub releases answered HTTP %d" % status)
    matching = [release for release in releases if release.get("tag_name") == tag]
    require(len(matching) <= 1, "more than one GitHub release for " + tag)
    return matching[0] if matching else None


def r2_public(path):
    status, body = http("GET", R2_PUBLIC + "/" + path, headers={"Cache-Control": "no-cache"})
    return body if status == 200 else None


def channel_state(version, headers):
    """What each public channel holds for `version`."""
    tag = "v" + version
    doc = npm_document()
    release = github_release(tag, headers)
    latest_status, latest = get_json(GITHUB_API + "/repos/" + REPO + "/releases/latest", headers)
    registry_status, _ = http("GET", registry_url(version))
    r2_manifest = r2_public("v%s/version.json" % version)
    r2_latest = r2_public("latest/version.json")
    return {
        "npm": version in doc.get("versions", {}),
        "npm_latest": doc.get("dist-tags", {}).get("latest"),
        "github": "absent" if release is None else ("draft" if release.get("draft") else "published"),
        "github_latest": latest.get("tag_name") if latest_status == 200 else None,
        "registry": registry_status == 200,
        "r2_commit": json.loads(r2_manifest)["source"]["commit"] if r2_manifest else None,
        "r2_latest": json.loads(r2_latest).get("version") if r2_latest else None,
    }


def decide(version, mode, tag_commit, head, state):
    """(ship, build, commit, reason) for a run. Pure: the tests drive it."""
    relation = contract.stable_version_relation
    if mode == "dry-run":
        return True, True, head, "dry run of %s at %s" % (version, head[:12])
    commit = tag_commit or head
    done = (state["npm"] and state["registry"] and state["github"] == "published" and state["r2_commit"] == commit
            and state["r2_latest"] is not None and relation(state["r2_latest"], version) != "older")
    if done:
        return False, False, commit, "v%s is fully released" % version
    if state["npm_latest"] and relation(version, state["npm_latest"]) == "older":
        raise Refused("v%s is older than npm latest %s; a release never goes back" % (version, state["npm_latest"]))
    if tag_commit is None:
        require(not state["npm"] and state["github"] == "absent" and state["r2_commit"] is None,
                "v%s has public artifacts but no tag; this needs a person" % version)
    if state["r2_commit"] is not None:
        require(state["r2_commit"] == commit, "R2 holds v%s from %s, not %s" % (version, state["r2_commit"][:12], commit[:12]))
        return True, False, commit, "resume v%s from the bundle on R2" % version
    return True, True, commit, ("resume" if tag_commit else "release") + " v%s at %s" % (version, commit[:12])


def remote_tag_commit(tag):
    rows = subprocess.run(["git", "ls-remote", "origin", "refs/tags/" + tag, "refs/tags/" + tag + "^{}"], cwd=ROOT,
                          capture_output=True, text=True, check=True, timeout=60).stdout.split("\n")
    refs = dict(reversed(row.split()) for row in rows if row.strip())
    if not refs:
        return None
    peeled = refs.get("refs/tags/" + tag + "^{}")
    require(peeled, tag + " is not an annotated tag")
    return peeled


def cmd_plan(args):
    version = contract.verify_repository_metadata(ROOT)
    head = subprocess.run(["git", "rev-parse", "HEAD"], cwd=ROOT, capture_output=True, text=True, check=True).stdout.strip()
    require(SHA.fullmatch(head), "no commit checked out")
    mode = args.mode
    patched = subprocess.run(["git", "status", "--porcelain"], cwd=ROOT, capture_output=True, text=True,
                             check=True).stdout.strip()
    patched = patched or head != os.environ.get("GITHUB_SHA", head)
    if mode == "release" and (args.ref != "refs/heads/main" or patched):
        # Only a clean checkout of main publishes; any other run (a branch, a laptop `depot ci run` patch) is a dry run.
        say(stage="plan", detail="not a clean checkout of main (%s%s): dry run" % (args.ref, ", patched" if patched else ""))
        mode = "dry-run"
    tag_commit = remote_tag_commit("v" + version)
    if tag_commit:
        status, compare = get_json("%s/repos/%s/compare/%s...main" % (GITHUB_API, REPO, tag_commit), github_headers())
        require(status == 200 and compare.get("status") in ("ahead", "identical"), "tag v%s is not on main" % version)
    state = channel_state(version, github_headers())
    ship, build, commit, reason = decide(version, mode, tag_commit, head, state)
    if commit != head:
        subprocess.run(["git", "fetch", "--quiet", "--depth", "1", "origin", commit], cwd=ROOT, check=True, timeout=300)
    epoch = subprocess.run(["git", "show", "-s", "--format=%ct", commit], cwd=ROOT, capture_output=True, text=True,
                           check=True).stdout.strip()
    say(stage="plan", version=version, mode=mode, ship=ship, build=build, commit=commit, reason=reason, **state)
    outputs = {"ship": ship, "build": build, "publish": ship and mode == "release", "mode": mode, "version": version,
               "commit": commit, "epoch": epoch}
    if os.environ.get("GITHUB_OUTPUT"):
        with open(os.environ["GITHUB_OUTPUT"], "a") as handle:
            for key, value in outputs.items():
                handle.write("%s=%s\n" % (key, str(value).lower() if isinstance(value, bool) else value))
    if os.environ.get("GITHUB_STEP_SUMMARY"):
        with open(os.environ["GITHUB_STEP_SUMMARY"], "a") as handle:
            handle.write("**MCP release plan:** %s\n" % reason)


# -- Darwin binaries ---------------------------------------------------------------------------------------------------


def macho_facts(data):
    """cputype, linked dylibs, minimum macOS (major, minor) and whether a code signature is present."""
    require(len(data) > 32 and struct.unpack_from("<I", data, 0)[0] == 0xFEEDFACF, "not a thin 64-bit Mach-O file")
    cputype, _, filetype, ncmds, _, _ = struct.unpack_from("<iiIIII", data, 4)
    require(filetype == 2, "not an executable")
    offset, dylibs, minos, signed = 32, [], None, False
    for _ in range(ncmds):
        cmd, size = struct.unpack_from("<II", data, offset)
        require(size >= 8 and offset + size <= len(data), "malformed load command")
        if cmd in (0xC, 0x80000018, 0x8000001F, 0x20, 0x80000023):
            name_offset = struct.unpack_from("<I", data, offset + 8)[0]
            dylibs.append(data[offset + name_offset:offset + size].split(b"\0", 1)[0].decode())
        elif cmd == 0x32:
            raw = struct.unpack_from("<I", data, offset + 12)[0]
            minos = (raw >> 16, (raw >> 8) & 0xFF)
        elif cmd == 0x24:
            raw = struct.unpack_from("<I", data, offset + 8)[0]
            minos = (raw >> 16, (raw >> 8) & 0xFF)
        elif cmd == 0x1D:
            signed = True
        offset += size
    return cputype & 0xFFFFFFFF, dylibs, minos, signed


def check_darwin(path, version):
    name = Path(path).name
    require(name in DARWIN_ARCH, name + " is not a darwin release artifact")
    data = Path(path).read_bytes()
    cputype, dylibs, minos, signed = macho_facts(data)
    want_cpu, want_minos = DARWIN_ARCH[name]
    require(cputype == want_cpu, "%s has cputype %#x" % (name, cputype))
    unknown = sorted(set(dylibs) - set(DARWIN_DYLIBS))
    require(not unknown, "%s links libraries a Mac may not have: %s" % (name, ", ".join(unknown)))
    missing = sorted(lib for lib, required in DARWIN_DYLIBS.items() if required and lib not in dylibs)
    require(not missing, "%s does not link %s" % (name, ", ".join(missing)))
    require(minos == want_minos, "%s needs macOS %s, not %s" % (name, minos, want_minos))
    require(signed or cputype != 0x0100000C, name + " has no code signature; macOS refuses an unsigned arm64 binary")
    require(version.encode() in data, "%s does not carry version %s" % (name, version))
    return {"artifact": name, "dylibs": dylibs, "minos": "%d.%d" % minos, "signed": signed}


def cmd_macho_check(args):
    say(stage="macho-check", **check_darwin(args.path, args.version))


# -- Assemble ----------------------------------------------------------------------------------------------------------


def fetch_tool(url_and_sha, member, work):
    url, digest = url_and_sha
    archive = work / Path(urllib.parse.urlparse(url).path).name
    status, body = http("GET", url, timeout=300)
    require(status == 200 and sha256_bytes(body) == digest, "the pinned download " + archive.name + " did not verify")
    archive.write_bytes(body)
    with tarfile.open(archive) as handle:
        handle.extract(member, work, filter="data")
    tool = work / member
    tool.chmod(0o755)
    return tool


def cmd_assemble(args):
    dist = Path(args.dist)
    for name in BINARIES:
        require((dist / name).is_file(), "the build left no " + name)
        (dist / name).chmod(0o755)
    for name in ("contextstream-mcp-darwin-arm64", "contextstream-mcp-darwin-x64"):
        check_darwin(dist / name, args.version)
    extra = sorted(path.name for path in dist.iterdir() if path.name not in BINARIES)
    require(not extra, "unexpected files in the bundle: " + ", ".join(extra))
    with tempfile.TemporaryDirectory() as work:
        syft = fetch_tool(SYFT, "syft", Path(work))
        subprocess.run([str(syft), "scan", "dir:" + str(dist), "--quiet", "--output", "spdx-json=" + str(dist / "sbom.spdx.json")],
                       check=True, stdin=subprocess.DEVNULL, timeout=600)
    contract.generate_release_files(dist, args.version, args.commit)
    contract.verify_release_files(dist, args.version, args.commit)
    say(stage="assemble", version=args.version, commit=args.commit,
        files={name: sha256_file(dist / name) for name in PAYLOAD})


def cmd_fetch_bundle(args):
    dist = Path(args.dist)
    dist.mkdir(parents=True, exist_ok=True)
    for name in PAYLOAD:
        body = r2_public("v%s/%s" % (args.version, name))
        require(body is not None, "R2 has no v%s/%s" % (args.version, name))
        (dist / name).write_bytes(body)
    contract.verify_release_files(dist, args.version, args.commit)
    say(stage="fetch-bundle", version=args.version, commit=args.commit, source="R2 mcp/v" + args.version)


# -- R2 (S3 API, SigV4, bucket-scoped key) -----------------------------------------------------------------------------


class Bucket:
    def __init__(self):
        self.base = secret("MCP_RELEASE_R2_BUCKET_URL").rstrip("/")
        self.key_id = secret("MCP_RELEASE_R2_ACCESS_KEY_ID")
        self.key = secret("MCP_RELEASE_R2_SECRET_ACCESS_KEY")

    def _signed(self, method, key, body, query=None):
        url = self.base + "/" + urllib.parse.quote(key, safe="/")
        # SigV4 signs the query in canonical form: names and values URI-encoded, sorted by name.
        canonical_query = "&".join("%s=%s" % (urllib.parse.quote(name, safe="-_.~"), urllib.parse.quote(value, safe="-_.~"))
                                   for name, value in sorted((query or {}).items()))
        parts = urllib.parse.urlparse(url)
        now = datetime.datetime.now(datetime.timezone.utc)
        stamp, day = now.strftime("%Y%m%dT%H%M%SZ"), now.strftime("%Y%m%d")
        payload = sha256_bytes(body)
        canonical = "\n".join([method, parts.path, canonical_query, "host:" + parts.netloc, "x-amz-content-sha256:" + payload,
                               "x-amz-date:" + stamp, "", "host;x-amz-content-sha256;x-amz-date", payload])
        scope = day + "/auto/s3/aws4_request"
        to_sign = "\n".join(["AWS4-HMAC-SHA256", stamp, scope, sha256_bytes(canonical.encode())])
        signing = ("AWS4" + self.key).encode()
        for part in (day, "auto", "s3", "aws4_request"):
            signing = hmac.new(signing, part.encode(), hashlib.sha256).digest()
        signature = hmac.new(signing, to_sign.encode(), hashlib.sha256).hexdigest()
        headers = {"x-amz-date": stamp, "x-amz-content-sha256": payload,
                   "Authorization": "AWS4-HMAC-SHA256 Credential=%s/%s, SignedHeaders=host;x-amz-content-sha256;x-amz-date, "
                                    "Signature=%s" % (self.key_id, scope, signature)}
        return http(method, url + ("?" + canonical_query if canonical_query else ""), data=body if method == "PUT" else None,
                    headers=headers, timeout=600)

    def get(self, key):
        status, body = self._signed("GET", key, b"")
        require(status in (200, 404), "R2 GET %s answered HTTP %d" % (key, status))
        return body if status == 200 else None

    def put(self, key, body):
        status, _ = self._signed("PUT", key, body)
        require(status == 200, "R2 PUT %s answered HTTP %d" % (key, status))
        require(self.get(key) == body, "R2 read-back of %s differs" % key)

    def list(self, prefix):
        """The keys under `prefix` (one page of up to 1000)."""
        status, body = self._signed("GET", "", b"", {"list-type": "2", "prefix": prefix, "max-keys": "1000"})
        require(status == 200, "R2 LIST %s answered HTTP %d" % (prefix, status))
        return re.findall(r"<Key>([^<]*)</Key>", body.decode())

    def delete(self, key):
        status, _ = self._signed("DELETE", key, b"")
        require(status in (200, 204), "R2 DELETE %s answered HTTP %d" % (key, status))


def cmd_r2_probe(args):
    """Prove the R2 key on a probe object (list, put, get, delete) without touching a release."""
    bucket = Bucket()
    key = "mcp/.release-probe/%s-%d" % (os.environ.get("GITHUB_RUN_ID", "local"), int(time.time()))
    body = os.urandom(32)
    require(bucket.get("mcp/latest/version.json") is not None, "R2 has no mcp/latest/version.json")
    bucket.put(key, body)
    require(key in bucket.list("mcp/.release-probe/"), "the probe object is not listed")
    bucket.delete(key)
    require(bucket.get(key) is None, "the probe object is still there after DELETE")
    say(stage="r2-probe", result="R2_PROBE_OK", key=key)


def publish_r2(dist, version, npm_published):
    bucket = Bucket()
    prefix = "mcp/v" + version + "/"
    # Without version.json (written last) the bundle never finished, and npm, which points users at it, came after.
    complete = bucket.get(prefix + "version.json") is not None
    for name in UPLOAD_ORDER:
        local = (dist / name).read_bytes()
        remote = bucket.get(prefix + name)
        if remote == local:
            say(stage="r2", key=prefix + name, result="same")
            continue
        if remote is not None:
            require(not complete and not npm_published, "refusing to overwrite different bytes at " + prefix + name)
        bucket.put(prefix + name, local)
        say(stage="r2", key=prefix + name, result="put" if remote is None else "replaced unfinished upload")
    current = bucket.get("mcp/latest/version.json")
    if current is not None:
        latest = json.loads(current)["version"]
        relation = contract.stable_version_relation(version, latest)
        require(relation != "older", "refusing to move mcp/latest back from %s to %s" % (latest, version))
    for name in UPLOAD_ORDER:
        local = (dist / name).read_bytes()
        if bucket.get("mcp/latest/" + name) != local:
            bucket.put("mcp/latest/" + name, local)
    for path in ("v%s/version.json" % version, "v%s/checksums.txt" % version, "latest/version.json"):
        for attempt in range(20):
            if r2_public(path) == (dist / Path(path).name).read_bytes():
                break
            time.sleep(6)
        else:
            raise Refused("the public R2 route does not serve the new " + path)
    say(stage="r2", result="R2_OK", version=version)


# -- Tag ---------------------------------------------------------------------------------------------------------------


def git_with_token(*args):
    token = secret("MCP_RELEASE_GITHUB_TOKEN")
    basic = base64.b64encode(("x-access-token:" + token).encode()).decode()
    env = {key: value for key, value in os.environ.items() if not key.startswith("MCP_RELEASE_")}
    env.update({"GIT_CONFIG_COUNT": "1", "GIT_CONFIG_KEY_0": "http.https://github.com/.extraheader",
                "GIT_CONFIG_VALUE_0": "AUTHORIZATION: basic " + basic, "GIT_TERMINAL_PROMPT": "0"})
    done = subprocess.run(["git", *args], cwd=ROOT, env=env, capture_output=True, text=True, stdin=subprocess.DEVNULL, timeout=300)
    require(done.returncode == 0, "git %s failed: %s" % (args[0], (done.stderr or done.stdout).strip()[-300:]))


def ensure_tag(version, commit):
    tag = "v" + version
    existing = remote_tag_commit(tag)
    if existing is None:
        subprocess.run(["git", "-c", "user.name=" + TAGGER[0], "-c", "user.email=" + TAGGER[1], "tag", "-f", "-a", tag,
                        "-m", "MCP " + version, commit], cwd=ROOT, check=True, capture_output=True)
        git_with_token("push", "origin", "refs/tags/%s:refs/tags/%s" % (tag, tag))
        say(stage="tag", tag=tag, commit=commit, result="pushed")
    else:
        require(existing == commit, "%s already points at %s" % (tag, existing[:12]))
        say(stage="tag", tag=tag, commit=commit, result="exists")
    contract.verify_remote_tag(ROOT, tag, commit)


# -- GitHub release ----------------------------------------------------------------------------------------------------


def changelog_section(text, version):
    """The CHANGELOG.md section of `version`, without its heading: the GitHub release notes."""
    match = re.search(r"^## " + re.escape(version) + r"[^\n]*\n(.*?)(?=^## |\Z)", text, re.M | re.S)
    require(match and match.group(1).strip(), "CHANGELOG.md has no section for " + version)
    return match.group(1).strip() + "\n"


def asset_matches(asset, path):
    digest = asset.get("digest")
    if digest:
        return digest == "sha256:" + sha256_file(path)
    status, body = http("GET", asset["browser_download_url"], timeout=600)
    return status == 200 and body == Path(path).read_bytes()


def github_draft(dist, version):
    headers = github_headers(secret("MCP_RELEASE_GITHUB_TOKEN"))
    tag = "v" + version
    release = github_release(tag, headers)
    if release is None:
        body = json.dumps({"tag_name": tag, "name": "MCP " + version, "draft": True, "prerelease": False,
                           "make_latest": "false",
                           "body": changelog_section((ROOT / "CHANGELOG.md").read_text(), version)}).encode()
        status, raw = http("POST", GITHUB_API + "/repos/" + REPO + "/releases", data=body,
                           headers={**headers, "Content-Type": "application/json"})
        require(status == 201, "creating the GitHub draft answered HTTP %d" % status)
        release = json.loads(raw)
        say(stage="github", release=release["id"], result="draft created")
    require(not release.get("prerelease"), tag + " is a prerelease on GitHub")
    present = {asset["name"]: asset for asset in release.get("assets", [])}
    unexpected = sorted(set(present) - set(PAYLOAD))
    require(not unexpected, "the GitHub release has unexpected assets: " + ", ".join(unexpected))
    for name, asset in present.items():
        require(asset_matches(asset, dist / name), "GitHub asset %s has different bytes" % name)
    missing = [name for name in PAYLOAD if name not in present]
    require(not missing or release.get("draft"), "the published GitHub release lacks " + ", ".join(missing))
    for name in missing:
        url = "%s/repos/%s/releases/%d/assets?name=%s" % (GITHUB_UPLOADS, REPO, release["id"], urllib.parse.quote(name))
        status, _ = http("POST", url, data=(dist / name).read_bytes(),
                         headers={**headers, "Content-Type": "application/octet-stream"}, timeout=600)
        require(status == 201, "uploading %s answered HTTP %d" % (name, status))
        say(stage="github", asset=name, result="uploaded")
    status, assets = get_json("%s/repos/%s/releases/%d/assets?per_page=50" % (GITHUB_API, REPO, release["id"]), headers)
    require(status == 200 and sorted(asset["name"] for asset in assets) == sorted(PAYLOAD), "the GitHub assets are not the nine")
    for asset in assets:
        require(asset_matches(asset, dist / asset["name"]), "GitHub asset %s has different bytes" % asset["name"])
    say(stage="github", release=release["id"], draft=release.get("draft"), result="nine assets verified")
    return release["id"]


def github_finalize(release_id, version):
    headers = github_headers(secret("MCP_RELEASE_GITHUB_TOKEN"))
    status, release = get_json("%s/repos/%s/releases/%d" % (GITHUB_API, REPO, release_id), headers)
    require(status == 200, "reading the GitHub release answered HTTP %d" % status)
    status, latest = get_json(GITHUB_API + "/repos/" + REPO + "/releases/latest", headers)
    if not release["draft"] and status == 200 and latest.get("tag_name") == "v" + version:
        say(stage="github", result="already published as Latest")
        return
    status, _ = http("PATCH", "%s/repos/%s/releases/%d" % (GITHUB_API, REPO, release_id),
                     data=json.dumps({"draft": False, "make_latest": "true"}).encode(),
                     headers={**headers, "Content-Type": "application/json"})
    require(status == 200, "publishing the GitHub release answered HTTP %d" % status)
    say(stage="github", result="published as Latest", tag="v" + version)


# -- npm ---------------------------------------------------------------------------------------------------------------


def npm_pack(work):
    done = subprocess.run(["npm", "pack", "--json", "--pack-destination", str(work)], cwd=ROOT, capture_output=True,
                          text=True, check=True, stdin=subprocess.DEVNULL, timeout=300)
    packed = json.loads(done.stdout)[0]
    return work / packed["filename"], packed["integrity"], len(packed["files"])


def publish_npm(version, dry_run, work):
    tarball, integrity, files = npm_pack(work)
    say(stage="npm", tarball=tarball.name, integrity=integrity, files=files)
    published = npm_document().get("versions", {}).get(version)
    if published:
        require(published["dist"]["integrity"] == integrity, "npm holds different bytes for " + version)
        say(stage="npm", result="already published, same integrity")
        return integrity
    if dry_run:
        subprocess.run(["npm", "publish", str(tarball), "--dry-run", "--access", "public", "--provenance=false"], cwd=work,
                       check=True, stdin=subprocess.DEVNULL, timeout=300)
        return integrity
    npmrc = work / "npmrc"
    # The file names the variable; npm reads the token from the environment.
    npmrc.write_text("//registry.npmjs.org/:_authToken=${MCP_RELEASE_NPM_TOKEN}\n")
    env = {key: value for key, value in os.environ.items() if not key.startswith("MCP_RELEASE_")}
    env["MCP_RELEASE_NPM_TOKEN"] = secret("MCP_RELEASE_NPM_TOKEN")
    done = subprocess.run(["npm", "publish", str(tarball), "--access", "public", "--provenance=false", "--userconfig", str(npmrc)],
                          cwd=work, env=env, stdin=subprocess.DEVNULL, timeout=600)
    require(done.returncode == 0, "npm publish failed")
    for attempt in range(20):
        published = npm_document().get("versions", {}).get(version)
        if published:
            require(published["dist"]["integrity"] == integrity, "npm returned different bytes after publishing")
            say(stage="npm", result="published", version=version)
            return integrity
        time.sleep(3 * (attempt + 1))
    raise Refused("npm %s did not become readable" % version)


# -- MCP Registry ------------------------------------------------------------------------------------------------------


def registry_verify(version, work):
    status, body = http("GET", registry_url(version))
    if status != 200:
        return False
    (work / "registry.json").write_bytes(body)
    contract.verify_registry_response(ROOT / "server.json", work / "registry.json")
    return True


def publish_registry(version, dry_run, work):
    publisher = fetch_tool(MCP_PUBLISHER, "mcp-publisher", work)
    subprocess.run([str(publisher), "validate", str(ROOT / "server.json")], check=True, stdin=subprocess.DEVNULL, timeout=120)
    if registry_verify(version, work):
        say(stage="registry", result="already published and identical")
        return
    if dry_run:
        say(stage="registry", result="validated (dry run)")
        return
    exchange = json.dumps({"github_token": secret("MCP_RELEASE_GITHUB_TOKEN")}).encode()
    status, raw = http("POST", MCP_REGISTRY + "/v0.1/auth/github-at", data=exchange, headers={"Content-Type": "application/json"})
    require(status == 200, "the registry token exchange answered HTTP %d" % status)
    token = json.loads(raw)["registry_token"]
    claims_part = token.split(".")[1]
    claims = json.loads(base64.urlsafe_b64decode(claims_part + "=" * (-len(claims_part) % 4)))
    resources = [permission.get("resource") for permission in claims.get("permissions", [])]
    require("io.github.contextstream/*" in resources, "the registry login does not grant io.github.contextstream/*")
    status, raw = http("POST", MCP_REGISTRY + "/v0.1/publish", data=(ROOT / "server.json").read_bytes(),
                       headers={"Content-Type": "application/json", "Authorization": "Bearer " + token}, attempts=1)
    require(status == 200, "registry publish answered HTTP %d: %s" % (status, raw[:300].decode(errors="replace")))
    for attempt in range(15):
        if registry_verify(version, work):
            say(stage="registry", result="published", version=version)
            return
        time.sleep(4)
    raise Refused("the registry entry for %s did not become readable" % version)


# -- publish, verify, await ------------------------------------------------------------------------------------------


def cmd_publish(args):
    dist, version, commit = Path(args.dist), args.version, args.commit
    dry_run = args.mode == "dry-run"
    contract.verify_release_files(dist, version, commit)
    require(subprocess.run(["git", "rev-parse", "HEAD"], cwd=ROOT, capture_output=True, text=True).stdout.strip() == commit,
            "the checkout is not the release commit")
    changelog_section((ROOT / "CHANGELOG.md").read_text(), version)
    with tempfile.TemporaryDirectory() as tmp:
        work = Path(tmp)
        if dry_run:
            publish_npm(version, True, work)
            publish_registry(version, True, work)
            say(stage="publish", result="DRY_RUN_OK", version=version, commit=commit,
                notes=len(changelog_section((ROOT / "CHANGELOG.md").read_text(), version)))
            return
        ensure_tag(version, commit)
        publish_r2(dist, version, version in npm_document().get("versions", {}))
        release_id = github_draft(dist, version)
        publish_npm(version, False, work)
        publish_registry(version, False, work)
        github_finalize(release_id, version)
    say(stage="publish", result="PUBLISHED", version=version, commit=commit)


def cmd_verify(args):
    version, dist = args.version, Path(args.dist)
    failures = []

    def check(ok, what):
        print(("ok   " if ok else "FAIL ") + what, flush=True)
        if not ok:
            failures.append(what)

    doc = npm_document()
    check(doc.get("dist-tags", {}).get("latest") == version, "npm latest is " + version)
    with tempfile.TemporaryDirectory() as tmp:
        work = Path(tmp)
        _, integrity, _ = npm_pack(work)
        check(doc.get("versions", {}).get(version, {}).get("dist", {}).get("integrity") == integrity,
              "npm integrity equals the packed tarball's")
        install = work / "fresh"
        install.mkdir()
        env = {key: value for key, value in os.environ.items() if not key.startswith("MCP_RELEASE_")}
        env["CONTEXTSTREAM_MCP_CACHE_DIR"] = str(work / "cache")
        subprocess.run(["npm", "init", "-y"], cwd=install, env=env, capture_output=True, check=True)
        for attempt in range(10):
            # The registry's CDN can lag a fresh publish by a minute or two (ETARGET).
            if subprocess.run(["npm", "install", "--prefer-online", "--ignore-scripts", "--no-audit", "--no-fund",
                               NPM_PACKAGE + "@" + version], cwd=install, env=env, capture_output=True, timeout=600).returncode == 0:
                break
            time.sleep(15)
        audit = subprocess.run(["npm", "audit", "signatures"], cwd=install, env=env, capture_output=True, text=True)
        check(audit.returncode == 0 and "verified registry signature" in audit.stdout, "npm signature audit passes")
        shown = subprocess.run([str(install / "node_modules/.bin/contextstream-mcp"), "--version"], cwd=install, env=env,
                               capture_output=True, text=True, timeout=300).stdout.strip().splitlines()
        check(bool(shown) and shown[-1] == "contextstream-mcp " + version,
              "a fresh npm install downloads and runs the R2 binary: " + (shown[-1] if shown else "nothing"))
        check(registry_verify(version, work), "MCP Registry has %s, identical to server.json" % version)
        status, body = http("GET", registry_url(version))
        meta = json.loads(body).get("_meta", {}).get("io.modelcontextprotocol.registry/official", {}) if status == 200 else {}
        check(meta.get("status") == "active" and meta.get("isLatest") is True, "the registry entry is active and latest")
    headers = github_headers()
    status, latest = get_json(GITHUB_API + "/repos/" + REPO + "/releases/latest", headers)
    check(status == 200 and latest.get("tag_name") == "v" + version and not latest.get("draft"),
          "GitHub Latest is v" + version)
    assets = {asset["name"]: asset for asset in (latest or {}).get("assets", [])}
    check(sorted(assets) == sorted(PAYLOAD) and all(assets[name].get("digest") == "sha256:" + sha256_file(dist / name)
                                                    for name in PAYLOAD), "the nine GitHub assets equal the bundle")
    for prefix in ("v" + version, "latest"):
        same = all(r2_public("%s/%s" % (prefix, name)) == (dist / name).read_bytes() for name in PAYLOAD)
        check(same, "R2 mcp/%s/ holds the nine files of the bundle" % prefix)
    if failures:
        raise Refused("%d public check(s) failed" % len(failures))
    say(stage="verify", result="ALL_CHANNELS_OK", version=version)


def cmd_await_hosted(args):
    deadline = time.time() + args.timeout
    seen = None
    while time.time() < deadline:
        try:
            status, body = http("GET", HOSTED_HEALTH, attempts=1, timeout=20)
            seen = json.loads(body).get("version") if status == 200 else "HTTP %d" % status
        except (Refused, ValueError):
            seen = "unreachable"
        if seen == args.version:
            say(stage="hosted", result="HOSTED_OK", version=args.version)
            return
        time.sleep(30)
    raise Refused("the hosted gateway still reports %s after %d s" % (seen, args.timeout))


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    commands = parser.add_subparsers(dest="command", required=True)
    plan = commands.add_parser("plan")
    plan.add_argument("--mode", choices=("release", "dry-run"), required=True)
    plan.add_argument("--ref", required=True)
    macho = commands.add_parser("macho-check")
    macho.add_argument("path")
    macho.add_argument("--version", required=True)
    for name in ("assemble", "fetch-bundle", "publish"):
        sub = commands.add_parser(name)
        sub.add_argument("--dist", required=True)
        sub.add_argument("--version", required=True)
        sub.add_argument("--commit", required=True)
        if name == "publish":
            sub.add_argument("--mode", choices=("release", "dry-run"), required=True)
    verify = commands.add_parser("verify")
    verify.add_argument("--dist", required=True)
    verify.add_argument("--version", required=True)
    commands.add_parser("r2-probe")
    hosted = commands.add_parser("await-hosted")
    hosted.add_argument("--version", required=True)
    hosted.add_argument("--timeout", type=int, default=4500)
    args = parser.parse_args(argv)
    handlers = {"plan": cmd_plan, "macho-check": cmd_macho_check, "assemble": cmd_assemble, "fetch-bundle": cmd_fetch_bundle,
                "publish": cmd_publish, "verify": cmd_verify, "await-hosted": cmd_await_hosted, "r2-probe": cmd_r2_probe}
    try:
        handlers[args.command](args)
    except (Refused, contract.ReleaseError) as refused:
        print("::error title=MCP release::" + str(refused), flush=True)
        say(stage=args.command, error=str(refused))
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
