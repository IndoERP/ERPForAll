# SPDX-License-Identifier: MIT OR Apache-2.0
import fcntl
import importlib.util
import json
import os
import shutil
import subprocess
import tempfile
import unittest
from pathlib import Path
from unittest.mock import patch


script = Path(__file__).resolve().parents[1] / "trim-debug-tests.py"
spec = importlib.util.spec_from_file_location("trim_debug_tests", script)
cache = importlib.util.module_from_spec(spec)
spec.loader.exec_module(cache)


class DebugCacheTests(unittest.TestCase):
    def setUp(self):
        temporary = tempfile.TemporaryDirectory()
        self.addCleanup(temporary.cleanup)
        self.debug = Path(temporary.name)
        (self.debug / "deps").mkdir()
        (self.debug / ".fingerprint").mkdir()
        for name in (".cargo-build-lock", ".cargo-lock", ".cargo-artifact-lock"):
            (self.debug / name).touch()

    def binary(self, package, marker, name, sequence):
        suffix = f"{sequence:016x}"
        fingerprint = self.debug / ".fingerprint" / f"{package}-{suffix}"
        fingerprint.mkdir()
        (fingerprint / (marker + ".json")).write_text("{}")
        path = self.debug / "deps" / f"{name}-{suffix}"
        path.write_bytes(b"harness")
        path.chmod(0o755)
        os.utime(path, (sequence, sequence))
        return path

    def test_retires_only_oldest_workspace_harness_to_meet_budget(self):
        old = self.binary("our-pkg", "test-integration-test-same", "same", 1)
        obj = old.with_suffix(".part.rcgu.o")
        obj.write_bytes(b"object")
        keep = [
            self.binary("our-pkg", "test-bin-same", "same", 2),
            self.binary("external", "test-lib-same", "same", 3),
            self.binary("our-pkg", "bin-same", "same", 4),
            self.binary("our-pkg", "lib-same", "same", 5),
        ]
        linked = self.binary("our-pkg", "test-lib-same", "same", 6)
        os.link(linked, self.debug / "kept-binary")
        keep.append(linked)
        with patch.object(cache, "test_process_running", return_value=False):
            cache.trim(self.debug, {"our-pkg"}, 1)
        self.assertFalse(old.exists())
        self.assertFalse(obj.exists())
        self.assertTrue(all(path.exists() for path in keep))

    def test_busy_build_lock_and_running_tests_defer_cleanup(self):
        old = self.binary("our-pkg", "test-integration-test-one", "one", 1)
        with (self.debug / ".cargo-build-lock").open("rb") as guard:
            fcntl.flock(guard, fcntl.LOCK_EX)
            cache.trim(self.debug, {"our-pkg"}, 1)
        self.assertTrue(old.exists())
        with patch.object(cache, "test_process_running", return_value=True):
            cache.trim(self.debug, {"our-pkg"}, 1)
        self.assertTrue(old.exists())

    def test_redirected_cargo_target_is_never_scanned(self):
        root = self.debug / "checkout"
        (root / ".cargo").mkdir(parents=True)
        (root / ".cargo/debug-cache-limit-gib").write_text("20")
        other = str(self.debug / "other-checkout/target")
        metadata = json.dumps({"target_directory": other, "build_directory": other})
        with (
            patch.object(cache, "ROOT", root),
            patch.object(cache.subprocess, "check_output", return_value=metadata),
            patch.object(cache, "size_bytes", side_effect=AssertionError("scanned another checkout")),
        ):
            cache.main()

    def test_installer_respects_local_budget_and_retains_original_default(self):
        root = self.debug / "checkout"
        scripts = root / "scripts"
        scripts.mkdir(parents=True)
        shutil.copy2(script, scripts)
        shutil.copy2(script.with_name("install.sh"), scripts)
        (root / ".cargo").mkdir()
        (root / "src").mkdir()
        (root / "src/lib.rs").touch()
        (root / "Cargo.toml").write_text('[package]\nname="fixture"\nversion="0.1.0"\nedition="2024"\n')
        subprocess.run(["cargo", "generate-lockfile", "--offline"], cwd=root, check=True)
        artifact = root / "target/debug/deps/libfixture.rlib"
        artifact.parent.mkdir(parents=True)
        artifact.write_bytes(b"cached")
        setting = root / ".cargo/debug-cache-limit-gib"
        setting.write_text("20")
        command = ["bash", str(scripts / "install.sh"), "--clean-cache", "--state-dir", str(root / "state")]
        subprocess.run(command, cwd=root, check=True, capture_output=True)
        self.assertEqual(artifact.read_bytes(), b"cached")
        setting.unlink()
        subprocess.run(command, cwd=root, check=True, capture_output=True)
        self.assertFalse(artifact.exists())


if __name__ == "__main__":
    unittest.main()
