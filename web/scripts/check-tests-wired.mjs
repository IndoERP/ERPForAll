/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
// Every discovered suite belongs to ordinary CI or the exact milestone-end manifest.
// Refuse unclassified files, stale milestone entries, duplicate entries and empty include patterns.
//
//   node scripts/check-tests-wired.mjs

import { existsSync, readdirSync, statSync } from "node:fs";
import { dirname, join, matchesGlob, relative, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const web = resolve(dirname(fileURLToPath(import.meta.url)), "..");
const repo = resolve(web, "..");
const { default: ordinaryConfig, include, milestoneInclude } = await import(join(web, "vitest.config.ts"));
const { default: milestoneConfig } = await import(join(web, "vitest.milestone.config.ts"));

function selected(file, config) {
  return config.test.include.some((pattern) => matchesGlob(file, pattern))
    && !config.test.exclude.some((pattern) => matchesGlob(file, pattern));
}

function suites(dir, out = []) {
  if (!existsSync(dir)) return out;
  for (const name of readdirSync(dir)) {
    if (name === "node_modules") continue;
    const p = join(dir, name);
    if (statSync(p).isDirectory()) suites(p, out);
    else if (name.endsWith(".test.ts")) out.push(relative(repo, p).split("\\").join("/"));
  }
  return out;
}

const files = suites(join(web, "tests"));
suites(join(web, "packages"), files);
const addons = join(repo, "addons");
for (const addon of readdirSync(addons)) suites(join(addons, addon, "static", "tests"), files);

const problems = [];
for (const f of files) {
  const ordinary = selected(f, ordinaryConfig);
  const milestone = selected(f, milestoneConfig);
  if (milestone !== milestoneInclude.includes(f)) problems.push(`${f} milestone selection differs from the exact manifest`);
  if (Number(ordinary) + Number(milestone) !== 1) problems.push(`${f} must belong to exactly one ordinary/milestone suite inventory`);
}
if (new Set(milestoneInclude).size !== milestoneInclude.length) problems.push("duplicate milestone suite");
for (const f of milestoneInclude) {
  if (!files.includes(f)) problems.push(`milestone suite ${f} does not exist in the discovered test trees`);
}
for (const g of include) {
  if (!files.some((f) => matchesGlob(f, g))) problems.push(`include pattern ${g} matches no suite`);
}
if (problems.length > 0) {
  for (const p of problems) console.error(`check-tests-wired: ${p}`);
  process.exit(1);
}
console.log(`check-tests-wired: ${files.length} suites, all classified as ordinary or milestone-end`);
