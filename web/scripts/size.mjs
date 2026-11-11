/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
// Size of the runtime as shipped: the public API and the ABI module
// with the vendored signal core and reconciler, bundled and minified by esbuild (legal comments kept
// inline), gzip level 9. tests/size-budget.test.ts pins the result at milestone end; size.test.ts keeps legal notices mandatory.
//
//   node scripts/size.mjs            prints {minified, gzip} bytes as JSON

import { gzipSync } from "node:zlib";
import { dirname, resolve } from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";
import { build } from "esbuild";

const web = resolve(dirname(fileURLToPath(import.meta.url)), "..");

export async function bundle() {
  const result = await build({
    stdin: {
      contents: 'export * from "./src/index.ts";\nexport * as abi from "./src/runtime/abi-impl.ts";\n',
      resolveDir: web,
      loader: "ts",
      sourcefile: "size-entry.ts",
    },
    bundle: true,
    minify: true,
    format: "esm",
    target: "es2023",
    legalComments: "inline",
    write: false,
    logLevel: "silent",
  });
  return result.outputFiles[0].contents;
}

export async function measure() {
  const code = await bundle();
  return {
    code: Buffer.from(code).toString("utf8"),
    minified: code.byteLength,
    gzip: gzipSync(code, { level: 9 }).byteLength,
  };
}

if (process.argv[1] !== undefined && import.meta.url === pathToFileURL(process.argv[1]).href) {
  const { minified, gzip } = await measure();
  console.log(JSON.stringify({ minified, gzip }));
}
