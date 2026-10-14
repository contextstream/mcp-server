#!/usr/bin/env python3
"""Tool-level dry run of the Agent Coach Kit's plugin review cases against a live /coach endpoint.

    CONTEXTSTREAM_API_KEY=... python3 plugins/review/dry_run.py [--url https://mcp.contextstream.io/coach] [--keep]

What it proves: the hosted tools behave the way each review case expects (create, start a job, save a
version, keep reusable and this-job-only feedback apart, carry only the reusable one into the next job,
compare without a score, record the person's verdict, refuse an unconfirmed delete) and that no tool exists
that could post or send anything. What it can NOT prove: what ChatGPT decides to call for a given prompt.
That needs the reviewer-account run in ChatGPT (see RUN-SHEET.md).

It creates one workflow named "Review dry run <MMDD HHMM>" on the account the key belongs to, never touches
any other workflow, and deletes its own with the exact slug at the end (skip with --keep). Standard library
only. The key is read from the environment and never printed. Exit code 0 only if every check passes.
"""

import argparse
import json
import os
import re
import sys
import time
import urllib.error
import urllib.request

DEFAULT_URL = "https://mcp.contextstream.io/coach"
BAD_PHRASES = ("without being asked", "never claim", "never say", "unless the reply", "never invent approval", "nothing is deleted")


class Client:
    def __init__(self, url, key):
        self.url, self.key, self.sid, self.n = url, key, None, 0
        init = self.rpc("initialize", {"protocolVersion": "2025-06-18", "capabilities": {},
                                        "clientInfo": {"name": "agent-coach-kit-review-dry-run", "version": "1"}})
        self.server = init["result"]["serverInfo"]
        self.instructions = init["result"].get("instructions", "")
        self.rpc("notifications/initialized", {}, notify=True)

    def rpc(self, method, params, notify=False):
        self.n += 1
        body = {"jsonrpc": "2.0", "method": method, "params": params}
        if not notify:
            body["id"] = self.n
        headers = {"Content-Type": "application/json", "Accept": "application/json, text/event-stream",
                   "X-ContextStream-API-Key": self.key,
                   # Cloudflare in front of the endpoint refuses urllib's default user agent.
                   "User-Agent": "agent-coach-kit-review-dry-run/1 (+https://contextstream.io)"}
        if self.sid:
            headers["Mcp-Session-Id"] = self.sid
        request = urllib.request.Request(self.url, data=json.dumps(body).encode(), method="POST", headers=headers)
        try:
            with urllib.request.urlopen(request, timeout=60) as response:
                raw = response.read().decode()
                self.sid = response.headers.get("Mcp-Session-Id") or self.sid
        except urllib.error.HTTPError as error:
            sys.exit(f"HTTP {error.code} from {method}: is the key valid and the account on the Coach pilot?")
        if notify or not raw.strip():
            return {}
        data = [line[5:].strip() for line in raw.splitlines() if line.startswith("data:")]
        return json.loads(data[-1] if data else raw)

    def tools(self):
        return {tool["name"]: tool for tool in self.rpc("tools/list", {})["result"]["tools"]}

    def call(self, tool, **arguments):
        result = self.rpc("tools/call", {"name": tool, "arguments": arguments}).get("result", {})
        text = "".join(part.get("text", "") for part in result.get("content", []))
        return text, bool(result.get("isError"))


results = []


def check(label, condition, detail=""):
    results.append(bool(condition))
    print(("PASS  " if condition else "FAIL  ") + label)
    if not condition and detail:
        print("      " + detail.replace("\n", " ")[:300])


def etag_of(text):
    match = re.search(r"context_etag[^A-Za-z0-9]{0,6}([A-Za-z0-9._:-]{6,})", text)
    return match.group(1) if match else None


