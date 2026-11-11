#!/usr/bin/env python3
"""Compare or replace the complete managed .d.ts set; never consult or modify git."""
import argparse
from pathlib import Path
import sys


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
