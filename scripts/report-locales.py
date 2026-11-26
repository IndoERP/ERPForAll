#!/usr/bin/env python3
# SPDX-License-Identifier: MIT OR Apache-2.0
"""Extract the pinned CLDR report subset; no runtime network access is needed."""

import argparse
import hashlib
import json
from pathlib import Path
import urllib.request

VERSION = "47.0.0"
BASE_URL = f"https://raw.githubusercontent.com/unicode-org/cldr-json/{VERSION}/"
LOCALES = ("en", "fr", "id")
WIDTHS = ("long", "short", "narrow")
UNITS = ("year", "month", "week", "day", "hour", "minute", "second")
DAYS = ("sun", "mon", "tue", "wed", "thu", "fri", "sat")
# Source identity is part of this extractor, not an operator-tunable setting.
SHA256 = {
    "LICENSE": "220ba0e1c43b99530d2d5bdb892a99dca0989414f51ab695ecd90163eaa1ec3b",
    "cldr-json/cldr-core/supplemental/plurals.json": "83f10932abb4ce2c4681725c6d11e4c01097a758830677127142c80c79bd6322",
    "cldr-json/cldr-dates-full/main/en/ca-gregorian.json": "0d0eaf3fa57a60e247de08636fae9071349415b16bd910e18cabaa081d96ab12",
    "cldr-json/cldr-units-full/main/en/units.json": "334e097aaf5ed98fdef711fc1ab676b1cc1a131936c124ad0e699171def39c89",
    "cldr-json/cldr-dates-full/main/fr/ca-gregorian.json": "ff2f88f3ed7c06cca43845e170776d22a2cc1fb952cdd236a07da5250d47260b",
    "cldr-json/cldr-units-full/main/fr/units.json": "f68909529c9b7d4d2ecae2bae799a9bbebba6c12a0d1709864fa7fb809e888f9",
    "cldr-json/cldr-dates-full/main/id/ca-gregorian.json": "6f58a418beae6d309dc331635c492b7958c6eb1ee5feb06637b56883f8c3a8b2",
    "cldr-json/cldr-units-full/main/id/units.json": "57873fd751f9acfd38c4683f26e3cd03c518ad252a91eed7fa3c8171a09af3bd",
}


def read_sources(source_dir):
    """Read local originals or download the pinned release; reject changed bytes."""
    sources = {}
    for path, expected in SHA256.items():
        if source_dir is None:
            with urllib.request.urlopen(BASE_URL + path, timeout=30) as response:
                data = response.read(8 * 1024 * 1024 + 1)
                if len(data) > 8 * 1024 * 1024:
                    raise ValueError(f"{path}: source exceeds 8 MiB acquisition bound")
        else:
            data = (source_dir / path).read_bytes()
        actual = hashlib.sha256(data).hexdigest()
        if actual != expected:
            raise ValueError(f"{path}: SHA256 {actual}, expected {expected}")
        sources[path] = data
    return sources


def text(value, location):
    if not isinstance(value, str) or not value:
        raise ValueError(f"{location}: expected a nonempty CLDR string")
    return value


def extract(locale, sources):
    """Keep Gregorian format names and duration unit patterns without rewriting."""
    calendar = json.loads(sources[
        f"cldr-json/cldr-dates-full/main/{locale}/ca-gregorian.json"
    ])["main"][locale]["dates"]["calendars"]["gregorian"]
    units = json.loads(sources[
        f"cldr-json/cldr-units-full/main/{locale}/units.json"
    ])["main"][locale]["units"]
    result = {}
    for kind, keys in (("months", tuple(str(i) for i in range(1, 13))), ("days", DAYS)):
        for output_width, cldr_width in (("short", "abbreviated"), ("long", "wide")):
            result[f"{kind}_{output_width}"] = [
                text(calendar[kind]["format"][cldr_width][key], f"{locale}/{kind}/{key}")
                for key in keys
            ]
    periods = calendar["dayPeriods"]["format"]["abbreviated"]
    result["periods"] = [text(periods[key], f"{locale}/{key}") for key in ("am", "pm")]
    # Only positive integer duration components reach this subset. French zero
    # and decimal rules are intentionally not represented by this selector.
    result["plural_rule"] = "other" if locale == "id" else "one"
    result["duration"] = {}
    for width in WIDTHS:
        result["duration"][width] = {}
        for unit in UNITS:
            patterns = units[width][f"duration-{unit}"]
            other = text(patterns["unitPattern-count-other"], f"{locale}/{width}/{unit}/other")
            # CLDR missing plural-category patterns resolve to the locale's
            # other form. This is not a fallback to another locale.
            one = patterns.get("unitPattern-count-one", other)
            pair = {"one": text(one, f"{locale}/{width}/{unit}/one"), "other": other}
            for category, pattern in pair.items():
                if pattern.count("{0}") != 1 or "{" in pattern.replace("{0}", "") or "}" in pattern.replace("{0}", ""):
                    raise ValueError(f"{locale}/{width}/{unit}/{category}: expected one {{0}} placeholder")
            many = patterns.get("unitPattern-count-many", other)
            if locale == "fr" and many != other:
                raise ValueError(f"fr/{width}/{unit}: many differs from other; expand the resource schema")
            result["duration"][width][unit] = pair
    return result


