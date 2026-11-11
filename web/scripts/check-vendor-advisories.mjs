/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
// Checks OSV advisories and npm release drift for every explicit npm identity in web/vendor/*.
// Package-level advisories are surfaced conservatively: this checker does not decide whether a
// finding is reachable in a partial vendor tree.

import { readdirSync, readFileSync, statSync } from "node:fs";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";
import { npmVersionFromTag, parseUpstream } from "./upstream-toml.mjs";

const here = dirname(fileURLToPath(import.meta.url));
const OSV_URL = "https://api.osv.dev/v1/query";
const NPM_REGISTRY = "https://registry.npmjs.org";
const SENTINEL = { name: "lodash", version: "4.17.20", advisory: "GHSA-35jh-r3h4-6jhm" };
const REQUEST_TIMEOUT_MS = 30_000;
const MAX_RESPONSE_BYTES = 2 * 1024 * 1024;
const SAFE_ADVISORY_ID = /^[A-Za-z0-9][A-Za-z0-9._-]{0,127}$/;

function packageUrl(name, version) {
  return `${NPM_REGISTRY}/${encodeURIComponent(name)}/${encodeURIComponent(version)}`;
}

function withTimeout(promise, signal, where) {
  if (signal.aborted) return Promise.reject(new Error(`${where}: request timed out`));
  let onAbort;
  const aborted = new Promise((_, reject) => {
    onAbort = () => reject(new Error(`${where}: request timed out`));
    signal.addEventListener("abort", onAbort, { once: true });
  });
  return Promise.race([promise, aborted]).finally(() => signal.removeEventListener("abort", onAbort));
}

async function readJson(url, init, fetchImpl, where, limits) {
  const signal = AbortSignal.timeout(limits.requestTimeoutMs);
  let response;
  try {
    response = await withTimeout(fetchImpl(url, {
      ...init,
      redirect: "error",
      signal,
    }), signal, where);
  } catch (error) {
    throw new Error(`${where}: request failed: ${error instanceof Error ? error.message : String(error)}`);
  }
  if (!response.ok) throw new Error(`${where}: HTTP ${response.status}`);
  if (!response.body) throw new Error(`${where}: response body is missing`);

  const reader = response.body.getReader();
  const chunks = [];
  let totalBytes = 0;
  try {
    for (;;) {
      const { done, value } = await withTimeout(reader.read(), signal, where);
      if (done) break;
      totalBytes += value.byteLength;
      if (totalBytes > limits.maxResponseBytes) {
        await withTimeout(reader.cancel(), signal, where);
        throw new Error(`${where}: response exceeds ${limits.maxResponseBytes} bytes`);
      }
      chunks.push(value);
    }
  } catch (error) {
    throw new Error(`${where}: response read failed: ${error instanceof Error ? error.message : String(error)}`);
  }

  let payload;
  try {
    payload = JSON.parse(Buffer.concat(chunks.map((chunk) => Buffer.from(chunk))).toString("utf8"));
  } catch {
    throw new Error(`${where}: response is not valid JSON`);
  }
  if (payload === null || typeof payload !== "object" || Array.isArray(payload)) {
    throw new Error(`${where}: response must be a JSON object`);
  }
  return payload;
}

function validateRegistryVersion(payload, name, version, where) {
  if (payload.error !== undefined) throw new Error(`${where}: registry returned an error`);
  if (payload.name !== name || payload.version !== version) {
    throw new Error(`${where}: registry identity mismatch; expected ${name}@${version}`);
  }
}

function validateAdvisories(payload, where) {
  if (payload.error !== undefined) throw new Error(`${where}: OSV returned an error`);
  const unexpectedKeys = Object.keys(payload).filter((key) => key !== "vulns" && key !== "next_page_token");
  if (unexpectedKeys.length > 0) {
    throw new Error(`${where}: unexpected response field '${unexpectedKeys.sort()[0]}'`);
  }
  if (payload.next_page_token !== undefined) {
    if (typeof payload.next_page_token !== "string") throw new Error(`${where}: invalid OSV pagination token`);
    if (payload.next_page_token !== "") throw new Error(`${where}: OSV response is paginated; lookup is incomplete`);
  }
  if (payload.vulns === undefined) return [];
  if (!Array.isArray(payload.vulns)) throw new Error(`${where}: OSV 'vulns' must be an array`);
  const ids = payload.vulns.map((vulnerability) => vulnerability?.id);
  if (ids.some((id) => typeof id !== "string" || !SAFE_ADVISORY_ID.test(id))) {
    throw new Error(`${where}: OSV response contains an invalid advisory ID`);
  }
  return [...new Set(ids)].sort();
}

