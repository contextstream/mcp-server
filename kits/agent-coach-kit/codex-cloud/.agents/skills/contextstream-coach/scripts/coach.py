#!/usr/bin/env python3
"""ContextStream Coach: the helper an agent runs to be coached on recurring work.

It talks to the ContextStream REST API (/api/v1/coach) with the person's own API
key, read from CONTEXTSTREAM_API_KEY (in Codex Cloud, a network secret: the task
sees a placeholder and the network proxy puts the real key on requests to
api.contextstream.io). The key is sent as X-API-Key and never printed. Python
standard library only.

Every write carries an operation id derived from the command and its arguments,
so running the same command again after a failure never saves anything twice.
Writes print [COACH_SAVED], [COACH_ALREADY_SAVED], [COACH_CONFLICT] or
[COACH_NOT_SAVED]; nothing else means a save happened.
"""

from __future__ import annotations

import argparse
import base64
import errno
import hashlib
import http.client
import json
import mimetypes
import os
import pathlib
import re
import socket
import ssl
import sys
import time
import urllib.error
import urllib.parse
import urllib.request

VERSION = "0.2.0"
DEFAULT_API_URL = "https://api.contextstream.io"
USER_AGENT = f"contextstream-coach-kit/{VERSION} (codex-cloud)"
COACH = "/api/v1/coach"
TEXT_LIMIT = 12_000
INLINE_MEDIA_LIMIT = 8 * 1024 * 1024
RETRY_STATUSES = {429, 502, 503, 504}
RETRY_DELAYS = (1.0, 3.0)
START_JOB_BUCKET_MINUTES = 10
PER_PAGE = 100
MAX_PINS = 200

EXIT_OK, EXIT_ERROR, EXIT_USAGE, EXIT_AUTH, EXIT_CONFLICT, EXIT_NETWORK, EXIT_NO_NETWORK = (
    0, 1, 2, 3, 4, 5, 6)

AUTH_HINT = (
    "Check the Codex network secret CONTEXTSTREAM_API_KEY: it must hold your own "
    "ContextStream API key, and its allowed domain must be api.contextstream.io."
)
NO_NETWORK = "[COACH_NOT_SAVED] this command needs network access; run it again with network enabled"

FEEDBACK_KINDS = {"correction": "correction", "approval": "partial_approval", "direction": "direction"}
FEEDBACK_SCOPES = {"reusable": "workflow", "this-job": "job"}
SCOPE_WORDS = {"workflow": "reusable", "job": "this job only"}
EFFORTS = ("quick", "explore", "clear")


class CoachError(Exception):
    def __init__(self, message: str, exit_code: int = EXIT_ERROR, code: str = ""):
        super().__init__(message)
        self.exit_code = exit_code
        self.code = code


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


def error_parts(body) -> tuple[str, str, dict]:
    """(code, message, details) from the envelope's error {code, message}."""
    if not isinstance(body, dict):
        return "", "", {}
    err = body.get("error")
    if isinstance(err, dict):
        message = str(err.get("message") or err.get("code") or "")
        code = str(err.get("code") or _leading_code(message))
        details = err.get("details") if isinstance(err.get("details"), dict) else {}
        return code, message, details
    for key in ("error", "message", "detail", "raw"):
        value = body.get(key)
        if value:
            return _leading_code(str(value)), str(value), {}
    return "", "", {}


def _leading_code(message: str) -> str:
    match = re.match(r"([a-z][a-z0-9_]*):", message or "")
    return match.group(1) if match else ""


def error_text(body) -> str:
    return error_parts(body)[1]


def _host(url: str) -> str:
    return urllib.parse.urlsplit(url).hostname or url


_NO_NETWORK_ERRNOS = {errno.ECONNREFUSED, errno.ENETUNREACH, errno.EHOSTUNREACH, errno.ENETDOWN}
_NO_NETWORK_TEXT = (
    "proxy", "connection refused", "network is unreachable", "no route to host",
    "name or service not known", "temporary failure in name resolution",
    "nodename nor servname", "failed to resolve", "getaddrinfo failed",
)


