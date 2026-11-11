/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
import { expect, test } from "vitest";
import { readFileSync } from "node:fs";
import { createHash } from "node:crypto";
import { createRequire } from "node:module";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const root = join(dirname(fileURLToPath(import.meta.url)), "../..");
const staticRoot = join(root, "addons/web/static");
const require = createRequire(join(staticRoot, "package.json"));
const read = (path: string): Buffer => readFileSync(path);
const hash = (value: Buffer): string => createHash("sha256").update(value).digest("hex");

test("code editor package and external CSS retain the exact admitted distribution and licence", () => {
  const addon = JSON.parse(read(join(staticRoot, "package.json")).toString()) as { dependencies: Record<string, string> };
  expect(addon.dependencies["ace-builds"]).toBe("1.45.0");
  const installedRoot = dirname(require.resolve("ace-builds/package.json"));
  const installed = JSON.parse(read(join(installedRoot, "package.json")).toString()) as { version: string; license: string };
  expect(installed.version).toBe("1.45.0"); expect(installed.license).toBe("BSD-3-Clause");
  const vendor = join(staticRoot, "src/client/vendor");
  const provenance = JSON.parse(read(join(vendor, "ace.PROVENANCE.json")).toString()) as { version: string; integrity: string; script_sha256: string; files: Record<string, { sha256: string }> };
  expect(provenance.version).toBe(installed.version);
  expect(provenance.integrity).toBe("sha512-tAAorvbQRlsdDeuf4RnBLgaRk4HVT9td2D4QKEqXyuQkZmFoqHxl9JuV2o1XkuDT+zOreooGPiG1tU/Y+pNFkg==");
  for (const [local, upstream] of [["ace.upstream.css", "css/ace.css"], ["ace.LICENSE", "LICENSE"]] as const) {
    const bytes = read(join(vendor, local));
    expect(bytes.equals(read(join(installedRoot, upstream)))).toBe(true);
    expect(hash(bytes)).toBe(provenance.files[local]!.sha256);
  }
  expect(hash(read(join(root, "web/scripts/update-code-editor-vendor.py")))).toBe(provenance.script_sha256);
  expect(hash(read(join(vendor, "ace.css")))).toBe(provenance.files["ace.css"]!.sha256);
  const notice = read(join(vendor, "ace-license.css")).toString();
  expect(notice).toContain(read(join(installedRoot, "LICENSE")).toString());
  expect(hash(read(join(vendor, "ace-license.css")))).toBe(provenance.files["ace-license.css"]!.sha256);
  const manifest = JSON.parse(read(join(staticRoot, "client.json")).toString()) as { styles: string[] };
  expect(manifest.styles).toContain("src/client/vendor/ace.css");
  expect(manifest.styles).toContain("src/client/vendor/ace-license.css");
  expect(manifest.styles).toContain("src/client/code-editor.css");
  expect(manifest.styles.indexOf("src/client/vendor/ace.css")).toBeLessThan(manifest.styles.indexOf("src/client/code-editor.css"));
});

test("code editor stylesheet composition packages every pinned image without relative runtime fetches", () => {
  const installedRoot = dirname(require.resolve("ace-builds/package.json"));
  const vendor = join(staticRoot, "src/client/vendor");
  const provenance = JSON.parse(read(join(vendor, "ace.PROVENANCE.json")).toString()) as { image_inputs: Record<string, { source: string; mime: string; sha256: string }> };
  const manifest = JSON.parse(read(join(staticRoot, "client.json")).toString()) as { styles: string[] };
  // build-human-ui.mjs concatenates these files verbatim; it does not resolve or copy CSS URLs.
  const shippedCss = manifest.styles.map(style => read(join(staticRoot, style)).toString()).join("\n");
  const runtimeUrls = [...shippedCss.matchAll(/url\(["']?([^"')]+)["']?\)/g)].map(match => match[1]!);
  expect(runtimeUrls.filter(url => !url.startsWith("data:") && !url.startsWith("#"))).toEqual([]);
  const upstreamCss = read(join(installedRoot, "css/ace.css")).toString();
  const sourceUrls = [...upstreamCss.matchAll(/url\("(\.\/main-\d+\.(png|svg))"\)/g)];
  expect(sourceUrls.length).toBeGreaterThan(0);
  for (const match of sourceUrls) {
    const relative = match[1]!, extension = match[2]!;
    const bytes = read(join(installedRoot, "css", relative));
    const mime = extension === "png" ? "image/png" : "image/svg+xml";
    expect(shippedCss).toContain(`url("data:${mime};base64,${bytes.toString("base64")}")`);
    expect(provenance.image_inputs[relative]).toEqual({ source: `package/css/${relative.slice(2)}`, mime, sha256: hash(bytes) });
  }
  expect(Object.keys(provenance.image_inputs).sort()).toEqual([...new Set(sourceUrls.map(match => match[1]!))].sort());
  const expected = upstreamCss.replace(/url\("(\.\/main-\d+\.(png|svg))"\)/g, (_match: string, relative: string, extension: string) => {
    const mime = extension === "png" ? "image/png" : "image/svg+xml";
    return `url("data:${mime};base64,${read(join(installedRoot, "css", relative)).toString("base64")}")`;
  });
  expect(read(join(vendor, "ace.css")).toString()).toBe(expected);
});