async function queryAdvisories(name, version, fetchImpl, limits) {
  const where = `OSV ${name}@${version}`;
  const payload = await readJson(OSV_URL, {
    method: "POST",
    headers: { "content-type": "application/json" },
    body: JSON.stringify({ package: { name, ecosystem: "npm" }, version }),
  }, fetchImpl, where, limits);
  return validateAdvisories(payload, where);
}

async function verifyRegistry(name, version, fetchImpl, limits, label) {
  const where = `npm registry ${name}@${label}`;
  const payload = await readJson(packageUrl(name, label), { method: "GET" }, fetchImpl, where, limits);
  validateRegistryVersion(payload, name, version, where);
}

/**
 * Check all vendor manifests against npm and OSV.
 * @param {{vendorDir?: string, fetchImpl?: typeof fetch, writeLine?: (line: string) => void, requestTimeoutMs?: number, maxResponseBytes?: number}} options
 */
export async function checkVendorAdvisories(options = {}) {
  const vendorDir = resolve(options.vendorDir ?? join(here, "../vendor"));
  const fetchImpl = options.fetchImpl ?? fetch;
  const writeLine = options.writeLine ?? console.log;
  const limits = {
    requestTimeoutMs: options.requestTimeoutMs ?? REQUEST_TIMEOUT_MS,
    maxResponseBytes: options.maxResponseBytes ?? MAX_RESPONSE_BYTES,
  };
  if (!Number.isSafeInteger(limits.requestTimeoutMs) || limits.requestTimeoutMs <= 0) {
    throw new Error("vendor-advisories: request timeout must be a positive integer");
  }
  if (!Number.isSafeInteger(limits.maxResponseBytes) || limits.maxResponseBytes <= 0) {
    throw new Error("vendor-advisories: response size limit must be a positive integer");
  }

  const names = readdirSync(vendorDir).filter((name) => statSync(join(vendorDir, name)).isDirectory()).sort();
  if (names.length === 0) throw new Error(`vendor-advisories: no vendor trees under ${vendorDir}`);

  const sentinelIds = await queryAdvisories(SENTINEL.name, SENTINEL.version, fetchImpl, limits);
  if (!sentinelIds.includes(SENTINEL.advisory)) {
    throw new Error(`vendor-advisories: OSV self-test for ${SENTINEL.name}@${SENTINEL.version} did not return ${SENTINEL.advisory}`);
  }
  writeLine(`vendor-advisories: OSV lookup self-test ok (${SENTINEL.name}@${SENTINEL.version}: ${SENTINEL.advisory})`);

  for (const name of names) {
    const manifestName = `vendor/${name}/UPSTREAM.toml`;
    const manifest = parseUpstream(readFileSync(join(vendorDir, name, "UPSTREAM.toml"), "utf8"), manifestName);
    const version = npmVersionFromTag(manifest.tag, manifestName);
    await verifyRegistry(manifest.npm_package, version, fetchImpl, limits, version);
    const advisoryIds = await queryAdvisories(manifest.npm_package, version, fetchImpl, limits);
    if (advisoryIds.length > 0) {
      throw new Error(`vendor-advisories: ${manifest.npm_package}@${version} has open advisories: ${advisoryIds.join(", ")} (review applicability and re-vendor if affected)`);
    }
    const latest = await readJson(packageUrl(manifest.npm_package, "latest"), { method: "GET" }, fetchImpl,
      `npm registry ${manifest.npm_package}@latest`, limits);
    if (latest.error !== undefined) throw new Error(`npm registry ${manifest.npm_package}@latest: registry returned an error`);
    if (latest.name !== manifest.npm_package || typeof latest.version !== "string") {
      throw new Error(`npm registry ${manifest.npm_package}@latest: package identity or version is invalid`);
    }
    npmVersionFromTag(latest.version, `npm registry ${manifest.npm_package}@latest`);
    const drift = latest.version === version ? "pinned" : "drift";
    writeLine(`vendor-advisories: ${name} ${manifest.npm_package} pinned=${version} latest=${latest.version} ${drift}`);
  }
  writeLine(`vendor-advisories: OK (${names.length} tree(s), advisory lookups complete)`);
}

if (process.argv[1] !== undefined && import.meta.url === pathToFileURL(resolve(process.argv[1])).href) {
  checkVendorAdvisories().catch((error) => {
    console.error(`vendor-advisories: FAILED: ${error instanceof Error ? error.message : String(error)}`);
    process.exitCode = 1;
  });
}