def classify_network_error(url: str, err: BaseException) -> CoachError:
    """A transport failure as a CoachError: no network at all (exit 6), the host
    blocked by the environment's allowlist, or anything else (exit 5)."""
    reason = getattr(err, "reason", err)
    text = str(reason)
    low = text.lower()
    host = _host(url)
    if "tunnel connection failed" in low:
        return CoachError(
            f"[COACH_NOT_SAVED] The network blocked {host} ({text}). In Codex Cloud, allow "
            f"{host} in the environment's internet access (the CONTEXTSTREAM_API_KEY network "
            "secret for api.contextstream.io does this) and allow all HTTP methods.", EXIT_NETWORK)
    if isinstance(reason, ssl.SSLError):
        return CoachError(
            f"[COACH_NOT_SAVED] Can't make a secure connection to {host} ({text}). If a network "
            "proxy is in the way, its certificate must be trusted (SSL_CERT_FILE).", EXIT_NETWORK)
    if (isinstance(reason, (ConnectionRefusedError, socket.gaierror))
            or getattr(reason, "errno", None) in _NO_NETWORK_ERRNOS
            or any(word in low for word in _NO_NETWORK_TEXT)):
        return CoachError(
            f"{NO_NETWORK}\n(Can't connect to {host}: {text}. If network access is already on, "
            f"check that the environment allows {host}.)", EXIT_NO_NETWORK)
    return CoachError(
        f"[COACH_NOT_SAVED] Can't reach {host} ({text}). In Codex Cloud, run the command with "
        f"network access and allow {host} in the environment's internet access.", EXIT_NETWORK)


class _SameHostRedirects(urllib.request.HTTPRedirectHandler):
    """Never follow a redirect to another host: the key must stay with the API."""

    def redirect_request(self, req, fp, code, msg, headers, newurl):
        if urllib.parse.urlsplit(newurl).netloc != urllib.parse.urlsplit(req.full_url).netloc:
            return None
        return super().redirect_request(req, fp, code, msg, headers, newurl)


def _ssl_context() -> ssl.SSLContext:
    """The default trust store, plus a proxy's CA bundle named in the environment."""
    context = ssl.create_default_context()
    for name in ("SSL_CERT_FILE", "REQUESTS_CA_BUNDLE", "CURL_CA_BUNDLE"):
        path = os.environ.get(name)
        if path and os.path.isfile(path):
            try:
                context.load_verify_locations(cafile=path)
            except (OSError, ssl.SSLError):
                pass
    return context


def _opener():
    return urllib.request.build_opener(
        urllib.request.HTTPSHandler(context=_ssl_context()), _SameHostRedirects())


_TRANSPORT_ERRORS = (urllib.error.URLError, http.client.HTTPException, OSError)


class Api:
    def __init__(self, base_url: str, api_key: str, auth: str = "x-api-key", sleep=time.sleep):
        self.base_url = base_url.rstrip("/")
        self.api_key = api_key
        self.auth = auth
        self.sleep = sleep
        self.opener = _opener()

    def _headers(self) -> dict:
        headers = {"Accept": "application/json", "User-Agent": USER_AGENT}
        if self.auth == "bearer":
            headers["Authorization"] = f"Bearer {self.api_key}"
        else:
            headers["X-API-Key"] = self.api_key
        return headers

    def request(self, method: str, path: str, body=None, query: dict | None = None):
        """Return (status, lowercase headers, decoded body). Retries network
        failures, 429/502/503/504 and 409 operation_in_progress with the same
        request (same operation id)."""
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
            last = attempt == attempts - 1
            req = urllib.request.Request(url, data=data, headers=headers, method=method)
            try:
                with self.opener.open(req, timeout=60) as resp:
                    return resp.status, _lower(resp.headers), _decode(resp.read())
            except urllib.error.HTTPError as err:
                payload, err_headers = err.read(), _lower(err.headers)
                decoded = _decode(payload)
                in_progress = err.code == 409 and error_parts(decoded)[0] == "operation_in_progress"
                if (err.code in RETRY_STATUSES or in_progress) and not last:
                    self.sleep(_retry_after(err_headers, RETRY_DELAYS[attempt]))
                    continue
                return err.code, err_headers, decoded
            except _TRANSPORT_ERRORS as err:
                if not last:
                    self.sleep(RETRY_DELAYS[attempt])
                    continue
                raise classify_network_error(url, err) from None
        raise CoachError("request failed", EXIT_ERROR)


