/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
// Compile-time check (tsc only, never imported at run time) that the runtime's *emitted* declarations
// (tsconfig.dts.json output, `node_modules/.elang-dts/src/runtime/abi-impl.d.ts` — the `src/` prefix is
// preserved because `tsconfig.dts.json`'s implicit rootDir is `web/`, not `web/src`: other root files
// matched by its `src/**/*.ts` include, such as `src/runtime/expr.ts`, import `@erpforall/expr`, whose
// `paths` mapping resolves to `packages/expr/src/index.ts`, outside `web/src`, so TypeScript's common-
// source-directory computation pulls the implicit rootDir up to `web/`; confirmed on the first real
// `tsc -p tsconfig.dts.json` run, not assumed) still match the frozen ABI (abi.d.ts), independently of
// `abi-conformance.ts`, which checks the source module. Source-only checking
// cannot see an emission or package-resolution drift: a helper tsc could not infer a declaration for, a
// generator bug that widens or narrows a signature only in its emitted form, or a mismatch between what
// `./abi-impl.ts` exports in source and what consumers actually import from the published package (the
// `"./abi"` export's `types` field points at the hand-written abi.d.ts, but nothing previously checked that
// the *implementation*'s own emission agrees with it). This project sets `skipLibCheck: false`
// (tsconfig.dts-consumer.json) so the emitted `.d.ts` is itself checked, not merely trusted because the
// workspace's ordinary configs skip library-file checking.
//
// Run order: `tsc -p tsconfig.dts.json` must emit before this project is compiled (scripts/ci-web-stage.sh
// `web_tsc`, `package.json` `typecheck`); a stale or missing emission fails here with a resolution error,
// not a false pass.

import type * as Contract from "../src/runtime/abi.js";
import type * as Emitted from "../node_modules/.elang-dts/src/runtime/abi-impl.js";
import type { Exact, Same } from "./type-identity.js";

type Signatures = {
  [K in keyof typeof Contract]: Same<(typeof Contract)[K], K extends keyof typeof Emitted ? (typeof Emitted)[K] : never>;
};

const signatures: { [K in keyof typeof Contract]: true } = null as unknown as Signatures;
const sameNames: Exact<keyof typeof Contract, keyof typeof Emitted> = true;
export const abiEmittedConformance = [signatures, sameNames] as const;
