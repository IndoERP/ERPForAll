/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
// Vitest for the web workspace: the runtime's suites under
// web/tests and every addon's client suites under addons/<name>/static/tests, in jsdom (§3.3).
// scripts/check-tests-wired.mjs verifies the disjoint ordinary/milestone suite inventories.
import { fileURLToPath } from "node:url";
import { configDefaults, defineConfig } from "vitest/config";

const src = (p: string): string => fileURLToPath(new URL(p, import.meta.url));

export const include = ["web/tests/**/*.test.ts", "web/packages/*/tests/**/*.test.ts", "addons/*/static/tests/**/*.test.ts"];

// Exact files only: a new test is mandatory unless deliberately classified here.
export const milestoneInclude = [
  "web/tests/costs.test.ts",
  "web/tests/size-budget.test.ts",
  "web/packages/expr/tests/allocation-budget.test.ts",
];

export const sharedConfig = {
  resolve: {
    alias: [
      // Boot tests use controller fixtures; the real XML is covered by compiled browser captures.
      { find: /^@erpforall\/human-templates$/, replacement: src("./tests/fixtures/client-landmark-templates.ts") },
      { find: /^@erpforall\/elang-test$/, replacement: src("./packages/elang-test/src/index.ts") },
      { find: /^@erpforall\/expr$/, replacement: src("./packages/expr/src/index.ts") },
      { find: /^@erpforall\/mail-store$/, replacement: src("./packages/mail-store/src/index.ts") },
      { find: /^@erpforall\/mail-thread-view$/, replacement: src("./packages/mail-thread-view/src/index.ts") },
      { find: /^@erpforall\/store-wire$/, replacement: src("./packages/store-wire/src/index.ts") },
      { find: /^@erpforall\/elang\/abi$/, replacement: src("./src/runtime/abi-impl.ts") },
      { find: /^@erpforall\/elang$/, replacement: src("./src/index.ts") },
    ],
  },
  // The addon suites live outside web/: let vite serve files from the whole repository.
  server: { fs: { allow: [src("../")] } },
  test: {
    dir: src("../"),
    include,
    exclude: [...configDefaults.exclude, ...milestoneInclude],
    environment: "jsdom",
    // Tests run in one process per file; a hanging test fails instead of blocking CI.
    testTimeout: 10_000,
    // A committed `test.only` fails the run on every host, also where CI is not set (ci.sh does not set
    // it); scripts/ci.sh also fails a report with skipped tests (review round 2, silent-failure 7).
    allowOnly: false,
  },
};

export default defineConfig(sharedConfig);
