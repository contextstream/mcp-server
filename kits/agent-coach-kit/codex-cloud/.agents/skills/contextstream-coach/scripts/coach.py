#!/usr/bin/env python3
"""ContextStream Coach: the helper an agent runs to be coached on recurring work.

It talks to the ContextStream REST API (/api/v1/coach) with the person's own API
key, read from CONTEXTSTREAM_API_KEY (in Codex Cloud, a network secret). Python
standard library only.

Every write carries an operation id derived from the command and its arguments,
so running the same command again after a failure never saves anything twice.
Writes print [COACH_SAVED], [COACH_ALREADY_SAVED], [COACH_CONFLICT] or
[COACH_NOT_SAVED]; nothing else means a save happened.
"""

from __future__ import annotations

import argparse
import base64
import hashlib
import json
import mimetypes
import os
import pathlib
import re
import sys
import time
import urllib.error
import urllib.parse
import urllib.request

VERSION = "0.1.0"
DEFAULT_API_URL = "https://api.contextstream.io"
USER_AGENT = f"contextstream-coach-kit/{VERSION} (codex-cloud)"
COACH = "/api/v1/coach"
TEXT_LIMIT = 12_000
INLINE_MEDIA_LIMIT = 8 * 1024 * 1024
RETRY_STATUSES = {429, 502, 503, 504}
RETRY_DELAYS = (1.0, 3.0)
START_JOB_BUCKET_MINUTES = 10

EXIT_OK, EXIT_ERROR, EXIT_USAGE, EXIT_AUTH, EXIT_CONFLICT, EXIT_NETWORK = 0, 1, 2, 3, 4, 5

AUTH_HINT = (
    "Check the Codex network secret CONTEXTSTREAM_API_KEY: it must hold your own "
    "ContextStream API key, and its allowed domain must be api.contextstream.io."
)

FEEDBACK_KINDS = {"correction": "correction", "approval": "partial_approval", "direction": "direction"}
FEEDBACK_SCOPES = {"reusable": "workflow", "this-job": "job"}


class CoachError(Exception):
    def __init__(self, message: str, exit_code: int = EXIT_ERROR):
        super().__init__(message)
        self.exit_code = exit_code


# ---------------------------------------------------------------------------
# HTTP
# ---------------------------------------------------------------------------


def _decode(payload: bytes):
    if not payload:
        return None
    try:
        return json.loads(payload.decode("utf-8"))
    except (ValueError, UnicodeDecodeError):
        return {"raw": payload[:500].decode("utf-8", "replace")}


def unwrap(body):
    """ContextStream wraps responses as {success, data, error}; return the data."""
    if isinstance(body, dict) and "data" in body and ("success" in body or "error" in body):
        return body["data"]
    return body


def error_text(body) -> str:
    if isinstance(body, dict):
        for key in ("error", "message", "detail", "raw"):
            value = body.get(key)
            if isinstance(value, dict):
                value = value.get("message") or value.get("code")
            if value:
                return str(value)
    return ""


def network_hint(base_url: str, err: Exception) -> str:
    host = urllib.parse.urlsplit(base_url).hostname or base_url
    return (
        f"Can't reach {host} ({err}). In Codex Cloud, allow internet access to {host} "
        "(adding the CONTEXTSTREAM_API_KEY network secret for that domain does this) "
        "and allow all HTTP methods."
    )


