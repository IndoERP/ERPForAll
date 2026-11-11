/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
import { expect, it } from "vitest";
import { AstDecodeError, decodeAst, parseBoundedJson } from "../src/index";

// Reproducible malformed-wire corpus owned by AST. Full evaluator differential
// properties remain INTEGRATE's gate. No wall-clock seed or unbounded shrinking.
const SEED = 0x55495831;
function random() {
  let state = SEED;
  return () => { state ^= state << 13; state ^= state >>> 17; state ^= state << 5; return state >>> 0; };
}
interface Case { wire: string; path: string; detail: string; }
function malformed(next: () => number): Case {
  const suffix = next().toString(16);
  const node = { kind: "name", name: `n_${suffix}` };
  const document = (nodes: unknown[]) => JSON.stringify({ schema: 1, nodes });
  switch (next() % 8) {
    case 0: return { wire: document([{ kind: "starred", value: next() % 32 }]), path: "$.nodes[0]", detail: "" };
    case 1: return { wire: document([node, { kind: "binary", op: "add", left: 0, right: 0 }]), path: "$.nodes[1]", detail: "child index 0 has more than one incoming edge" };
    case 2: return { wire: document([node, node]), path: "$.nodes[0]", detail: "nonroot node must have exactly one incoming edge" };
    case 3: return { wire: `{"schema":1,"nodes":[],"schema":${next()}}`, path: "$.schema", detail: "duplicate object key" };
    case 4: return { wire: document([{ kind: "literal", value: { kind: "int", value: `0${next()}` } }]), path: "$.nodes[0].value.value", detail: "expected canonical i64 decimal string" };
    case 5: return { wire: document([{ kind: "literal", value: { kind: "float", value: suffix.padStart(15, "0") } }]), path: "$.nodes[0].value.value", detail: "expected 16 lowercase hexadecimal float bits" };
    case 6: return { wire: `{"schema":1,"nodes":[{"kind":"name","name":"\\u${(0xd800 + next() % 0x800).toString(16)}"}]}`, path: "$.nodes[0].name", detail: "unpaired Unicode surrogate" };
    default: return { wire: `{"schema":1,"nodes":[{"kind":"name","name":"${suffix}","__proto__":{"polluted":true}}]}`, path: '$.nodes[0]["__proto__"]', detail: "unknown field" };
  }
}

it("rejects 1000 seeded malformed documents with exact diagnostics and unchanged prototypes", () => {
  const next = random();
  for (let index = 0; index < 1000; index++) {
    const sample = malformed(next);
    // The generated bad edge value is part of the fixture, not an observed result.
    if (sample.detail === "") {
      const value = (JSON.parse(sample.wire) as { nodes: { value: number }[] }).nodes[0]!.value;
      sample.detail = `child index ${value} must point strictly backwards`;
    }
    const descriptors = Object.getOwnPropertyDescriptors(Object.prototype);
    const prototype = Object.getPrototypeOf(Object.prototype);
    const unrelated = {};
    const context = `seed=${SEED} case=${index} shrink=none limits=default wire=${sample.wire}`;
    let failure: unknown;
    try { decodeAst(parseBoundedJson(sample.wire)); }
    catch (error) { failure = error; }
    expect(failure, context).toBeInstanceOf(AstDecodeError);
    expect(failure, context).toMatchObject({ kind: "InvalidAst", path: sample.path, message: `InvalidAst at ${sample.path}: ${sample.detail}` });
    expect(Object.getOwnPropertyDescriptors(Object.prototype), context).toEqual(descriptors);
    expect(Object.getPrototypeOf(Object.prototype), context).toBe(prototype);
    expect(Object.getOwnPropertyDescriptors(unrelated), context).toEqual({});
    expect(Object.getPrototypeOf(unrelated), context).toBe(Object.prototype);
  }
});
