"""The Agent Coach Kit plugin package: submission rules, the ZIP, and the skill kept in step with the kit."""

import importlib.util
import json
import pathlib
import re
import shutil
import tempfile
import unittest
import zipfile

ROOT = pathlib.Path(__file__).resolve().parents[2]
PLUGINS = ROOT / "plugins"
spec = importlib.util.spec_from_file_location("build_agent_coach_kit", PLUGINS / "build_agent_coach_kit.py")
build = importlib.util.module_from_spec(spec)
spec.loader.exec_module(build)

PLUGIN_SKILL = PLUGINS / "agent-coach-kit" / "skills" / "agent-coach" / "SKILL.md"
KIT_SKILL = ROOT / "kits" / "agent-coach-kit" / "codex-cloud" / ".agents" / "skills" / "contextstream-coach" / "SKILL.md"


def section(text, title):
    start = text.index(f"## {title}\n")
    end = text.find("\n## ", start + 1)
    return text[start:end if end != -1 else len(text)]


class PluginPackageTest(unittest.TestCase):
    def test_the_package_meets_the_checkable_submission_rules(self):
        manifest = build.validate()
        self.assertEqual(manifest["name"], "agent-coach-kit")
        face = manifest["extensions"]["com.openai"]["interface"]
        self.assertEqual(face["displayName"], "Agent Coach Kit")

    def test_the_mcp_server_is_the_hosted_coach_endpoint_with_oauth(self):
        servers = json.loads((PLUGINS / "agent-coach-kit" / "mcp.json").read_text())
        (server,) = servers["mcpServers"].values()
        self.assertEqual(server["url"], "https://mcp.contextstream.io/coach")
        auth = server["extensions"]["com.openai"]["auth"]
        self.assertEqual(auth["type"], "oauth")
        self.assertEqual(sorted(auth["baseScopes"]), ["read:coach", "write:coach"])

    def test_the_zip_holds_the_package_and_nothing_else(self):
        with tempfile.TemporaryDirectory() as out:
            target = build.build(out)
            with zipfile.ZipFile(target) as archive:
                names = sorted(archive.namelist())
            self.assertIn("plugin.json", names)
            self.assertIn("mcp.json", names)
            self.assertIn("skills/agent-coach/SKILL.md", names)
            self.assertIn("assets/icon.svg", names)
            self.assertFalse([n for n in names if n.startswith(".") or "/." in n or n.endswith(".py")], names)
            folder = build.build(out, folder=True)
            with zipfile.ZipFile(folder) as archive:
                self.assertTrue(all(n.startswith("agent-coach-kit/") for n in archive.namelist()))

    def test_test_cases_use_the_real_tools_and_actions(self):
        manifest = json.loads((PLUGINS / "agent-coach-kit" / "plugin.json").read_text())
        cases = manifest["extensions"]["com.openai"]["review"]["test_cases"]
        text = json.dumps(cases)
        for action in ("create", "start_job", "save_output", "add_feedback", "record_verdict", "compare"):
            self.assertIn(action, text)

    def test_the_skill_keeps_the_kits_rules(self):
        kit_rules = section(KIT_SKILL.read_text(), "Rules")
        plugin = PLUGIN_SKILL.read_text()
        plugin_rules = section(plugin, "Rules")
        leads = re.findall(r"^\d+\. \*\*(.+?)\*\*", kit_rules, re.M)
        self.assertEqual(len(leads), 6)
        for lead in leads:
            self.assertIn(f"**{lead}**", plugin_rules)
        # The plugin speaks in tools, never in the kit's command line.
        self.assertNotIn("coach.py", plugin)
        for tool in ("`workflow`", "`workflow_save`", "`workflow_delete`"):
            self.assertIn(tool, plugin)

    def test_broken_packages_are_refused(self):
        def broken(change):
            with tempfile.TemporaryDirectory() as tmp:
                copy = pathlib.Path(tmp) / "agent-coach-kit"
                shutil.copytree(PLUGINS / "agent-coach-kit", copy)
                change(copy)
                original, build.PLUGIN = build.PLUGIN, copy
                try:
                    with self.assertRaises(build.PackageError):
                        build.validate()
                finally:
                    build.PLUGIN = original

        def edit(name, mutate):
            def change(copy):
                data = json.loads((copy / name).read_text())
                mutate(data)
                (copy / name).write_text(json.dumps(data))
            return change

        interface = lambda m: m["extensions"]["com.openai"]["interface"]
        broken(edit("plugin.json", lambda m: interface(m).update(shortDescription="x" * 31)))
        broken(edit("plugin.json", lambda m: interface(m).update(privacyPolicyURL="http://contextstream.io/privacy")))
        broken(edit("plugin.json", lambda m: m["extensions"]["com.openai"]["review"]["test_cases"]["negative"].pop()))
        broken(edit("plugin.json", lambda m: m["extensions"]["com.openai"].update(hooks="./hooks.json")))
        broken(edit("mcp.json", lambda m: m["mcpServers"]["agent-coach-kit"].update(headers={"X-API-Key": "k"})))
        broken(lambda copy: (copy / "assets" / "icon.svg").unlink())


if __name__ == "__main__":
    unittest.main()
