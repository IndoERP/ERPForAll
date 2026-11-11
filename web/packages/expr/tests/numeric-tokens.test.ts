/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
import { readFileSync } from "node:fs";
import { resolve } from "node:path";
import { expect, it } from "vitest";
import { AstDecodeError, decodeAst, parseBoundedJson, type JsonValue } from "../src/index";

type Fixture = { token: string; rounded: number | null };
const fixtures = JSON.parse(readFileSync(resolve(process.cwd(), "../crates/erp-expr/tests/data/wire_number_tokens.json"), "utf8")) as Fixture[];
const invalid: Record<string, string> = {
  "1.0000000000000002": "child index 1.0000000000000002 must point strictly backwards",
  "0.9999999999999999": "child index 0.9999999999999999 must point strictly backwards",
  "4294967295.0000005": "child index 4294967295.0000005 must point strictly backwards",
  "4294967296": "child index 4294967296 must point strictly backwards",
  "-1": "child index -1 must point strictly backwards",
  "null": "child index <invalid> must point strictly backwards",
  "true": "child index <invalid> must point strictly backwards",
  '\"0\"': "child index <invalid> must point strictly backwards",
};

it("matches the shared Rust numeric-token rounding corpus before index admission", () => {
  for (const { token, rounded } of fixtures) {
    const descriptors = Object.getOwnPropertyDescriptors(Object.prototype);
    const prototype = Object.getPrototypeOf(Object.prototype);
    const unrelated = {};
    if (rounded !== null) {
      // Equality deliberately treats -0 as zero for unsigned wire indices.
      expect(Number(token) === rounded, token).toBe(true);
      const parsed = parseBoundedJson(`[${token}]`) as readonly JsonValue[];
      expect(parsed[0] === rounded, token).toBe(true);
    }
    const wire = '{"schema":1,"nodes":[{"kind":"name","name":"x"},{"kind":"starred","value":' + token + '}]}';
    if (rounded === 0) expect(decodeAst(parseBoundedJson(wire)).height, token).toBe(2);
    else {
      const path = token === "1e309" ? "$.nodes[1].value" : "$.nodes[1]";
      const detail = token === "1e309" ? "expected finite JSON number" : rounded === null ? invalid[token] : `child index ${rounded} must point strictly backwards`;
      expect(detail, token).toBeTypeOf("string");
      let error: unknown;
      try { decodeAst(parseBoundedJson(wire)); } catch (failure) { error = failure; }
      expect(error, token).toBeInstanceOf(AstDecodeError);
      expect(error, token).toMatchObject({ kind: "InvalidAst", path, message: `InvalidAst at ${path}: ${detail}` });
    }
    if (rounded === 1) {
      const schemaWire = '{"schema":' + token + ',"nodes":[{"kind":"name","name":"x"}]}';
      expect(decodeAst(parseBoundedJson(schemaWire)).height, token).toBe(1);
    }
    expect(Object.getOwnPropertyDescriptors(Object.prototype), token).toEqual(descriptors);
    expect(Object.getPrototypeOf(Object.prototype), token).toBe(prototype);
    expect(Object.getOwnPropertyDescriptors(unrelated), token).toEqual({});
    expect(Object.getPrototypeOf(unrelated), token).toBe(Object.prototype);
  }
});
