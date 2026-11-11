#!/usr/bin/env python3
"""Check that fuzz/Cargo.toml repeats the root `[patch.crates-io]` entries it needs.

`fuzz/` is an independent workspace, so it does not inherit the root patch section. Every entry in the
fuzz patch must point at the same vendored directory as the root's (relative to `fuzz/`), and a fuzz
workspace that depends on `erp-html` must patch html5ever (the bounded fork, feature `budget`).
Exit status 1 and one line per finding.

Usage: python3 -I scripts/check-fuzz-patch.py [--root DIR]
"""

import argparse
import re
import sys
from pathlib import Path


def section(text, header):
    """The `key = value` lines of one `[header]` table, as {key: value text}.

    Only what this check needs: a table of single-line entries. A python3 older than 3.11 has no
    tomllib (the GitHub runner), so the two manifests are not parsed with it.
    """
    lines, inside = {}, False
    for raw in text.splitlines():
        line = raw.split("#", 1)[0].strip()
        if line.startswith("["):
            inside = line == f"[{header}]"
            continue
        match = re.match(r"^([A-Za-z0-9_-]+)\s*=\s*(.*)$", line)
        if inside and match:
            lines[match.group(1)] = match.group(2)
    return lines


def path_of(entry):
    match = re.search(r'path\s*=\s*"([^"]+)"', entry)
    return match.group(1) if match else None


def check(root_text, fuzz_text):
    """Return the findings for the two manifests' text."""
    root = section(root_text, "patch.crates-io")
    fuzz = section(fuzz_text, "patch.crates-io")
    findings = []
    if "erp-html" in section(fuzz_text, "dependencies") and "html5ever" not in fuzz:
        findings.append("fuzz depends on erp-html but its [patch.crates-io] has no html5ever entry")
    for name, entry in sorted(fuzz.items()):
        if name not in root:
            findings.append(f"fuzz patches {name}, which the root [patch.crates-io] does not")
            continue
        expected = "../" + (path_of(root[name]) or "")
        if path_of(entry) != expected:
            findings.append(
                f"fuzz patches {name} to {path_of(entry)}, the root workspace to {path_of(root[name])}"
            )
    return findings


def main(argv):
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--root", default=Path(__file__).resolve().parents[1], type=Path)
    args = parser.parse_args(argv)
    findings = check(
        (args.root / "Cargo.toml").read_text(encoding="utf-8"),
        (args.root / "fuzz" / "Cargo.toml").read_text(encoding="utf-8"),
    )
    for finding in findings:
        print(f"check-fuzz-patch: {finding}", file=sys.stderr)
    if findings:
        return 1
    print("check-fuzz-patch: ok")
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
