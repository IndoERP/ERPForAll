/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
import { mkdtempSync, mkdirSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import { afterEach, expect, test, vi } from "vitest";
import { parseUpstream } from "../scripts/upstream-toml.mjs";
import { checkVendorAdvisories } from "../scripts/check-vendor-advisories.mjs";
import { runAdvisoryCheck } from "../scripts/check-vendor.mjs";

const COMMIT = "0123456789abcdef0123456789abcdef01234567";
const sha = "e70c5589a4d1ad7e1a1b89c6d6a1c3b8a182e5403b8d074cc5e982eac6a938bf";
const manifest = (npmPackage = "example-lib", tag = "v1.2.3") =>
  `repository = "https://github.com/example/lib"\ntag = "${tag}"\ncommit = "${COMMIT}"\nlicense = "MIT"\npatches = []\nnpm_package = "${npmPackage}"\n\n[[files]]\npath = "src/lib.js"\nupstream = "src/lib.js"\nsha256 = "${sha}"\n`;

const validRegistry = (name: string, version: string) => ({ name, version });
const cleanOsv = () => ({});
const okFetch = (latest = "1.2.3", extraOsv: unknown = cleanOsv()) => vi.fn(async (input: string | URL | Request, init?: RequestInit) => {
  const url = new URL(String(input));
  if (url.hostname === "api.osv.dev") {
    const body = JSON.parse(String(init?.body ?? "{}"));
    return Response.json(body.package.name === "lodash" ? { vulns: [{ id: "GHSA-35jh-r3h4-6jhm" }] } : extraOsv);
  }
  const [, packageName, version] = url.pathname.match(/^\/([^/]+)\/([^/]+)$/) ?? [];
  const decodedName = decodeURIComponent(packageName ?? "");
  return Response.json(validRegistry(decodedName, version === "latest" ? latest : version ?? ""));
});

const fixtures: string[] = [];
afterEach(() => {
  vi.restoreAllMocks();
  for (const fixture of fixtures.splice(0)) rmSync(fixture, { recursive: true, force: true });
});

// UI-VENDOR-T01: manifest identity validation is tooling behavior, outside product parity cases.
test("UPSTREAM.toml requires and validates an explicit npm package and concrete tag version", () => {
  const parsed = parseUpstream(manifest(), "vendor/lib/UPSTREAM.toml") as unknown as { npm_package: string };
  expect(parsed.npm_package).toBe("example-lib");
  expect(() => parseUpstream(manifest().replace(/^npm_package = .*\n/m, ""), "vendor/lib/UPSTREAM.toml"))
    .toThrow(/npm_package/);
  expect(() => parseUpstream(manifest("Example Lib"), "vendor/lib/UPSTREAM.toml"))
    .toThrow(/npm package/i);
  expect(() => parseUpstream(manifest("example-lib", "latest"), "vendor/lib/UPSTREAM.toml"))
    .toThrow(/version/);
  expect(() => parseUpstream(manifest("example-lib", "1.2.3-01"), "vendor/lib/UPSTREAM.toml"))
    .toThrow(/version/);
});

// UI-VENDOR-T01: every discovered real tree has an explicitly verified npm package/version.
test("all real vendor trees declare the npm identity for their pinned release", () => {
  const identities: [string, string, string][] = [
    ["alien-signals", "alien-signals", "3.2.1"],
    ["dom-expressions", "dom-expressions", "0.40.12"],
    ["lucide", "lucide", "1.61.0"],
  ];
  for (const [tree, npmPackage, version] of identities) {
    const text = readFileSync(join(webVendorDir(), tree, "UPSTREAM.toml"), "utf8");
    const parsed = parseUpstream(text, `vendor/${tree}/UPSTREAM.toml`) as unknown as { npm_package: string; tag: string };
    expect(parsed.npm_package).toBe(npmPackage);
    expect(parsed.tag.replace(/^v/, "")).toBe(version);
  }
});

// UI-VENDOR-T02: clean OSV result requires the known-advisory sentinel.
test("checks exact registry and OSV identities and the service sentinel", async () => {
  const vendorDir = createVendorFixture("example-lib", "v1.2.3");
  const fetchImpl = okFetch("1.2.3");
  const lines: string[] = [];
  await checkVendorAdvisories({ vendorDir, fetchImpl, writeLine: (line: string) => lines.push(line) });

  const requests = fetchImpl.mock.calls;
  const registryUrls = requests.map(([url]) => new URL(String(url))).filter((url) => url.hostname === "registry.npmjs.org");
  expect(registryUrls.map((url) => url.pathname)).toEqual(["/example-lib/1.2.3", "/example-lib/latest"]);
  const osvBodies = requests.filter(([url]) => new URL(String(url)).hostname === "api.osv.dev")
    .map(([, init]) => JSON.parse(String(init?.body)));
  expect(osvBodies).toContainEqual({ package: { name: "example-lib", ecosystem: "npm" }, version: "1.2.3" });
  expect(osvBodies).toContainEqual({ package: { name: "lodash", ecosystem: "npm" }, version: "4.17.20" });
  expect(lines.join("\n")).toMatch(/example-lib.*1\.2\.3.*1\.2\.3/);
});

// UI-VENDOR-T02: a missing sentinel advisory prevents a clean result.
test("a failed OSV service self-test stops the check before inspecting vendor packages", async () => {
  const vendorDir = createVendorFixture("example-lib", "1.2.3");
  const fetchImpl = vi.fn(async () => Response.json({}));
  const lines: string[] = [];
  await expect(checkVendorAdvisories({ vendorDir, fetchImpl, writeLine: (line: string) => lines.push(line) }))
    .rejects.toThrow(/OSV self-test.*did not return GHSA-35jh-r3h4-6jhm/);
  expect(fetchImpl).toHaveBeenCalledOnce();
  expect(lines.join("\n")).not.toContain("advisory lookups complete");
});

// UI-VENDOR-T05: release drift is reported without changing a vendor pin.
test("reports latest-version drift without failing or changing the pin", async () => {
  const vendorDir = createVendorFixture("example-lib", "v1.2.3");
  const fetchImpl = okFetch("1.3.0");
  const lines: string[] = [];
  await checkVendorAdvisories({ vendorDir, fetchImpl, writeLine: (line: string) => lines.push(line) });
  expect(lines.join("\n")).toMatch(/example-lib.*1\.2\.3.*1\.3\.0.*drift/i);
  expect((parseUpstream(readFileSync(join(vendorDir, "example-lib", "UPSTREAM.toml"), "utf8"), "manifest") as unknown as { tag: string }).tag)
    .toBe("v1.2.3");
});

// UI-VENDOR-T01: encoded scoped package names retain the exact npm identity in URLs and OSV queries.
test("supports scoped npm package names without changing their identity", async () => {
  const vendorDir = createVendorFixture("@example/signal-core", "1.2.3");
  const fetchImpl = okFetch("1.2.3");
  await checkVendorAdvisories({ vendorDir, fetchImpl });
  const requests = fetchImpl.mock.calls;
  expect(requests.map(([url]) => new URL(String(url)).pathname)).toContain("/%40example%2Fsignal-core/1.2.3");
  const osvBodies = requests.filter(([url]) => new URL(String(url)).hostname === "api.osv.dev")
    .map(([, init]) => JSON.parse(String(init?.body)));
  expect(osvBodies).toContainEqual({ package: { name: "@example/signal-core", ecosystem: "npm" }, version: "1.2.3" });
});

// UI-VENDOR-T03: pinned package advisories block the check.
test("a package advisory fails with package, pinned version, and advisory ID", async () => {
  const vendorDir = createVendorFixture("example-lib", "1.2.3");
  await expect(checkVendorAdvisories({
    vendorDir,
    fetchImpl: okFetch("1.2.3", { vulns: [{ id: "GHSA-example-1234-5678" }] }),
  })).rejects.toThrow(/example-lib.*1\.2\.3.*GHSA-example-1234-5678/);
});

// UI-VENDOR-T04: bad identity and absent publication are incomplete checks.
test("registry identity/version mismatch and missing packages fail closed", async () => {
  const vendorDir = createVendorFixture("example-lib", "1.2.3");
  const wrongIdentity = vi.fn(async (input: string | URL | Request) => {
    const url = new URL(String(input));
    if (url.hostname === "api.osv.dev") return Response.json({ vulns: [{ id: "GHSA-35jh-r3h4-6jhm" }] });
    return Response.json({ name: "other", version: "1.2.3" });
  });
  await expect(checkVendorAdvisories({ vendorDir, fetchImpl: wrongIdentity }))
    .rejects.toThrow(/registry.*expected example-lib@1\.2\.3/i);
  const missing = vi.fn(async () => new Response("not found", { status: 404 }));
  await expect(checkVendorAdvisories({ vendorDir, fetchImpl: missing })).rejects.toThrow(/HTTP 404/);
});

// UI-VENDOR-T04: all incomplete OSV responses fail closed.
test.each([
  ["HTTP failure", async () => new Response("", { status: 503 })],
  ["redirect", async () => new Response(null, { status: 302, headers: { location: "https://elsewhere.invalid" } })],
  ["invalid JSON", async () => new Response("<html>maintenance</html>")],
  ["oversized response", async () => new Response("x".repeat(2 * 1024 * 1024 + 1))],
  ["service error envelope", async () => Response.json({ error: "database unavailable" })],
  ["pagination", async () => Response.json({ next_page_token: "more" })],
  ["invalid advisory ID", async () => Response.json({ vulns: [{ id: "<script>alert(1)</script>" }] })],
])("%s cannot be reported as a clean lookup", async (_name, response) => {
  const vendorDir = createVendorFixture("example-lib", "1.2.3");
  const fetchImpl = vi.fn(async (input: string | URL | Request) => {
    const url = new URL(String(input));
    if (url.hostname === "api.osv.dev" && url.pathname === "/v1/query") return response();
    const [, name, version] = url.pathname.match(/^\/([^/]+)\/([^/]+)$/) ?? [];
    return Response.json(validRegistry(decodeURIComponent(name ?? ""), version === "latest" ? "1.2.3" : version ?? ""));
  });
  await expect(checkVendorAdvisories({ vendorDir, fetchImpl })).rejects.toThrow();
});

// UI-VENDOR-T04: an unknown OSV response envelope is not a clean result.
test("rejects an unexpected OSV response envelope before querying package versions", async () => {
  const vendorDir = createVendorFixture("example-lib", "1.2.3");
  const fetchImpl = vi.fn(async (input: string | URL | Request) => {
    const url = new URL(String(input));
    if (url.hostname === "api.osv.dev") return Response.json({ message: "temporarily unavailable" });
    const [, name, version] = url.pathname.match(/^\/([^/]+)\/([^/]+)$/) ?? [];
    return Response.json(validRegistry(decodeURIComponent(name ?? ""), version === "latest" ? "1.2.3" : version ?? ""));
  });
  await expect(checkVendorAdvisories({ vendorDir, fetchImpl }))
    .rejects.toThrow(/OSV .* unexpected response field 'message'/);
  expect(fetchImpl).toHaveBeenCalledOnce();
});

// UI-VENDOR-T04: timeout while waiting on a response body is an incomplete lookup.
test("a stalled response body times out instead of hanging or passing", async () => {
  const vendorDir = createVendorFixture("example-lib", "1.2.3");
  const fetchImpl = vi.fn(async (input: string | URL | Request) => {
    const url = new URL(String(input));
    if (url.hostname === "api.osv.dev") {
      return new Response(new ReadableStream({ start() { /* deliberately never enqueue */ } }), {
        headers: { "content-type": "application/json" },
      });
    }
    return Response.json(validRegistry("example-lib", "1.2.3"));
  });
  await expect(checkVendorAdvisories({ vendorDir, fetchImpl, requestTimeoutMs: 20 }))
    .rejects.toThrow(/timed out/);
});

// UI-VENDOR-T06: the regular vendor path enforces network lookups.
test("the default vendor path invokes advisory checks and propagates failures", async () => {
  const check = vi.fn(async () => { throw new Error("advisory lookup failed"); });
  await expect(runAdvisoryCheck({ source: null }, check)).rejects.toThrow("advisory lookup failed");
  expect(check).toHaveBeenCalledOnce();
  expect(check).toHaveBeenCalledWith({ vendorDir: undefined });

  const offline = vi.fn();
  const lines: string[] = [];
  await runAdvisoryCheck({ source: "/fixture/source" }, offline, (line: string) => lines.push(line));
  expect(offline).not.toHaveBeenCalled();
  expect(lines.join("\n")).toContain("lookups not run");
  expect(lines.join("\n")).not.toContain("advisory lookups complete");
});

function createVendorFixture(npmPackage: string, tag: string): string {
  const root = mkdtempSync(join(tmpdir(), "vendor-advisory-test-"));
  fixtures.push(root);
  const tree = join(root, "example-lib");
  mkdirSync(tree);
  writeFileSync(join(tree, "UPSTREAM.toml"), manifest(npmPackage, tag));
  return root;
}

function webVendorDir(): string {
  return join(dirname(fileURLToPath(import.meta.url)), "../vendor");
}
