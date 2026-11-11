/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
import { describe, expect, it } from "vitest";
import { AstDecodeError, decodeAst, parseBoundedJson, readResponseJson, DescriptionBudget, isValidatedAst, inspectParsed, inspectValidated } from "../src/index";

const literal = { kind: "literal", value: { kind: "int", value: "9223372036854775807" } };
const document = (nodes: unknown[] = [literal]): string => JSON.stringify({ schema: 1, nodes });
function rejects(run: () => unknown, path: string, detail: string): void {
  const descriptors = Object.getOwnPropertyDescriptors(Object.prototype);
  const prototype = Object.getPrototypeOf(Object.prototype);
  const unrelated = {};
  try { run(); throw new Error("expected InvalidAst"); }
  catch (error) {
    expect(error).toBeInstanceOf(AstDecodeError);
    expect(error).toMatchObject({ kind: "InvalidAst", path, message: `InvalidAst at ${path}: ${detail}` });
  }
  expect(Object.getOwnPropertyDescriptors(Object.prototype)).toEqual(descriptors);
  expect(Object.getPrototypeOf(Object.prototype)).toBe(prototype);
  expect(Object.getOwnPropertyDescriptors(unrelated)).toEqual({});
  expect(Object.getPrototypeOf(unrelated)).toBe(Object.prototype);
}

describe("bounded response and AST admission", () => {
  it("rejects obviously oversized text before scanning its Unicode payload", () => {
    rejects(() => parseBoundedJson('"\ud800"', { maxBytes: 2 }), "$", "response byte limit exceeded (2)");
  });
  it("admits a frozen nested document from one parse", () => {
    const value = parseBoundedJson(`{"view":{"modifier":${document()}}}`) as { view: { modifier: unknown } };
    const ast = decodeAst(value.view.modifier);
    expect(isValidatedAst(ast)).toBe(true);
    expect(ast.nodes).toEqual([literal]);
    expect(ast.height).toBe(1);
    expect(Object.isFrozen(ast.nodes)).toBe(true);
    expect(() => Object.assign(value.view.modifier as object, { schema: 2 })).toThrow(TypeError);
    expect(ast.nodes).toEqual([literal]);
    expect(isValidatedAst({ nodes: ast.nodes })).toBe(false);
  });
  it("rejects duplicate keys in wrappers and escaped spellings", () => {
    rejects(() => parseBoundedJson('{"view":0,"v\\u0069ew":1}'), '$.view', "duplicate object key");
    rejects(() => parseBoundedJson('{"__proto__":{},"__proto__":{}}'), '$["__proto__"]', "duplicate object key");
  });
  it("does not inspect arbitrary objects, getters or proxies", () => {
    const hostile = new Proxy({}, { get() { throw new Error("trap"); }, getPrototypeOf() { throw new Error("trap"); } });
    rejects(() => decodeAst(hostile), "$", "AST must originate from the bounded JSON reader");
    rejects(() => decodeAst({ get schema() { throw new Error("getter"); } }), "$", "AST must originate from the bounded JSON reader");
  });
  it("rejects lone surrogates but admits scalar Unicode and hostile keys inertly", () => {
    rejects(() => parseBoundedJson('"\\ud800"'), "$", "unpaired Unicode surrogate");
    expect(parseBoundedJson('"\\ud83d\\ude00"')).toBe("😀");
    const value = parseBoundedJson('{"__proto__":{"polluted":true},"constructor":0}');
    expect(Object.getPrototypeOf(value)).toBe(null);
    expect(({} as { polluted?: boolean }).polluted).toBeUndefined();
  });
  it("enforces byte, depth and logical allocation boundaries", () => {
    expect(parseBoundedJson('"é"', { maxBytes: 4 })).toBe("é");
    rejects(() => parseBoundedJson('"é"', { maxBytes: 3 }), "$", "response byte limit exceeded (3)");
    expect(parseBoundedJson("[[0]]", { maxDepth: 2 })).toEqual([[0]]);
    rejects(() => parseBoundedJson("[[[0]]]", { maxDepth: 2 }), "$[0][0]", "JSON depth limit exceeded (2)");
    expect(parseBoundedJson("0", { maxAllocationBytes: 32 })).toBe(0);
    rejects(() => parseBoundedJson("0", { maxAllocationBytes: 31 }), "$", "logical allocation limit exceeded (31)");
  });
  it("rejects unknown fields and invalid scalar representations", () => {
    rejects(() => decodeAst(parseBoundedJson('{"schema":1,"nodes":[],"root":0}')), '$.root', "unknown field");
    rejects(() => decodeAst(parseBoundedJson(document([{ kind: "literal", value: { kind: "int", value: "01" } }]))), '$.nodes[0].value.value', "expected canonical i64 decimal string");
    rejects(() => decodeAst(parseBoundedJson(document([{ kind: "literal", value: { kind: "float", value: "FFFFFFFFFFFFFFFF" } }]))), '$.nodes[0].value.value', "expected 16 lowercase hexadecimal float bits");
  });
  it("rejects forward edges, sharing and unreachable nodes", () => {
    rejects(() => decodeAst(parseBoundedJson(document([{ kind: "starred", value: 0 }]))), '$.nodes[0]', "child index 0 must point strictly backwards");
    rejects(() => decodeAst(parseBoundedJson(document([literal, { kind: "binary", op: "add", left: 0, right: 0 }]))), '$.nodes[1]', "child index 0 has more than one incoming edge");
    rejects(() => decodeAst(parseBoundedJson(document([literal, literal]))), '$.nodes[0]', "nonroot node must have exactly one incoming edge");
  });
  it("enforces exact tree height and aggregate node budgets", () => {
    const raw = parseBoundedJson(document([literal, { kind: "starred", value: 0 }]));
    expect(decodeAst(raw, new DescriptionBudget({ maxHeight: 2 })).height).toBe(2);
    rejects(() => decodeAst(raw, new DescriptionBudget({ maxHeight: 1 })), '$.nodes', "logical AST depth exceeds 1");
    const budget = new DescriptionBudget({ maxTotalNodes: 2 });
    decodeAst(raw, budget);
    rejects(() => decodeAst(raw, budget), "$", "description node limit exceeded (2)");
  });
  it("cancels bounded fetch accumulation before consuming oversized bodies", async () => {
    let cancelled = false;
    const body = new ReadableStream<Uint8Array>({ start(controller) { controller.enqueue(new Uint8Array([48, 48])); }, cancel() { cancelled = true; } });
    const response = { body } as Response;
    await expect(readResponseJson(response, { maxBytes: 1 })).rejects.toMatchObject({ kind: "InvalidAst", path: "$", message: "InvalidAst at $: response byte limit exceeded (1)" });
    expect(cancelled).toBe(true);
  });
});