def readme():
    lines = [
        "<!-- SPDX-License-Identifier: MIT OR Apache-2.0 -->",
        "# Report locale resources", "",
        "These derived data files are licensed under **Unicode-3.0**, not the project's",
        "source-code licence. The unmodified upstream notice is preserved in [LICENSE](LICENSE).", "",
        f"Source: [unicode-org/cldr-json {VERSION}](https://github.com/unicode-org/cldr-json/tree/{VERSION}).",
        "The extractor pins and verifies the SHA256 of every input before writing outputs.", "",
        "Regenerate from the repository root (Python standard library only):", "",
        "```sh", "python3 -I scripts/report-locales.py", "```", "",
        "For offline regeneration, pass `--source-dir /path/to/cldr-json-checkout` containing",
        "the exact upstream paths below. `--out` selects an alternative output directory.", "",
        "## Extraction contract", "",
        "- Gregorian `format` names: abbreviated/wide months in calendar order and",
        "  abbreviated/wide weekdays in Sunday-first order. No stand-alone names are used.",
        "- Day periods are the locale's abbreviated `am` and `pm` entries.",
        "- Duration year/month/week/day/hour/minute/second patterns retain `{0}` and",
        "  all original Unicode spacing in long, short and narrow widths.",
        "- Missing `one` patterns explicitly reuse the same locale's `other` pattern.",
        "  There is no fallback to English or to another locale.",
        "- `plural_rule: one` selects `one` only for positive integer 1; `other` always",
        "  selects `other`. These selectors are restricted to positive integer duration",
        "  components, not a general CLDR plural engine. English and French use `one`;",
        "  Indonesian uses `other`. French zero, decimals and compact exponents are out of scope.",
        "- French has a cardinal `many` category, but the selected duration units have",
        "  no explicit `many` patterns in this release. CLDR's category fallback uses",
        "  `other`. Extraction refuses an explicit `many` pattern differing from `other`.", "",
        "## Input provenance", "",
        "Paths are relative to the pinned source repository. Digests cover downloaded",
        "bytes, including the unmodified license and supplemental plural rules.", "",
        "| Source | SHA256 |", "| --- | --- |",
    ]
    lines += [f"| [{path}]({BASE_URL}{path}) | `{digest}` |" for path, digest in SHA256.items()]
    return "\n".join(lines) + "\n"


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--source-dir", type=Path, help="read exact upstream paths locally instead of downloading")
    parser.add_argument("--out", type=Path, default=Path(__file__).resolve().parents[1] / "crates/erp-report/locales")
    args = parser.parse_args()
    sources = read_sources(args.source_dir)
    # Extract all data before changing any output, so a source error fails loudly.
    outputs = {f"{locale}.json": (json.dumps(extract(locale, sources), ensure_ascii=False, indent=2) + "\n").encode("utf-8") for locale in LOCALES}
    outputs["LICENSE"] = sources["LICENSE"]
    outputs["README.md"] = readme().encode("utf-8")
    args.out.mkdir(parents=True, exist_ok=True)
    for name, data in outputs.items():
        (args.out / name).write_bytes(data)
        print(f"wrote {name}: SHA256 {hashlib.sha256(data).hexdigest()}")


if __name__ == "__main__":
    main()