def _retry_after(headers: dict, default: float) -> float:
    try:
        return min(float(headers.get("retry-after", "")), 10.0)
    except ValueError:
        return default


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
    return pathlib.Path(root) / "state.json"


def _load_state() -> dict:
    try:
        state = json.loads(_state_path().read_text())
        return state if isinstance(state, dict) else {}
    except (OSError, ValueError):
        return {}


def _save_state(state: dict) -> None:
    path = _state_path()
    try:
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(json.dumps(state))
    except OSError:
        pass


def remember_etag(job: str, etag) -> None:
    if etag:
        state = _load_state()
        state.setdefault("etags", {})[job] = etag
        _save_state(state)


def recall_etag(job: str):
    return _load_state().get("etags", {}).get(job)


def pin(op: str, fields: dict) -> dict:
    """The live values (versions, context etag) the first send of `op` used, so a
    rerun of the same command sends the same request and the server replays it."""
    state = _load_state()
    pins = state.setdefault("pins", {})
    if op in pins:
        return pins[op]
    pins[op] = fields
    while len(pins) > MAX_PINS:
        pins.pop(next(iter(pins)))
    _save_state(state)
    return fields


def unpin(op: str) -> None:
    state = _load_state()
    if state.get("pins", {}).pop(op, None) is not None:
        _save_state(state)


# ---------------------------------------------------------------------------
# Reads and writes
# ---------------------------------------------------------------------------


def failure(status: int, body, op: str | None = None) -> CoachError:
    code, text, _ = error_parts(body)
    text = text or f"HTTP {status}"
    if status in (401, 403):
        if code == "coach_not_enabled" or "coach_not_enabled" in text:
            return CoachError(f"[COACH_NOT_SAVED] Coach isn't turned on for this account yet ({text}).",
                              EXIT_AUTH)
        if code == "insufficient_scope" or "insufficient_scope" in text:
            return CoachError(f"[COACH_NOT_SAVED] This sign-in can't use Coach ({text}).", EXIT_AUTH)
        return CoachError(f"[COACH_NOT_SAVED] ContextStream refused the key ({text}). {AUTH_HINT}", EXIT_AUTH)
    if code == "operation_id_conflict":
        return CoachError(
            f"[COACH_CONFLICT] Operation id {op} was already used for a different request "
            f"({text}). Nothing changed. An earlier run of this command may have saved it: check "
            "with coach.py get. For a new change, change the arguments or pass a new --operation-id.",
            EXIT_CONFLICT, code)
    if status == 409:
        return CoachError(f"[COACH_CONFLICT] {text}. Nothing changed.", EXIT_CONFLICT, code)
    retry = f" Safe to retry: the same command reuses operation id {op}." if op else ""
    return CoachError(f"[COACH_NOT_SAVED] {text}.{retry}", EXIT_ERROR, code)


def read(api: Api, path: str, query: dict | None = None):
    status, _, body = api.request("GET", path, query=query)
    if status != 200:
        text = error_text(body) or f"HTTP {status}"
        if status in (401, 403):
            raise CoachError(f"ContextStream refused the request ({text}). {AUTH_HINT}", EXIT_AUTH)
        raise CoachError(text, EXIT_ERROR)
    return unwrap(body)


def read_all(api: Api, path: str, query: dict | None = None) -> list:
    """Every item of a paged list ({items, total, page, per_page, total_pages})."""
    items, page = [], 1
    while True:
        data = read(api, path, dict(query or {}, page=page, per_page=PER_PAGE)) or {}
        if not isinstance(data, dict):
            return list(data or [])
        items.extend(data.get("items") or [])
        if page >= int(data.get("total_pages") or 1):
            return items
        page += 1


