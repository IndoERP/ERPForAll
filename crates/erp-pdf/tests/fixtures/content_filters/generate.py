# SPDX-License-Identifier: MIT OR Apache-2.0
"""Run on an authorized fixture host: python3 generate.py OUTPUT_DIRECTORY.

libtiff performs encoding; this script only extracts the single TIFF strip.
"""
import datetime
import hashlib
import json
from pathlib import Path
import platform
import struct
import subprocess
import sys

out = Path(sys.argv[1]).resolve()
out.mkdir(parents=True, exist_ok=True)
payload = b''.join(hashlib.sha256(f'ERPForAll original filter payload {n}'.encode()).digest()
                   for n in range(128))
(out / 'payload.bin').write_bytes(payload)
commands = []
for predictor in (1, 2):
    name = f'libtiff-lzw-predictor-{predictor}'
    command = ['raw2tiff', '-M', '-w', '64', '-l', '64', '-r', '64', '-c',
               f'lzw:{predictor}', str(out / 'payload.bin'), str(out / f'{name}.tif')]
    result = subprocess.run(command, check=True, capture_output=True, text=True)
    commands.append({'argv': command, 'stdout': result.stdout, 'stderr': result.stderr})
    data = (out / f'{name}.tif').read_bytes()
    if data[:4] == b'II\x2a\x00':
        endian = '<'
    elif data[:4] == b'MM\x00\x2a':
        endian = '>'
    else:
        raise ValueError('producer output must have an explicit ClassicTIFF header')
    offset = struct.unpack_from(endian + 'I', data, 4)[0]
    count = struct.unpack_from(endian + 'H', data, offset)[0]
    tags = {}
    for index in range(count):
        entry = offset + 2 + 12 * index
        tag, kind, length = struct.unpack_from(endian + 'HHI', data, entry)
        if length == 1 and kind in (3, 4):
            tags[tag] = struct.unpack_from(endian + ('H' if kind == 3 else 'I'), data, entry + 8)[0]
    assert tags[256] == 64 and tags[257] == 64 and tags[259] == 5
    assert tags[266] == 1, 'PDF LZW requires MSB-first codes; TIFF FillOrder must be 1'
    assert tags[258] == 8 and tags[277] == 1 and tags[278] == 64
    assert tags[317] == predictor
    assert struct.unpack_from(endian + 'I', data, offset + 2 + 12 * count)[0] == 0
    commands[-1]['tiff_tags'] = tags
    commands[-1]['tiff_byte_order'] = endian
    strip = data[tags[273]:tags[273] + tags[279]]
    assert len(strip) == tags[279]
    (out / f'{name}.bin').write_bytes(strip)
version = subprocess.run(['raw2tiff', '-h'], capture_output=True, text=True)
provenance = {'utc': datetime.datetime.now(datetime.timezone.utc).isoformat(),
              'host': platform.node(), 'python': sys.version,
              'producer': version.stdout + version.stderr, 'commands': commands,
              'sha256': {p.name: hashlib.sha256(p.read_bytes()).hexdigest()
                         for p in sorted(out.iterdir()) if p.suffix in ('.bin', '.tif')}}
(out / 'provenance.json').write_text(json.dumps(provenance, indent=2) + '\n')
