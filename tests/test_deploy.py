"""Exercise deployment argument/data handling with fake Docker, SSH and SCP only."""

import json
import os
from pathlib import Path
import shutil
import subprocess
import tempfile
import unittest

DEPLOY = Path(__file__).resolve().parents[1] / "scripts" / "deploy.sh"
FAKE_COMMAND = '''#!/usr/bin/env python3
import hashlib, json, os, sys
from pathlib import Path
name = Path(sys.argv[0]).name
with open(os.environ["DEPLOY_TEST_LOG"], "a") as log:
    log.write(json.dumps([name, *sys.argv[1:]]) + "\\n")
if name == "scp" and Path(sys.argv[-2]).name == os.environ.get("DEPLOY_TEST_FAIL_UPLOAD"):
    raise SystemExit(23)
if name == "ssh" and sys.argv[-1].startswith("chmod 600") and os.environ.get("DEPLOY_TEST_FAIL_CHMOD"):
    raise SystemExit(24)
if name == "ssh" and "[ -f" in sys.argv[-1]:
    print("0")
    sys.exit(0)
if name == "ssh" and "sha256sum" in sys.argv[-1]:
    if os.environ.get("DEPLOY_TEST_EMPTY_HASH"):
        sys.exit(0)
    if os.environ.get("DEPLOY_TEST_MISMATCH_HASH"):
        print("0000000000000000000000000000000000000000000000000000000000000000  fake")
        sys.exit(0)
    print(f"{hashlib.sha256(b'fake build artifact; never executed').hexdigest()}  fake")
    sys.exit(0)
'''


