#!/usr/bin/env python3
"""Check and package the Agent Coach Kit plugin for the ChatGPT and Codex plugin directory.

    python3 plugins/build_agent_coach_kit.py [--out DIR] [--folder]

Writes agent-coach-kit-<version>.zip with plugin.json at the archive root (or, with
--folder, inside one agent-coach-kit/ folder). It refuses to build a package that breaks
a submission rule it can check: listing lengths, HTTPS URLs, exactly five positive and
three negative test cases naming real tools, one remote MCP server with no headers or
secrets, ./ paths that exist, square icons, and a skill with front matter.
"""

import argparse
import json
import pathlib
import re
import sys
import zipfile

HERE = pathlib.Path(__file__).resolve().parent
PLUGIN = HERE / "agent-coach-kit"
TOOLS = {"workflow", "workflow_save", "workflow_delete"}
PACKAGE_FILES = ("plugin.json", "mcp.json", "skills", "assets")


class PackageError(Exception):
    pass


def check(condition, message):
    if not condition:
        raise PackageError(message)


def https(value, field):
    check(isinstance(value, str) and value.startswith("https://") and len(value) <= 1024,
          f"{field} must be an HTTPS URL of at most 1024 characters")


def relative(path, field):
    check(isinstance(path, str) and path.startswith("./") and ".." not in path.split("/"),
          f"{field} must be a ./ path inside the plugin")
    target = PLUGIN / path[2:]
    check(target.is_file(), f"{field} points at a missing file: {path}")
    return target


def square_icon(path, field):
    target = relative(path, field)
    check(target.suffix.lower() in (".svg", ".png", ".jpg", ".jpeg", ".webp"), f"{field}: unsupported format")
    check(target.stat().st_size <= 5 * 1024 * 1024, f"{field}: larger than 5 MiB")
    if target.suffix.lower() == ".svg":
        box = re.search(r'viewBox="0 0 (\d+(?:\.\d+)?) (\d+(?:\.\d+)?)"', target.read_text())
        check(box and box.group(1) == box.group(2) and float(box.group(1)) >= 48,
              f"{field}: icons must be square and at least 48 by 48")


def validate():
    manifest = json.loads((PLUGIN / "plugin.json").read_text())
    check(manifest.get("$schema") == "https://agent-plugins.org/schemas/1.0.0/plugin.schema.json",
          "plugin.json must declare the Agent Plugins schema")
    check(re.fullmatch(r"[a-z0-9]+(?:-[a-z0-9]+)*", manifest.get("name", "")), "name must be kebab-case")
    check(re.fullmatch(r"\d+\.\d+\.\d+", manifest.get("version", "")), "version must be x.y.z")
    check(manifest.get("description"), "description is required")
    openai = manifest["extensions"]["com.openai"]
    check("apps" not in openai and "hooks" not in openai, "app references and hooks cannot be submitted")
    face = openai["interface"]
    check(0 < len(face["displayName"]) <= 30, "displayName: 1 to 30 characters")
    check(0 < len(face["shortDescription"]) <= 30, "shortDescription: 1 to 30 characters")
    check(0 < len(face["longDescription"]) <= 4000, "longDescription: 1 to 4000 characters")
    for field in ("websiteURL", "supportURL", "privacyPolicyURL", "termsOfServiceURL"):
        https(face.get(field), field)
    for field in ("composerIcon", "logo"):
        square_icon(face[field], field)
    cases = openai["review"]["test_cases"]
    check(len(cases["positive"]) == 5, "exactly five positive test cases")
    check(len(cases["negative"]) == 3, "exactly three negative test cases")
    for case in cases["positive"]:
        for field in ("description", "prompt", "tools_triggered", "expected_behavior"):
            check(case.get(field), f"positive case needs {field}")
        check(set(case["tools_triggered"]) <= TOOLS, f"unknown tool in {case['tools_triggered']}")
    for case in cases["negative"]:
        check(case.get("description") and case.get("prompt"), "negative case needs description and prompt")
    for banned in ("test_credentials", "reviewer_instructions"):
        check(banned not in json.dumps(openai), f"{banned} belongs in the dashboard, not the package")

    servers = json.loads((PLUGIN / "mcp.json").read_text())
    check(servers.get("$schema") == "https://agent-plugins.org/schemas/1.0.0/mcp.schema.json",
          "mcp.json must declare the Agent Plugins MCP schema")
    check(len(servers["mcpServers"]) == 1, "exactly one remote MCP server")
    (server,) = servers["mcpServers"].values()
    check(server["type"] == "streamable-http", "the MCP server must be streamable-http")
    https(server["url"], "mcp url")
    check(not server.get("headers"), "public submission rejects server headers")
    auth = server["extensions"]["com.openai"]["auth"]
    check(auth["type"] in ("oauth", "mixed"), "a protected server needs oauth auth")
    check("clientSecret" not in json.dumps(server), "client secrets never go in the package")
    client = auth.get("client", {})
    check(client.get("mode") in ("provided", "dcr", "cimd"), "client.mode must be provided, dcr or cimd")
    if client["mode"] != "provided":
        check("clientId" not in client, "a dcr or cimd client carries no client id")

    skills = sorted((PLUGIN / "skills").glob("*/SKILL.md"))
    check(skills, "at least one skill")
    for skill in skills:
        text = skill.read_text()
        front = re.match(r"---\nname: ([a-z0-9-]+)\ndescription: (.+?)\n---\n", text, re.S)
        check(front and front.group(1) == skill.parent.name, f"{skill}: front matter name must match its folder")
    return manifest


def build(out_dir, folder=False):
    manifest = validate()
    out_dir = pathlib.Path(out_dir)
    out_dir.mkdir(parents=True, exist_ok=True)
    target = out_dir / f"{manifest['name']}-{manifest['version']}.zip"
    prefix = f"{manifest['name']}/" if folder else ""
    files = [p for name in PACKAGE_FILES for p in sorted((PLUGIN / name).rglob("*") if (PLUGIN / name).is_dir()
                                                          else [PLUGIN / name]) if p.is_file()]
    with zipfile.ZipFile(target, "w", zipfile.ZIP_DEFLATED) as archive:
        for path in files:
            info = zipfile.ZipInfo(prefix + path.relative_to(PLUGIN).as_posix(), date_time=(2026, 1, 1, 0, 0, 0))
            info.compress_type = zipfile.ZIP_DEFLATED
            info.external_attr = 0o644 << 16
            archive.writestr(info, path.read_bytes())
    return target


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    parser.add_argument("--out", default=str(HERE.parent / "dist"))
    parser.add_argument("--folder", action="store_true", help="put the package inside one top-level folder")
    args = parser.parse_args(argv)
    try:
        target = build(args.out, args.folder)
    except PackageError as error:
        print(f"Not built: {error}", file=sys.stderr)
        return 1
    print(f"Built {target}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
