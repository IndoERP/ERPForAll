/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
// Checks every vendored tree under web/vendor: each
// file listed in UPSTREAM.toml is downloaded at its pinned commit and must have the pinned SHA-256;
// the listed patches are applied in order to that pristine copy with `git apply`; the result must
// equal the tree byte for byte, and the tree must hold no file the manifest does not account for
// (upstream files, `local` files of our own, UPSTREAM.toml, the patches). Any failure exits 1 with the
// reason; a failed or stalled download (no answer within DOWNLOAD_TIMEOUT_MS) is a failure, never a skip.
// `local` files (our own typings such as reconcile.d.ts) are not pinned by hash: they are our own MIT OR Apache-2.0 source of
// this repository, reviewed like any other file; the check only requires that they are listed.
//
//   node scripts/check-vendor.mjs [--vendor DIR] [--source DIR]
//
// --source DIR reads upstream files from DIR/<host>/<owner>/<repo>/<commit>/<path> instead of
// downloading them (tests only).

import { createHash } from "node:crypto";
import { execFileSync } from "node:child_process";
import { mkdtempSync, readdirSync, readFileSync, rmSync, statSync, writeFileSync, mkdirSync } from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join, relative, resolve } from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";
import { parseUpstream } from "./upstream-toml.mjs";
import { checkVendorAdvisories } from "./check-vendor-advisories.mjs";

const here = dirname(fileURLToPath(import.meta.url));

function parseArgs(argv) {
  const opts = { vendor: resolve(here, "../vendor"), source: null };
  for (let i = 0; i < argv.length; i++) {
    const a = argv[i];
    if ((a === "--vendor" || a === "--source") && i + 1 < argv.length) {
      opts[a.slice(2)] = resolve(argv[++i]);
    } else {
      throw new Error(`unknown argument: ${a}`);
    }
  }
  return opts;
}

/** How long one upstream download may take before the check fails. */
const DOWNLOAD_TIMEOUT_MS = 60_000;

function sha256(buf) {
  return createHash("sha256").update(buf).digest("hex");
}

function listFiles(dir, base = dir) {
  const out = [];
  for (const name of readdirSync(dir).sort()) {
    const p = join(dir, name);
    if (statSync(p).isDirectory()) out.push(...listFiles(p, base));
    else out.push(relative(base, p).split("\\").join("/"));
  }
  return out;
}

function repoPath(repository, where) {
  const m = /^https:\/\/github\.com\/([A-Za-z0-9_.-]+)\/([A-Za-z0-9_.-]+)$/.exec(repository);
  if (!m) throw new Error(`${where}: repository must be https://github.com/<owner>/<repo>, got ${repository}`);
  return `${m[1]}/${m[2]}`;
}

async function fetchUpstream(opts, repository, commit, path, where) {
  if (!/^[0-9a-f]{40}$/.test(commit)) throw new Error(`${where}: commit must be a full 40-digit sha, got ${commit}`);
  const repo = repoPath(repository, where);
  if (opts.source) {
    return readFileSync(join(opts.source, "github.com", repo, commit, path));
  }
  const url = `https://raw.githubusercontent.com/${repo}/${commit}/${path}`;
  let res;
  try {
    res = await fetch(url, { redirect: "error", signal: AbortSignal.timeout(DOWNLOAD_TIMEOUT_MS) });
    if (!res.ok) throw new Error(`HTTP ${res.status}`);
    return Buffer.from(await res.arrayBuffer());
  } catch (e) {
    throw new Error(`${where}: download failed: ${url}: ${e.message}`);
  }
}

async function checkTree(opts, name) {
  const dir = join(opts.vendor, name);
  const manifestName = `vendor/${name}/UPSTREAM.toml`;
  const m = parseUpstream(readFileSync(join(dir, "UPSTREAM.toml"), "utf8"), manifestName);
  const work = mkdtempSync(join(tmpdir(), `check-vendor-${name}-`));
  try {
    for (const f of m.files) {
      const where = `${manifestName} ${f.path}`;
      const bytes = await fetchUpstream(opts, f.repository ?? m.repository, f.commit ?? m.commit, f.upstream, where);
      const got = sha256(bytes);
      if (got !== f.sha256) throw new Error(`${where}: upstream SHA-256 is ${got}, pinned ${f.sha256}`);
      const out = join(work, f.path);
      mkdirSync(dirname(out), { recursive: true });
      writeFileSync(out, bytes);
    }
    for (const p of m.patches) {
      try {
        execFileSync("git", ["apply", "--whitespace=nowarn", join(dir, p)], { cwd: work, stdio: "pipe" });
      } catch (e) {
        throw new Error(`${manifestName}: patch ${p} does not apply to the pristine files: ${String(e.stderr || e.message).trim()}`);
      }
    }
    const allowed = new Set(["UPSTREAM.toml", ...m.patches, ...m.local, ...m.files.map((f) => f.path)]);
    const tree = listFiles(dir);
    for (const f of tree) {
      if (!allowed.has(f)) throw new Error(`vendor/${name}/${f} is not listed in UPSTREAM.toml (files, local or patches)`);
    }
    for (const f of allowed) {
      if (!tree.includes(f)) throw new Error(`vendor/${name}/${f} is listed in UPSTREAM.toml but missing`);
    }
    for (const f of m.files) {
      const want = readFileSync(join(work, f.path));
      const have = readFileSync(join(dir, f.path));
      if (!want.equals(have)) {
        throw new Error(`vendor/${name}/${f.path} differs from upstream ${m.tag} plus the listed patches`);
      }
    }
    return `${name} ${m.tag} (${m.commit.slice(0, 12)}): ${m.files.length} files, ${m.patches.length} patches, tree matches`;
  } finally {
    rmSync(work, { recursive: true, force: true });
  }
}

export async function runAdvisoryCheck(opts, checker = checkVendorAdvisories, writeLine = console.log) {
  if (opts.source) {
    writeLine("check-vendor: advisory and release-drift lookups not run (--source fixture; no network check)");
    return;
  }
  await checker({ vendorDir: opts.vendor });
}

async function main(argv = process.argv.slice(2)) {
  const opts = parseArgs(argv);
  const names = readdirSync(opts.vendor).filter((n) => statSync(join(opts.vendor, n)).isDirectory()).sort();
  if (names.length === 0) throw new Error(`no vendored tree under ${opts.vendor}`);
  for (const n of names) {
    console.log(`check-vendor: ${await checkTree(opts, n)}`);
  }
  await runAdvisoryCheck(opts);
}

if (process.argv[1] !== undefined && import.meta.url === pathToFileURL(resolve(process.argv[1])).href) {
  main().catch((e) => {
    console.error(`check-vendor: FAILED: ${e instanceof Error ? e.message : String(e)}`);
    process.exitCode = 1;
  });
}