class Api:
    def __init__(self, base_url: str, api_key: str, auth: str = "x-api-key", sleep=time.sleep):
        self.base_url = base_url.rstrip("/")
        self.api_key = api_key
        self.auth = auth
        self.sleep = sleep

    def _headers(self) -> dict:
        headers = {"Accept": "application/json", "User-Agent": USER_AGENT}
        if self.auth == "bearer":
            headers["Authorization"] = f"Bearer {self.api_key}"
        else:
            headers["X-API-Key"] = self.api_key
        return headers

    def request(self, method: str, path: str, body=None, query: dict | None = None):
        """Return (status, lowercase headers, decoded body). Retries network
        failures and 429/502/503/504 with the same request (same operation id)."""
        url = self.base_url + path
        if query:
            url += "?" + urllib.parse.urlencode({k: v for k, v in query.items() if v is not None})
        headers = self._headers()
        data = None
        if body is not None:
            data = json.dumps(body, ensure_ascii=False).encode("utf-8")
            headers["Content-Type"] = "application/json"
        attempts = len(RETRY_DELAYS) + 1
        for attempt in range(attempts):
            req = urllib.request.Request(url, data=data, headers=headers, method=method)
            try:
                with urllib.request.urlopen(req, timeout=60) as resp:
                    return resp.status, _lower(resp.headers), _decode(resp.read())
            except urllib.error.HTTPError as err:
                payload = err.read()
                if err.code in RETRY_STATUSES and attempt < attempts - 1:
                    self.sleep(RETRY_DELAYS[attempt])
                    continue
                return err.code, _lower(err.headers), _decode(payload)
            except (urllib.error.URLError, TimeoutError, ConnectionError) as err:
                if attempt < attempts - 1:
                    self.sleep(RETRY_DELAYS[attempt])
                    continue
                raise CoachError(network_hint(self.base_url, err), EXIT_NETWORK) from None
        raise CoachError("request failed", EXIT_ERROR)


def _lower(headers) -> dict:
    return {k.lower(): v for k, v in (headers.items() if headers else [])}


def seg(ref: str) -> str:
    """A reference as one URL path segment (refs contain '#' and '/')."""
    return urllib.parse.quote(ref, safe="")


# ---------------------------------------------------------------------------
# Operation ids, local state
# ---------------------------------------------------------------------------


def operation_id(action: str, payload: dict, bucket_minutes: int | None = None, now=None) -> str:
    """Derived from the action and its exact arguments, so a retry reuses it."""
    canonical = json.dumps({"action": action, "payload": payload}, sort_keys=True,
                           ensure_ascii=False, separators=(",", ":"))
    if bucket_minutes:
        now = time.time() if now is None else now
        canonical += f"|{int(now // (bucket_minutes * 60))}"
    return "cli-" + hashlib.sha256(canonical.encode("utf-8")).hexdigest()[:40]


def _state_path() -> pathlib.Path:
    root = os.environ.get("COACH_STATE_DIR") or os.path.join(
        os.environ.get("XDG_CACHE_HOME") or os.path.join(os.path.expanduser("~"), ".cache"),
        "contextstream-coach")
    return pathlib.Path(root) / "context-etags.json"


def remember_etag(job: str, etag) -> None:
    if not etag:
        return
    path = _state_path()
    try:
        state = json.loads(path.read_text()) if path.exists() else {}
        state[job] = etag
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(json.dumps(state))
    except (OSError, ValueError):
        pass


def recall_etag(job: str):
    try:
        return json.loads(_state_path().read_text()).get(job)
    except (OSError, ValueError):
        return None


# ---------------------------------------------------------------------------
# Reads and writes
# ---------------------------------------------------------------------------


def failure(status: int, body, op: str | None = None) -> CoachError:
    text = error_text(body) or f"HTTP {status}"
    if status in (401, 403):
        if "coach_not_enabled" in text:
            return CoachError(f"[COACH_NOT_SAVED] Coach isn't turned on for this account yet ({text}).",
                              EXIT_AUTH)
        if "insufficient_scope" in text:
            return CoachError(f"[COACH_NOT_SAVED] This sign-in can't use Coach ({text}).", EXIT_AUTH)
        return CoachError(f"[COACH_NOT_SAVED] ContextStream refused the key ({text}). {AUTH_HINT}", EXIT_AUTH)
    if status == 409:
        return CoachError(f"[COACH_CONFLICT] {text}. Nothing changed.", EXIT_CONFLICT)
    retry = f" Safe to retry: the same command reuses operation id {op}." if op else ""
    return CoachError(f"[COACH_NOT_SAVED] {text}.{retry}", EXIT_ERROR)


def read(api: Api, path: str, query: dict | None = None):
    status, _, body = api.request("GET", path, query=query)
    if status != 200:
        text = error_text(body) or f"HTTP {status}"
        if status in (401, 403):
            raise CoachError(f"ContextStream refused the request ({text}). {AUTH_HINT}", EXIT_AUTH)
        raise CoachError(text, EXIT_ERROR)
    return unwrap(body)