def main():
    parser = argparse.ArgumentParser(description=__doc__.split("\n")[0])
    parser.add_argument("--url", default=DEFAULT_URL)
    parser.add_argument("--keep", action="store_true", help="leave the dry-run workflow in the account")
    args = parser.parse_args()
    key = os.environ.get("CONTEXTSTREAM_API_KEY", "").strip()
    if not key:
        sys.exit("Set CONTEXTSTREAM_API_KEY to the key of the account to test with (never the reviewer's password).")
    c = Client(args.url, key)
    print(f"server {c.server.get('name')} {c.server.get('version')}\n")

    tools = c.tools()
    check("only the three coaching tools exist, so nothing can post or email", sorted(tools) == ["workflow", "workflow_delete", "workflow_save"], str(sorted(tools)))
    flagged = [p for name, tool in tools.items() for p in BAD_PHRASES if p in tool.get("description", "").lower()]
    flagged += [p for p in BAD_PHRASES if p in c.instructions.lower()]
    check("tool text has none of the phrases ChatGPT flagged as suspicious instructions", not flagged, str(flagged))
    check("workflow is read-only", tools["workflow"].get("annotations", {}).get("readOnlyHint") is True)
    check("workflow_delete is marked destructive", tools["workflow_delete"].get("annotations", {}).get("destructiveHint") is True)

    stamp = time.strftime("%m%d %H%M")
    name = f"Review dry run {stamp}"
    text, _ = c.call("workflow_save", action="create", name=name, output_kind="text", audience="Leadership team",
                     goal="Each Monday, a short update for the leadership team: what changed, who is affected and what they need to do.")
    check("case 1: the workflow is created and saved", "[COACH_SAVED]" in text, text)
    found = re.search(r"review-dry-run-\d+-\d+", text)
    if not found:
        sys.exit("Could not read the new workflow's reference from the reply; stopping before anything else is touched.")
    ref = found.group(0)
    try:
        text, _ = c.call("workflow_save", action="start_job", workflow=ref,
                         objective="Write this week's update: the billing migration finished and invoices now use the new layout.")
        job = (re.search(re.escape(ref) + r"#\d+", text) or [None])[0]
        check("case 2: the job starts and returns its context", job is not None and etag_of(text) is not None, text)
        text, _ = c.call("workflow_save", action="save_output", job=job, context_etag=etag_of(text),
                         text="Billing migration finished this week. Invoices now use the new layout. No action is needed from leadership.\n\nThe migration ran over the weekend with no downtime.",
                         carry_note="First draft; no guidance yet.")
        v1 = f"{job}/v1"
        check("case 2: the draft is saved as a version before it is shown", "[COACH_SAVED]" in text and v1 in text, text)

        text, _ = c.call("workflow_save", action="add_feedback", output=v1, kind="correction", scope="workflow",
                         words="Too long. Every time, open with what leadership needs to do.",
                         interpretation="Open every update with the action leadership needs to take; keep it short.")
        check("case 3: the reusable reaction is saved and becomes guidance", "[COACH_SAVED]" in text and "guidance" in text.lower(), text)
        text, _ = c.call("workflow_save", action="add_feedback", output=v1, kind="direction", scope="job",
                         words="Just for this week, mention the Friday maintenance window.",
                         interpretation="Mention the Friday maintenance window in this week's update only.")
        check("case 3: the this-week-only reaction is saved for this job", "[COACH_SAVED]" in text, text)
        text, _ = c.call("workflow_save", action="save_output", job=job, revises=v1,
                         text="Action for leadership: none this week.\n\nThe billing migration finished and invoices now use the new layout. Friday's maintenance window is the only change to plan around.",
                         carry_note="Applied the reusable correction (open with the action) and the this-week-only window mention.")
        v2 = f"{job}/v2"
        check("case 3: the revision is saved as a new version", "[COACH_SAVED]" in text and v2 in text, text)
        text, _ = c.call("workflow_save", action="close_job", job=job, used=v2)
        check("case 3: the job closes with the version that was used", "[COACH_SAVED]" in text, text)

        text, _ = c.call("workflow_save", action="start_job", workflow=ref,
                         objective="Write next week's leadership update: the new weekly report ships on Monday.")
        job2 = (re.search(re.escape(ref) + r"#2\b", text) or [None])[0]
        packet, _ = c.call("workflow", action="context", ref=job2) if job2 else ("", False)
        both = (text + "\n" + packet).lower()
        check("case 4: the next job's context carries the reusable guidance", "open every update with the action" in both or "open with what leadership needs to do" in both, packet[:300])
        check("case 4: the this-week-only request does not carry over", "friday" not in both, "the one-off appeared in the next job's context")
        text, _ = c.call("workflow_save", action="save_output", job=job2, context_etag=etag_of(packet) or etag_of(text),
                         text="Action for leadership: none.\n\nThe new weekly report ships on Monday.",
                         carry_note="Opened with the action, per the reusable guidance.")
        v3 = f"{job2}/v1"
        check("case 4: the next draft is saved as a version", "[COACH_SAVED]" in text and v3 in text, text)

        text, _ = c.call("workflow", action="compare", a=v2, b=v3)
        check("case 5: both versions are shown side by side with no score", v2 in text and v3 in text and "no score" in text.lower(), text)
        text, _ = c.call("workflow_save", action="record_verdict", job=job2, baseline=v2, candidate=v3, verdict="better",
                         note="This one is better: I didn't have to repeat myself.")
        check("case 5: the person's verdict is recorded in their words", "[COACH_SAVED]" in text, text)

        text, is_error = c.call("workflow_delete", action="delete", workflow=ref)
        check("negative 2: deleting without a confirmation is refused", "[COACH_SAVED]" not in text, text)
        text, _ = c.call("workflow_delete", action="delete", workflow=ref, confirm="leadership")
        check("negative 2: deleting with the wrong confirmation is refused", "[COACH_SAVED]" not in text, text)
        text, _ = c.call("workflow", action="list")
        check("negative 2: the workflow is still there after both refusals", ref in text, text)
    finally:
        if args.keep:
            print(f"\nkept {ref} (delete it with workflow_delete and confirm={ref})")
        else:
            text, _ = c.call("workflow_delete", action="delete", workflow=ref, confirm=ref)
            gone, _ = c.call("workflow", action="list")
            check("cleanup: the dry-run workflow is deleted with its exact slug", ref not in gone, text)

    print("\nNot checked here (only the reviewer-account run in ChatGPT can show it): negative 1 (a one-off poem calls no tool),")
    print("negative 3 (a request to post or email is declined, and nothing is claimed sent), and which tools ChatGPT chooses for each prompt.")
    print("\nRESULT:", "ALL PASS" if all(results) else f"{results.count(False)} FAILED")
    sys.exit(0 if all(results) else 1)


if __name__ == "__main__":
    main()
