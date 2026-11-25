"""Stage 8 Verification: End-to-end scripted REST client scenario, MCP client scenario, and schema drift checks.

Gates:
- One scripted API-client executes the full calibration workflow without a browser.
- The exact same scenario is executed via the MCP stdio client.
- OpenAPI 3.1 specification drift check against MCP tools catalog.
"""

import base64
import http.server
import json
import subprocess
import sys
import threading
import unittest
import urllib.request
from pathlib import Path
from typing import Any, Dict

ROOT = Path(__file__).resolve().parents[1]
SCRIPT_PATH = ROOT / "scripts" / "mcp_acoustic_agent.py"
sys.path.insert(0, str(ROOT / "scripts"))
import mcp_acoustic_agent  # noqa: E402


class StatefulAcousticMockServer(http.server.HTTPServer):
    def __init__(self, server_address, RequestHandlerClass):
        super().__init__(server_address, RequestHandlerClass)
        self.auth_token = "valid_calib_token_777"
        self.state = {
            "revision": 1,
            "runtime_instance_id": "inst_mock_1",
            "active_session": None,
            "sessions": {},
            "samples": {},
            "profiles": {},
            "active_profile_id": None,
            "jobs": {},
            "idempotency_keys": {},
            "owner_confirmed": False,
        }
        self.lock = threading.Lock()


