#!/usr/bin/env python3
"""Crate layer check ( "Layer check" / design note §2 rule for addons).

Rules enforced today (a later release):

1. An addon crate (a workspace package under ``addons/`` or named ``erp-addon-*``) depends on
   framework crates through an allowlist: ``erp-sdk`` in any section, ``erp-testkit`` in
   ``[dev-dependencies]`` only, and other addon crates (``erp-addon-*``, an addon's ``depends``).
   Any other ``erp-*`` crate or workspace member is rejected by name, so a new core crate is
   closed to addons until this list opens it. Third-party crates are not checked here
   (``cargo deny`` covers them).
2. Every ``addons/<name>/Cargo.toml`` is a workspace member (listed explicitly in the root
   ``Cargo.toml`` workspace conventions).
3. Framework crates of the crate map (``LAYERS`` below) depend on each other only
   downward: a ``[dependencies]`` or ``[build-dependencies]`` edge between two of them goes to a
   strictly lower layer, or is a listed same-layer edge. a later release added
   ``erp-security`` at L6. Dev-dependencies and crates outside the map (``erp-testkit``,
   ``erp-test-models``, ``erp-bench``, addon crates) are not checked by this rule.
4. ``erp-html`` (L1) depends on no other workspace crate in any section,
   dev-dependencies included; the only same-layer edge involving it is ``erp-fields`` ->
   ``erp-html``.
5. ``erp-store-wire`` (L1 amendment 2026-11-06) is pure like ``erp-html`` (rule 4) and only
   ``erp-orm`` may depend on it in ``[dependencies]`` or ``[build-dependencies]``.
6. ``erp-blob`` (L5) has no workspace dependency in any section; only ``erp-sdk``,
   ``erp-cli`` and ``erp-server`` may depend on it in ``[dependencies]`` or ``[build-dependencies]``.

Reads ``cargo metadata --no-deps`` (or ``--metadata FILE`` for tests). Exit status 1 and
one line per violation when a rule fails.

Usage: python3 -I scripts/check-layers.py [--root DIR] [--metadata FILE]
"""

import argparse
import json
from pathlib import Path
import subprocess
import sys

# framework crate -> dependency kinds an addon may use it with ("normal" = [dependencies]);
# every other erp-* crate or workspace member is refused (addon crates aside)
ALLOWED_FOR_ADDONS = {
    "erp-sdk": {"normal", "build", "dev"},
    "erp-testkit": {"dev"},
}
SECTIONS = {"normal": "dependencies", "build": "build-dependencies", "dev": "dev-dependencies"}

# crate map: framework crate -> layer (L0 bottom). Rule 3 refuses a normal or build edge
# that does not go strictly downward, except the same-layer edges listed below.
LAYERS = {
    "erp-macros": 0,
    "erp-fields": 1,
    "erp-expr": 1,
    "erp-guest": 1,
    "erp-html": 1,
    "erp-store-wire": 1,
    "erp-registry": 2,
    "erp-domain": 3,
    "erp-sql": 4,
    "erp-orm": 5,
    "erp-blob": 5,
    "erp-security": 6,
    "erp-views": 7,
    "erp-data": 7,
    "erp-script": 7,
    "erp-rpc": 7,
    "erp-ui-compiler": 7,
    "erp-wasm-host": 7,
    "erp-modules": 8,
    "erp-server": 9,
    "erp-sdk": 10,
    "erp-cli": 12,
    "erp-ui-compiler-cli": 12,
}
SAME_LAYER_EDGES = {
    ("erp-data", "erp-views"),
    ("erp-fields", "erp-html"),  # field conversion calls the sanitiser, never the reverse
    ("erp-ui-compiler", "erp-views"),
}
# Framework crates that depend on no other workspace crate in any section (erp-html is
# pure and guest-compatible). Rule 4 refuses every internal edge, whatever the layer.
NO_INTERNAL_DEPENDENCIES = {
    "erp-html": "",
    "erp-store-wire": " amendment 2026-11-06",
    "erp-blob": "",
}
# Framework crates that only the listed crates may depend on in [dependencies] or
# [build-dependencies] (amendment 2026-11-06: erp-store-wire is the messaging wire contract;
# erp-orm reads the snapshot descriptor, nothing else uses it yet).
# : erp-blob (L5, no workspace dependency) is used by the SDK facade that re-exports
# it to the base addon and by the composition roots that open the stores.
RESTRICTED_DEPENDENTS = {
    "erp-store-wire": {"erp-orm"},
    "erp-blob": {"erp-sdk", "erp-cli", "erp-server"},
}


