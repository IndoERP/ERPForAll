/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
// Compile-time check (tsc only, never imported at run time) that the implementation module matches the
// frozen ABI: the two modules export the same names, and each helper's implementation has exactly the
// type abi.d.ts declares, not merely an assignable one (type review M4: a `void` return implemented as
// `unknown`, or an extra optional parameter, fails here).

import type * as Contract from "./abi.js";
import type * as Impl from "./abi-impl.js";

/** Mutual assignability (for the name sets). */
type Exact<A, B> = [A] extends [B] ? ([B] extends [A] ? true : false) : false;

/** Type identity: true only when `A` and `B` are the same type (the compiler's own identity relation). */
type Same<A, B> = (<T>() => T extends A ? 1 : 2) extends <T>() => T extends B ? 1 : 2 ? true : false;

type Signatures = {
  [K in keyof typeof Contract]: Same<(typeof Contract)[K], K extends keyof typeof Impl ? (typeof Impl)[K] : never>;
};

const signatures: { [K in keyof typeof Contract]: true } = null as unknown as Signatures;
const sameNames: Exact<keyof typeof Contract, keyof typeof Impl> = true;
export const abiConformance = [signatures, sameNames] as const;