def write(api: Api, args, method: str, path: str, action: str, payload: dict,
          op_payload: dict | None = None, bucket: int | None = None):
    op = args.operation_id or operation_id(action, {"path": path, **(op_payload or payload)}, bucket)
    status, headers, body = api.request(method, path, dict(payload, operation_id=op))
    if status not in (200, 201):
        raise failure(status, body, op)
    replayed = headers.get("idempotent-replayed", "").lower() == "true"
    return ("[COACH_ALREADY_SAVED]" if replayed else "[COACH_SAVED]"), unwrap(body) or {}, op


def ref_of(data, fallback: str = "") -> str:
    if isinstance(data, dict):
        for key in ("ref", "job_ref", "output_ref", "feedback_ref", "guidance_ref", "slug"):
            if data.get(key):
                return str(data[key])
        for nested in ("workflow", "job", "output", "feedback", "guidance"):
            value = data.get(nested)
            if isinstance(value, dict) and value.get("ref"):
                return str(value["ref"])
    return fallback


def print_json(data) -> None:
    print(json.dumps(data, indent=2, ensure_ascii=False))


def report(args, tag: str, what: str, data, op: str) -> int:
    if args.json:
        print_json({"status": tag.strip("[]"), "operation_id": op, "data": data})
    else:
        print(f"{tag} {what}")
        print(f"operation id: {op}")
    return EXIT_OK


# ---------------------------------------------------------------------------
# Helpers for arguments
# ---------------------------------------------------------------------------


def read_text(path: str | None) -> str:
    if path and path != "-":
        text = pathlib.Path(path).read_text(encoding="utf-8")
    else:
        text = sys.stdin.read()
    if not text.strip():
        raise CoachError("The draft is empty.", EXIT_USAGE)
    if len(text) > TEXT_LIMIT:
        raise CoachError(f"The draft is {len(text)} characters; the limit is {TEXT_LIMIT}.", EXIT_USAGE)
    return text


def parse_time_ms(value: str | None):
    if value is None:
        return None
    if not re.fullmatch(r"\d+(:\d{1,2}){0,2}(\.\d+)?", value.strip()):
        raise CoachError(f"Use a time like 0:34, 1:02:05 or 34 (seconds), not {value!r}.", EXIT_USAGE)
    parts = value.strip().split(":")
    seconds = 0.0
    for part in parts:
        seconds = seconds * 60 + float(part)
    return int(round(seconds * 1000))


def media_kind(mime: str) -> str:
    for kind in ("image", "audio", "video"):
        if mime.startswith(kind + "/"):
            return kind
    return "document"


def put_file(url: str, data: bytes, headers: dict, sleep=time.sleep) -> None:
    attempts = len(RETRY_DELAYS) + 1
    for attempt in range(attempts):
        req = urllib.request.Request(url, data=data, headers=dict(headers), method="PUT")
        try:
            with urllib.request.urlopen(req, timeout=300) as resp:
                if resp.status in (200, 201, 204):
                    return
                raise CoachError(f"[COACH_NOT_SAVED] The upload returned HTTP {resp.status}.")
        except urllib.error.HTTPError as err:
            if err.code in RETRY_STATUSES and attempt < attempts - 1:
                sleep(RETRY_DELAYS[attempt])
                continue
            raise CoachError(f"[COACH_NOT_SAVED] The upload returned HTTP {err.code}.") from None
        except (urllib.error.URLError, TimeoutError, ConnectionError) as err:
            if attempt < attempts - 1:
                sleep(RETRY_DELAYS[attempt])
                continue
            host = urllib.parse.urlsplit(url).hostname or "the upload host"
            raise CoachError(
                f"[COACH_NOT_SAVED] Can't reach {host} for the upload ({err}). Files over "
                f"{INLINE_MEDIA_LIMIT // (1024 * 1024)} MiB go there directly: allow {host} in the "
                "Codex environment's internet access, or send a smaller file.", EXIT_NETWORK) from None


