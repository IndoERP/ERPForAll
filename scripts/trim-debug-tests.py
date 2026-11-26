#!/usr/bin/env python3
# SPDX-License-Identifier: MIT OR Apache-2.0
"""Trim completed Cargo test executables in this checkout when its local disk budget is exceeded."""

import fcntl
import json
import os
import re
import stat
import subprocess
from contextlib import ExitStack
from pathlib import Path


ROOT = Path(__file__).resolve().parents[1]
GIB = 1024**3


def size_bytes(directory):
    output = subprocess.check_output(["du", "-sk", str(directory)], text=True)
    return int(output.split()[0]) * 1024


def test_outputs(debug, packages):
    """Use Cargo's unit kind and hash so a library or CLI with a similar name survives."""
    outputs = []
    for unit in (debug / ".fingerprint").iterdir():
        package, _, suffix = unit.name.rpartition("-")
        if package not in packages or unit.is_symlink() or not re.fullmatch("[0-9a-f]{16}", suffix):
            continue
        for marker in unit.glob("test-*.json"):
            match = re.fullmatch(r"test-(?:integration-test|lib|bin)-([\w-]+)\.json", marker.name)
            if not match:
                continue
            name = match[1].replace("-", "_")
            for binary in {name, match[1]}:
                path = debug / "deps" / (binary + "-" + suffix)
                try:
                    info = path.lstat()
                except FileNotFoundError:
                    continue
                if stat.S_ISREG(info.st_mode) and info.st_mode & 0o111 and info.st_nlink == 1:
                    outputs.append((info.st_mtime_ns, path, info.st_blocks * 512))
    return sorted(outputs)


def test_process_running(debug):
    # nextest can execute binaries after Cargo releases its build lock.
    commands = subprocess.check_output(["ps", "-axo", "comm="], text=True).splitlines()
    for line in commands:
        command = line.strip()
        if Path(command).name in {"cargo", "cargo-nextest", "nextest"}:
            return True
        if command.startswith(str(debug / "deps") + os.sep):
            return True
    return False


def trim(debug, packages, excess):
    with ExitStack() as stack:
        for name in (".cargo-build-lock", ".cargo-lock", ".cargo-artifact-lock"):
            try:
                handle = stack.enter_context((debug / name).open("rb"))
                fcntl.flock(handle, fcntl.LOCK_EX | fcntl.LOCK_NB)
            except (FileNotFoundError, BlockingIOError):
                print("debug cache: cleanup deferred (Cargo lock unavailable)")
                return
        if test_process_running(debug):
            print("debug cache: cleanup deferred while Cargo or tests are running")
            return
        outputs = test_outputs(debug, packages)
        # Apple builds can leave an unpacked debug object alongside the harness.
        # Group objects in one directory scan, using the full Cargo unit hash.
        objects = {path.name: [] for _, path, _ in outputs}
        for entry in os.scandir(debug / "deps"):
            prefix = entry.name.partition(".")[0]
            if prefix in objects and entry.name.endswith(".rcgu.o") and entry.is_file(follow_symlinks=False):
                objects[prefix].append(Path(entry.path))
        removed = freed = 0
        for _, path, blocks in outputs:
            if freed >= excess:
                break
            path.unlink()
            removed += 1
            freed += blocks
            for obj in objects[path.name]:
                info = obj.stat()
                if info.st_nlink == 1:
                    obj.unlink()
                    freed += info.st_blocks * 512
        print(f"debug cache: retired {removed} test executables ({freed / GIB:.2f} GiB)")


def main():
    setting = ROOT / ".cargo/debug-cache-limit-gib"
    if not setting.exists():
        return
    limit = int(setting.read_text().strip())
    if limit < 1:
        raise ValueError(f"{setting} must contain a positive integer")
    metadata = json.loads(subprocess.check_output(
        ["cargo", "metadata", "--offline", "--locked", "--no-deps", "--format-version", "1"],
        cwd=ROOT, text=True,
    ))
    target = ROOT / "target"
    debug = target / "debug"
    build = Path(metadata.get("build_directory", metadata["target_directory"]))
    if Path(metadata["target_directory"]).resolve() != target or build.resolve() != target:
        print("debug cache: skipping a shared or redirected target directory")
        return
    if any(path.resolve() != path for path in (debug, debug / ".fingerprint", debug / "deps")):
        print("debug cache: skipping a symlinked build directory")
        return
    if not (debug / ".fingerprint").is_dir() or not (debug / "deps").is_dir():
        return
    if test_process_running(debug):
        print("debug cache: cleanup deferred while Cargo or tests are running")
        return
    excess = size_bytes(debug) - limit * GIB
    if excess > 0:
        members = set(metadata["workspace_members"])
        packages = {p["name"] for p in metadata["packages"] if p["id"] in members}
        trim(debug, packages, excess)


if __name__ == "__main__":
    main()
