#!/usr/bin/env python3
"""Build Ace's self-contained external CSS from the exact admitted npm distribution.

This downloads data, verifies its pinned SHA-512 and reads named tar members; it never
extracts a tree or executes downloaded code. Run on an approved build host with Python 3.
The original CSS is retained verbatim; its referenced PNG/SVG bytes are embedded into
the served derivative because the client stylesheet composer does not copy sidecars.
"""
from __future__ import annotations

import base64
import datetime
import hashlib
import io
import json
from pathlib import Path
import platform
import re
import sys
import tarfile
import urllib.request

VERSION = "1.45.0"
URL = f"https://registry.npmjs.org/ace-builds/-/ace-builds-{VERSION}.tgz"
INTEGRITY = "sha512-tAAorvbQRlsdDeuf4RnBLgaRk4HVT9td2D4QKEqXyuQkZmFoqHxl9JuV2o1XkuDT+zOreooGPiG1tU/Y+pNFkg=="
ROOT = Path(__file__).resolve().parents[2]
DEST = ROOT / "addons/web/static/src/client/vendor"


def main() -> None:
    with urllib.request.urlopen(URL, timeout=60) as response:
        payload = response.read(32 * 1024 * 1024 + 1)
    if len(payload) > 32 * 1024 * 1024:
        raise ValueError("Ace archive exceeds 32 MiB admission limit")
    actual = "sha512-" + base64.b64encode(hashlib.sha512(payload).digest()).decode("ascii")
    if actual != INTEGRITY:
        raise ValueError("Ace archive integrity does not match the pinned distribution")
    names = {"package/css/ace.css": "ace.upstream.css", "package/LICENSE": "ace.LICENSE"}
    files: dict[str, bytes] = {}
    image_inputs: dict[str, dict[str, str]] = {}
    with tarfile.open(fileobj=io.BytesIO(payload), mode="r:gz") as archive:
        metadata_member = archive.extractfile("package/package.json")
        if metadata_member is None:
            raise ValueError("Ace package metadata is missing")
        metadata = json.load(metadata_member)
        if metadata["name"] != "ace-builds" or metadata["version"] != VERSION or metadata["license"] != "BSD-3-Clause":
            raise ValueError("Ace package identity/licence differs from the admitted distribution")
        for source, target in names.items():
            member = archive.getmember(source)
            if not member.isfile() or member.size > 1024 * 1024:
                raise ValueError(f"Unexpected Ace archive member: {source}")
            stream = archive.extractfile(member)
            if stream is None:
                raise ValueError(f"Ace archive member is missing: {source}")
            files[target] = stream.read()
        source_css = files["ace.upstream.css"]
        image_urls = re.compile(rb'url\("(\./main-[0-9]+\.(png|svg))"\)')
        distinct_urls = {match.group(1) for match in image_urls.finditer(source_css)}
        if not distinct_urls or len(distinct_urls) > 64:
            raise ValueError("Ace CSS must reference between 1 and 64 admitted image assets")

        def embedded_image(match: re.Match[bytes]) -> bytes:
            relative = match.group(1).decode("ascii")
            source = "package/css/" + relative.removeprefix("./")
            member = archive.getmember(source)
            if not member.isfile() or member.size > 1024 * 1024:
                raise ValueError(f"Unexpected Ace image member: {source}")
            stream = archive.extractfile(member)
            if stream is None:
                raise ValueError(f"Ace image member is missing: {source}")
            data = stream.read()
            mime = "image/png" if match.group(2) == b"png" else "image/svg+xml"
            image_inputs[relative] = {"source": source, "mime": mime, "sha256": hashlib.sha256(data).hexdigest()}
            return b'url("data:' + mime.encode("ascii") + b";base64," + base64.b64encode(data) + b'")'

        files["ace.css"] = image_urls.sub(embedded_image, source_css)
        for url in re.findall(rb"url\(([^)]*)\)", files["ace.css"]):
            if not url.strip(b"\"' ").startswith((b"data:", b"#")):
                raise ValueError("Generated Ace CSS retains an unresolved asset URL")
    DEST.mkdir(parents=True, exist_ok=True)
    for target, data in files.items():
        (DEST / target).write_bytes(data)
    # client.json's stylesheet assembly retains this comment verbatim in the shipped CSS,
    # so the binary distribution carries the notice even without source-tree licence files.
    license_css = b"/*\n" + files["ace.LICENSE"].replace(b"*/", b"* /") + b"\n*/\n"
    (DEST / "ace-license.css").write_bytes(license_css)
    provenance = {
        "package": "ace-builds", "version": VERSION, "license": "BSD-3-Clause",
        "url": URL, "integrity": actual,
        "command": "python3 web/scripts/update-code-editor-vendor.py",
        "host": platform.node(), "utc": datetime.datetime.now(datetime.timezone.utc).isoformat(),
        "python": sys.version, "script_sha256": hashlib.sha256(Path(__file__).read_bytes()).hexdigest(),
        "files": {target: {"source": source, "sha256": hashlib.sha256(files[target]).hexdigest()} for source, target in names.items()},
        "image_inputs": image_inputs,
    }
    provenance["files"]["ace.css"] = {"source": "derived from package/css/ace.css with pinned PNG/SVG data URLs", "sha256": hashlib.sha256(files["ace.css"]).hexdigest()}
    provenance["files"]["ace-license.css"] = {"source": "generated notice from package/LICENSE", "sha256": hashlib.sha256(license_css).hexdigest()}
    text = json.dumps(provenance, indent=2) + "\n"
    (DEST / "ace.PROVENANCE.json").write_text(text)
    print(text, end="")


if __name__ == "__main__":
    main()