def write(api: Api, args, method: str, path: str, action: str, payload: dict,
          op_payload: dict | None = None, bucket: int | None = None, live: dict | None = None):
    """Send a write with its operation id. `live` holds values read from the
    server or the local state (versions, the context etag); they are pinned to
    the operation id so a rerun sends exactly the same request."""
    op = args.operation_id or operation_id(action, {"path": path, **(op_payload or payload)}, bucket)
    if live:
        payload = dict(payload, **pin(op, live))
    status, headers, body = api.request(method, path, dict(payload, operation_id=op))
    if status not in (200, 201):
        if live and 400 <= status < 500:
            unpin(op)
        raise failure(status, body, op)
    replayed = str(headers.get("idempotent-replayed", "")).strip().lower() == "true"
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


def sha256_text(text: str) -> str:
    return hashlib.sha256(text.encode("utf-8")).hexdigest()


def parse_time_ms(value: str | None):
    if value is None:
        return None
    if not re.fullmatch(r"\d+(:\d{1,2}){0,2}(\.\d+)?", value.strip()):
        raise CoachError(f"Use a time like 0:34, 1:02:05 or 34 (seconds), not {value!r}.", EXIT_USAGE)
    seconds = 0.0
    for part in value.strip().split(":"):
        seconds = seconds * 60 + float(part)
    return int(round(seconds * 1000))


def media_kind(mime: str) -> str:
    for kind in ("image", "audio", "video"):
        if mime.startswith(kind + "/"):
            return kind
    return "document"


def is_uuid(ref: str) -> bool:
    return bool(re.fullmatch(r"[0-9a-fA-F]{8}-([0-9a-fA-F]{4}-){3}[0-9a-fA-F]{12}", ref))


def put_file(url: str, data: bytes, headers: dict, sleep=time.sleep) -> None:
    """PUT the bytes to the presigned link with exactly its headers (no API key)."""
    opener = urllib.request.build_opener(urllib.request.HTTPSHandler(context=_ssl_context()))
    attempts = len(RETRY_DELAYS) + 1
    for attempt in range(attempts):
        req = urllib.request.Request(url, data=data, headers=dict(headers), method="PUT")
        try:
            with opener.open(req, timeout=300) as resp:
                if resp.status in (200, 201, 204):
                    return
                raise CoachError(f"[COACH_NOT_SAVED] The upload returned HTTP {resp.status}.")
        except urllib.error.HTTPError as err:
            if err.code in RETRY_STATUSES and attempt < attempts - 1:
                sleep(RETRY_DELAYS[attempt])
                continue
            raise CoachError(f"[COACH_NOT_SAVED] The upload returned HTTP {err.code}.") from None
        except _TRANSPORT_ERRORS as err:
            if attempt < attempts - 1:
                sleep(RETRY_DELAYS[attempt])
                continue
            classified = classify_network_error(url, err)
            if classified.exit_code == EXIT_NO_NETWORK:
                raise classified from None
            host = _host(url)
            raise CoachError(
                f"[COACH_NOT_SAVED] Can't reach {host} for the upload ({getattr(err, 'reason', err)}). "
                f"Files over {INLINE_MEDIA_LIMIT // (1024 * 1024)} MiB go there directly: allow {host} "
                "in the Codex environment's internet access, or send a smaller file.", EXIT_NETWORK) from None


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
        print(f"Uploading {len(data) / (1024 * 1024):.1f} MiB directly to {_host(info['upload_url'])} "
              f"(files over {INLINE_MEDIA_LIMIT // (1024 * 1024)} MiB go there).", file=sys.stderr)
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
    proxy = urllib.request.getproxies().get("https")
    print(f"network proxy: {'yes' if proxy else 'no'}")
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
    items = read_all(api, f"{COACH}/workflows")
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
    payload = {"goal": args.goal, "reason": args.reason}
    if args.source_feedback:
        payload["source_feedback"] = args.source_feedback
    path = f"{COACH}/workflows/{seg(args.workflow)}/goals"
    if args.expected_version is not None:
        payload["expected_goal_version"] = args.expected_version
        tag, data, op = write(api, args, "POST", path, "refine_goal", payload)
    else:
        op = args.operation_id or operation_id("refine_goal", {"path": path, **payload})
        live = _pinned(op) or {"expected_goal_version": _goal_version(api, args.workflow)}
        tag, data, op = write(api, args, "POST", path, "refine_goal", payload, live=live)
    return report(args, tag, f"goal of {args.workflow} is now version "
                  f"{(data or {}).get('version', '?')}", data, op)