def upload_media(api: Api, args) -> tuple[str, str]:
    path = pathlib.Path(args.media)
    data = path.read_bytes()
    if not data:
        raise CoachError(f"{path} is empty.", EXIT_USAGE)
    mime = args.mime_type or mimetypes.guess_type(path.name)[0] or "application/octet-stream"
    kind = media_kind(mime)
    digest = hashlib.sha256(data).hexdigest()
    base = {"kind": kind, "filename": path.name, "mime_type": mime}
    if len(data) <= INLINE_MEDIA_LIMIT:
        payload = dict(base, data_base64=base64.b64encode(data).decode("ascii"))
    else:
        payload = dict(base, upload={"size_bytes": len(data)})
    route = f"{COACH}/jobs/{seg(args.job)}/uploads"
    op = (args.operation_id + "-upload") if args.operation_id else operation_id(
        "upload", {"path": route, "sha256": digest, "size_bytes": len(data), **base})
    status, _, body = api.request("POST", route, dict(payload, operation_id=op))
    if status not in (200, 201):
        raise failure(status, body, op)
    info = unwrap(body) or {}
    if info.get("upload_url"):
        put_file(info["upload_url"], data, info.get("upload_headers") or {"Content-Type": mime}, api.sleep)
    content_id = info.get("content_id") or info.get("content_ref")
    if not content_id:
        raise CoachError("[COACH_NOT_SAVED] The upload did not return a media reference.")
    return content_id, kind


# ---------------------------------------------------------------------------
# Commands
# ---------------------------------------------------------------------------


def cmd_doctor(api: Api, args) -> int:
    present = bool(os.environ.get("CONTEXTSTREAM_API_KEY"))
    print(f"key: {'set' if present else 'MISSING'} (the value is never shown)")
    if not present:
        print(AUTH_HINT)
        return EXIT_AUTH
    status, _, body = api.request("GET", "/api/v1/auth/me")
    print(f"sign-in: HTTP {status}")
    if status != 200:
        print(AUTH_HINT)
        return EXIT_AUTH
    status, _, body = api.request("GET", f"{COACH}/capabilities")
    if status == 404:
        print("coach: not available on this server yet")
        return EXIT_ERROR
    if status != 200:
        print(f"coach: HTTP {status} {error_text(body)}")
        return EXIT_AUTH if status in (401, 403) else EXIT_ERROR
    data = unwrap(body) or {}
    enabled = bool(data.get("enabled"))
    print(f"coach: {'enabled' if enabled else 'not turned on for this account yet'} "
          f"(contract {data.get('contract', '?')})")
    return EXIT_OK if enabled else EXIT_AUTH


def cmd_list(api: Api, args) -> int:
    data = read(api, f"{COACH}/workflows", {"per_page": 100})
    items = data.get("items", []) if isinstance(data, dict) else (data or [])
    if args.json:
        print_json(items)
    elif not items:
        print('No workflows yet. Create one with: coach.py create --name "..." --goal "..."')
    else:
        for w in items:
            print(f"{ref_of(w)}: {w.get('name')} (goal v{w.get('goal_version', '?')}, "
                  f"{w.get('job_count', 0)} jobs, {w.get('status', 'active')})")
    return EXIT_OK


def collection_for(ref: str) -> str:
    if "#" in ref:
        tail = ref.split("#", 1)[1]
        if "/v" in tail:
            return "outputs"
        if "/f" in tail:
            return "feedback"
        return "jobs"
    if "/g" in ref:
        return "guidance"
    return "workflows"


def cmd_get(api: Api, args) -> int:
    collection = args.kind or collection_for(args.ref)
    print_json(read(api, f"{COACH}/{collection}/{seg(args.ref)}"))
    return EXIT_OK


def show_context(api: Api, args, job: str) -> int:
    data = read(api, f"{COACH}/jobs/{seg(job)}/context") or {}
    remember_etag(job, data.get("context_etag"))
    if args.json:
        print_json(data)
    else:
        print(data.get("text") or json.dumps(data, indent=2, ensure_ascii=False))
    return EXIT_OK


def cmd_context(api: Api, args) -> int:
    return show_context(api, args, args.job)


