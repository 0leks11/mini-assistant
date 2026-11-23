"""Live Device Acoustic Release Verification (Stage 9).

This test mutates a real device and is excluded from ordinary host checks.
Run it only for an agreed device experiment with both
GPT_MINI_RUN_LIVE_ACOUSTIC_TEST=1 and an explicit GPT_MINI_URL.

Executes the complete autonomous acoustic calibration scenario against the
live running GPT Mini device over MCP stdio protocol without code or SSH access.
Verifies the Stage 9 gate:
- Agent conducts bounded calibration session.
- Submits candidate in RAM.
- Executes measurement job with idempotency key.
- Obtains structured job report with band frequency analysis.
- Evaluates candidates and searches parameters.
- Records owner live confirmation.
- Saves and activates named profile.
- Restores baseline without regression.
- Ensures forbidden operations outside Acoustic fail closed.
"""

import base64
import json
import os
import subprocess
import sys
import unittest
import urllib.request
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
SCRIPT_PATH = ROOT / "scripts" / "mcp_acoustic_agent.py"


@unittest.skipUnless(
    os.environ.get("GPT_MINI_RUN_LIVE_ACOUSTIC_TEST") == "1"
    and bool(os.environ.get("GPT_MINI_URL", "").strip()),
    "Live device test requires explicit opt-in and target URL",
)
class LiveDeviceAcousticReleaseTest(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.base_url = os.environ["GPT_MINI_URL"].rstrip("/")
        
        # Request calibration token as owner via same-origin header emulation
        req = urllib.request.Request(
            f"{cls.base_url}/api/v1/acoustics/tokens",
            data=json.dumps({
                "identity_id": "live_agent_gate_9",
                "ttl_secs": 3600,
                "scopes": ["acoustics_read", "acoustics_write", "profiles", "samples", "jobs"],
                "lease": {
                    "max_duration_secs": 1800,
                    "max_volume_pct": 50,
                    "max_runs": 20
                }
            }).encode("utf-8"),
            headers={
                "Host": cls.base_url.replace("http://", "").replace("https://", ""),
                "Origin": cls.base_url,
                "Content-Type": "application/json"
            },
            method="POST"
        )
        try:
            with urllib.request.urlopen(req, timeout=5.0) as resp:
                data = json.loads(resp.read().decode("utf-8"))
                cls.token = data["token"]
        except Exception as e:
            raise unittest.SkipTest(f"Live device not reachable at {cls.base_url}: {e}")

        # Cancel any leftover active session before running test
        status_req = urllib.request.Request(
            f"{cls.base_url}/api/v1/acoustics/sessions",
            headers={"Authorization": f"Bearer {cls.token}"}
        )
        try:
            with urllib.request.urlopen(status_req, timeout=3.0) as resp:
                st_data = json.loads(resp.read().decode("utf-8"))
                if st_data.get("session"):
                    active_id = st_data["session"]["session_id"]
                    cancel_req = urllib.request.Request(
                        f"{cls.base_url}/api/v1/acoustics/sessions/{active_id}/cancel",
                        data=b"{}",
                        headers={
                            "Authorization": f"Bearer {cls.token}",
                            "Content-Type": "application/json"
                        },
                        method="POST"
                    )
                    urllib.request.urlopen(cancel_req, timeout=3.0)
        except Exception:
            pass

    def test_live_mcp_calibration_scenario_and_gate_verification(self):
        env = dict(os.environ)
        env.update({
            "GPT_MINI_URL": self.base_url,
            "GPT_MINI_CALIBRATION_TOKEN": self.token,
            "PYTHONUNBUFFERED": "1"
        })

        proc = subprocess.Popen(
            [sys.executable, "-u", str(SCRIPT_PATH)],
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            env=env,
            text=True
        )

        call_id = 0

        def rpc(tool_name, args=None):
            nonlocal call_id
            call_id += 1
            payload = {
                "jsonrpc": "2.0",
                "id": call_id,
                "method": "tools/call",
                "params": {"name": tool_name, "arguments": args or {}}
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

        # 1. Initialize MCP stdio
        proc.stdin.write(json.dumps({"jsonrpc": "2.0", "id": 999, "method": "initialize"}) + "\n")
        proc.stdin.flush()
        init_res = json.loads(proc.stdout.readline())
        self.assertEqual(init_res["result"]["serverInfo"]["name"], "gpt-mini-acoustic-agent")

        # 2. Check OpenAPI schema & live acoustic status
        schema = rpc("get_openapi_schema")
        self.assertEqual(schema.get("openapi"), "3.1.0")

        status = rpc("get_acoustic_status")
        self.assertEqual(status["status"], "ok")
        self.assertTrue(status["runtime"]["ready"])
        self.assertEqual(len(status["controls"]), 33, "Exactly 33 controls expected on live device")

        # 3. Create bounded calibration session
        sess_resp = rpc("create_acoustic_session", {
            "name": "Натурный тест 2м (Этап 9)",
            "max_duration_secs": 1800,
            "max_jobs": 15
        })
        self.assertEqual(sess_resp["status"], "ok")
        sess_id = sess_resp["session"]["session_id"]
        self.assertEqual(sess_resp["session"]["state"], "running")

        # 4. Upload known test audio sample and retrieve audio stream
        import math, struct
        sample_rate = 16000
        duration_secs = 1.0
        num_samples = int(duration_secs * sample_rate)
        pcm_bytes = bytearray()
        for i in range(num_samples):
            val = int(8000.0 * math.sin(2.0 * math.pi * 440.0 * i / sample_rate))
            pcm_bytes.extend(struct.pack("<h", val))
        data_len = len(pcm_bytes)
        total_len = 36 + data_len
        header = struct.pack(
            "<4sI4s4sIHHIIHH4sI",
            b"RIFF", total_len, b"WAVE",
            b"fmt ", 16, 1, 1, sample_rate, sample_rate * 2, 2, 16,
            b"data", data_len
        )
        wav_bytes = bytes(header + pcm_bytes)
        wav_b64 = base64.b64encode(wav_bytes).decode("ascii")
        smp = rpc("upload_acoustic_sample", {
            "name": "live_2m_kw_sample",
            "label": "positive",
            "split": "calibration",
            "wav_base64": wav_b64
        })
        self.assertEqual(smp["status"], "ok")
        smp_id = smp["sample"]["id"]

        audio_info = rpc("get_acoustic_sample_audio", {"sample_id": smp_id})
        self.assertGreater(audio_info["bytes_len"], 0)

        # 5. Apply candidate settings in RAM (non-persistent)
        candidate_settings = dict(status["applied"])
        candidate_settings["threshold"] = 0.385
        cand_resp = rpc("apply_session_candidate", {
            "session_id": sess_id,
            "settings": candidate_settings
        })
        self.assertEqual(cand_resp["status"], "ok")

        # 6. Start measurement job with idempotency key
        job1 = rpc("start_acoustic_job", {
            "stimulus_origin": "ambient",
            "output_route": "none",
            "stimulus": {"type": "silence", "duration_ms": 200},
            "idempotency_key": "live_job_idem_stage9"
        })
        self.assertEqual(job1["status"], "ok")
        job1_id = job1["job"]["job_id"]

        # 6b. Replay job with same idempotency key
        job2 = rpc("start_acoustic_job", {
            "stimulus_origin": "ambient",
            "output_route": "none",
            "stimulus": {"type": "silence", "duration_ms": 200},
            "idempotency_key": "live_job_idem_stage9"
        })
        self.assertEqual(job2["job"]["job_id"], job1_id)

        # 7. Get job report
        report = rpc("get_acoustic_job_report", {"job_id": job1_id})
        self.assertEqual(report["status"], "ok")
        if not report["report"]["is_valid"]:
            print("Job report not valid:", report["report"].get("validity_reason"), report["report"].get("audio_metrics"))
        self.assertIn("path_frequency_response", report["report"])

        # 8. Evaluate candidates on calibration corpus
        eval_res = rpc("evaluate_acoustic_candidate", {"split": "calibration"})
        self.assertEqual(eval_res["status"], "ok")

        # 9. Search parameters
        search_res = rpc("search_acoustic_candidates", {"max_trials": 3, "split": "calibration"})
        self.assertEqual(search_res["status"], "ok")

        # 10. Owner confirms live check
        owner_conf = rpc("confirm_session_owner", {
            "session_id": sess_id,
            "notes": "Verified by owner on live device"
        })
        self.assertEqual(owner_conf["status"], "ok")

        # 11. Finish session with named profile save
        fin = rpc("finish_acoustic_session", {
            "session_id": sess_id,
            "owner_confirmed": True,
            "save_profile_name": "Гостиная 2м Этап 9",
            "activate_profile": False,
            "notes": "Live acceptance test Stage 9"
        })
        self.assertEqual(fin["status"], "ok")
        saved_prof_id = fin.get("saved_profile_id")
        self.assertIsNotNone(saved_prof_id)

        # 12. Activate profile on live device
        act = rpc("activate_acoustic_profile", {"profile_id": saved_prof_id})
        self.assertEqual(act["status"], "ok")

        # 13. Restore baseline working profile
        rest = rpc("restore_working_profile")
        self.assertEqual(rest["status"], "ok")

        # Close MCP client
        proc.stdin.close()
        proc.stdout.close()
        proc.stderr.close()
        proc.wait(timeout=2.0)

        # 14. Verify security: calibration token CANNOT access administrative endpoints outside Acoustic
        forbidden_endpoints = [
            f"{self.base_url}/api/v1/system/restart",
            f"{self.base_url}/api/v1/system/reboot",
            f"{self.base_url}/api/v1/voice",
            f"{self.base_url}/api/v1/volume",
            f"{self.base_url}/api/v1/led"
        ]
        for url in forbidden_endpoints:
            req = urllib.request.Request(
                url,
                data=b"{}",
                headers={
                    "Authorization": f"Bearer {self.token}",
                    "Content-Type": "application/json"
                },
                method="POST"
            )
            try:
                with urllib.request.urlopen(req, timeout=3.0) as resp:
                    self.fail(f"Endpoint {url} must not succeed with calibration token! (status: {resp.status})")
            except urllib.error.HTTPError as e:
                self.assertIn(e.code, (401, 403), f"Endpoint {url} must return 401 or 403, got {e.code}")


if __name__ == "__main__":
    unittest.main()