def _pinned(op: str):
    return _load_state().get("pins", {}).get(op)


def _goal_version(api: Api, workflow: str):
    current = read(api, f"{COACH}/workflows/{seg(workflow)}") or {}
    return current.get("goal_version") or (current.get("goal") or {}).get("version")


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
    payload: dict = {}
    op_payload: dict = {}
    if args.title:
        payload["title"] = op_payload["title"] = args.title
    if args.url and args.media:
        raise CoachError("Use --url or --media, not both.", EXIT_USAGE)
    if args.media:
        content_id, kind = upload_media(api, args)
        payload.update(kind=kind, content_id=content_id)
        op_payload.update(kind=kind, content_id=content_id)
    elif args.url:
        if not args.url.startswith("https://"):
            raise CoachError("A link output needs an https:// address.", EXIT_USAGE)
        payload.update(kind="link", url=args.url)
        op_payload.update(kind="link", url=args.url)
        if args.file:
            snapshot = read_text(args.file)
            payload["body"] = snapshot
            op_payload["body_sha256"] = sha256_text(snapshot)
    else:
        text = read_text(args.file)
        payload.update(kind="text", body=text)
        op_payload.update(kind="text", body_sha256=sha256_text(text))
    if args.caption:
        payload["caption"] = op_payload["caption"] = args.caption
    if args.revises:
        payload["source_output"] = op_payload["source_output"] = args.revises
    if args.carry_note:
        payload["carry_note"] = op_payload["carry_note"] = args.carry_note
    live = None
    if args.context_etag:
        payload["context_etag"] = op_payload["context_etag"] = args.context_etag
    else:
        live = {"context_etag": recall_etag(args.job)}
    tag, data, op = write(api, args, "POST", f"{COACH}/jobs/{seg(args.job)}/outputs",
                          "save_output", payload, op_payload, live=live)
    return report(args, tag, f"version {ref_of(data)}", data, op)


def cmd_add_feedback(api: Api, args) -> int:
    if args.picked and args.kind != "direction":
        raise CoachError("--picked is only for a suggestion the person chose: use --kind direction.",
                         EXIT_USAGE)
    text = args.text if args.text is not None else read_text(args.text_file)
    payload = {
        "kind": FEEDBACK_KINDS[args.kind],
        "scope": FEEDBACK_SCOPES[args.scope],
        "body": text,
        "origin": "picked" if args.picked else "typed",
    }
    if args.reading:
        payload["interpretation"] = args.reading
    if args.excerpt:
        payload["excerpt"] = args.excerpt
    start, end = parse_time_ms(args.time_from), parse_time_ms(args.time_to)
    if (start is None) != (end is None):
        raise CoachError("Give both --from and --to for a time range.", EXIT_USAGE)
    if start is not None:
        payload.update(time_start_ms=start, time_end_ms=end)
    if args.repeat_of:
        payload["repeat_of_guidance"] = args.repeat_of
    if args.applies_when:
        payload["applies_when"] = args.applies_when
    if args.leak:
        payload["reports_leak"] = True
    tag, data, op = write(api, args, "POST", f"{COACH}/outputs/{seg(args.output)}/feedback",
                          "add_feedback", payload)
    feedback = data.get("feedback") if isinstance(data, dict) else data
    what = f"feedback {ref_of(feedback)} ({SCOPE_WORDS[payload['scope']]})"
    guidance = data.get("guidance") if isinstance(data, dict) else None
    if guidance:
        what += f", guidance {ref_of(guidance)}"
    code = report(args, tag, what, data, op)
    repeats = data.get("possible_repeats") if isinstance(data, dict) else None
    if repeats and not args.json:
        print("This may repeat earlier guidance: " + ", ".join(ref_of(r) for r in repeats))
    return code


def cmd_retract_feedback(api: Api, args) -> int:
    payload = {"reason": args.reason} if args.reason else {}
    tag, data, op = write(api, args, "POST", f"{COACH}/feedback/{seg(args.feedback)}/retract",
                          "retract_feedback", payload)
    what = f"feedback {args.feedback} retracted"
    if isinstance(data, dict) and data.get("retired_guidance"):
        what += f"; guidance {ref_of(data['retired_guidance'])} retired"
    return report(args, tag, what, data, op)