describe("strict scalar and shape coverage", () => {
  it("rejects malformed JSON without invoking a native reparsing step", () => {
    const cases = [
      ["", "expected JSON value"], ["01", "trailing JSON input"],
      ["[1,]", "expected JSON value", "$[1]"], ['{"x":}', "expected JSON value", "$.x"],
      ["true false", "trailing JSON input"], ["NaN", "expected JSON value"],
      ["1e999", "expected finite JSON number"], ['"\\x20"', "invalid string escape"],
      ['"\n"', "unescaped control character"],
    ];
    for (const [wire, detail, path] of cases) rejects(() => parseBoundedJson(wire!), path ?? "$", detail!);
  });
  it("rejects noncanonical and out-of-range integers", () => {
    for (const value of ["-0", "+1", " 1", "01", "9223372036854775808", "-9223372036854775809", 1, null]) {
      rejects(() => decodeAst(parseBoundedJson(document([{ kind: "literal", value: { kind: "int", value } }]))), "$.nodes[0].value.value", "expected canonical i64 decimal string");
    }
    for (const value of ["-9223372036854775808", "0", "9223372036854775807"]) {
      expect(decodeAst(parseBoundedJson(document([{ kind: "literal", value: { kind: "int", value } }]))).height).toBe(1);
    }
  });
  it("preserves binary64 wire bits including signed zero and nonfinite payloads", () => {
    for (const value of ["0000000000000000", "8000000000000000", "7ff0000000000000", "fff0000000000000", "7ff8000000001234"]) {
      const node = { kind: "literal", value: { kind: "float", value } };
      expect(decodeAst(parseBoundedJson(document([node]))).nodes).toEqual([node]);
    }
  });
  it("counts comprehension clauses in logical height", () => {
    const raw = parseBoundedJson(document([
      literal, { kind: "name", name: "x" }, { kind: "list", items: [] },
      { kind: "name", name: "y" }, { kind: "list", items: [] },
      { kind: "list_comp", elt: 0, generators: [{ target: 1, iter: 2, ifs: [] }, { target: 3, iter: 4, ifs: [] }] },
    ]));
    expect(decodeAst(raw, new DescriptionBudget({ maxHeight: 4 })).height).toBe(4);
    rejects(() => decodeAst(raw, new DescriptionBudget({ maxHeight: 3 })), "$.nodes", "logical AST depth exceeds 3");
  });
});

