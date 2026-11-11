#!/usr/bin/env python3
"""Check that `Limits::recommended` in crates/erp-html/src/limits.rs follows the committed envelope log
.

The source names its log on a `// ENVELOPE-LOG: <path>` line. From the log's rows (entries the real
sanitiser parsed): C is the heap peak of the empty entry and K_input = max((peak_bytes - C) /
input_bytes). The input cap is `min(ceiling, floor((W - C) / K_input))` for the working limit W the
source declares (`sink_reserved` is 0: the sink is the real one, so its allocations are in the rows).
The ratio is an exact fraction; nothing is typed by hand. Exit status 1 and one line per finding.

Usage: python3 -I scripts/check-html-limits.py [--root DIR]
"""

import argparse
import json
import re
import sys
from fractions import Fraction
from pathlib import Path

LIMITS_RS = "crates/erp-html/src/limits.rs"


def constant(source, name):
    match = re.search(rf"^const {name}: u64 = ([0-9_]+);", source, re.M)
    return None if match is None else int(match.group(1).replace("_", ""))


def log_rows(text):
    lines = text.splitlines()
    try:
        start = lines.index("# raw rows") + 1
        end = lines.index("# summary")
    except ValueError:
        return None
    return [json.loads(line) for line in lines[start:end] if line.strip()]


def check(source, root):
    """Return the findings (empty when `source` follows its log)."""
    marker = re.search(r"^// ENVELOPE-LOG: (\S+)$", source, re.M)
    if marker is None:
        return ["limits.rs has no `// ENVELOPE-LOG:` line"]
    names = (
        "CEILING_INPUT_BYTES",
        "CEILING_WORKING_BYTES",
        "RECOMMENDED_WORKING_BYTES",
        "RECOMMENDED_INPUT_BYTES",
    )
    values = {}
    for name in names:
        values[name] = constant(source, name)
        if values[name] is None:
            return [f"limits.rs has no {name} constant"]
    log_path = Path(root) / marker.group(1)
    if not log_path.is_file():
        return [f"the envelope log {marker.group(1)} does not exist"]
    text = log_path.read_text(encoding="utf-8")
    if not re.search(r"^sink: erp-html\b", text, re.M):
        return ["the envelope log does not name sink erp-html"]
    summary = text.split("# summary", 1)[1] if "# summary" in text else None
    if summary is not None:
        if "summary: FAILED" in summary or "VIOLATED" in summary:
            return ["the envelope log summary failed or a bound is violated"]
        if summary.count(": HOLDS") != 2:
            return ["the envelope log summary has no HOLDS verdict for both invariants"]
    rows = log_rows(text)
    if rows is None:
        return ["the envelope log has no `# raw rows` and `# summary` sections"]
    parsed = [r for r in rows if r["outcome"] == "parsed"]
    empty = [r for r in parsed if r["entry"] == "empty"]
    if not empty:
        return ["the envelope log has no parsed `empty` row"]
    with_input = [r for r in parsed if r["input_bytes"] > 0]
    if not with_input:
        return ["the envelope log has no parsed row with input"]
    c = empty[0]["peak_bytes"]
    k_input = max(Fraction(r["peak_bytes"] - c, r["input_bytes"]) for r in with_input)
    findings = []
    working = values["RECOMMENDED_WORKING_BYTES"]
    if working > values["CEILING_WORKING_BYTES"]:
        findings.append(
            f"working limit {working} is above the ceiling {values['CEILING_WORKING_BYTES']}"
        )
    if working <= c or k_input <= 0:
        findings.append(f"working limit {working} leaves no room over C={c} for any input")
        return findings
    expected = min(values["CEILING_INPUT_BYTES"], int((working - c) / k_input))
    actual = values["RECOMMENDED_INPUT_BYTES"]
    if actual != expected:
        findings.append(f"input cap is {actual} in limits.rs but the formula gives {expected}")
    return findings


def main(argv):
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--root", default=Path(__file__).resolve().parents[1], type=Path)
    args = parser.parse_args(argv)
    findings = check((args.root / LIMITS_RS).read_text(encoding="utf-8"), args.root)
    for finding in findings:
        print(f"check-html-limits: {finding}", file=sys.stderr)
    if findings:
        return 1
    print("check-html-limits: ok")
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
