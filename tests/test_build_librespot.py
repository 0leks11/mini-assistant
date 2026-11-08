import hashlib
import json
import os
from pathlib import Path
import shutil
import stat
import subprocess
import tempfile
import unittest


SCRIPT = Path(__file__).resolve().parents[1] / "scripts" / "build_librespot.sh"
TARGET = "armv7-unknown-linux-musleabihf"
REVISION = "d36f9f1907e8cc9d68a93f8ebc6b627b1bf7267d"
FIXTURE = b"fixture executable, readelf output supplied by the stub"
FAKE_TOOL = r"""#!/usr/bin/env python3
import json
import os
from pathlib import Path
import sys

tool = Path(sys.argv[0]).name
args = sys.argv[1:]
with Path(os.environ["BUILD_TEST_LOG"]).open("a", encoding="utf-8") as log:
    print(json.dumps([tool, *args]), file=log)
if tool == "git" and args[0] == "clone":
    Path(args[-1]).mkdir()
elif tool == "git" and args[-2:] == ["rev-parse", "HEAD"]:
    print(os.environ["BUILD_TEST_REVISION"])
elif tool == "docker":
    mount = args[args.index("--mount") + 1]
    source = Path(mount.split("source=", 1)[1].split(",target=", 1)[0])
    target = args[args.index("--target") + 1]
    artifact = source / "target" / target / "release" / "librespot"
    artifact.parent.mkdir(parents=True)
    artifact.write_bytes(b"fixture executable, readelf output supplied by the stub")
elif tool == "llvm-readelf" and "-h" in args:
    print("  Class:                             ELF32")
    print("  Data:                              2's complement, little endian")
    print("  Machine:                           ARM")
    print("  Flags:                             " + os.environ["BUILD_TEST_FLAGS"])
elif tool == "llvm-readelf" and "-l" in args and "-d" in args:
    print(os.environ.get("BUILD_TEST_LINKAGE", "There is no dynamic section in this file."))
else:
    raise SystemExit("unexpected command")
"""


class BuildLibrespotTests(unittest.TestCase):
    def setUp(self):
        temp = tempfile.TemporaryDirectory(prefix="librespot-build-test.")
        self.addCleanup(temp.cleanup)
        self.temp = Path(temp.name)
        self.project = self.temp / "project with spaces"
        scripts = self.project / "scripts"
        scripts.mkdir(parents=True)
        self.script = scripts / SCRIPT.name
        shutil.copy2(SCRIPT, self.script)
        tool_dir = self.temp / "tools"
        tool_dir.mkdir()
        for name in ("git", "docker", "llvm-readelf"):
            tool = tool_dir / name
            tool.write_text(FAKE_TOOL, encoding="utf-8")
            tool.chmod(0o755)
        self.log = self.temp / "commands.jsonl"
        self.env = dict(
            os.environ,
            PATH=f"{tool_dir}{os.pathsep}{os.environ['PATH']}",
            BUILD_TEST_LOG=str(self.log),
            BUILD_TEST_REVISION=REVISION,
            BUILD_TEST_FLAGS="0x5000400",
        )

    def run_build(self, *args, **overrides):
        return subprocess.run(
            ["bash", self.script, *args],
            cwd=self.temp,
            env=dict(self.env, **overrides),
            text=True,
            capture_output=True,
            timeout=10,
        )

    def commands(self):
        return [json.loads(line) for line in self.log.read_text().splitlines()]

    def assert_clean(self):
        self.assertFalse(list((self.project / "target").glob("librespot-build.*")))

    def test_pinned_pipe_build_publishes_verified_arm32_artifact(self):
        result = self.run_build()
        self.assertEqual(result.returncode, 0, result.stderr)
        artifact = self.project / "target" / "librespot-arm32" / "librespot"
        self.assertEqual(artifact.read_bytes(), FIXTURE)
        self.assertEqual(stat.S_IMODE(artifact.stat().st_mode), 0o755)
        self.assertIn(hashlib.sha256(FIXTURE).hexdigest(), result.stdout)
        clone, _, build, *_ = self.commands()
        self.assertEqual(clone[1:6], ["clone", "--depth", "1", "--branch", "v0.8.0"])
        self.assertEqual(
            build[build.index("cargo") + 1 :],
            [
                "build", "--locked", "--release", "--bin", "librespot",
                "--target", TARGET, "--no-default-features", "--features",
                "rustls-tls-webpki-roots,with-libmdns",
            ],
        )
        self.assert_clean()

    def test_wrong_commit_fails_before_build(self):
        result = self.run_build(BUILD_TEST_REVISION="0" * 40)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("Unexpected upstream commit", result.stderr)
        self.assertEqual([command[0] for command in self.commands()], ["git", "git"])
        self.assert_clean()

    def test_unsafe_elf_never_replaces_previous_artifact(self):
        artifact = self.temp / "existing"
        artifact.write_bytes(b"previous verified artifact")
        for overrides in (
            {"BUILD_TEST_FLAGS": "0x5000200, Version5 EABI, soft-float ABI"},
            {"BUILD_TEST_LINKAGE": "  INTERP 0x01234"},
            {"BUILD_TEST_LINKAGE": "0x00000001 (NEEDED) Shared library: [libc.so.6]"},
        ):
            with self.subTest(overrides=overrides):
                result = self.run_build(artifact, **overrides)
                self.assertNotEqual(result.returncode, 0)
                self.assertIn("Expected", result.stderr)
                self.assertEqual(artifact.read_bytes(), b"previous verified artifact")
                self.assert_clean()


if __name__ == "__main__":
    unittest.main()
