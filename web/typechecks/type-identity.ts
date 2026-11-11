/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
// Type-only equality utilities shared by the ABI conformance checks (src/runtime/abi-conformance.ts,
// typechecks/abi-emitted-conformance.ts) and proved by web/tests/abi-emitted-consumer.test.ts. Both
// conformance checks need *identity*, not assignability: a frozen `(x: string) => void` helper and an
// emitted `(x: string, extra?: number) => void` helper are mutually assignable (every caller of the
// frozen signature also satisfies the wider one), so an ordinary `extends` check would let an added
// optional parameter or a widened return type through silently.

/**
 * True only when key-set unions `A` and `B` are the same set (mutual extends over the union). Scoped to
 * `PropertyKey` (its only valid use, two `keyof` results) so a future caller cannot apply it to an
 * arbitrary pair of types and silently get an assignability check instead of the set-equality it claims.
 */
export type Exact<A extends PropertyKey, B extends PropertyKey> = [A] extends [B] ? ([B] extends [A] ? true : false) : false;

/**
 * Type identity: true only when `A` and `B` are the written-out same type. The generic-distribution
 * trick (compare two *unresolved* conditional types parameterised by a fresh `T`) distinguishes types
 * that are mutually assignable but not identical — e.g. an added optional parameter, or `void`/`any`/
 * `unknown` standing in for each other — because TypeScript only accepts `(<T>() => T extends A ? 1 : 2)
 * extends (<T>() => T extends B ? 1 : 2)` when `A` and `B` are the same type, not merely each assignable
 * to the other. Plain return-type narrowing/widening is usually *not* mutually assignable either way
 * (TS's return-type check is normally one-directional), so it is already caught by ordinary `extends`;
 * `Same` is what additionally catches the mutually-assignable cases above.
 */
export type Same<A, B> = (<T>() => T extends A ? 1 : 2) extends <T>() => T extends B ? 1 : 2 ? true : false;
