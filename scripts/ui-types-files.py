#!/usr/bin/env python3
"""Compare or replace the complete managed .d.ts set; never consult or modify git.

Also the shared helper of the two identity records (scripts/ui-generated.py, scripts/store-wire-generated.py):
`cargo_identity` hashes the part of Cargo.lock and of the root Cargo.toml that one generator crate can see, so
an unrelated slice's dependency or workspace-member change neither invalidates the record nor conflicts in it
when staging moves (TOOL.GENERATED-MERGE). Python 3.10 compatible (no tomllib): the two files are read line by line.
"""
import argparse
import hashlib
from pathlib import Path
import re
import sys


def _sha(text):
    return hashlib.sha256(text.encode()).hexdigest()


def cargo_identity(root, crate):
    """{"Cargo.lock": ..., "Cargo.toml": ...} digests restricted to the dependency closure of `crate`."""
    blocks = {}
    for block in (root / "Cargo.lock").read_text().split("[[package]]")[1:]:
        name = re.search(r'^name = "([^"]+)"', block, re.M)
        version = re.search(r'^version = "([^"]+)"', block, re.M)
        if not name or not version:
            raise ValueError("Cargo.lock: a package block without name or version")
        blocks.setdefault(name.group(1), {})[version.group(1)] = block.rstrip() + "\n"
    if crate not in blocks:
        raise ValueError(f"Cargo.lock has no package {crate}")
    closure, stack = {}, [(crate, v) for v in blocks[crate]]
    while stack:
        key = stack.pop()
        if key in closure:
            continue
        closure[key] = blocks[key[0]][key[1]]
        deps = re.search(r"^dependencies = \[(.*?)^\]", closure[key], re.M | re.S)
        for dep in re.findall(r'"([^"]+)"', deps.group(1)) if deps else []:
            name, *rest = dep.split(" ")
            versions = [rest[0]] if rest else list(blocks.get(name, {}))
            if not versions or any(v not in blocks.get(name, {}) for v in versions):
                raise ValueError(f"Cargo.lock: dependency {dep} of {key[0]} has no package block")
            stack.extend((name, v) for v in versions)
    names = {name for name, _ in closure}
    kept, section, skipping = [], "", False
    for line in (root / "Cargo.toml").read_text().splitlines():
        if skipping:
            skipping = line.strip() != "]"
            continue
        if not line.strip() or line.lstrip().startswith("#"):
            continue
        if line.startswith("["):
            section = line.strip()
        elif section == "[workspace]" and re.match(r"(members|exclude)\s*=", line):
            skipping = line.rstrip().endswith("[")  # a one-line `exclude = [...]` is dropped alone
            continue
        elif section == "[workspace.dependencies]":
            entry = re.match(r"([A-Za-z0-9_-]+) = (.*)$", line)
            if not entry or entry.group(2).count("{") != entry.group(2).count("}") \
                    or entry.group(2).count("[") != entry.group(2).count("]"):
                raise ValueError(f"Cargo.toml: unsupported line in [workspace.dependencies]: {line!r}")
            package = re.search(r'package\s*=\s*"([^"]+)"', entry.group(2))
            if entry.group(1) not in names and not (package and package.group(1) in names):
                continue
        kept.append(line.rstrip())
    return {"Cargo.lock": _sha("".join(closure[k] for k in sorted(closure))),
            "Cargo.toml": _sha("\n".join(kept))}


def declarations(directory):
    if directory.is_symlink():
        raise ValueError(f"declaration directory is a symlink: {directory}")
    if not directory.exists():
        return {}
    result = {}
    for path in sorted(directory.rglob("*")):
        if path.is_symlink():
            raise ValueError(f"symlink in declaration directory: {path}")
        if path.name.endswith(".d.ts"):
            if not path.is_file():
                raise ValueError(f"declaration is not a file: {path}")
            result[path.relative_to(directory).as_posix()] = path.read_bytes()
    return result


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("mode", choices=("--write", "--check"))
    parser.add_argument("generated", type=Path)
    parser.add_argument("managed", type=Path)
    # Modes start with --; treat the complete argv as positional values.
    args = parser.parse_args(["--", *sys.argv[1:]])
    expected = declarations(args.generated)
    if not expected:
        raise ValueError("schema generator emitted no declaration files")
    actual = declarations(args.managed)
    if args.mode == "--check":
        differences = []
        differences.extend(f"missing: {name}" for name in sorted(expected.keys() - actual.keys()))
        differences.extend(f"extra: {name}" for name in sorted(actual.keys() - expected.keys()))
        differences.extend(f"changed: {name}" for name in sorted(expected.keys() & actual.keys())
                           if expected[name] != actual[name])
        if differences:
            print("UI declaration drift:\n" + "\n".join(differences), file=sys.stderr)
            return 1
        return 0
    args.managed.mkdir(parents=True, exist_ok=True)
    for name, content in expected.items():
        destination = args.managed / name
        destination.parent.mkdir(parents=True, exist_ok=True)
        destination.write_bytes(content)
    for stale in sorted(actual.keys() - expected.keys()):
        (args.managed / stale).unlink()
    return 0


if __name__ == "__main__":
    try:
        sys.exit(main())
    except (OSError, ValueError) as error:
        print(f"UI declaration generation failed: {error}", file=sys.stderr)
        sys.exit(1)