class DeployTests(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory(prefix="mini deploy tests ")
        self.addCleanup(self.temporary.cleanup)
        self.root = Path(self.temporary.name)
        self.project = self.root / "project with spaces"
        (self.project / "scripts").mkdir(parents=True)
        self.script = self.project / "scripts" / "deploy.sh"
        shutil.copyfile(DEPLOY, self.script)
        binary = self.project / "target/armv7-unknown-linux-musleabihf/release/mini-assistant"
        binary.parent.mkdir(parents=True)
        binary.write_bytes(b"fake build artifact; never executed")
        (self.project / "config.toml").write_text("# fixture configuration\n")
        self.key = self.root / "fixture key"
        self.key.write_text("fake key; never used by a real SSH client\n")
        self.env_file = self.project / ".env"
        self.env_file.write_text(
            f'MINI_HOST=fixture.invalid\nMINI_PORT=2222\nMINI_SSH_KEY="{self.key}"\nMINI_JUMP_HOST=fixture-user@fixture-pi\n'
        )
        self.log = self.root / "commands.jsonl"
        fakebin = self.root / "fakebin"
        fakebin.mkdir()
        for name in ("docker", "ssh", "scp"):
            path = fakebin / name
            path.write_text(FAKE_COMMAND)
            path.chmod(0o700)
        self.env = {
            name: value
            for name, value in os.environ.items()
            if not name.startswith(("MINI_", "DEPLOY_TEST_"))
        }
        self.env["PATH"] = str(fakebin) + os.pathsep + self.env["PATH"]
        self.env["DEPLOY_TEST_LOG"] = str(self.log)

    def run_deploy(self, *args):
        return subprocess.run(
            ["bash", str(self.script), *args],
            cwd=self.root,
            env=self.env,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
            timeout=10,
        )

    def commands(self):
        return [json.loads(line) for line in self.log.read_text().splitlines()] if self.log.exists() else []

    def test_from_external_cwd_with_spaces_keeps_key_and_jump_separate(self):
        run = self.run_deploy()
        self.assertEqual(run.returncode, 0, run.stderr)
        commands = self.commands()
        self.assertTrue(any(command[0] == "docker" and "--locked" in command for command in commands))
        for command in commands:
            if command[0] == "ssh":
                self.assertEqual(command[command.index("-i") + 1], str(self.key))
                self.assertEqual(command[command.index("-J") + 1], "fixture-user@fixture-pi")
            elif command[0] == "scp":
                self.assertEqual(command[command.index("-i") + 1], str(self.key))
                self.assertIn("ProxyJump=fixture-user@fixture-pi", command)
        self.assertTrue(any(command[-1] == "chmod 600 /data/opt/mini-assistant/.env" for command in commands))
        self.assertTrue(any("chmod 700 /data/opt/mini-assistant" in command[-1] for command in commands))

    def test_dotenv_commands_are_data_and_other_service_secrets_not_executed(self):
        literal_key = self.root / "key $(touch selected-marker)"
        literal_key.write_text("fake key\n")
        self.env_file.write_text(
            f'MINI_HOST=fixture.invalid\nMINI_SSH_KEY="{literal_key}"\n'
            'SPOTIFY_PASSWORD=$(touch "$PROJECT_DIR/unselected-marker")\n'
        )
        run = self.run_deploy()
        self.assertEqual(run.returncode, 0, run.stderr)
        self.assertFalse((self.root / "selected-marker").exists())
        self.assertFalse((self.project / "unselected-marker").exists())
        ssh = next(command for command in self.commands() if command[0] == "ssh")
        self.assertEqual(ssh[ssh.index("-i") + 1], str(literal_key))

    def test_each_existing_config_upload_failure_fails_closed(self):
        for name in ("config.toml", ".env"):
            with self.subTest(name=name):
                self.env["DEPLOY_TEST_FAIL_UPLOAD"] = name
                run = self.run_deploy()
                self.assertEqual(run.returncode, 23)
                self.assertNotIn("Deploy complete!", run.stdout)
                self.assertNotIn("No local", run.stdout)

    def test_remote_credentials_permission_failure_fails_closed(self):
        self.env["DEPLOY_TEST_FAIL_CHMOD"] = "1"
        run = self.run_deploy()
        self.assertEqual(run.returncode, 24)
        self.assertNotIn("Deploy complete!", run.stdout)

    def test_remote_hash_mismatch_fails_closed(self):
        self.env["DEPLOY_TEST_MISMATCH_HASH"] = "1"
        run = self.run_deploy()
        self.assertNotEqual(run.returncode, 0)
        self.assertIn("mismatch", run.stderr)
        self.assertNotIn("Deploy complete!", run.stdout)

    def test_remote_helper_hash_mismatch_fails_closed(self):
        helper = self.project / "mini_pcm_sink"
        helper.write_bytes(b"fake helper artifact")
        self.env["DEPLOY_TEST_MISMATCH_HASH"] = "1"
        run = self.run_deploy()
        self.assertNotEqual(run.returncode, 0)
        self.assertIn("mismatch", run.stderr)
        self.assertNotIn("Deploy complete!", run.stdout)

    def test_remote_source_helper_hash_mismatch_fails_closed(self):
        source = self.project / "mini_pcm_source"
        source.write_bytes(b"fake source helper artifact")
        self.env["DEPLOY_TEST_MISMATCH_HASH"] = "1"
        run = self.run_deploy()
        self.assertNotEqual(run.returncode, 0)
        self.assertIn("mismatch", run.stderr)
        self.assertNotIn("Deploy complete!", run.stdout)

    def test_remote_empty_hash_fails_closed(self):
        self.env["DEPLOY_TEST_EMPTY_HASH"] = "1"
        run = self.run_deploy()
        self.assertNotEqual(run.returncode, 0)
        self.assertIn("empty", run.stderr)
        self.assertNotIn("Deploy complete!", run.stdout)

    def test_invalid_arguments_fail_before_any_remote_or_build_command(self):
        for args in (("--host",), ("--key",), ("--jump",), ("--port",), ("--unknown",), ("--port", "0"), ("--port", "65536"), ("--port", "abc"), ("--host", "bad host")):
            with self.subTest(args=args):
                run = self.run_deploy(*args)
                self.assertEqual(run.returncode, 2)
                self.assertEqual(self.commands(), [])

    def test_cli_overrides_dotenv_host_and_port(self):
        run = self.run_deploy("--host", "override.invalid", "--port", "2200")
        self.assertEqual(run.returncode, 0, run.stderr)
        ssh = next(command for command in self.commands() if command[0] == "ssh")
        self.assertEqual(ssh[ssh.index("-p") + 1], "2200")
        self.assertEqual(ssh[-2], "root@override.invalid")

    def test_malformed_selected_dotenv_value_fails_without_echoing_contents(self):
        self.env_file.write_text('MINI_HOST="unterminated-private-fixture\n')
        run = self.run_deploy()
        self.assertNotEqual(run.returncode, 0)
        self.assertEqual(self.commands(), [])
        self.assertNotIn("unterminated-private-fixture", run.stderr)

    def test_help_has_no_config_or_build_side_effects(self):
        self.env_file.write_text('MINI_HOST="invalid\n')
        run = self.run_deploy("--help")
        self.assertEqual(run.returncode, 0)
        self.assertEqual(self.commands(), [])

    def test_skip_build_flag_skips_docker_invocation(self):
        run = self.run_deploy("--skip-build")
        self.assertEqual(run.returncode, 0, run.stderr)
        commands = self.commands()
        self.assertFalse(any(command[0] == "docker" for command in commands))
        self.assertTrue(any(command[0] == "ssh" for command in commands))


if __name__ == "__main__":
    unittest.main()