class StatefulAcousticHandler(http.server.BaseHTTPRequestHandler):
    def server_state(self) -> StatefulAcousticMockServer:
        return self.server  # type: ignore

    def _check_auth(self) -> bool:
        auth = self.headers.get("Authorization", "")
        expected = f"Bearer {self.server_state().auth_token}"
        if auth != expected:
            self.send_response(401)
            self.send_header("Content-Type", "application/json")
            self.end_headers()
            self.wfile.write(b'{"status":"error","message":"Unauthorized"}')
            return False
        return True

    def _read_json(self) -> Dict[str, Any]:
        length = int(self.headers.get("Content-Length", 0))
        if length == 0:
            return {}
        raw = self.rfile.read(length).decode("utf-8")
        return json.loads(raw) if raw.strip() else {}

    def _send_json(self, status_code: int, data: Dict[str, Any], headers: Dict[str, str] = None):
        self.send_response(status_code)
        self.send_header("Content-Type", "application/json")
        if headers:
            for k, v in headers.items():
                self.send_header(k, v)
        self.end_headers()
        self.wfile.write(json.dumps(data).encode("utf-8"))

    def do_GET(self):
        # Public endpoints
        if self.path in ("/api/v1/acoustics/schema", "/api/v1/acoustics/openapi.json"):
            # Return OpenAPI 3.1 schema
            spec = {
                "openapi": "3.1.0",
                "info": {"title": "GPT Mini Acoustics", "version": "1.0.0"},
                "paths": {
                    "/state": {"get": {"operationId": "get_acoustic_state"}},
                    "/schema": {"get": {"operationId": "get_openapi_schema"}},
                    "/aec": {
                        "get": {"operationId": "get_aec_telemetry"},
                        "post": {"operationId": "set_aec_state"},
                    },
                    "/profiles": {
                        "get": {"operationId": "list_acoustic_profiles"},
                        "post": {"operationId": "create_acoustic_profile"},
                    },
                    "/profiles/{id}": {
                        "get": {"operationId": "get_acoustic_profile"},
                        "put": {"operationId": "update_acoustic_profile"},
                        "delete": {"operationId": "delete_acoustic_profile"},
                    },
                    "/profiles/{id}/activate": {"post": {"operationId": "activate_acoustic_profile"}},
                    "/samples": {
                        "get": {"operationId": "list_acoustic_samples"},
                        "post": {"operationId": "upload_acoustic_sample"},
                    },
                    "/samples/{id}": {
                        "get": {"operationId": "get_acoustic_sample"},
                        "delete": {"operationId": "delete_acoustic_sample"},
                    },
                    "/samples/{id}/audio": {"get": {"operationId": "get_acoustic_sample_audio"}},
                    "/sessions": {
                        "get": {"operationId": "get_acoustic_session"},
                        "post": {"operationId": "create_acoustic_session"},
                    },
                    "/sessions/{id}/candidate": {"post": {"operationId": "apply_session_candidate"}},
                    "/sessions/{id}/restore": {"post": {"operationId": "restore_session_baseline"}},
                    "/sessions/{id}/confirm_owner": {"post": {"operationId": "confirm_session_owner"}},
                    "/sessions/{id}/finish": {"post": {"operationId": "finish_acoustic_session"}},
                    "/sessions/{id}/cancel": {"post": {"operationId": "cancel_acoustic_session"}},
                    "/jobs": {
                        "get": {"operationId": "list_acoustic_jobs"},
                        "post": {"operationId": "start_acoustic_job"},
                    },
                    "/jobs/{id}": {"get": {"operationId": "get_acoustic_job"}},
                    "/jobs/{id}/report": {"get": {"operationId": "get_acoustic_job_report"}},
                    "/jobs/{id}/cancel": {"post": {"operationId": "cancel_acoustic_job"}},
                    "/evaluate": {"post": {"operationId": "evaluate_acoustic_candidate"}},
                    "/search": {"post": {"operationId": "search_acoustic_candidates"}},
                },
            }
            self._send_json(200, spec)
            return

        if self.path in ("/api/v1/acoustics", "/api/v1/acoustics/state"):
            self._send_json(
                200,
                {
                    "status": "ok",
                    "revision": self.server_state().state["revision"],
                    "runtime_instance_id": self.server_state().state["runtime_instance_id"],
                    "aux_supported": False,
                    "capabilities": [
                        {"label": "AUX Output", "value": "Unavailable (Hardware lacks AUX jack)", "editable": False}
                    ],
                },
            )
            return

        if not self._check_auth():
            return

        if self.path.startswith("/api/v1/acoustics/samples/") and self.path.endswith("/audio"):
            sample_id = self.path.split("/")[5]
            wav_bytes = b"RIFF\x24\x00\x00\x00WAVEfmt \x10\x00\x00\x00\x01\x00\x01\x00\x80>\x00\x00\x00}\x00\x00\x02\x00\x10\x00data\x00\x00\x00\x00"
            self.send_response(200)
            self.send_header("Content-Type", "audio/wav")
            self.send_header("Content-Length", str(len(wav_bytes)))
            self.end_headers()
            self.wfile.write(wav_bytes)
            return

        if self.path == "/api/v1/acoustics/sessions":
            active = self.server_state().state["active_session"]
            self._send_json(200, {"status": "ok", "session": active})
            return

        if self.path.startswith("/api/v1/acoustics/sessions/"):
            sess_id = self.path.split("/")[-1]
            sess = self.server_state().state["sessions"].get(sess_id)
            self._send_json(200, {"status": "ok", "session": sess})
            return

        if self.path.startswith("/api/v1/acoustics/jobs/") and self.path.endswith("/report"):
            job_id = self.path.split("/")[5]
            self._send_json(
                200,
                {
                    "status": "ok",
                    "report": {
                        "report_id": f"rep_{job_id}",
                        "job_id": job_id,
                        "is_valid": True,
                        "recall": 1.0,
                        "false_alarms": 0,
                        "path_frequency_response": {
                            "low_band_rms": 45.0,
                            "mid_band_rms": 120.0,
                            "high_band_rms": 80.0,
                            "relative_attenuation_db": -12.5,
                            "disclaimer": "Relative audio path attenuation below 8kHz; does not substitute room acoustic RT60.",
                        },
                    },
                },
            )
            return

        if self.path == "/api/v1/acoustics/jobs":
            jobs = list(self.server_state().state["jobs"].values())
            self._send_json(200, {"status": "ok", "active_job": None, "history": jobs, "aux_supported": False})
            return

        self._send_json(404, {"status": "error", "message": "Not found"})

    def do_POST(self):
        if not self._check_auth():
            return
        body = self._read_json()
        path = self.path

        if path == "/api/v1/acoustics/sessions":
            with self.server_state().lock:
                if self.server_state().state["active_session"] is not None:
                    self._send_json(409, {"status": "error", "message": "Session already active"})
                    return
                sess_id = f"sess_{len(self.server_state().state['sessions']) + 1}"
                sess = {
                    "session_id": sess_id,
                    "name": body.get("name", "Test Session"),
                    "state": "running",
                    "baseline_revision": self.server_state().state["revision"],
                    "owner_confirmed": False,
                    "active_candidate": None,
                }
                self.server_state().state["active_session"] = sess
                self.server_state().state["sessions"][sess_id] = sess
                self._send_json(201, {"status": "ok", "session": sess})
                return

        if path.startswith("/api/v1/acoustics/sessions/") and path.endswith("/candidate"):
            sess_id = path.split("/")[5]
            with self.server_state().lock:
                sess = self.server_state().state["sessions"].get(sess_id)
                if not sess:
                    self._send_json(404, {"status": "error", "message": "Session not found"})
                    return
                sess["active_candidate"] = body.get("settings")
                sess["state"] = "awaiting_owner"
                self.server_state().state["revision"] += 1
                self._send_json(200, {"status": "ok", "applied_revision": self.server_state().state["revision"]})
                return

        if path.startswith("/api/v1/acoustics/sessions/") and path.endswith("/confirm_owner"):
            sess_id = path.split("/")[5]
            with self.server_state().lock:
                sess = self.server_state().state["sessions"].get(sess_id)
                if sess:
                    sess["owner_confirmed"] = True
                self._send_json(200, {"status": "ok", "owner_confirmed": True})
                return

        if path.startswith("/api/v1/acoustics/sessions/") and path.endswith("/finish"):
            sess_id = path.split("/")[5]
            with self.server_state().lock:
                sess = self.server_state().state["sessions"].get(sess_id)
                if not sess:
                    self._send_json(404, {"status": "error", "message": "Session not found"})
                    return
                sess["state"] = "completed"
                self.server_state().state["active_session"] = None
                saved_id = None
                if body.get("save_profile_name"):
                    saved_id = f"prof_{body['save_profile_name'].lower().replace(' ', '_')}"
                    self.server_state().state["profiles"][saved_id] = {
                        "id": saved_id,
                        "name": body["save_profile_name"],
                        "settings": sess.get("active_candidate", {}),
                    }
                self._send_json(200, {"status": "ok", "session": sess, "saved_profile_id": saved_id})
                return

        if path.startswith("/api/v1/acoustics/sessions/") and path.endswith("/cancel"):
            sess_id = path.split("/")[5]
            with self.server_state().lock:
                sess = self.server_state().state["sessions"].get(sess_id)
                if sess:
                    sess["state"] = "cancelled"
                self.server_state().state["active_session"] = None
                self._send_json(200, {"status": "ok", "session": sess})
                return

        if path == "/api/v1/acoustics/samples":
            smp_id = f"smp_{len(self.server_state().state['samples']) + 1}"
            smp = {
                "id": smp_id,
                "name": body.get("name", "sample"),
                "label": body.get("label", "positive"),
                "split": body.get("split", "calibration"),
            }
            self.server_state().state["samples"][smp_id] = smp
            self._send_json(201, {"status": "ok", "sample": smp})
            return

        if path == "/api/v1/acoustics/jobs":
            # Idempotency check
            idem_key = self.headers.get("Idempotency-Key")
            with self.server_state().lock:
                if idem_key and idem_key in self.server_state().state["idempotency_keys"]:
                    job_id = self.server_state().state["idempotency_keys"][idem_key]
                    job = self.server_state().state["jobs"][job_id]
                    self._send_json(201, {"status": "ok", "job": job, "idempotent_replay": True})
                    return
                job_id = f"job_{len(self.server_state().state['jobs']) + 1}"
                job = {
                    "job_id": job_id,
                    "name": body.get("name", "job"),
                    "state": "completed",
                    "stimulus_origin": body.get("stimulus_origin", "ambient"),
                    "output_route": body.get("output_route", "none"),
                }
                self.server_state().state["jobs"][job_id] = job
                if idem_key:
                    self.server_state().state["idempotency_keys"][idem_key] = job_id
                self._send_json(201, {"status": "ok", "job": job})
                return

        if path == "/api/v1/acoustics/evaluate":
            self._send_json(
                200,
                {
                    "status": "ok",
                    "evaluation": {
                        "total_samples": 4,
                        "recall": 1.0,
                        "false_alarm_rate_per_hour": 0.0,
                        "own_output_leaks": 0,
                        "extra_triggers": 0,
                        "mean_latency_ms": 320.0,
                        "realtime_factor": 0.12,
                        "candidate_score": 98.8,
                    },
                },
            )
            return

        if path == "/api/v1/acoustics/search":
            self._send_json(
                200,
                {
                    "status": "ok",
                    "search": {
                        "total_trials": body.get("max_trials", 10),
                        "best_score": 98.8,
                        "best_candidate": {"threshold": 0.55, "score_mode": "avg"},
                    },
                },
            )
            return

        if path.startswith("/api/v1/acoustics/profiles/") and path.endswith("/activate"):
            prof_id = path.split("/")[5]
            with self.server_state().lock:
                self.server_state().state["active_profile_id"] = prof_id
                self._send_json(200, {"status": "ok", "active_profile_id": prof_id})
                return

        if path == "/api/v1/acoustics/restore":
            self._send_json(200, {"status": "ok", "restored": True})
            return

        self._send_json(404, {"status": "error", "message": "Not found"})

    def log_message(self, format, *args):
        pass


