# SPDX-License-Identifier: MIT OR Apache-2.0
"""Independent Ghostscript encoders; run: python3 generate_ghostscript.py OUTPUT."""
import datetime
import hashlib
import json
from pathlib import Path
import platform
import subprocess
import sys

out = Path(sys.argv[1]).resolve()
out.mkdir(parents=True, exist_ok=True)
payload = b''.join(hashlib.sha256(f'ERPForAll original filter payload {n}'.encode()).digest() for n in range(128))
(out / 'payload.bin').write_bytes(payload)
commands = []
for name, codec, params in [
    ('gs-lzw-early-0', 'LZWEncode', '/EarlyChange 0'),
    ('gs-lzw-early-1', 'LZWEncode', '/EarlyChange 1'),
    ('gs-flate-png', 'FlateEncode', '/Predictor 15 /Columns 64 /Colors 1 /BitsPerComponent 8'),
]:
    ps = f'''/src ({out / 'payload.bin'}) (r) file def
/dst ({out / (name + '.bin')}) (w) file << {params} >> /{codec} filter def
src 4096 string readstring pop dst exch writestring dst closefile src closefile
quit
'''
    script = out / f'{name}.ps'
    script.write_text(ps)
    command = ['gs', '-q', '-dNODISPLAY', '-dNOSAFER', '-dBATCH', str(script)]
    result = subprocess.run(command, check=True, capture_output=True, text=True)
    commands.append({'argv': command, 'stdout': result.stdout, 'stderr': result.stderr})
version = subprocess.run(['gs', '--version'], check=True, capture_output=True, text=True)
(out / 'ghostscript-provenance.json').write_text(json.dumps({
    'utc': datetime.datetime.now(datetime.timezone.utc).isoformat(), 'host': platform.node(),
    'python': sys.version, 'producer': version.stdout, 'commands': commands,
    'sha256': {p.name: hashlib.sha256(p.read_bytes()).hexdigest() for p in sorted(out.iterdir()) if p.suffix in ('.bin', '.ps')}
}, indent=2) + '\n')
