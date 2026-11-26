<!-- SPDX-License-Identifier: MIT OR Apache-2.0 -->
# Report locale resources

These derived data files are licensed under **Unicode-3.0**, not the project's
source-code licence. The unmodified upstream notice is preserved in [LICENSE](LICENSE).

Source: [unicode-org/cldr-json 47.0.0](https://github.com/unicode-org/cldr-json/tree/47.0.0).
The extractor pins and verifies the SHA256 of every input before writing outputs.

Regenerate from the repository root (Python standard library only):

```sh
python3 -I scripts/report-locales.py
```

For offline regeneration, pass `--source-dir /path/to/cldr-json-checkout` containing
the exact upstream paths below. `--out` selects an alternative output directory.

## Extraction contract

- Gregorian `format` names: abbreviated/wide months in calendar order and
  abbreviated/wide weekdays in Sunday-first order. No stand-alone names are used.
- Day periods are the locale's abbreviated `am` and `pm` entries.
- Duration year/month/week/day/hour/minute/second patterns retain `{0}` and
  all original Unicode spacing in long, short and narrow widths.
- Missing `one` patterns explicitly reuse the same locale's `other` pattern.
  There is no fallback to English or to another locale.
- `plural_rule: one` selects `one` only for positive integer 1; `other` always
  selects `other`. These selectors are restricted to positive integer duration
  components, not a general CLDR plural engine. English and French use `one`;
  Indonesian uses `other`. French zero, decimals and compact exponents are out of scope.
- French has a cardinal `many` category, but the selected duration units have
  no explicit `many` patterns in this release. CLDR's category fallback uses
  `other`. Extraction refuses an explicit `many` pattern differing from `other`.

## Input provenance

Paths are relative to the pinned source repository. Digests cover downloaded
bytes, including the unmodified license and supplemental plural rules.

| Source | SHA256 |
| --- | --- |
| [LICENSE](https://raw.githubusercontent.com/unicode-org/cldr-json/47.0.0/LICENSE) | `220ba0e1c43b99530d2d5bdb892a99dca0989414f51ab695ecd90163eaa1ec3b` |
| [cldr-json/cldr-core/supplemental/plurals.json](https://raw.githubusercontent.com/unicode-org/cldr-json/47.0.0/cldr-json/cldr-core/supplemental/plurals.json) | `83f10932abb4ce2c4681725c6d11e4c01097a758830677127142c80c79bd6322` |
| [cldr-json/cldr-dates-full/main/en/ca-gregorian.json](https://raw.githubusercontent.com/unicode-org/cldr-json/47.0.0/cldr-json/cldr-dates-full/main/en/ca-gregorian.json) | `0d0eaf3fa57a60e247de08636fae9071349415b16bd910e18cabaa081d96ab12` |
| [cldr-json/cldr-units-full/main/en/units.json](https://raw.githubusercontent.com/unicode-org/cldr-json/47.0.0/cldr-json/cldr-units-full/main/en/units.json) | `334e097aaf5ed98fdef711fc1ab676b1cc1a131936c124ad0e699171def39c89` |
| [cldr-json/cldr-dates-full/main/fr/ca-gregorian.json](https://raw.githubusercontent.com/unicode-org/cldr-json/47.0.0/cldr-json/cldr-dates-full/main/fr/ca-gregorian.json) | `ff2f88f3ed7c06cca43845e170776d22a2cc1fb952cdd236a07da5250d47260b` |
| [cldr-json/cldr-units-full/main/fr/units.json](https://raw.githubusercontent.com/unicode-org/cldr-json/47.0.0/cldr-json/cldr-units-full/main/fr/units.json) | `f68909529c9b7d4d2ecae2bae799a9bbebba6c12a0d1709864fa7fb809e888f9` |
| [cldr-json/cldr-dates-full/main/id/ca-gregorian.json](https://raw.githubusercontent.com/unicode-org/cldr-json/47.0.0/cldr-json/cldr-dates-full/main/id/ca-gregorian.json) | `6f58a418beae6d309dc331635c492b7958c6eb1ee5feb06637b56883f8c3a8b2` |
| [cldr-json/cldr-units-full/main/id/units.json](https://raw.githubusercontent.com/unicode-org/cldr-json/47.0.0/cldr-json/cldr-units-full/main/id/units.json) | `57873fd751f9acfd38c4683f26e3cd03c518ad252a91eed7fa3c8171a09af3bd` |