describe("closed wire shape", () => {
  it("rejects unsupported versions and wrong document shapes", () => {
    rejects(() => decodeAst(parseBoundedJson('{"schema":2,"nodes":[]}')), "$.schema", "unsupported schema version 2; reload or upgrade required");
    rejects(() => decodeAst(parseBoundedJson('{"schema":"1","nodes":[]}')), "$.schema", "expected schema version integer");
    rejects(() => decodeAst(parseBoundedJson('{"schema":1,"nodes":[]}')), "$.nodes", "expected 1..=65536 nodes");
    rejects(() => decodeAst(parseBoundedJson('{"schema":1,"nodes":[{"kind":"unknown"}]}')), "$.nodes[0].kind", "unknown node kind");
    rejects(() => decodeAst(parseBoundedJson(document([{ kind: "name", name: 7 }]))), "$.nodes[0].name", "expected string");
    rejects(() => decodeAst(parseBoundedJson(document([{ kind: "name", name: "x", extra: true }]))), "$.nodes[0].extra", "unknown field");
    rejects(() => decodeAst(parseBoundedJson(document([{ kind: "name" }]))), "$.nodes[0].name", "missing required field");
  });
  it("validates byte scalars and comparison cardinality", () => {
    for (const value of [-1, 256, 1.5, "1"]) rejects(() => decodeAst(parseBoundedJson(document([{ kind: "literal", value: { kind: "bytes", value: [value] } }]))), "$.nodes[0].value.value[0]", "expected byte integer in 0..255");
    rejects(() => decodeAst(parseBoundedJson(document([literal, { kind: "compare", left: 0, ops: ["eq"], comparators: [] }]))), "$.nodes[1].comparators", "comparison requires matching nonempty operators and comparators");
    rejects(() => decodeAst(parseBoundedJson(document([literal, { kind: "bool_op", op: "and", values: [0] }]))), "$.nodes[1].values", "boolean operation requires at least two values");
  });
  it("never raises hard resource ceilings or disables limits", () => {
    rejects(() => parseBoundedJson("0", { maxDepth: 0 }), "$.limits.maxDepth", "expected integer in 1..64");
    rejects(() => parseBoundedJson("0", { maxBytes: 67108865 }), "$.limits.maxBytes", "expected integer in 1..67108864");
    rejects(() => new DescriptionBudget({ maxHeight: 101 }), "$.limits.maxHeight", "expected integer in 1..100");
  });
  it("rejects excessive node counts before allocating edge tables", () => {
    rejects(() => decodeAst(parseBoundedJson(document([literal, literal])), new DescriptionBudget({ maxNodes: 1 })), "$.nodes", "expected 1..=1 nodes");
  });
});

