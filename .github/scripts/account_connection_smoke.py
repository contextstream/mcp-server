#!/usr/bin/env python3
"""Exercise the actual stdio binary against a deterministic loopback API."""
import json
import os
from pathlib import Path
import queue
import subprocess
import sys
import tempfile
import threading
import unittest
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

BINARY = Path(sys.argv.pop(1)).resolve()
KEY = 'cbiq_account_smoke_fixture_only'
SECRET = 'signup-client-fixture-only'
TOKEN = 'browser-jwt-fixture-only'


class ConnectionTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.home = Path(self.tmp.name)
        self.creds = self.home / '.contextstream/credentials.json'
        self.pending = self.home / '.contextstream/pending-connection.json'
        self.requests = []
        self.acks = []
        self.fail_ack = False
        self.fail_start = False
        self.inline_available = True
        self.processes = []
        owner = self

        class Handler(BaseHTTPRequestHandler):
            def log_message(self, *_):
                pass

            def do_GET(self):
                self.respond()

            def do_POST(self):
                self.respond()

            def respond(self):
                size = int(self.headers.get('Content-Length', 0))
                body = json.loads(self.rfile.read(size)) if size else {}
                path = self.path
                owner.requests.append((path, body))
                code, result = 200, {}
                if path.endswith('/signup-config'):
                    result = {'phone_required': True, 'inline_signup_available': owner.inline_available}
                elif path.endswith('/device/start'):
                    result = {'device_code': SECRET, 'user_code': 'ABCD-EFGH',
                              'verification_uri': owner.url + '/device', 'expires_in': 600, 'interval': 1}
                elif path.endswith('/device/token'):
                    result = {'status': 'authorized', 'access_token': TOKEN}
                elif path.endswith('/auth/me'):
                    result = {'id': '00000000-0000-0000-0000-000000000001',
                              'email': 'fixture@example.test', 'created_at': '2026-01-01T00:00:00Z'}
                elif path.endswith('/auth/api-keys'):
                    result = {'secret_key': KEY}
                elif path.endswith('/mcp-signup/start'):
                    if owner.fail_start:
                        code, result = 409, {'code': 'account_exists', 'message': 'Account already exists'}
                    else:
                        result = {'attempt_id': 'attempt-fixture', 'client_secret': SECRET,
                                  'email': 'fixture@example.test', 'email_code_expires_in': 600}
                elif path.endswith('/request-sms-consent'):
                    result = {'status': 'consent_required', 'phone_last4': '0123', 'line_type': 'mobile',
                              'consent_text': 'May we send one verification text?', 'consent_version': '1',
                              'consent_token': 'consent-fixture'}
                elif path.endswith('/add-phone'):
                    result = {'status': 'sms_sent', 'phone_last4': '0123'}
                elif path.endswith(('/verify-phone', '/credentials')):
                    result = {'status': 'complete', 'user_id': 'user-fixture', 'email': 'fixture@example.test',
                              'api_key': {'id': 'key-fixture', 'secret': KEY}, 'api_url': owner.url}
                elif path.endswith('/ack'):
                    # This observation fails if the client acknowledges before committing the key.
                    owner.acks.append(json.loads(owner.creds.read_text()).get('api_key') if owner.creds.is_file() else None)
                    code = 503 if owner.fail_ack else 200
                    result = {'status': 'acked'}
                else:
                    result = {'status': 'ok'}
                payload = json.dumps(result).encode()
                self.send_response(code)
                self.send_header('Content-Type', 'application/json')
                self.send_header('Content-Length', str(len(payload)))
                self.end_headers()
                self.wfile.write(payload)

        self.server = ThreadingHTTPServer(('127.0.0.1', 0), Handler)
        self.url = 'http://127.0.0.1:' + str(self.server.server_port)
        threading.Thread(target=self.server.serve_forever, daemon=True).start()

    def tearDown(self):
        for process in self.processes:
            if process.poll() is None:
                process.kill()
            process.wait(timeout=10)
            process.stdin.close()
            process.stdout.close()
        self.server.shutdown()
        self.server.server_close()
        self.tmp.cleanup()

    def launch(self, override=None):
        env = {k: v for k, v in os.environ.items() if not k.startswith('CONTEXTSTREAM_')}
        env.update(HOME=str(self.home), CONTEXTSTREAM_API_URL=self.url,
                   CONTEXTSTREAM_ALLOW_LOCAL_MCP='1', CONTEXTSTREAM_LOG_LEVEL='error',
                   CONTEXTSTREAM_AUTO_UPDATE='false', CONTEXTSTREAM_TRANSCRIPTS_ENABLED='false',
                   CONTEXTSTREAM_HOOK_TRANSCRIPTS_ENABLED='false')
        if override:
            env['CONTEXTSTREAM_API_KEY'] = override
        self.child = subprocess.Popen([str(BINARY)], cwd=self.home, env=env, stdin=subprocess.PIPE,
                                      stdout=subprocess.PIPE, stderr=subprocess.DEVNULL, text=True)
        self.processes.append(self.child)
        self.lines = queue.Queue()
        lines, stdout = self.lines, self.child.stdout
        def read():
            for line in stdout:
                lines.put(line)
        threading.Thread(target=read, daemon=True).start()
        self.seq = 0
        self.rpc('initialize', {'protocolVersion': '2024-11-05', 'capabilities': {},
                               'clientInfo': {'name': 'account-smoke', 'version': '1'}})
        self.child.stdin.write(json.dumps({'jsonrpc': '2.0', 'method': 'notifications/initialized'}) + '\n')
        self.child.stdin.flush()

    def rpc(self, method, params):
        self.seq += 1
        self.child.stdin.write(json.dumps({'jsonrpc': '2.0', 'id': self.seq, 'method': method, 'params': params}) + '\n')
        self.child.stdin.flush()
        while True:
            line = self.lines.get(timeout=45)
            for secret in (KEY, SECRET, TOKEN, "112233", "445566"):
                self.assertNotIn(secret, line, 'secret exposed on JSON-RPC output')
            response = json.loads(line)
            if response.get('id') == self.seq:
                self.assertNotIn('error', response)
                return response['result']

    def account(self, action, **kwargs):
        return self.rpc('tools/call', {'name': 'account', 'arguments': {'action': action, **kwargs}})

    def start_inline(self):
        self.launch()
        result = self.account('signup_start', email='fixture@example.test')
        self.assertFalse(result.get('isError', False), result)
        self.assertEqual(json.loads(self.pending.read_text())['client_secret'], SECRET)
        self.assertEqual(self.pending.stat().st_mode & 0o777, 0o600)

    def verify_to_phone(self):
        self.start_inline()
        self.account('signup_verify_email', code='112233')
        self.account('signup_request_sms_consent', phone='+15555550123')
        self.account('signup_confirm_sms_consent', user_response='yes')

    def test_browser_save_and_authenticated_reconnect(self):
        self.launch()
        names = {t['name'] for t in self.rpc('tools/list', {})['tools']}
        self.assertEqual(names, {'init', 'account'})
        self.account('connect_browser')
        self.account('connect_poll')
        self.assertEqual(json.loads(self.creds.read_text())['api_key'], KEY)
        self.assertEqual(json.loads(self.creds.read_text())['api_url'], self.url)
        self.assertEqual(self.creds.stat().st_mode & 0o777, 0o600)
        self.child.kill()
        self.child.wait()
        self.launch()
        self.assertGreater(len(self.rpc('tools/list', {})['tools']), 2)

    def test_inline_saves_before_ack(self):
        self.verify_to_phone()
        result = self.account('signup_verify_phone', code='445566')
        self.assertFalse(result.get('isError', False), result)
        self.assertEqual(self.acks, [KEY])
        self.assertFalse(self.pending.exists())

    def test_restart_recovers_after_finalize_write_failure(self):
        self.verify_to_phone()
        self.creds.mkdir()  # deterministic write failure, even when tests run as root
        result = self.account('signup_verify_phone', code='445566')
        self.assertTrue(result.get('isError'), result)
        self.assertEqual(self.acks, [])
        self.assertEqual(json.loads(self.pending.read_text())['stage'], 'finalized')
        self.child.kill()
        self.child.wait()
        self.creds.rmdir()
        self.launch()
        result = self.account('signup_start', email='fixture@example.test')
        self.assertIn('fetch_credentials', json.dumps(result))
        self.account('fetch_credentials')
        self.assertEqual(self.acks, [KEY])
        self.assertEqual(sum(p.endswith('/mcp-signup/start') for p, _ in self.requests), 1)

    def test_ack_failure_keeps_saved_credentials(self):
        self.verify_to_phone()
        self.fail_ack = True
        result = self.account('signup_verify_phone', code='445566')
        self.assertFalse(result.get('isError', False), result)
        self.assertEqual(json.loads(self.creds.read_text())['api_key'], KEY)
        self.assertEqual(self.acks, [KEY])

    def test_declined_consent_sends_no_sms(self):
        self.start_inline()
        self.account('signup_verify_email', code='112233')
        self.account('signup_request_sms_consent', phone='+15555550123')
        result = self.account('signup_confirm_sms_consent', user_response='No, do not text me')
        self.assertTrue(result.get('isError'), result)
        self.assertFalse(any(p.endswith('/add-phone') for p, _ in self.requests))

    def test_server_rejection_is_mcp_error(self):
        self.launch()
        self.fail_start = True
        result = self.account('signup_start', email='fixture@example.test')
        self.assertTrue(result.get('isError'), result)
        self.assertIn('connect_browser', json.dumps(result))

    def test_editor_override_warning_survives_save(self):
        self.launch(override='fixture-editor-key')
        self.account('connect_browser')
        result = self.account('connect_poll')
        self.assertIn('editor configuration supplies its own', json.dumps(result))

    def test_failed_account_hook_does_not_persist_inputs(self):
        env = {k: v for k, v in os.environ.items() if not k.startswith('CONTEXTSTREAM_')}
        env.update(HOME=str(self.home), CONTEXTSTREAM_API_URL=self.url,
                   CONTEXTSTREAM_API_KEY='fixture-hook-key')
        payload = {'cwd': str(self.home), 'tool_name': 'mcp__contextstream__account',
                   'tool_input': {'action': 'signup_verify_phone', 'code': '445566'},
                   'error': 'rejected 445566 ' + SECRET}
        result = subprocess.run([str(BINARY), 'hook', 'post-tool-use-failure'], input=json.dumps(payload),
                                text=True, capture_output=True, env=env, cwd=self.home, timeout=20)
        self.assertEqual(result.returncode, 0, result.stderr)
        json.loads(result.stdout)
        self.assertNotIn('445566', result.stdout + result.stderr)
        self.assertFalse((self.home / '.contextstream/hook-failure-counts.json').exists())
        self.assertFalse(self.requests, 'account failure hook must not write a remote event')

    def test_disabled_inline_never_starts_signup(self):
        self.inline_available = False
        self.launch()
        self.account('signup_start', email='fixture@example.test')
        self.assertFalse(any(p.endswith('/mcp-signup/start') for p, _ in self.requests))


if __name__ == '__main__':
    unittest.main()
