"""Tests for the MCP stdio acoustic calibration adapter."""

import http.server
import json
import subprocess
import sys
import threading
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
SCRIPT_PATH = ROOT / "scripts" / "mcp_acoustic_agent.py"


class MockMiniHttpServer(http.server.HTTPServer):
    def __init__(self, server_address, RequestHandlerClass):
        super().__init__(server_address, RequestHandlerClass)
        self.received_auth = None
        self.last_method = None
        self.last_path = None
        self.last_body = None


class MockHandler(http.server.BaseHTTPRequestHandler):
    def do_GET(self):
        self.server.received_auth = self.headers.get("Authorization")
        self.server.last_method = "GET"
        self.server.last_path = self.path
        if self.path == "/api/v1/acoustics":
            self.send_response(200)
            self.send_header("Content-Type", "application/json")
            self.end_headers()
            self.wfile.write(b'{"status":"ok","revision":3}')
        elif self.path.startswith("/api/v1/acoustics/events"):
            self.send_response(200)
            self.send_header("Content-Type", "application/json")
            self.end_headers()
            self.wfile.write(b'{"status":"ok","events":[]}')
        else:
            self.send_response(404)
            self.end_headers()

    def do_POST(self):
        self.server.received_auth = self.headers.get("Authorization")
        self.server.last_method = "POST"
        self.server.last_path = self.path
        length = int(self.headers.get("Content-Length", 0))
        self.server.last_body = self.rfile.read(length).decode("utf-8")
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.end_headers()
        self.wfile.write(b'{"status":"ok"}')

    def log_message(self, format, *args):
        pass