def load_metadata(root, metadata_file):
    if metadata_file is not None:
        return json.loads(Path(metadata_file).read_text(encoding="utf-8"))
    result = subprocess.run(
        ["cargo", "metadata", "--format-version", "1", "--no-deps", "--locked"],
        cwd=root, check=True, text=True, capture_output=True,
    )
    return json.loads(result.stdout)


def is_addon(package, root):
    manifest = Path(package["manifest_path"]).resolve()
    under_addons = (root / "addons").resolve() in manifest.parents
    return under_addons or package["name"].startswith("erp-addon-")


def check(metadata, root):
    """Return the violations as sorted strings."""
    root = Path(root).resolve()
    errors = []
    members = set(metadata.get("workspace_members", []))
    packages = [p for p in metadata["packages"] if p["id"] in members]
    addon_names = {p["name"] for p in packages if is_addon(p, root)}
    workspace_names = {p["name"] for p in packages}
    for package in packages:
        if package["name"] not in addon_names:
            continue
        for dep in package.get("dependencies", []):
            name = dep["name"]
            kind = dep.get("kind") or "normal"
            if kind not in SECTIONS:
                errors.append(
                    f"addon crate {package['name']} depends on {name} with unknown "
                    f"dependency kind {kind!r}"
                )
                continue
            if name in addon_names or name.startswith("erp-addon-"):
                continue
            # lead-approved existing HookImpl seam, scoped to this native addon.
            # Do not open erp-modules globally or permit a dev/build/path variant of this edge.
            if (package["name"] == "erp-addon-uom" and name == "erp-modules"
                    and kind == "normal"
                    and Path(package["manifest_path"]).resolve().parent == root / "addons" / "uom"):
                continue
            allowed = ALLOWED_FOR_ADDONS.get(name)
            if allowed is None and not (name.startswith("erp-") or name in workspace_names):
                continue  # third-party crate
            if allowed is None or kind not in allowed:
                errors.append(
                    f"addon crate {package['name']} depends on {name} in "
                    f"[{SECTIONS[kind]}]: addons use erp-sdk, other addons, and erp-testkit "
                    f"as a dev-dependency only"
                )
    for package in packages:
        layer = LAYERS.get(package["name"])
        if layer is None:
            continue
        for dep in package.get("dependencies", []):
            name = dep["name"]
            kind = dep.get("kind") or "normal"
            target = LAYERS.get(name)
            if kind == "dev" or target is None:
                continue
            if target < layer or (package["name"], name) in SAME_LAYER_EDGES:
                continue
            errors.append(
                f"{package['name']} (L{layer}) depends on {name} (L{target}) in "
                f"[{SECTIONS.get(kind, kind)}]: framework crates depend only on lower layers "
                f""
            )
    for package in packages:
        if package["name"] not in NO_INTERNAL_DEPENDENCIES:
            continue
        for dep in package.get("dependencies", []):
            if dep["name"] in workspace_names:
                kind = dep.get("kind") or "normal"
                errors.append(
                    f"{package['name']} depends on workspace crate {dep['name']} in "
                    f"[{SECTIONS.get(kind, kind)}]: it must have no internal dependencies "
                    f"({NO_INTERNAL_DEPENDENCIES[package['name']]})"
                )
    for package in packages:
        for dep in package.get("dependencies", []):
            allowed = RESTRICTED_DEPENDENTS.get(dep["name"])
            kind = dep.get("kind") or "normal"
            if allowed is None or kind == "dev" or package["name"] in allowed:
                continue
            errors.append(
                f"{package['name']} depends on {dep['name']} in [{SECTIONS.get(kind, kind)}]: "
                f"only {', '.join(sorted(allowed))} may"
            )
    member_manifests = {Path(p["manifest_path"]).resolve() for p in packages}
    addons_dir = root / "addons"
    if addons_dir.is_dir():
        for manifest in sorted(addons_dir.glob("*/Cargo.toml")):
            if manifest.resolve() not in member_manifests:
                errors.append(
                    f"{manifest.relative_to(root)} is not a workspace member: list "
                    f"\"{manifest.parent.relative_to(root)}\" in the root Cargo.toml members"
                )
    return sorted(errors)


def main(argv):
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--root", default=Path(__file__).resolve().parents[1], type=Path)
    parser.add_argument("--metadata", default=None)
    args = parser.parse_args(argv)
    errors = check(load_metadata(args.root, args.metadata), args.root)
    for error in errors:
        print(f"check-layers: {error}", file=sys.stderr)
    if errors:
        return 1
    print("check-layers: ok")
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
