/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
// npm licence check of the web workspace. Reads
// `pnpm licenses list --json` for the production graph and for the whole graph: a production package
// must be under the production list, a dev package under the production or the dev-only list. SPDX `OR`
// needs one allowed alternative, `AND` every part; `X WITH Y` is matched as a whole. An unknown,
// missing or unparsable licence fails, naming the package and version. The vendored trees are checked
// from their UPSTREAM.toml licence against the production list.
//
//   node scripts/check-licenses.mjs

import { execFileSync } from "node:child_process";
import { readdirSync, readFileSync, statSync } from "node:fs";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";
import { parseUpstream } from "./upstream-toml.mjs";

const web = resolve(dirname(fileURLToPath(import.meta.url)), "..");

/** Tokenises an SPDX expression: parentheses, AND, OR, and identifiers (`X WITH Y` kept as one). */
function tokens(expr) {
  const raw = expr.replace(/\(/g, " ( ").replace(/\)/g, " ) ").trim().split(/\s+/);
  const out = [];
  for (let i = 0; i < raw.length; i++) {
    const t = raw[i];
    if (raw[i + 1] === "WITH" && raw[i + 2] !== undefined) {
      out.push(`${t} WITH ${raw[i + 2]}`);
      i += 2;
    } else {
      out.push(t);
    }
  }
  return out;
}

/**
 * Whether the SPDX expression `expr` is satisfied by `allowed` (a Set of identifiers). Throws on an
 * expression it cannot parse.
 */
export function satisfies(expr, allowed) {
  if (typeof expr !== "string" || expr.trim() === "") throw new Error(`empty licence expression`);
  const ts = tokens(expr);
  let i = 0;
  const parseOr = () => {
    let v = parseAnd();
    while (ts[i] === "OR") {
      i++;
      const r = parseAnd();
      v = v || r;
    }
    return v;
  };
  const parseAnd = () => {
    let v = parseAtom();
    while (ts[i] === "AND") {
      i++;
      const r = parseAtom();
      v = v && r;
    }
    return v;
  };
  const parseAtom = () => {
    const t = ts[i++];
    if (t === "(") {
      const v = parseOr();
      if (ts[i++] !== ")") throw new Error(`unbalanced parentheses in licence ${JSON.stringify(expr)}`);
      return v;
    }
    if (t === undefined || t === ")" || t === "AND" || t === "OR") {
      throw new Error(`cannot parse licence ${JSON.stringify(expr)}`);
    }
    return allowed.has(t);
  };
  const v = parseOr();
  if (i !== ts.length) throw new Error(`cannot parse licence ${JSON.stringify(expr)}`);
  return v;
}

function pnpmLicenses(prod) {
  const args = ["licenses", "list", "--json", "--recursive"];
  if (prod) args.push("--prod");
  const out = execFileSync("pnpm", args, { cwd: web, encoding: "utf8", stdio: ["ignore", "pipe", "pipe"] }).trim();
  if (out === "" || /^No licenses in packages found/.test(out)) return [];
  const byLicence = JSON.parse(out);
  const packages = [];
  for (const [licence, list] of Object.entries(byLicence)) {
    for (const p of list) packages.push({ licence, name: p.name, versions: p.versions });
  }
  return packages;
}

export function check({ production, devOnly, prodPackages, allPackages, vendored }) {
  const prod = new Set(production);
  const dev = new Set([...production, ...devOnly]);
  const problems = [];
  const judge = (p, set, kind) => {
    try {
      if (!satisfies(p.licence, set)) {
        problems.push(`${p.name}@${p.versions.join(",")}: licence ${JSON.stringify(p.licence)} is not allowed for a ${kind} dependency`);
      }
    } catch (e) {
      problems.push(`${p.name}@${p.versions.join(",")}: ${e.message}`);
    }
  };
  const prodNames = new Set(prodPackages.map((p) => `${p.name}@${p.versions.join(",")}`));
  for (const p of prodPackages) judge(p, prod, "production");
  for (const p of allPackages) {
    if (!prodNames.has(`${p.name}@${p.versions.join(",")}`)) judge(p, dev, "dev");
  }
  for (const v of vendored) judge(v, prod, "vendored (production)");
  return problems;
}

function main() {
  const lists = JSON.parse(readFileSync(join(web, "licenses.json"), "utf8"));
  const vendorDir = join(web, "vendor");
  const vendored = readdirSync(vendorDir)
    .filter((n) => statSync(join(vendorDir, n)).isDirectory())
    .map((n) => {
      const m = parseUpstream(readFileSync(join(vendorDir, n, "UPSTREAM.toml"), "utf8"), `vendor/${n}/UPSTREAM.toml`);
      return { licence: m.license, name: `vendor/${n}`, versions: [m.tag] };
    });
  const prodPackages = pnpmLicenses(true);
  const allPackages = pnpmLicenses(false);
  if (allPackages.length === 0) throw new Error("pnpm reported no packages: run pnpm install first");
  const problems = check({ production: lists.production, devOnly: lists.dev_only, prodPackages, allPackages, vendored });
  if (problems.length > 0) {
    for (const p of problems) console.error(`check-licenses: ${p}`);
    process.exit(1);
  }
  console.log(
    `check-licenses: ${prodPackages.length} production, ${allPackages.length - prodPackages.length} dev, ${vendored.length} vendored: all allowed`,
  );
}

if (process.argv[1] !== undefined && import.meta.url === pathToFileURL(process.argv[1]).href) {
  try {
    main();
  } catch (e) {
    console.error(`check-licenses: FAILED: ${e.message}`);
    process.exit(1);
  }
}
