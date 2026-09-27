#!/usr/bin/env python3
"""Replay ContextStream's advertised MCP tools against real model providers.

Providers validate tool schemas strictly and reject the whole request when a
single tool is invalid (Moonshot/Kimi answers HTTP 400 outside its schema
dialect). This sends the exact tool surfaces the server advertises as
OpenAI-compatible function tools, then checks that each provider accepts them
and answers with a well-formed call to one of those tools.

Opt-in and networked; providers without an API key are skipped:

  python3 testing/providers/check_tool_schemas.py
  python3 testing/providers/check_tool_schemas.py --tools surfaces.json --surface hosted

Without --tools, the surfaces are dumped by the ignored Rust test
`dump_advertised_tools_for_provider_check` (needs cargo).

Providers (the API key enables each one; base URL and model are overridable):
  MOONSHOT_API_KEY  MOONSHOT_BASE_URL  MOONSHOT_MODEL
  ZAI_API_KEY       ZAI_BASE_URL       ZAI_MODEL
  PROVIDER_CHECK_API_KEY  PROVIDER_CHECK_BASE_URL  PROVIDER_CHECK_MODEL
    (any other OpenAI-compatible endpoint: OpenRouter, Qwen, DeepSeek, ...)
  PROVIDER_CHECK_EXTRA_BODY  optional JSON object merged into each request,
    e.g. '{"provider": {"order": ["moonshotai"], "allow_fallbacks": false}}'
    to pin OpenRouter to the model maker's own serving stack.
"""
import argparse
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import urllib.error
import urllib.request

PROVIDERS = (
    {
        "name": "moonshot",
        "key": "MOONSHOT_API_KEY",
        "base_url": ("MOONSHOT_BASE_URL", "https://api.moonshot.ai/v1"),
        "model": ("MOONSHOT_MODEL", "kimi-k2.5"),
    },
    {
        "name": "zai",
        "key": "ZAI_API_KEY",
        "base_url": ("ZAI_BASE_URL", "https://api.z.ai/api/paas/v4"),
        "model": ("ZAI_MODEL", "glm-4.7"),
    },
    {
        "name": "custom",
        "key": "PROVIDER_CHECK_API_KEY",
        "base_url": ("PROVIDER_CHECK_BASE_URL", None),
        "model": ("PROVIDER_CHECK_MODEL", None),
        "extra_body": "PROVIDER_CHECK_EXTRA_BODY",
    },
)
DEFAULT_SURFACES = ("hosted", "compact", "router")
PROMPT = (
    "Find where this project normalizes MCP tool input schemas. "
    "Use the ContextStream tools rather than answering from memory."
)
REPOSITORY_ROOT = Path(__file__).resolve().parents[2]

PASS, WARN, FAIL = "PASS", "WARN", "FAIL"


def configured_providers(env):
    """Providers whose API key is present, with resolved base URL and model."""
    providers = []
    for spec in PROVIDERS:
        key = env.get(spec["key"], "").strip()
        if not key:
            continue
        base_env, base_default = spec["base_url"]
        model_env, model_default = spec["model"]
        base_url = env.get(base_env, "").strip() or base_default
        model = env.get(model_env, "").strip() or model_default
        if not base_url or not model:
            raise ValueError(f"{spec['name']}: set {base_env} and {model_env} with {spec['key']}")
        provider = {"name": spec["name"], "api_key": key, "base_url": base_url.rstrip("/"), "model": model}
        extra = env.get(spec.get("extra_body", ""), "").strip()
        if extra:
            extra_body = json.loads(extra)
            if not isinstance(extra_body, dict):
                raise ValueError(f"{spec['extra_body']} must be a JSON object")
            provider["extra_body"] = extra_body
        providers.append(provider)
    return providers


def to_function_tools(tools):
    """MCP `tools/list` entries -> OpenAI-compatible `tools` payload."""
    return [
        {
            "type": "function",
            "function": {
                "name": tool["name"],
                "description": tool.get("description", ""),
                "parameters": tool["inputSchema"],
            },
        }
        for tool in tools
    ]


def build_request(model, tools, extra_body=None):
    # No temperature: thinking models (e.g. Kimi K3) reject non-default values.
    request = {
        "model": model,
        "messages": [{"role": "user", "content": PROMPT}],
        "tools": to_function_tools(tools),
        "tool_choice": "auto",
        "max_tokens": 8192,
    }
    request.update(extra_body or {})
    return request


def classify_response(status, body, tool_names):
    """Return (verdict, detail) for one provider response."""
    if status != 200:
        return FAIL, f"HTTP {status}: {body[:400]}"
    try:
        message = json.loads(body)["choices"][0]["message"]
    except (ValueError, KeyError, IndexError, TypeError):
        return FAIL, f"unparseable completion: {body[:200]}"
    calls = message.get("tool_calls") or []
    if not calls:
        return WARN, "accepted the tools but answered without calling one"
    for call in calls:
        function = call.get("function") or {}
        name = function.get("name")
        if name not in tool_names:
            return FAIL, f"called unknown tool {name!r}"
        try:
            arguments = json.loads(function.get("arguments") or "{}")
        except ValueError:
            return FAIL, f"{name}: arguments are not JSON: {function.get('arguments')!r:.200}"
        if not isinstance(arguments, dict):
            return FAIL, f"{name}: arguments are not an object"
    return PASS, ", ".join(call["function"]["name"] for call in calls)


def post(provider, payload, timeout):
    request = urllib.request.Request(
        f"{provider['base_url']}/chat/completions",
        data=json.dumps(payload).encode(),
        headers={
            "Authorization": f"Bearer {provider['api_key']}",
            "Content-Type": "application/json",
        },
        method="POST",
    )
    try:
        with urllib.request.urlopen(request, timeout=timeout) as response:
            return response.status, response.read().decode("utf-8", "replace")
    except urllib.error.HTTPError as error:
        return error.code, error.read().decode("utf-8", "replace")


def dump_surfaces():
    with tempfile.TemporaryDirectory() as directory:
        path = Path(directory) / "surfaces.json"
        subprocess.run(
            [
                "cargo", "test", "--locked", "-p", "mcp-server", "--lib",
                "dump_advertised_tools_for_provider_check", "--", "--ignored", "--exact",
                "server::tests::dump_advertised_tools_for_provider_check",
            ],
            cwd=REPOSITORY_ROOT,
            env={**os.environ, "CONTEXTSTREAM_DUMP_TOOLS_TO": str(path)},
            check=True,
            stdout=subprocess.DEVNULL,
        )
        return json.loads(path.read_text())


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--tools", help="surfaces JSON (default: dump via cargo)")
    parser.add_argument("--surface", action="append", help="surface(s) to check")
    parser.add_argument("--timeout", type=float, default=180.0)
    args = parser.parse_args(argv)

    providers = configured_providers(os.environ)
    if not providers:
        print("SKIPPED: no provider API key set (see --help); nothing was checked.")
        return 0

    surfaces = json.loads(Path(args.tools).read_text()) if args.tools else dump_surfaces()
    selected = args.surface or [name for name in DEFAULT_SURFACES if name in surfaces]
    failures = 0
    for provider in providers:
        for surface in selected:
            tools = surfaces[surface]
            request = build_request(provider["model"], tools, provider.get("extra_body"))
            status, body = post(provider, request, args.timeout)
            verdict, detail = classify_response(status, body, {tool["name"] for tool in tools})
            failures += verdict == FAIL
            print(f"{verdict} {provider['name']}/{provider['model']} {surface} ({len(tools)} tools): {detail}")
    return 1 if failures else 0


if __name__ == "__main__":
    sys.exit(main())
