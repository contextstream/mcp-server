"""Offline tests for the live provider schema check (no network)."""
import json
import unittest

import check_tool_schemas as check

TOOLS = [
    {
        "name": "search",
        "description": "Search code",
        "inputSchema": {"type": "object", "properties": {"query": {"type": "string"}}},
    }
]


def completion(tool_calls=None, content="done"):
    return json.dumps(
        {"choices": [{"message": {"role": "assistant", "content": content, "tool_calls": tool_calls}}]}
    )


class ConfiguredProvidersTest(unittest.TestCase):
    def test_only_providers_with_keys_are_enabled(self):
        self.assertEqual(check.configured_providers({}), [])
        providers = check.configured_providers({"MOONSHOT_API_KEY": "k"})
        self.assertEqual(
            providers,
            [{"name": "moonshot", "api_key": "k", "base_url": "https://api.moonshot.ai/v1", "model": "kimi-k2.5"}],
        )

    def test_overrides_and_custom_endpoint(self):
        providers = check.configured_providers(
            {
                "ZAI_API_KEY": "z",
                "ZAI_BASE_URL": "https://api.z.ai/api/coding/paas/v4/",
                "ZAI_MODEL": "glm-5.2",
                "PROVIDER_CHECK_API_KEY": "c",
                "PROVIDER_CHECK_BASE_URL": "https://openrouter.ai/api/v1",
                "PROVIDER_CHECK_MODEL": "qwen/qwen3-coder",
            }
        )
        self.assertEqual([p["name"] for p in providers], ["zai", "custom"])
        self.assertEqual(providers[0]["base_url"], "https://api.z.ai/api/coding/paas/v4")
        self.assertEqual(providers[0]["model"], "glm-5.2")

    def test_extra_body_pins_upstream_provider(self):
        providers = check.configured_providers(
            {
                "PROVIDER_CHECK_API_KEY": "c",
                "PROVIDER_CHECK_BASE_URL": "https://openrouter.ai/api/v1",
                "PROVIDER_CHECK_MODEL": "moonshotai/kimi-k3",
                "PROVIDER_CHECK_EXTRA_BODY": '{"provider": {"order": ["moonshotai"], "allow_fallbacks": false}}',
            }
        )
        request = check.build_request("moonshotai/kimi-k3", TOOLS, providers[0]["extra_body"])
        self.assertEqual(request["provider"], {"order": ["moonshotai"], "allow_fallbacks": False})
        with self.assertRaises(ValueError):
            check.configured_providers(
                {
                    "PROVIDER_CHECK_API_KEY": "c",
                    "PROVIDER_CHECK_BASE_URL": "https://x",
                    "PROVIDER_CHECK_MODEL": "m",
                    "PROVIDER_CHECK_EXTRA_BODY": "[1]",
                }
            )

    def test_custom_endpoint_requires_url_and_model(self):
        with self.assertRaises(ValueError):
            check.configured_providers({"PROVIDER_CHECK_API_KEY": "c"})


class RequestTest(unittest.TestCase):
    def test_mcp_tools_become_function_tools_verbatim(self):
        request = check.build_request("kimi-k2.5", TOOLS)
        self.assertEqual(request["tool_choice"], "auto")
        self.assertNotIn("temperature", request)
        self.assertEqual(
            request["tools"],
            [{"type": "function", "function": {"name": "search", "description": "Search code", "parameters": TOOLS[0]["inputSchema"]}}],
        )


class ClassifyResponseTest(unittest.TestCase):
    names = {"search"}

    def test_rejected_schema_fails_with_provider_message(self):
        verdict, detail = check.classify_response(
            400, '{"error":"tools.function.parameters is not a valid moonshot flavored json schema"}', self.names
        )
        self.assertEqual(verdict, check.FAIL)
        self.assertIn("moonshot flavored", detail)

    def test_well_formed_tool_call_passes(self):
        body = completion([{"id": "1", "type": "function", "function": {"name": "search", "arguments": '{"query":"schema"}'}}])
        self.assertEqual(check.classify_response(200, body, self.names), (check.PASS, "search"))

    def test_malformed_arguments_fail(self):
        body = completion([{"function": {"name": "search", "arguments": "<arg_key>query</arg_key>"}}])
        self.assertEqual(check.classify_response(200, body, self.names)[0], check.FAIL)

    def test_unknown_tool_fails(self):
        body = completion([{"function": {"name": "grep", "arguments": "{}"}}])
        self.assertEqual(check.classify_response(200, body, self.names)[0], check.FAIL)

    def test_accepted_without_tool_call_warns(self):
        self.assertEqual(check.classify_response(200, completion(), self.names)[0], check.WARN)


if __name__ == "__main__":
    unittest.main()