describe("stream and raw-byte boundary", () => {
  it("decodes split UTF-8 chunks without response.text", async () => {
    const body = new ReadableStream<Uint8Array>({ start(controller) {
      controller.enqueue(new Uint8Array([34, 0xc3]));
      controller.enqueue(new Uint8Array([0xa9, 34]));
      controller.close();
    } });
    expect(await readResponseJson({ body } as Response, { maxBytes: 4 })).toBe("é");
  });
  it("rejects malformed UTF-8 before parsing", () => {
    rejects(() => parseBoundedJson(new Uint8Array([34, 0xc0, 0xaf, 34])), "$", "invalid UTF-8 response");
  });
  it("accounts key and decoded string UTF-8 payloads", () => {
    expect(parseBoundedJson('{"é":"😀"}', { maxAllocationBytes: 86 })).toEqual({ é: "😀" });
    rejects(() => parseBoundedJson('{"é":"😀"}', { maxAllocationBytes: 85 }), "$.é", "logical allocation limit exceeded (85)");
  });
  it("rejects a per-expression byte cap without reparsing its subtree", () => {
    const raw = parseBoundedJson(document());
    const bytes = new TextEncoder().encode(document()).length;
    expect(decodeAst(raw, new DescriptionBudget({ maxExpressionBytes: bytes })).height).toBe(1);
    rejects(() => decodeAst(raw, new DescriptionBudget({ maxExpressionBytes: bytes - 1 })), "$", `encoded JSON exceeds ${bytes - 1} bytes`);
  });
});

describe("numeric JSON interoperability", () => {
  it("admits integral decimal and exponent number spellings as JSON numbers", () => {
    const raw = '{"schema":1.0,"nodes":[{"kind":"name","name":"x"},{"kind":"starred","value":0e0}]}';
    expect(decodeAst(parseBoundedJson(raw)).height).toBe(2);
  });
});

describe("runtime input authenticity", () => {
  it("rejects proxy byte input without executing its traps", () => {
    const proxy = new Proxy(new Uint8Array([48]), { get() { throw new Error("trap"); }, getPrototypeOf() { throw new Error("trap"); } });
    rejects(() => parseBoundedJson(proxy), "$", "expected JSON text or UTF-8 bytes");
  });
  it("rejects raw unpaired UTF-16 independently of escapes", () => {
    rejects(() => parseBoundedJson('"\ud800\\udc00"'), "$", "unpaired Unicode surrogate");
  });
  it("rejects revoked proxy documents without executing traps", () => {
    const { proxy, revoke } = Proxy.revocable({}, {}); revoke();
    rejects(() => decodeAst(proxy), "$", "AST must originate from the bounded JSON reader");
  });
  it("a budget cannot be forged or overridden to raise its limits", () => {
    rejects(() => decodeAst(parseBoundedJson(document()), {} as DescriptionBudget), "$", "invalid description budget");
  });
});

it("retains immutable nodes after caller bytes and all reachable arrays are mutated", () => {
  const bytes = new TextEncoder().encode(document([literal, { kind: "list", items: [0] }]));
  const ast = decodeAst(parseBoundedJson(bytes));
  bytes.fill(0);
  expect(ast.height).toBe(2);
  expect(() => (ast.nodes as unknown[]).push(literal)).toThrow(TypeError);
  const root = ast.nodes[1] as unknown as { items: number[] };
  expect(() => root.items.push(0)).toThrow(TypeError);
  expect(ast.nodes[1]).toEqual({ kind: "list", items: [0] });
});


it("decodes equivalent long plain and escaped string tokens", () => {
  const plain = parseBoundedJson(JSON.stringify({ value: "é😀".repeat(8192) }));
  const escaped = parseBoundedJson('{"value":"' + "\\u00e9\\ud83d\\ude00".repeat(8192) + '"}');
  expect(escaped).toEqual(plain);
});