def cmd_rescope(api: Api, args) -> int:
    scope = FEEDBACK_SCOPES[args.scope]
    tag, data, op = write(api, args, "POST", f"{COACH}/feedback/{seg(args.feedback)}/rescope",
                          "rescope_feedback", {"scope": scope})
    restated = ref_of((data or {}).get("restated"))
    what = f"feedback {args.feedback} is now {SCOPE_WORDS[scope]}: restated as {restated or '?'}"
    if (data or {}).get("guidance"):
        what += f", guidance {ref_of(data['guidance'])}"
    if (data or {}).get("retired_guidance"):
        what += f"; guidance {ref_of(data['retired_guidance'])} retired"
    return report(args, tag, what, data, op)


def _guidance_change(api: Api, args, action: str, payload: dict) -> int:
    path = f"{COACH}/guidance/{seg(args.guidance)}"
    if args.reason:
        payload["reason"] = args.reason
    if args.expected_version is not None:
        payload["expected_version"] = args.expected_version
        tag, data, op = write(api, args, "PATCH", path, action, payload)
    else:
        op = args.operation_id or operation_id(action, {"path": path, **payload})
        live = _pinned(op) or {
            "expected_version": (read(api, path) or {}).get("version")}
        tag, data, op = write(api, args, "PATCH", path, action, payload, live=live)
    status = (data or {}).get("status", "?")
    return report(args, tag, f"guidance {args.guidance} is now version "
                  f"{(data or {}).get('version', '?')} ({status})", data, op)


def _status_flag(args):
    if getattr(args, "remove", False):
        return "removed"
    if getattr(args, "restore", False):
        return "active"
    return None


def cmd_edit_guidance(api: Api, args) -> int:
    status = _status_flag(args)
    edit = args.text is not None or args.applies_when is not None
    if status and edit:
        raise CoachError("Change the text or limit, or remove/restore it, not both at once.", EXIT_USAGE)
    if not status and not edit:
        raise CoachError("Give --text, --applies-when, --remove or --restore.", EXIT_USAGE)
    payload: dict = {}
    if status:
        payload["status"] = status
    if args.text is not None:
        payload["body"] = args.text
    if args.applies_when is not None:
        payload["applies_when"] = args.applies_when.strip() or None
    return _guidance_change(api, args, "edit_guidance", payload)


def cmd_retire_guidance(api: Api, args) -> int:
    return _guidance_change(api, args, "retire_guidance", {"status": _status_flag(args) or "retired"})


def cmd_import_guidance(api: Api, args) -> int:
    tag, data, op = write(api, args, "POST", f"{COACH}/workflows/{seg(args.into)}/guidance/import",
                          "import_guidance", {"sources": list(args.guidance)})
    imported = (data or {}).get("imported") or []
    names = ", ".join(f"{ref_of(item.get('guidance'))} (from {item.get('from') or '?'})"
                      for item in imported if isinstance(item, dict))
    return report(args, tag, f"imported into {args.into}: {names or 'nothing new'}", data, op)


def cmd_set_effort(api: Api, args) -> int:
    target = args.target
    job = "#" in target or args.job
    path = f"{COACH}/{'jobs' if job else 'workflows'}/{seg(target)}/effort"
    if args.effort == "clear":
        if args.said:
            raise CoachError("--said goes with quick or explore, not clear.", EXIT_USAGE)
        payload: dict = {"effort": None}
    else:
        if not args.said:
            raise CoachError("--said is required: the person's own words when they chose this.",
                             EXIT_USAGE)
        payload = {"effort": args.effort, "said": args.said}
    tag, data, op = write(api, args, "PUT", path, "set_effort", payload)
    if args.effort == "clear":
        what = f"coaching effort of {target} cleared" + (
            " (the workflow's applies)" if job else "")
    else:
        what = f"coaching effort of {target}: {args.effort} (their words: {args.said!r})"
    return report(args, tag, what, data, op)