class TestMcpAcousticAgent(unittest.TestCase):
    def setUp(self):
        self.mock_server = MockMiniHttpServer(("127.0.0.1", 0), MockHandler)
        self.port = self.mock_server.server_address[1]
        self.server_thread = threading.Thread(
            target=self.mock_server.serve_forever, daemon=True
        )
        self.server_thread.start()

    def tearDown(self):
        self.mock_server.shutdown()
        self.mock_server.server_close()

    def test_mcp_lifecycle_and_tools(self):
        env = {
            "GPT_MINI_URL": f"http://127.0.0.1:{self.port}",
            "GPT_MINI_CALIBRATION_TOKEN": "calib_test_secret_123",
            "PATH": sys.path[0] or "",
        }
        proc = subprocess.Popen(
            [sys.executable, str(SCRIPT_PATH)],
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            env=env,
            text=True,
        )

        # 1. Initialize
        proc.stdin.write(
            json.dumps({"jsonrpc": "2.0", "id": 1, "method": "initialize"}) + "\n"
        )
        proc.stdin.flush()
        resp1 = json.loads(proc.stdout.readline())
        self.assertEqual(resp1["id"], 1)
        self.assertEqual(
            resp1["result"]["serverInfo"]["name"], "gpt-mini-acoustic-agent"
        )

        # 2. Tools list
        proc.stdin.write(
            json.dumps({"jsonrpc": "2.0", "id": 2, "method": "tools/list"}) + "\n"
        )
        proc.stdin.flush()
        resp2 = json.loads(proc.stdout.readline())
        tool_names = [t["name"] for t in resp2["result"]["tools"]]
        self.assertIn("get_acoustic_status", tool_names)
        self.assertIn("apply_acoustic_profile", tool_names)
        self.assertIn("restore_working_profile", tool_names)
        self.assertIn("get_acoustic_events", tool_names)
        self.assertIn("set_test_session", tool_names)
        self.assertIn("revoke_calibration_lease", tool_names)
        self.assertIn("list_room_profiles", tool_names)
        self.assertIn("create_room_profile", tool_names)
        self.assertIn("activate_room_profile", tool_names)
        self.assertIn("preview_candidate_profile", tool_names)
        self.assertIn("list_samples", tool_names)
        self.assertIn("get_sample", tool_names)
        self.assertIn("upload_sample", tool_names)
        self.assertIn("update_sample", tool_names)
        self.assertIn("archive_sample", tool_names)
        self.assertIn("delete_sample", tool_names)
        self.assertIn("list_acoustic_jobs", tool_names)
        self.assertIn("start_acoustic_job", tool_names)
        self.assertIn("get_acoustic_job", tool_names)
        self.assertIn("cancel_acoustic_job", tool_names)
        self.assertIn("get_acoustic_job_events", tool_names)

        # 3. Call get_acoustic_status
        proc.stdin.write(
            json.dumps(
                {
                    "jsonrpc": "2.0",
                    "id": 3,
                    "method": "tools/call",
                    "params": {"name": "get_acoustic_status"},
                }
            )
            + "\n"
        )
        proc.stdin.flush()
        resp3 = json.loads(proc.stdout.readline())
        self.assertEqual(resp3["id"], 3)
        self.assertFalse(resp3["result"]["isError"])
        payload = json.loads(resp3["result"]["content"][0]["text"])
        self.assertEqual(payload["status"], "ok")
        self.assertEqual(payload["revision"], 3)
        self.assertEqual(
            self.mock_server.received_auth, "Bearer calib_test_secret_123"
        )
        self.assertEqual(self.mock_server.last_path, "/api/v1/acoustics")

        # 4. Call set_test_session
        proc.stdin.write(
            json.dumps(
                {
                    "jsonrpc": "2.0",
                    "id": 4,
                    "method": "tools/call",
                    "params": {"name": "set_test_session", "arguments": {"active": True}},
                }
            )
            + "\n"
        )
        proc.stdin.flush()
        resp4 = json.loads(proc.stdout.readline())
        self.assertEqual(resp4["id"], 4)
        self.assertEqual(self.mock_server.last_path, "/api/v1/acoustics/test_session")
        self.assertIn('"active": true', self.mock_server.last_body)

        # 5. Call list_samples
        proc.stdin.write(
            json.dumps(
                {
                    "jsonrpc": "2.0",
                    "id": 5,
                    "method": "tools/call",
                    "params": {"name": "list_samples", "arguments": {"split": "train"}},
                }
            )
            + "\n"
        )
        proc.stdin.flush()
        resp5 = json.loads(proc.stdout.readline())
        self.assertEqual(resp5["id"], 5)
        self.assertEqual(self.mock_server.last_path, "/api/v1/acoustics/samples?split=train")

        # 6. Call start_acoustic_job
        proc.stdin.write(
            json.dumps(
                {
                    "jsonrpc": "2.0",
                    "id": 6,
                    "method": "tools/call",
                    "params": {
                        "name": "start_acoustic_job",
                        "arguments": {
                            "stimulus_origin": "own_output",
                            "output_route": "built_in",
                            "stimulus": {
                                "type": "chirp_sweep",
                                "start_hz": 200.0,
                                "end_hz": 2000.0,
                                "duration_ms": 500,
                                "volume_dbfs": -12.0,
                            },
                            "pre_roll_ms": 200,
                            "post_roll_ms": 300,
                            "volume_pct": 50,
                            "name": "mcp_test_sweep",
                        },
                    },
                }
            )
            + "\n"
        )
        proc.stdin.flush()
        resp6 = json.loads(proc.stdout.readline())
        self.assertEqual(resp6["id"], 6)
        self.assertEqual(self.mock_server.last_path, "/api/v1/acoustics/jobs")
        self.assertIn('"stimulus_origin": "own_output"', self.mock_server.last_body)
        self.assertIn('"mcp_test_sweep"', self.mock_server.last_body)

        # 7. Call list_acoustic_jobs
        proc.stdin.write(
            json.dumps(
                {
                    "jsonrpc": "2.0",
                    "id": 7,
                    "method": "tools/call",
                    "params": {"name": "list_acoustic_jobs"},
                }
            )
            + "\n"
        )
        proc.stdin.flush()
        resp7 = json.loads(proc.stdout.readline())
        self.assertEqual(resp7["id"], 7)
        self.assertEqual(self.mock_server.last_path, "/api/v1/acoustics/jobs")

        # Clean shutdown
        proc.stdin.close()
        proc.stdout.close()
        proc.stderr.close()
        proc.wait(timeout=2.0)


if __name__ == "__main__":
    unittest.main()
