/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
// Measure the public runtime and compiled-template ABI together with inline legal notices.
// Pass --inputs for esbuild's per-module minified byte contributions.

import { gzipSync } from "node:zlib";
import { dirname, resolve } from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";
import { build } from "esbuild";

const web = resolve(dirname(fileURLToPath(import.meta.url)), "..");
const runtimeEntry = 'export * from "./src/index.ts";\nexport * as abi from "./src/runtime/abi-impl.ts";\n';

export async function analyzeEntry(contents = runtimeEntry) {
  const result = await build({
    absWorkingDir: web,
    stdin: {
      contents,
      resolveDir: web,
      loader: "ts",
      sourcefile: "size-entry.ts",
    },
    bundle: true,
    minify: true,
    format: "esm",
    target: "es2023",
    legalComments: "inline",
    metafile: true,
    write: false,
    logLevel: "silent",
  });
  const { inputs } = Object.values(result.metafile.outputs)[0];
  return { bytes: result.outputFiles[0].contents, inputs };
}

export async function bundle() {
  return (await analyzeEntry()).bytes;
}

export async function measure() {
  const { bytes: code, inputs } = await analyzeEntry();
  return {
    code: Buffer.from(code).toString("utf8"),
    minified: code.byteLength,
    gzip: gzipSync(code, { level: 9 }).byteLength,
    inputs,
  };
}

if (process.argv[1] !== undefined && import.meta.url === pathToFileURL(process.argv[1]).href) {
  const { minified, gzip, inputs } = await measure();
  console.log(JSON.stringify(process.argv.includes("--inputs") ? { minified, gzip, inputs } : { minified, gzip }));
}