describe("description scope and byte accounting", () => {
  it("counts enclosing nonexpression content and does not double charge nested bytes", () => {
    const text = JSON.stringify({ expressions: [JSON.parse(document())], title: "é😀" });
    const raw = parseBoundedJson(text) as { expressions: unknown[] };
    const bytes = new TextEncoder().encode(text).length;
    const budget = new DescriptionBudget({ maxDescriptionBytes: bytes }, raw);
    expect(decodeAst(raw.expressions[0], budget).height).toBe(1);
    rejects(() => new DescriptionBudget({ maxDescriptionBytes: bytes - 1 }, raw), "$", `description byte limit exceeded (${bytes - 1})`);
  });
  it("does not admit siblings or another response into a description subtree", () => {
    const raw = parseBoundedJson(JSON.stringify({ left: JSON.parse(document()), right: JSON.parse(document()) })) as { left: unknown; right: unknown };
    const budget = new DescriptionBudget({}, raw.left);
    rejects(() => decodeAst(raw.right, budget), "$.right", "AST is outside its description");
    const shared = new DescriptionBudget();
    decodeAst(raw.left, shared);
    rejects(() => decodeAst(parseBoundedJson(document()), shared), "$", "AST is outside its description");
  });
  it("enforces the expression JSON-depth boundary independently of its wrapper", () => {
    const raw = parseBoundedJson(document());
    expect(decodeAst(raw, new DescriptionBudget({ maxJsonDepth: 4 })).height).toBe(1);
    rejects(() => decodeAst(raw, new DescriptionBudget({ maxJsonDepth: 3 })), "$", "JSON nesting exceeds 3");
  });
});

it("admits exact default height 100 and rejects a left-associated chain at 101", () => {
  function chain(height: number) {
    const nodes: unknown[] = [literal];
    for (let level = 1; level < height; level++) {
      const left = nodes.length - 1;
      nodes.push(literal);
      nodes.push({ kind: "binary", op: "add", left, right: nodes.length - 1 });
    }
    return parseBoundedJson(document(nodes));
  }
  expect(decodeAst(chain(100)).height).toBe(100);
  rejects(() => decodeAst(chain(101)), "$.nodes", "logical AST depth exceeds 100");
});

it("decodes all JSON escapes, BOM payload and the highest Unicode scalar losslessly", () => {
  const value = '\"\\/\b\f\n\r\t\0\ufeff\u{10ffff}';
  expect(parseBoundedJson(JSON.stringify(value))).toBe(value);
  expect(parseBoundedJson('"\\ufeff"')).toBe("\ufeff");
});

it("tracks nested encoded spans across escaped and raw Unicode wrapper values", () => {
  const expression = document();
  const text = '{"before":"é😀\\u00e9","modifier":' + expression + ',"after":"\\ud83d\\ude00"}';
  const raw = parseBoundedJson(text) as { modifier: unknown };
  expect(inspectParsed(raw.modifier).encodedBytes).toBe(new TextEncoder().encode(expression).length);
  expect(inspectParsed(raw).encodedBytes).toBe(new TextEncoder().encode(text).length);
  expect(decodeAst(raw.modifier).height).toBe(1);
});

it("checks encoded Unicode, wrapper punctuation and trailing whitespace at exact byte boundaries", () => {
  const text = ' \n { "é" : ["😀"] } \r\n ';
  const bytes = new TextEncoder().encode(text).length;
  expect(parseBoundedJson(text, { maxBytes: bytes })).toEqual({ é: ["😀"] });
  rejects(() => parseBoundedJson(text, { maxBytes: bytes - 1 }), "$", `response byte limit exceeded (${bytes - 1})`);
  const escaped = ' {"value":"\\u00e9\\ud83d\\ude00"} ';
  expect(parseBoundedJson(escaped, { maxBytes: escaped.length })).toEqual({ value: "é😀" });
  rejects(() => parseBoundedJson(escaped, { maxBytes: escaped.length - 1 }), "$", `response byte limit exceeded (${escaped.length - 1})`);
  rejects(() => parseBoundedJson('{"value":"\\ud800\udc00"}'), "$", "unpaired Unicode surrogate");
});


it("exposes authenticated, stable reader and decoder application-site counters", () => {
  const raw = parseBoundedJson(document());
  const first = inspectValidated(decodeAst(raw));
  const second = inspectValidated(decodeAst(raw));
  expect(first.applicationAllocations).toBeGreaterThan(0);
  expect(second).toEqual(first);
  expect(Object.isFrozen(first)).toBe(true);
  rejects(() => inspectValidated({ nodes: [] }), "$", "expected authenticated validated AST");
});