def cmd_create(api: Api, args) -> int:
    payload = {"name": args.name, "goal": args.goal}
    if args.audience:
        payload["audience"] = args.audience
    if args.output_kind:
        payload["output_kind"] = args.output_kind
    tag, data, op = write(api, args, "POST", f"{COACH}/workflows", "create", payload)
    return report(args, tag, f"workflow {ref_of(data, args.name)}", data, op)


def cmd_refine_goal(api: Api, args) -> int:
    expected = args.expected_version
    if expected is None:
        current = read(api, f"{COACH}/workflows/{seg(args.workflow)}") or {}
        expected = current.get("goal_version") or (current.get("goal") or {}).get("version")
    payload = {"expected_goal_version": expected, "goal": args.goal, "reason": args.reason}
    tag, data, op = write(api, args, "POST", f"{COACH}/workflows/{seg(args.workflow)}/goals",
                          "refine_goal", payload)
    return report(args, tag, f"goal of {args.workflow} is now version "
                  f"{(data or {}).get('version', '?')}", data, op)


def cmd_start_job(api: Api, args) -> int:
    payload = {"objective": args.objective}
    if args.label:
        payload["label"] = args.label
    tag, data, op = write(api, args, "POST", f"{COACH}/workflows/{seg(args.workflow)}/jobs",
                          "start_job", payload, bucket=START_JOB_BUCKET_MINUTES)
    job = ref_of(data)
    report(args, tag, f"job {job}", data, op)
    if job and not args.json:
        print()
        return show_context(api, args, job)
    return EXIT_OK


def cmd_save_output(api: Api, args) -> int:
    payload: dict = {"title": args.title}
    op_payload: dict = {"title": args.title}
    if args.media:
        content_id, kind = upload_media(api, args)
        payload.update(kind=kind, content_id=content_id)
        op_payload.update(kind=kind, content_id=content_id)
        if args.caption:
            payload["caption"] = op_payload["caption"] = args.caption
    else:
        text = read_text(args.file)
        payload.update(kind="text", body=text)
        op_payload.update(kind="text", body_sha256=hashlib.sha256(text.encode("utf-8")).hexdigest())
    if args.revises:
        payload["source_output"] = op_payload["source_output"] = args.revises
    if args.carry_note:
        payload["carry_note"] = op_payload["carry_note"] = args.carry_note
    etag = args.context_etag or recall_etag(args.job)
    if etag:
        payload["context_etag"] = etag
    tag, data, op = write(api, args, "POST", f"{COACH}/jobs/{seg(args.job)}/outputs",
                          "save_output", payload, op_payload)
    return report(args, tag, f"version {ref_of(data)}", data, op)


def cmd_add_feedback(api: Api, args) -> int:
    text = args.text if args.text is not None else read_text(args.text_file)
    payload = {
        "kind": FEEDBACK_KINDS[args.kind],
        "scope": FEEDBACK_SCOPES[args.scope],
        "body": text,
        "origin": "picked" if args.picked else "typed",
    }
    if args.picked and args.kind != "direction":
        raise CoachError("--picked is only for a suggestion the person chose: use --kind direction.",
                         EXIT_USAGE)
    if args.excerpt:
        payload["excerpt"] = args.excerpt
    start, end = parse_time_ms(args.time_from), parse_time_ms(args.time_to)
    if (start is None) != (end is None):
        raise CoachError("Give both --from and --to for a time range.", EXIT_USAGE)
    if start is not None:
        payload.update(time_start_ms=start, time_end_ms=end)
    if args.repeat_of:
        payload["repeat_of_guidance"] = args.repeat_of
    if args.leak:
        payload["reports_leak"] = True
    tag, data, op = write(api, args, "POST", f"{COACH}/outputs/{seg(args.output)}/feedback",
                          "add_feedback", payload)
    what = f"feedback {ref_of(data.get('feedback') if isinstance(data, dict) else data)}"
    guidance = data.get("guidance") if isinstance(data, dict) else None
    if guidance:
        what += f" (reusable: guidance {ref_of(guidance)})"
    code = report(args, tag, what, data, op)
    repeats = data.get("possible_repeats") if isinstance(data, dict) else None
    if repeats and not args.json:
        print("This may repeat earlier guidance: " + ", ".join(ref_of(r) for r in repeats))
    return code