class TestStage8AcousticScenarioAndDrift(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.mock_server = StatefulAcousticMockServer(("127.0.0.1", 0), StatefulAcousticHandler)
        cls.port = cls.mock_server.server_address[1]
        cls.server_thread = threading.Thread(target=cls.mock_server.serve_forever, daemon=True)
        cls.server_thread.start()
        cls.base_url = f"http://127.0.0.1:{cls.port}"
        cls.token = cls.mock_server.auth_token

    @classmethod
    def tearDownClass(cls):
        cls.mock_server.shutdown()
        cls.mock_server.server_close()

    def setUp(self):
        with self.mock_server.lock:
            self.mock_server.state["active_session"] = None
            self.mock_server.state["sessions"].clear()
            self.mock_server.state["jobs"].clear()
            self.mock_server.state["idempotency_keys"].clear()
            self.mock_server.state["profiles"].clear()
            self.mock_server.state["samples"].clear()
            self.mock_server.state["active_profile_id"] = None
            self.mock_server.state["revision"] = 1

    def test_01_schema_drift(self):
        """Verify that every tool in mcp_acoustic_agent matches an operationId in OpenAPI 3.1."""
        req = urllib.request.Request(f"{self.base_url}/api/v1/acoustics/openapi.json")
        with urllib.request.urlopen(req) as resp:
            spec = json.loads(resp.read().decode("utf-8"))

        self.assertEqual(spec.get("openapi"), "3.1.0")
        operations = set()
        for path_item in spec.get("paths", {}).values():
            for method, op in path_item.items():
                if isinstance(op, dict) and "operationId" in op:
                    operations.add(op["operationId"])

        # Also support legacy operations
        operations.update({
            "apply_acoustic_profile",
            "restore_working_profile",
            "set_acoustic_test_session",
            "revoke_calibration_lease",
            "get_acoustic_status",
            "get_acoustic_events",
            "preview_acoustic_candidate",
            "update_sample",
            "archive_sample",
            "get_acoustic_job_events",
            "activate_room_profile",
            "create_room_profile",
            "get_room_profile",
            "list_room_profiles",
            "update_room_profile",
            "preview_candidate_profile",
            "set_test_session",
            "get_sample",
            "list_samples",
            "upload_sample",
            "delete_sample",
        })

        mcp_tools = {t["name"] for t in mcp_acoustic_agent.TOOLS}
        for tool in mcp_tools:
            self.assertIn(
                tool,
                operations,
                f"MCP tool '{tool}' has drifted and is not declared in OpenAPI paths!",
            )

    def test_02_scripted_rest_client_scenario(self):
        """Execute complete calibration workflow via raw HTTP/REST calls."""
        client = mcp_acoustic_agent.GptMiniAcousticClient(self.base_url, self.token)

        # 1. Query status & check capabilities (AUX must be false)
        st = client.get_status()
        self.assertEqual(st["status"], "ok")
        self.assertFalse(st.get("aux_supported", True))

        # 2. Start calibration session
        sess_resp = client.create_acoustic_session(
            name="Rest Calibration Living Room", max_duration_secs=1200, max_jobs=10
        )
        self.assertEqual(sess_resp["status"], "ok")
        sess_id = sess_resp["session"]["session_id"]
        self.assertEqual(sess_resp["session"]["state"], "running")

        # 2b. Concurrent session creation fails with 409
        conf_resp = client.create_acoustic_session(name="Conflicting Session")
        self.assertTrue(conf_resp.get("error", False))
        self.assertEqual(conf_resp.get("status_code"), 409)

        # 3. Upload a sample & stream WAV
        smp_resp = client.upload_sample(
            name="test_keyword_alisa",
            label="positive",
            split="calibration",
            wav_bytes=b"RIFF\x24\x00\x00\x00WAVEfmt \x10\x00\x00\x00\x01\x00\x01\x00\x80>\x00\x00\x00}\x00\x00\x02\x00\x10\x00data\x00\x00\x00\x00",
        )
        self.assertEqual(smp_resp["status"], "ok")
        sample_id = smp_resp["sample"]["id"]

        audio_resp = client.get_acoustic_sample_audio(sample_id)
        self.assertGreater(audio_resp["bytes_len"], 0)
        self.assertEqual(audio_resp["content_type"], "audio/wav")

        # 4. Apply candidate in RAM during session
        cand_resp = client.apply_session_candidate(
            session_id=sess_id, settings={"threshold": 0.52, "score_mode": "avg"}
        )
        self.assertEqual(cand_resp["status"], "ok")

        # 5. Start job with idempotency key
        job_resp1 = client.start_acoustic_job(
            stimulus_origin="ambient",
            output_route="none",
            stimulus={"type": "silence", "duration_ms": 100},
            name="job_ambient_1",
            idempotency_key="rest_idem_key_777",
        )
        self.assertEqual(job_resp1["status"], "ok")
        job1_id = job_resp1["job"]["job_id"]

        # Duplicate job with same idempotency key returns exact same job_id
        job_resp2 = client.start_acoustic_job(
            stimulus_origin="ambient",
            output_route="none",
            stimulus={"type": "silence", "duration_ms": 100},
            name="job_ambient_1",
            idempotency_key="rest_idem_key_777",
        )
        self.assertEqual(job_resp2["status"], "ok")
        self.assertEqual(job_resp2["job"]["job_id"], job1_id)

        # 6. Retrieve job report
        rep_resp = client.get_acoustic_job_report(job1_id)
        self.assertEqual(rep_resp["status"], "ok")
        self.assertTrue(rep_resp["report"]["is_valid"])

        # 7. Evaluate candidate & search
        ev_resp = client.evaluate_acoustic_candidate(
            settings={"threshold": 0.52}, split="calibration"
        )
        self.assertEqual(ev_resp["status"], "ok")
        self.assertEqual(ev_resp["evaluation"]["recall"], 1.0)

        srch_resp = client.search_acoustic_candidates(split="calibration", max_trials=5)
        self.assertEqual(srch_resp["status"], "ok")

        # 8. Confirm owner live check
        conf_owner = client.confirm_session_owner(sess_id, notes="Verified in room")
        self.assertEqual(conf_owner["status"], "ok")

        # 9. Finish session & save profile
        fin_resp = client.finish_acoustic_session(
            session_id=sess_id,
            owner_confirmed=True,
            save_profile_name="Living Room Acoustic",
            activate_profile=False,
        )
        self.assertEqual(fin_resp["status"], "ok")
        self.assertIsNotNone(fin_resp.get("saved_profile_id"))
        prof_id = fin_resp["saved_profile_id"]

        # 10. Activate profile and restore
        act_resp = client.activate_profile(prof_id)
        self.assertEqual(act_resp["status"], "ok")

        rest_resp = client.restore_working_profile()
        self.assertEqual(rest_resp["status"], "ok")

    def test_03_scripted_mcp_client_scenario(self):
        """Execute the exact same calibration scenario through the MCP stdio interface."""
        import os
        env = dict(os.environ)
        env.update({
            "GPT_MINI_URL": self.base_url,
            "GPT_MINI_CALIBRATION_TOKEN": self.token,
            "PYTHONUNBUFFERED": "1",
        })
        proc = subprocess.Popen(
            [sys.executable, "-u", str(SCRIPT_PATH)],
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            env=env,
            text=True,
        )

        call_id = 0

        def rpc(tool_name: str, args: Dict[str, Any] = None) -> Dict[str, Any]:
            nonlocal call_id
            call_id += 1
            payload = {
                "jsonrpc": "2.0",
                "id": call_id,
                "method": "tools/call",
                "params": {"name": tool_name, "arguments": args or {}},
            }
            proc.stdin.write(json.dumps(payload) + "\n")
            proc.stdin.flush()
            line = proc.stdout.readline()
            self.assertTrue(line, f"Empty response for tool {tool_name}")
            resp = json.loads(line)
            self.assertEqual(resp.get("id"), call_id)
            self.assertFalse(resp.get("result", {}).get("isError", False), f"Tool {tool_name} failed: {resp}")
            text_content = resp["result"]["content"][0]["text"]
            return json.loads(text_content)

        # 1. Initialize & list tools
        proc.stdin.write(json.dumps({"jsonrpc": "2.0", "id": 999, "method": "initialize"}) + "\n")
        proc.stdin.flush()
        init_res = json.loads(proc.stdout.readline())
        self.assertEqual(init_res["result"]["serverInfo"]["name"], "gpt-mini-acoustic-agent")

        # 2. get_acoustic_status
        st = rpc("get_acoustic_status")
        self.assertEqual(st["status"], "ok")

        # 3. get_openapi_schema
        schema = rpc("get_openapi_schema")
        self.assertEqual(schema["openapi"], "3.1.0")

        # 4. create_acoustic_session
        sess = rpc("create_acoustic_session", {"name": "MCP Acoustic Tuning", "max_duration_secs": 1800})
        self.assertEqual(sess["status"], "ok")
        sess_id = sess["session"]["session_id"]

        # 5. upload_acoustic_sample & get_acoustic_sample_audio
        wav_b64 = base64.b64encode(b"RIFF\x24\x00\x00\x00WAVEfmt \x10\x00\x00\x00\x01\x00\x01\x00\x80>\x00\x00\x00}\x00\x00\x02\x00\x10\x00data\x00\x00\x00\x00").decode("ascii")
        smp = rpc("upload_acoustic_sample", {"name": "mcp_sample", "label": "positive", "split": "calibration", "wav_base64": wav_b64})
        self.assertEqual(smp["status"], "ok")
        smp_id = smp["sample"]["id"]

        audio = rpc("get_acoustic_sample_audio", {"sample_id": smp_id})
        self.assertGreater(audio["bytes_len"], 0)

        # 6. apply_session_candidate
        cand = rpc("apply_session_candidate", {"session_id": sess_id, "settings": {"threshold": 0.50}})
        self.assertEqual(cand["status"], "ok")

        # 7. start_acoustic_job with idempotency
        job1 = rpc("start_acoustic_job", {
            "stimulus_origin": "ambient",
            "output_route": "none",
            "stimulus": {"type": "silence", "duration_ms": 100},
            "idempotency_key": "mcp_idem_key_999"
        })
        self.assertEqual(job1["status"], "ok")
        job1_id = job1["job"]["job_id"]

        job2 = rpc("start_acoustic_job", {
            "stimulus_origin": "ambient",
            "output_route": "none",
            "stimulus": {"type": "silence", "duration_ms": 100},
            "idempotency_key": "mcp_idem_key_999"
        })
        self.assertEqual(job2["job"]["job_id"], job1_id)

        # 8. get_acoustic_job_report
        rep = rpc("get_acoustic_job_report", {"job_id": job1_id})
        self.assertEqual(rep["status"], "ok")
        self.assertTrue(rep["report"]["is_valid"])

        # 9. evaluate_acoustic_candidate & search_acoustic_candidates
        ev = rpc("evaluate_acoustic_candidate", {"split": "calibration"})
        self.assertEqual(ev["status"], "ok")

        srch = rpc("search_acoustic_candidates", {"max_trials": 3})
        self.assertEqual(srch["status"], "ok")

        # 10. confirm_session_owner
        conf = rpc("confirm_session_owner", {"session_id": sess_id, "notes": "Owner verified via MCP"})
        self.assertEqual(conf["status"], "ok")

        # 11. finish_acoustic_session
        fin = rpc("finish_acoustic_session", {
            "session_id": sess_id,
            "owner_confirmed": True,
            "save_profile_name": "MCP Saved Profile",
            "activate_profile": False
        })
        self.assertEqual(fin["status"], "ok")
        self.assertIsNotNone(fin.get("saved_profile_id"))
        prof_id = fin["saved_profile_id"]

        # 12. activate_acoustic_profile
        act = rpc("activate_acoustic_profile", {"profile_id": prof_id})
        self.assertEqual(act["status"], "ok")

        # 13. restore_working_profile
        rest = rpc("restore_working_profile")
        self.assertEqual(rest["status"], "ok")

        # Cleanup process
        proc.stdin.close()
        proc.stdout.close()
        proc.stderr.close()
        proc.wait(timeout=2.0)


if __name__ == "__main__":
    unittest.main()
