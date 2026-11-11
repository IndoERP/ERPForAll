/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
// Reader for web/vendor/<name>/UPSTREAM.toml. The files use a small
// TOML subset: `key = "string"`, `key = ["a", "b"]` on one line, `[[files]]` tables and `#` comments.
// Anything else is an error naming the line: an unknown key or a construct outside the subset never
// passes silently.

const TOP_KEYS = new Set(["repository", "tag", "commit", "license", "patches", "local", "npm_package"]);
const FILE_KEYS = new Set(["path", "upstream", "sha256", "repository", "commit"]);
const REQUIRED_TOP = ["repository", "tag", "commit", "license", "patches", "npm_package"];
const REQUIRED_FILE = ["path", "upstream", "sha256"];
const NPM_NAME = /^(?:@[a-z0-9][a-z0-9._-]{0,213}\/)?[a-z0-9][a-z0-9._-]{0,213}$/;
const NPM_VERSION = /^(?:0|[1-9]\d*)\.(?:0|[1-9]\d*)\.(?:0|[1-9]\d*)(?:-(?:0|[1-9]\d*|\d*[A-Za-z-][0-9A-Za-z-]*)(?:\.(?:0|[1-9]\d*|\d*[A-Za-z-][0-9A-Za-z-]*))*)?(?:\+[0-9A-Za-z-]+(?:\.[0-9A-Za-z-]+)*)?$/;

/** Returns a concrete npm version from a release tag and rejects labels or ranges. */
export function npmVersionFromTag(tag, where) {
  if (typeof tag !== "string") throw new Error(`${where}: 'tag' must be a string`);
  const version = tag.startsWith("v") ? tag.slice(1) : tag;
  if (!NPM_VERSION.test(version)) throw new Error(`${where}: tag '${tag}' is not a concrete npm version`);
  return version;
}

function parseString(raw, where) {
  const m = /^"((?:[^"\\]|\\.)*)"$/.exec(raw);
  if (!m) throw new Error(`${where}: expected a double-quoted string, got ${raw}`);
  return m[1].replace(/\\(.)/g, (_, c) => {
    if (c === '"' || c === "\\") return c;
    throw new Error(`${where}: unsupported escape \\${c}`);
  });
}

function parseValue(raw, where) {
  if (raw.startsWith("[")) {
    if (!raw.endsWith("]")) throw new Error(`${where}: an array must close on the same line`);
    const inner = raw.slice(1, -1).trim();
    if (inner === "") return [];
    return inner.split(",").map((p) => parseString(p.trim(), where));
  }
  return parseString(raw, where);
}

/** Parses the text of an UPSTREAM.toml; `name` is used in error messages. */
export function parseUpstream(text, name) {
  const top = {};
  const files = [];
  let current = top;
  let keys = TOP_KEYS;
  const lines = text.split("\n");
  for (let i = 0; i < lines.length; i++) {
    const where = `${name}:${i + 1}`;
    const line = lines[i].trim();
    if (line === "" || line.startsWith("#")) continue;
    if (line === "[[files]]") {
      current = {};
      files.push(current);
      keys = FILE_KEYS;
      continue;
    }
    const m = /^([a-z0-9_]+)\s*=\s*(.+)$/.exec(line);
    if (!m) throw new Error(`${where}: not a key = value line: ${line}`);
    const [, key, raw] = m;
    if (!keys.has(key)) throw new Error(`${where}: unknown key '${key}'`);
    if (key in current) throw new Error(`${where}: duplicate key '${key}'`);
    current[key] = parseValue(raw.trim(), where);
  }
  for (const k of REQUIRED_TOP) {
    if (!(k in top)) throw new Error(`${name}: missing key '${k}'`);
  }
  if (!Array.isArray(top.patches)) throw new Error(`${name}: 'patches' must be an array`);
  if ("local" in top && !Array.isArray(top.local)) throw new Error(`${name}: 'local' must be an array`);
  if (typeof top.npm_package !== "string" || top.npm_package.length > 214 || !NPM_NAME.test(top.npm_package)) {
    throw new Error(`${name}: 'npm_package' must be a valid npm package name`);
  }
  npmVersionFromTag(top.tag, name);
  if (files.length === 0) throw new Error(`${name}: no [[files]] entry`);
  for (const [i, f] of files.entries()) {
    for (const k of REQUIRED_FILE) {
      if (!(k in f)) throw new Error(`${name}: [[files]] #${i + 1} is missing '${k}'`);
    }
    if (!/^[0-9a-f]{64}$/.test(f.sha256)) throw new Error(`${name}: [[files]] #${i + 1} sha256 is not 64 hex digits`);
    if (("repository" in f) !== ("commit" in f)) {
      throw new Error(`${name}: [[files]] #${i + 1} must set both 'repository' and 'commit' or neither`);
    }
  }
  return { ...top, local: top.local ?? [], files };
}
