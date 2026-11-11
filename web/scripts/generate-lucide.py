#!/usr/bin/env python3
"""Convert the pinned local SVG subset to inert DOM node data; no network or runtime XML parser."""
import json
import pathlib
import sys
import xml.etree.ElementTree as ET

vendor = pathlib.Path(sys.argv[1]) if len(sys.argv) > 1 else pathlib.Path(__file__).resolve().parent.parent / 'vendor' / 'lucide'
allowed = {
    'path': {'d'}, 'circle': {'cx', 'cy', 'r', 'fill'}, 'ellipse': {'cx', 'cy', 'rx', 'ry'},
    'rect': {'x', 'y', 'width', 'height', 'rx', 'ry'},
    'line': {'x1', 'y1', 'x2', 'y2'}, 'polyline': {'points'}, 'polygon': {'points'},
}
# A filled dot (tag, palette) is the only paint an icon may carry, and only in the inherited colour.
allowed_values = {'fill': {'currentColor'}}
nodes = {}
for source in sorted((vendor / 'icons').glob('*.svg')):
    root = ET.fromstring(source.read_bytes())
    if root.tag != '{http://www.w3.org/2000/svg}svg' or root.attrib.get('viewBox') != '0 0 24 24':
        raise ValueError(f'Unexpected SVG root: {source.name}')
    children = []
    for child in root:
        tag = child.tag.removeprefix('{http://www.w3.org/2000/svg}')
        if tag not in allowed or set(child.attrib) - allowed[tag] or len(child) or (child.text or '').strip():
            raise ValueError(f'Unexpected SVG content: {source.name}')
        for key, values in allowed_values.items():
            if key in child.attrib and child.attrib[key] not in values:
                raise ValueError(f'Unexpected SVG {key} value in {source.name}: {child.attrib[key]!r}')
        children.append([tag, child.attrib])
    nodes[source.stem] = children
license_text = (vendor / 'LICENSE').read_text()
if '*/' in license_text:
    raise ValueError('Unexpected license comment delimiter')
(vendor / 'nodes.ts').write_text('/*!\n' + license_text + '\n*/\n// Generated from pinned icons by web/scripts/generate-lucide.py. Do not edit geometry.\nexport const iconNodes = ' + json.dumps(nodes, indent=2) + ' as const;\n')
