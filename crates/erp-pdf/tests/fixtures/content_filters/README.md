<!-- SPDX-License-Identifier: MIT OR Apache-2.0 -->
# Independent stream producers

`payload.bin` is an original deterministic 4096-byte binary payload made from SHA-256
blocks. The generators contain the source recipe; none implements LZW or a predictor.
The Rust tests read checked-in bytes and invoke no external programs.

`generate.py` invokes libtiff's `raw2tiff -M` with a single 64-by-64 strip, then extracts
the strip through TIFF directory entries. Predictor 1 and TIFF predictor 2 use
8-bit single-component samples. The source TIFF containers are retained for independent
inspection. The generator requires explicit ClassicTIFF byte order and validates
FillOrder 1 (MSB-first), because PDF LZW code bits use that order. The initial
producer used raw2tiff's default FillOrder 2 and the extracted strips were invalid
PDF LZW inputs; the corrected producer requests and checks FillOrder 1 instead.
`provenance.json` records the actual producer, commands, host, time and hashes.

`generate_ghostscript.py` invokes Ghostscript's LZW encoder with EarlyChange 0 and 1,
and its Flate encoder with adaptive PNG prediction (15). It retains the PostScript
producer inputs. The original payload is large and varied enough to exercise LZW
code-width growth; the two mode outputs differ. `ghostscript-provenance.json` records
the actual producer run. This is fixture-generation evidence, not evidence that the
ERPForAll tests passed.

Regenerate on an authorized build host, with the respective installed tool on PATH:

```sh
python3 generate.py /tmp/erp-pdf-filters-independent
python3 generate_ghostscript.py /tmp/erp-pdf-filters-gs
```

Copy outputs back into this directory together with their provenance. The two scripts
use the same source payload. No downloaded or reference-product data is included.

Coverage limits: the independent TIFF fixture covers only 8-bit, single-component
horizontal prediction. Other TIFF widths/components and explicit PNG filter tags are
covered by synthetic vectors in `content_filters.rs`, whose encoders resemble the
inverse transformations; those vectors are not independent producer evidence. The
adaptive PNG fixture does not establish that every possible row tag occurs. Consumer
coverage and strict completion/admission coverage are separate from full PDF format
conformance and from parser-internal stream decoding.