def cmd_close_job(api: Api, args) -> int:
    if args.note and not args.verdict:
        raise CoachError("--note goes with --verdict.", EXIT_USAGE)
    payload: dict = {}
    if args.used:
        payload["used"] = args.used
    verdict = None
    if args.verdict:
        verdict = {"verdict": args.verdict}
        if args.note:
            verdict["note"] = args.note
        payload["verdict"] = verdict
    path = f"{COACH}/jobs/{seg(args.job)}/close"
    try:
        tag, data, op = write(api, args, "POST", path, "close_job", payload)
    except CoachError as err:
        if not (verdict and err.code == "job_not_open"):
            raise
        # Already closed: the person's verdict is recorded as a new revision.
        if args.used:
            verdict = dict(verdict, candidate=args.used)
        tag, data, op = write(api, args, "POST", f"{COACH}/jobs/{seg(args.job)}/verdicts",
                              "record_verdict", verdict)
        return report(args, tag, f"verdict on {args.job}: {args.verdict} (the job was already closed)",
                      data, op)
    what = f"job {args.job} closed"
    if args.used:
        what += f", used {args.used}"
    if args.verdict:
        what += f", verdict {args.verdict}"
    return report(args, tag, what, data, op)


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
        "feedback": read_all(api, f"{COACH}/workflows/{wf}/feedback", {"include_retracted": "true"}),
    }
    print_json(history)
    return EXIT_OK


def cmd_export(api: Api, args) -> int:
    data = read(api, f"{COACH}/workflows/{seg(args.workflow)}/export", {"format": args.format}) or {}
    if args.format == "markdown" and isinstance(data, dict) and "markdown" in data:
        text = data["markdown"]
        if args.out:
            pathlib.Path(args.out).write_text(text, encoding="utf-8")
            print(f"Exported {args.workflow} to {args.out} ({len(text)} characters).")
        else:
            print(text)
        return EXIT_OK
    if args.out:
        pathlib.Path(args.out).write_text(json.dumps(data, indent=2, ensure_ascii=False), encoding="utf-8")
        print(f"Exported {args.workflow} to {args.out}.")
    else:
        print_json(data)
    return EXIT_OK


def cmd_delete_workflow(api: Api, args) -> int:
    slug = args.workflow
    if is_uuid(slug):
        status, _, body = api.request("GET", f"{COACH}/workflows/{seg(slug)}")
        slug = (unwrap(body) or {}).get("slug") if status == 200 else None
    if slug is not None and args.confirm != slug:
        raise CoachError(
            f"[COACH_NOT_SAVED] Not deleted: --confirm must be exactly the workflow's slug "
            f"({slug}). Deleting hides it now and erases everything in it within 30 days.",
            EXIT_USAGE)
    tag, data, op = write(api, args, "DELETE", f"{COACH}/workflows/{seg(args.workflow)}",
                          "delete_workflow", {"confirm": args.confirm})
    erase = (data or {}).get("erase_after")
    return report(args, tag, f"workflow {args.confirm} deleted: hidden now, erased"
                  + (f" after {erase}" if erase else " within 30 days"), data, op)


# ---------------------------------------------------------------------------
# CLI
# ---------------------------------------------------------------------------