def cmd_retract_feedback(api: Api, args) -> int:
    tag, data, op = write(api, args, "POST", f"{COACH}/feedback/{seg(args.feedback)}/retract",
                          "retract_feedback", {"reason": args.reason})
    return report(args, tag, f"feedback {args.feedback} retracted", data, op)


def _guidance_version(api: Api, ref: str, given):
    if given is not None:
        return given
    return (read(api, f"{COACH}/guidance/{seg(ref)}") or {}).get("version")


def cmd_edit_guidance(api: Api, args) -> int:
    payload = {"expected_version": _guidance_version(api, args.guidance, args.expected_version),
               "body": args.text, "reason": args.reason}
    tag, data, op = write(api, args, "PATCH", f"{COACH}/guidance/{seg(args.guidance)}",
                          "edit_guidance", payload)
    return report(args, tag, f"guidance {args.guidance} edited", data, op)


def cmd_retire_guidance(api: Api, args) -> int:
    payload = {"expected_version": _guidance_version(api, args.guidance, args.expected_version),
               "status": "retired", "reason": args.reason}
    tag, data, op = write(api, args, "PATCH", f"{COACH}/guidance/{seg(args.guidance)}",
                          "retire_guidance", payload)
    return report(args, tag, f"guidance {args.guidance} retired", data, op)


def cmd_close_job(api: Api, args) -> int:
    payload: dict = {}
    if args.verdict:
        payload["verdict"] = {"verdict": args.verdict}
        if args.note:
            payload["verdict"]["note"] = args.note
    tag, data, op = write(api, args, "POST", f"{COACH}/jobs/{seg(args.job)}/close", "close_job", payload)
    return report(args, tag, f"job {args.job} closed", data, op)


def cmd_compare(api: Api, args) -> int:
    left, right = args.left, args.right
    if right is None:
        job = read(api, f"{COACH}/jobs/{seg(left)}") or {}
        left, right = ref_of(job.get("previous_output")), ref_of(job.get("latest_output"))
        if not left or not right:
            raise CoachError("This job needs a saved version, and an earlier job with one, to compare.")
    data = read(api, f"{COACH}/compare", {"left": left, "right": right}) or {}
    if args.json or not data.get("text"):
        print_json(data)
    else:
        print(data["text"])
    return EXIT_OK


def cmd_history(api: Api, args) -> int:
    wf = seg(args.workflow)
    history = {
        "goals": read(api, f"{COACH}/workflows/{wf}/goals"),
        "guidance": read(api, f"{COACH}/workflows/{wf}/guidance"),
        "feedback": read(api, f"{COACH}/workflows/{wf}/feedback", {"include_retracted": "true"}),
    }
    print_json(history)
    return EXIT_OK


# ---------------------------------------------------------------------------
# CLI
# ---------------------------------------------------------------------------


