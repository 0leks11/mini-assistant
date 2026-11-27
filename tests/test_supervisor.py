"""Exercise run-supervisor.sh boot sequencing and Soloist network/DNS/TLS gate with fake tools."""

import json
import os
from pathlib import Path
import shutil
import signal
import subprocess
import tempfile
import time
import unittest

SUPERVISOR = Path(__file__).resolve().parents[1] / "scripts" / "run-supervisor.sh"


class SupervisorBootReadinessTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory(prefix="mini_sup_test_")
        self.addCleanup(self.tmp.cleanup)
        self.root = Path(self.tmp.name)
        self.sup_dir = self.root / "mini-assistant"
        (self.sup_dir / "soloist-env").mkdir(parents=True)
        self.fakebin = self.root / "fakebin"
        self.fakebin.mkdir()
        self.events = self.root / "events.jsonl"
        self.state_file = self.root / "net_state.json"
        self.state_file.write_text(
            json.dumps(
                {
                    "ipv4_tick": 2,
                    "ntp_tick": 0,
                    "dns_tick": 4,
                    "tls_tick": 5,
                    "ticks": 0,
                    "soloist_running": False,
                    "logged_in": False,
                }
            )
        )
        self.ca_bundle = self.root / "ca-certificates.crt"
        self.ca_bundle.write_text("fixture ca")

        self.script = self.sup_dir / "run-supervisor.sh"
        shutil.copyfile(SUPERVISOR, self.script)
        self.script.chmod(0o755)

        daemon = self.sup_dir / "mini-assistant"
        daemon.write_text(
            "#!/bin/sh\n"
            'echo "{\\"event\\": \\"mini-assistant\\"}" >> "$SUP_TEST_EVENTS"\n'
            'exec python3 -c "import time; time.sleep(10)"\n'
        )
        daemon.chmod(0o755)

        bcm = self.sup_dir / "bcm-attach"
        bcm.write_text('#!/bin/sh\nexec python3 -c "import time; time.sleep(10)"\n')
        bcm.chmod(0o755)

        self.soloist_up = self.root / "soloist.up"
        soloist_run = self.sup_dir / "soloist-env" / "soloist-run.sh"
        soloist_run.write_text(
            "#!/bin/sh\n"
            ': > "$SUP_TEST_SOLOIST_UP"\n'
            'exec python3 -c \''
            "import fcntl, json, os, time\n"
            'sf = os.environ["SUP_TEST_STATE"]\n'
            'ev = os.environ["SUP_TEST_EVENTS"]\n'
            "with open(sf, \"r+\") as fh:\n"
            "    fcntl.flock(fh, fcntl.LOCK_EX)\n"
            "    d = json.loads(fh.read())\n"
            '    d["soloist_running"] = True\n'
            '    d["logged_in"] = True\n'
            "    fh.seek(0)\n"
            "    fh.truncate()\n"
            "    fh.write(json.dumps(d))\n"
            '    ticks = d["ticks"]\n'
            "with open(ev, \"a\") as f:\n"
            '    f.write(json.dumps({"event": "soloist_started", "tick": ticks}) + "\\n")\n'
            "time.sleep(10)\n"
            "'\n"
        )
        soloist_run.chmod(0o755)

        soloist_ctl = self.sup_dir / "soloist-ctl"
        soloist_ctl.write_text(
            "#!/usr/bin/env python3\n"
            "import json, os, sys\n"
            'd = json.loads(open(os.environ["SUP_TEST_STATE"]).read())\n'
            "print('soloist: reachable')\n"
            "print('logged in: ' + ('yes' if d.get('logged_in') else 'no'))\n"
        )
        soloist_ctl.chmod(0o755)

        self._install_fake_tool(
            "date",
            "#!/usr/bin/env python3\n"
            "import json, os, sys\n"
            'd = json.loads(open(os.environ["SUP_TEST_STATE"]).read())\n'
            'yr = "2026" if d["ticks"] >= d.get("ntp_tick", 0) else "2015"\n'
            'print(yr if "+%Y" in sys.argv else "2026-11-27T14:00:00Z")\n',
        )
        self._install_fake_tool(
            "ip",
            "#!/usr/bin/env python3\n"
            "import json, os, sys\n"
            'd = json.loads(open(os.environ["SUP_TEST_STATE"]).read())\n'
            'if d["ticks"] < d["ipv4_tick"]:\n'
            "    if 'addr' in sys.argv:\n"
            '        print("5: wlan0: <UP>\\n    inet6 fe80::1/64 scope link")\n'
            "    sys.exit(0)\n"
            "if 'addr' in sys.argv:\n"
            '    print("5: wlan0: <UP>\\n    inet 10.77.0.111/24 scope global wlan0")\n'
            "elif 'route' in sys.argv:\n"
            '    print("default via 10.77.0.1 dev wlan0")\n',
        )
        self._install_fake_tool(
            "nslookup",
            "#!/usr/bin/env python3\n"
            "import json, os, sys\n"
            'd = json.loads(open(os.environ["SUP_TEST_STATE"]).read())\n'
            'with open(os.environ["SUP_TEST_EVENTS"], "a") as f:\n'
            '    f.write(json.dumps({"event": "nslookup", "tick": d["ticks"]}) + "\\n")\n'
            'sys.exit(0 if d["ticks"] >= d["dns_tick"] else 1)\n',
        )
        self._install_fake_tool(
            "wget",
            "#!/usr/bin/env python3\n"
            "import json, os, sys\n"
            'd = json.loads(open(os.environ["SUP_TEST_STATE"]).read())\n'
            'assert any("--ca-certificate=" in a for a in sys.argv), sys.argv\n'
            'with open(os.environ["SUP_TEST_EVENTS"], "a") as f:\n'
            '    f.write(json.dumps({"event": "wget", "tick": d["ticks"]}) + "\\n")\n'
            'sys.exit(0 if d["ticks"] >= d["tls_tick"] else 4)\n',
        )
        self._install_fake_tool(
            "ps",
            "#!/bin/sh\n"
            'echo " 100 root $MINI_SUPERVISOR_DIR/bcm-attach"\n'
            'if [ -e "$SUP_TEST_SOLOIST_UP" ]; then\n'
            '    echo " 200 root /data/opt/mini-assistant/soloist-env/soloist"\n'
            "fi\n",
        )
        self._install_fake_tool(
            "sleep",
            "#!/usr/bin/env python3\n"
            "import fcntl, json, os, sys, time\n"
            'sf = os.environ["SUP_TEST_STATE"]\n'
            "with open(sf, 'r+') as fh:\n"
            "    fcntl.flock(fh, fcntl.LOCK_EX)\n"
            "    d = json.loads(fh.read())\n"
            '    d["ticks"] += 1\n'
            "    fh.seek(0)\n"
            "    fh.truncate()\n"
            "    fh.write(json.dumps(d))\n"
            'time.sleep(0.2 if "4" in sys.argv[1:] else 0.02)\n',
        )
        self._install_fake_tool(
            "sync",
            "#!/usr/bin/env python3\n"
            "import json, os\n"
            'd = json.loads(open(os.environ["SUP_TEST_STATE"]).read())\n'
            'with open(os.environ["SUP_TEST_EVENTS"], "a") as f:\n'
            '    f.write(json.dumps({"event": "sync", "tick": d["ticks"]}) + "\\n")\n',
        )
        for noop in ("i2cset", "chronyc", "chronyd", "killall"):
            self._install_fake_tool(noop, "#!/bin/sh\nexit 0\n")

    def _install_fake_tool(self, name: str, content: str):
        p = self.fakebin / name
        p.write_text(content)
        p.chmod(0o755)

    def _run_supervisor_until_synced(self):
        env = dict(os.environ)
        env.update(
            {
                "PATH": f"{self.fakebin}:{env.get('PATH', '')}",
                "MINI_SUPERVISOR_DIR": str(self.sup_dir),
                "MINI_PAUSE_GUARD": str(self.root / "nonexistent.paused"),
                "MINI_CA_BUNDLE": str(self.ca_bundle),
                "SUP_TEST_EVENTS": str(self.events),
                "SUP_TEST_STATE": str(self.state_file),
                "SUP_TEST_SOLOIST_UP": str(self.soloist_up),
            }
        )
        stdout_path = self.root / "sup.stdout"
        stderr_path = self.root / "sup.stderr"
        with open(stdout_path, "wb") as out_fh, open(stderr_path, "wb") as err_fh:
            proc = subprocess.Popen(
                ["/bin/sh", str(self.script)],
                env=env,
                stdout=out_fh,
                stderr=err_fh,
                start_new_session=True,
            )
            events = []
            try:
                deadline = time.time() + 12.0
                while time.time() < deadline:
                    if self.events.exists():
                        events = [
                            json.loads(line)
                            for line in self.events.read_text().splitlines()
                            if line
                        ]
                        if any(e["event"] == "sync" for e in events):
                            break
                    time.sleep(0.05)
            finally:
                try:
                    os.killpg(proc.pid, signal.SIGTERM)
                except ProcessLookupError:
                    pass
                try:
                    proc.wait(timeout=2)
                except subprocess.TimeoutExpired:
                    try:
                        os.killpg(proc.pid, signal.SIGKILL)
                    except ProcessLookupError:
                        pass
                    proc.wait(timeout=2)
        unexpected_stderr = [
            line
            for line in stderr_path.read_text().splitlines()
            if line
            and "/proc/sys/abi/" not in line
            and "/sys/devices/virtual/" not in line
            and "/sys/class/tdm_state/" not in line
            and not line.startswith("Terminated")
        ]
        self.assertEqual(unexpected_stderr, [], f"Unexpected supervisor stderr: {unexpected_stderr}")
        return events

    def test_soloist_waits_for_ipv4_dns_and_tls_even_when_year_already_2026(self):
        events = self._run_supervisor_until_synced()

        soloist_events = [e for e in events if e["event"] == "soloist_started"]
        wget_events = [e for e in events if e["event"] == "wget"]
        self.assertTrue(soloist_events, f"Expected Soloist to start after readiness; events={events}")
        self.assertGreaterEqual(
            soloist_events[0]["tick"],
            5,
            f"Soloist started before DNS+TLS gate cleared: {soloist_events[0]}",
        )
        self.assertGreaterEqual(
            len(wget_events),
            2,
            f"Expected at least one failed HTTPS probe at tick 4 before tick 5 success: {wget_events}",
        )
        self.assertTrue(any(e["event"] == "sync" for e in events), "Expected flash sync once logged in")

        logfile = (self.sup_dir / "assistant.log").read_text()
        self.assertIn("Boot readiness OK: year=2026 net_wait=", logfile)
        self.assertIn("Delaying Spotify Soloist startup", logfile)
        self.assertEqual(
            logfile.count("Delaying Spotify Soloist startup"),
            1,
            "Delay log must emit once per outage, not spam every tick",
        )
        self.assertEqual(
            logfile.count("Watchdog: Network/DNS/HTTPS ready; starting Spotify Soloist..."),
            1,
            f"Expected single Soloist start log on initial readiness; log={logfile}",
        )

    def test_cold_boot_without_rtc_holds_daemon_and_soloist_until_ntp_and_network_ready(self):
        state = json.loads(self.state_file.read_text())
        state.update({"ntp_tick": 3, "ipv4_tick": 2, "dns_tick": 5, "tls_tick": 6})
        self.state_file.write_text(json.dumps(state))

        events = self._run_supervisor_until_synced()

        names = [e["event"] for e in events]
        self.assertIn("mini-assistant", names)
        self.assertIn("soloist_started", names)
        self.assertLess(names.index("mini-assistant"), names.index("soloist_started"))
        soloist_tick = next(e["tick"] for e in events if e["event"] == "soloist_started")
        self.assertGreaterEqual(soloist_tick, 6, f"Soloist started before tick 6: {events}")

        logfile = (self.sup_dir / "assistant.log").read_text()
        self.assertIn("Waiting for NTP clock sync", logfile)
        self.assertIn("Boot readiness OK: year=2026", logfile)


if __name__ == "__main__":
    unittest.main()