def _guidance_status_flags(p) -> None:
    flags = p.add_mutually_exclusive_group()
    flags.add_argument("--remove", action="store_true", help="remove it (the person asked)")
    flags.add_argument("--restore", action="store_true", help="make it active again")


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
    p.add_argument("--output-kind",
                   choices=["text", "link", "image", "audio", "video", "document", "mixed"])
    p = sub.add_parser("refine-goal", help="change a workflow's goal (the person asked)")
    p.add_argument("workflow")
    p.add_argument("--goal", required=True)
    p.add_argument("--reason", required=True)
    p.add_argument("--source-feedback", help="the feedback that prompted it, like weekly-note#4/f1")
    p.add_argument("--expected-version", type=int)
    p = sub.add_parser("start-job", help="start a job and print its context")
    p.add_argument("workflow")
    p.add_argument("--objective", required=True)
    p.add_argument("--label")
    p = sub.add_parser("save-output", help="save a draft (text, a link or media) as a new version")
    p.add_argument("job")
    p.add_argument("--title")
    source = p.add_mutually_exclusive_group()
    source.add_argument("--file", help="text file with the draft, or a link's text snapshot "
                                       "(default for text: stdin)")
    source.add_argument("--media", help="image, audio, video or document file")
    p.add_argument("--url", help="an https link to the artifact (with --file for a text snapshot)")
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
    p.add_argument("--reading", help="your reading of their words, kept apart from them")
    p.add_argument("--excerpt")
    p.add_argument("--from", dest="time_from")
    p.add_argument("--to", dest="time_to")
    p.add_argument("--picked", action="store_true", help="the person picked one of your suggestions")
    p.add_argument("--repeat-of", help="guidance this feedback repeats, like weekly-note/g3")
    p.add_argument("--applies-when", help="where reusable feedback applies")
    p.add_argument("--leak", action="store_true", help="the person saw feedback from another job")
    p = sub.add_parser("retract-feedback", help="take back feedback the person withdrew")
    p.add_argument("feedback")
    p.add_argument("--reason")
    p = sub.add_parser("rescope", help="make feedback reusable or this-job only (the person asked)")
    p.add_argument("feedback")
    p.add_argument("--scope", required=True, choices=sorted(FEEDBACK_SCOPES))
    p = sub.add_parser("edit-guidance", help="edit, limit, remove or restore guidance (the person asked)")
    p.add_argument("guidance")
    p.add_argument("--text")
    p.add_argument("--applies-when", help='where it applies; "" removes the limit')
    p.add_argument("--reason")
    p.add_argument("--expected-version", type=int)
    _guidance_status_flags(p)
    p = sub.add_parser("retire-guidance", help="retire (or remove, restore) guidance (the person asked)")
    p.add_argument("guidance")
    p.add_argument("--reason")
    p.add_argument("--expected-version", type=int)
    _guidance_status_flags(p)
    p = sub.add_parser("import-guidance", help="reuse guidance of another workflow (the person asked)")
    p.add_argument("guidance", nargs="+")
    p.add_argument("--into", required=True, help="the workflow to copy it into")
    p = sub.add_parser("set-effort", help="the coaching effort the person asked for")
    p.add_argument("target", metavar="WORKFLOW|JOB")
    p.add_argument("--effort", required=True, choices=EFFORTS)
    p.add_argument("--said", help="the person's own words")
    p.add_argument("--job", action="store_true", help="the target is a job given by id")
    p = sub.add_parser("close-job", help="close a job with the version used and the person's verdict")
    p.add_argument("job")
    p.add_argument("--used", help="the version the person used, like weekly-note#4/v2")
    p.add_argument("--verdict", choices=["better", "same", "worse"])
    p.add_argument("--note")
    p = sub.add_parser("compare", help="compare a job's latest version with the last job's, or two versions")
    p.add_argument("left")
    p.add_argument("right", nargs="?")
    p = sub.add_parser("history", help="goal versions, guidance and feedback of a workflow")
    p.add_argument("workflow")
    p = sub.add_parser("export", help="everything in a workflow, as Markdown or JSON")
    p.add_argument("workflow")
    p.add_argument("--format", choices=["markdown", "json"], default="markdown")
    p.add_argument("--out", help="write it to this file")
    p = sub.add_parser("delete-workflow", help="delete a workflow (the person asked and confirmed)")
    p.add_argument("workflow")
    p.add_argument("--confirm", required=True, help="the workflow's slug, exactly")
    return parser


COMMANDS = {
    "doctor": cmd_doctor, "list": cmd_list, "get": cmd_get, "context": cmd_context,
    "create": cmd_create, "refine-goal": cmd_refine_goal, "start-job": cmd_start_job,
    "save-output": cmd_save_output, "add-feedback": cmd_add_feedback,
    "retract-feedback": cmd_retract_feedback, "rescope": cmd_rescope,
    "edit-guidance": cmd_edit_guidance, "retire-guidance": cmd_retire_guidance,
    "import-guidance": cmd_import_guidance, "set-effort": cmd_set_effort,
    "close-job": cmd_close_job, "compare": cmd_compare, "history": cmd_history,
    "export": cmd_export, "delete-workflow": cmd_delete_workflow,
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
