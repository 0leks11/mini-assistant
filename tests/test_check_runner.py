"""Check the host runner with fake commands; never recurse into Cargo or unittest."""

import json
import os
from pathlib import Path
import re
import shutil
import subprocess
import sys
import tempfile
import unittest


RUNNER = Path(__file__).resolve().parents[1] / "scripts/check.sh"
STAGES = ["format", "lint", "rust", "vendor", "python"]
FAKE_COMMAND = r'''
import json
import os
from pathlib import Path
import sys

args = sys.argv[1:]
if Path(sys.argv[0]).name == "python3":
    stage = "python"
elif args[0] == "fmt":
    stage = "format"
elif args[0] == "clippy":
    stage = "lint"
elif "--manifest-path" in args:
    stage = "vendor"
else:
    stage = "rust"
with open(os.environ["CHECK_RUNNER_TEST_COMMANDS"], "a") as log:
    log.write(json.dumps({"stage": stage, "args": args, "cwd": os.getcwd()}) + "\n")
print("fixture output for " + stage, flush=True)
if stage == os.environ.get("CHECK_RUNNER_TEST_NOISY"):
    for i in range(500):
        print("fixture stdout " + "x" * 1024)
        print("fixture stderr " + "y" * 1024, file=sys.stderr)
raise SystemExit(json.loads(os.environ["CHECK_RUNNER_TEST_CODES"]).get(stage, 0))
'''


class CheckRunnerTests(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory(prefix="gpt mini check runner ")
        self.addCleanup(self.temporary.cleanup)
        self.root = Path(self.temporary.name).resolve()
        self.project = self.root / "project with spaces"
        (self.project / "scripts").mkdir(parents=True)
        self.runner = self.project / "scripts/check.sh"
        shutil.copyfile(RUNNER, self.runner)
        self.command_log = self.root / "commands.jsonl"
        self.fakebin = self.root / "fakebin"
        self.fakebin.mkdir()
        for name in ("cargo", "python3"):
            command = self.fakebin / name
            command.write_text(f"#!{sys.executable}\n" + FAKE_COMMAND)
            command.chmod(0o700)
        self.log_root = self.root / "private logs"
        self.log_root.mkdir()
        self.env = {
            key: value
            for key, value in os.environ.items()
            if not key.startswith("CHECK_RUNNER_TEST_")
        }
        self.env.update(
            PATH=str(self.fakebin) + os.pathsep + os.environ.get("PATH", ""),
            TMPDIR=str(self.log_root),
            CHECK_RUNNER_TEST_COMMANDS=str(self.command_log),
            CHECK_RUNNER_TEST_CODES="{}",
        )

    def run_checks(self, codes=None, noisy=None):
        self.env["CHECK_RUNNER_TEST_CODES"] = json.dumps(codes or {})
        if noisy:
            self.env["CHECK_RUNNER_TEST_NOISY"] = noisy
        return subprocess.run(
            ["bash", str(self.runner)],
            cwd=self.root,
            env=self.env,
            text=True,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            timeout=10,
            check=False,
        )

    def commands(self):
        return [json.loads(line) for line in self.command_log.read_text().splitlines()]

    def summary_code(self, result, stage):
        match = re.search(rf"^{stage}\s+(PASS|FAIL) exit=(\d+)$", result.stdout, re.M)
        self.assertIsNotNone(match, result.stdout)
        return match.group(1), int(match.group(2))

    def test_all_checks_run_from_project_directory_and_preserve_required_flags(self):
        result = self.run_checks()
        self.assertEqual(result.returncode, 0, result.stderr)
        commands = self.commands()
        self.assertEqual([item["stage"] for item in commands], STAGES)
        self.assertTrue(all(item["cwd"] == str(self.project) for item in commands))
        self.assertEqual(commands[0]["args"], ["fmt", "--all", "--", "--check"])
        self.assertEqual(
            commands[1]["args"],
            ["clippy", "--locked", "--all-targets", "--", "-D", "warnings"],
        )
        self.assertEqual(commands[2]["args"], ["test", "--locked", "--all-targets"])
        self.assertEqual(
            commands[3]["args"],
            ["test", "--locked", "--manifest-path", "vendor/rustpotter-ref/Cargo.toml", "--no-default-features"],
        )
        self.assertEqual(
            commands[4]["args"],
            ["-B", "-m", "unittest", "discover", "-s", "tests", "-p", "test_*.py", "-v"],
        )
        for stage in STAGES:
            self.assertEqual(self.summary_code(result, stage), ("PASS", 0))
        self.assertIn("RESULT PASS (5/5 stages passed)", result.stdout)

    def test_each_failed_stage_remains_failed_after_later_stages_finish(self):
        for stage in STAGES:
            with self.subTest(stage=stage):
                self.command_log.unlink(missing_ok=True)
                result = self.run_checks({stage: 23})
                self.assertEqual(result.returncode, 1, result.stderr)
                self.assertEqual([item["stage"] for item in self.commands()], STAGES)
                self.assertEqual(self.summary_code(result, stage), ("FAIL", 23))
                for other in STAGES:
                    if other != stage:
                        self.assertEqual(self.summary_code(result, other), ("PASS", 0))
                self.assertIn("RESULT FAIL (1/5 stages failed)", result.stdout)

    def test_summary_retains_all_failure_codes_including_command_unavailable(self):
        codes = {stage: 11 + i for i, stage in enumerate(STAGES)}
        codes["lint"] = 127
        result = self.run_checks(codes)
        self.assertEqual(result.returncode, 1)
        self.assertEqual([item["stage"] for item in self.commands()], STAGES)
        for stage, code in codes.items():
            self.assertEqual(self.summary_code(result, stage), ("FAIL", code))
        self.assertIn("RESULT FAIL (5/5 stages failed)", result.stdout)

    def test_failure_output_is_bounded_and_full_log_remains_available(self):
        result = self.run_checks({"rust": 31}, noisy="rust")
        self.assertEqual(result.returncode, 1, result.stderr)
        self.assertLess(len(result.stdout.encode()), 20_000)
        self.assertEqual([item["stage"] for item in self.commands()], STAGES)
        directories = list(self.log_root.glob("gpt-mini-check.*"))
        self.assertEqual(len(directories), 1)
        full_log = directories[0] / "rust.log"
        self.assertGreater(full_log.stat().st_size, 1_000_000)
        content = full_log.read_text()
        self.assertIn("fixture stdout", content)
        self.assertIn("fixture stderr", content)
        self.assertEqual({path.name for path in directories[0].iterdir()}, {stage + ".log" for stage in STAGES})
        self.assertIn(str(directories[0]), result.stdout)
        if os.name == "posix":
            self.assertEqual(directories[0].stat().st_mode & 0o077, 0)

    def test_dotenv_is_never_executed(self):
        marker = self.project / "dotenv-executed"
        (self.project / ".env").write_text("touch dotenv-executed\nexit 39\n")
        result = self.run_checks()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertFalse(marker.exists())
        self.assertEqual([item["stage"] for item in self.commands()], STAGES)


if __name__ == "__main__":
    unittest.main()