def build_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(prog="coach.py", description=__doc__.split("\n\n")[0])
    parser.add_argument("--json", action="store_true", help="print JSON instead of text")
    parser.add_argument("--auth", choices=["x-api-key", "bearer"], default="x-api-key",
                        help="how to send the key (default: X-API-Key header)")
    parser.add_argument("--operation-id", help="reuse an operation id from an earlier attempt")
    sub = parser.add_subparsers(dest="command", required=True)

    sub.add_parser("doctor", help="check the key, the network and that Coach is on")
    sub.add_parser("list", help="list your workflows")
    p = sub.add_parser("get", help="show a workflow, job, version, feedback or guidance")
    p.add_argument("ref")
    p.add_argument("--kind", choices=["workflows", "jobs", "outputs", "feedback", "guidance"])
    p = sub.add_parser("context", help="print what you need for a job")
    p.add_argument("job")
    p = sub.add_parser("create", help="create a workflow")
    p.add_argument("--name", required=True)
    p.add_argument("--goal", required=True)
    p.add_argument("--audience")
    p.add_argument("--output-kind", choices=["text", "image", "audio", "video", "document", "mixed"])
    p = sub.add_parser("refine-goal", help="change a workflow's goal (the person asked)")
    p.add_argument("workflow")
    p.add_argument("--goal", required=True)
    p.add_argument("--reason", required=True)
    p.add_argument("--expected-version", type=int)
    p = sub.add_parser("start-job", help="start a job and print its context")
    p.add_argument("workflow")
    p.add_argument("--objective", required=True)
    p.add_argument("--label")
    p = sub.add_parser("save-output", help="save a draft (text or media) as a new version")
    p.add_argument("job")
    p.add_argument("--title", required=True)
    source = p.add_mutually_exclusive_group()
    source.add_argument("--file", help="text file with the draft (default: stdin)")
    source.add_argument("--media", help="image, audio, video or document file")
    p.add_argument("--mime-type")
    p.add_argument("--caption")
    p.add_argument("--revises", help="the version this one revises, like weekly-note#4/v1")
    p.add_argument("--carry-note", help="what you kept from approvals and which feedback you applied")
    p.add_argument("--context-etag")
    p = sub.add_parser("add-feedback", help="record the person's feedback, word for word")
    p.add_argument("output")
    p.add_argument("--kind", required=True, choices=sorted(FEEDBACK_KINDS))
    p.add_argument("--scope", required=True, choices=sorted(FEEDBACK_SCOPES))
    text = p.add_mutually_exclusive_group(required=True)
    text.add_argument("--text")
    text.add_argument("--text-file")
    p.add_argument("--excerpt")
    p.add_argument("--from", dest="time_from")
    p.add_argument("--to", dest="time_to")
    p.add_argument("--picked", action="store_true", help="the person picked one of your suggestions")
    p.add_argument("--repeat-of", help="guidance this feedback repeats, like weekly-note/g3")
    p.add_argument("--leak", action="store_true", help="the person saw feedback from another job")
    p = sub.add_parser("retract-feedback", help="take back feedback the person withdrew")
    p.add_argument("feedback")
    p.add_argument("--reason", required=True)
    p = sub.add_parser("edit-guidance", help="edit reusable guidance (the person asked)")
    p.add_argument("guidance")
    p.add_argument("--text", required=True)
    p.add_argument("--reason", required=True)
    p.add_argument("--expected-version", type=int)
    p = sub.add_parser("retire-guidance", help="retire reusable guidance (the person asked)")
    p.add_argument("guidance")
    p.add_argument("--reason", required=True)
    p.add_argument("--expected-version", type=int)
    p = sub.add_parser("close-job", help="close a job with the person's verdict")
    p.add_argument("job")
    p.add_argument("--verdict", choices=["better", "same", "worse"])
    p.add_argument("--note")
    p = sub.add_parser("compare", help="compare a job's latest version with the last job's, or two versions")
    p.add_argument("left")
    p.add_argument("right", nargs="?")
    p = sub.add_parser("history", help="goal versions, guidance and feedback of a workflow")
    p.add_argument("workflow")
    return parser


COMMANDS = {
    "doctor": cmd_doctor, "list": cmd_list, "get": cmd_get, "context": cmd_context,
    "create": cmd_create, "refine-goal": cmd_refine_goal, "start-job": cmd_start_job,
    "save-output": cmd_save_output, "add-feedback": cmd_add_feedback,
    "retract-feedback": cmd_retract_feedback, "edit-guidance": cmd_edit_guidance,
    "retire-guidance": cmd_retire_guidance, "close-job": cmd_close_job,
    "compare": cmd_compare, "history": cmd_history,
}


def main(argv=None, sleep=time.sleep) -> int:
    args = build_parser().parse_args(argv)
    key = os.environ.get("CONTEXTSTREAM_API_KEY", "")
    if not key and args.command != "doctor":
        print(f"CONTEXTSTREAM_API_KEY is not set. {AUTH_HINT}", file=sys.stderr)
        return EXIT_AUTH
    api = Api(os.environ.get("CONTEXTSTREAM_API_URL") or DEFAULT_API_URL, key, args.auth, sleep)
    try:
        return COMMANDS[args.command](api, args)
    except CoachError as err:
        print(str(err), file=sys.stderr)
        return err.exit_code
    except OSError as err:
        print(f"Can't read the file: {err}", file=sys.stderr)
        return EXIT_USAGE


if __name__ == "__main__":
    sys.exit(main())
