/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
import { expect, test, vi } from "vitest";
import {
  parseConditionLiteral,
  serializeConditionLiteral,
} from "../src/client/condition-literals.js";
import {
  parseDomainSource,
  serializeDomainTree,
} from "../src/client/domain-editor-codec.js";
import {
  parseExpressionSource,
  serializeExpressionTree,
} from "../src/client/expression-editor-codec.js";
import {
  appendConditionNode,
  type ConditionNode,
  nestConditionNode,
  removeConditionNode,
  replaceConditionNode,
  validateConditionTree,
} from "../src/client/condition-tree.js";

const integer = (value: string) => ({ kind: "integer" as const, value });
const condition = (
  id: string,
  path = "amount",
  value = integer("1"),
): ConditionNode => ({ id, kind: "condition", path, operator: "=", value });
function domain(source: string): ConditionNode {
  const result = parseDomainSource(source);
  expect(result.status).toBe("supported");
  if (result.status !== "supported") throw new Error(result.error.message);
  return result.tree;
}
function expression(source: string): ConditionNode {
  const result = parseExpressionSource(source, ["active", "amount", "name"]);
  expect(result.status).toBe("supported");
  if (result.status !== "supported") throw new Error(result.error.message);
  return result.tree;
}
function shape(node: ConditionNode): unknown {
  const { id: _id, ...rest } = node;
  if (node.kind === "and" || node.kind === "or") {
    return { kind: node.kind, children: node.children.map(shape) };
  }
  if (node.kind === "not") return { kind: "not", child: shape(node.child) };
  return rest;
}

/// (partial: bounded authoring codec, prefix arity, implicit AND and constants)
test("domain authoring parses prefix, implicit conjunction, NOT and canonical constants", () => {
  expect(
    shape(
      domain(
        "['|', ('name', '=', 'A'), ('amount', '>', 0), '!', ('active', '=', False)]",
      ),
    ),
  ).toEqual({
    kind: "and",
    children: [{
      kind: "or",
      children: [{
        kind: "condition",
        path: "name",
        operator: "=",
        value: { kind: "string", value: "A" },
      }, {
        kind: "condition",
        path: "amount",
        operator: ">",
        value: integer("0"),
      }],
    }, {
      kind: "not",
      child: {
        kind: "condition",
        path: "active",
        operator: "=",
        value: { kind: "boolean", value: false },
      },
    }],
  });
  expect(shape(domain("[]"))).toEqual({ kind: "and", children: [] });
  expect(shape(domain("[(1, '=', 1)]"))).toEqual({
    kind: "constant",
    value: true,
  });
  expect(shape(domain("[(0, '=', 1)]"))).toEqual({
    kind: "constant",
    value: false,
  });
  expect(serializeDomainTree(domain("['!', ('amount', '<', 3)]"))).toBe(
    "['!', ('amount', '<', 3)]",
  );
});

test("domain values preserve exact integers, finite float identity, tuples, false and None semantics", () => {
  const tree = domain(
    "[('amount', 'in', [-9223372036854775808, 9223372036854775807, 1.0, False, None, ('é\\n',)])]",
  );
  expect(shape(tree)).toEqual({
    kind: "condition",
    path: "amount",
    operator: "in",
    value: {
      kind: "list",
      items: [
        integer("-9223372036854775808"),
        integer("9223372036854775807"),
        { kind: "float", value: 1 },
        { kind: "boolean", value: false },
        { kind: "boolean", value: false },
        { kind: "tuple", items: [{ kind: "string", value: "é\n" }] },
      ],
    },
  });
  expect(serializeDomainTree(tree)).toBe(
    "[('amount', 'in', [-9223372036854775808, 9223372036854775807, 1.0, False, False, ('é\\n',)])]",
  );
});

test("malformed and unsupported domains retain the exact source and precise errors", () => {
  const examples = [
    [
      "['|', ('x', '=', 1)]",
      "invalid",
      "syntax",
      "Domain connector | requires two operands.",
    ],
    [
      "[('x', '=', 9223372036854775808)]",
      "invalid",
      "integer",
      "Integer is outside the signed 64-bit range.",
    ],
    [
      "[('x', 'any!', [])]",
      "invalid",
      "operator",
      "Internal domain operator any! is forbidden.",
    ],
    [
      "[('x', 'child_of', 1)]\r\n",
      "unsupported",
      "operator",
      "Domain operator child_of has no authoring control in a later release.",
    ],
    [
      "[('x', '=', context.get('x'))]",
      "unsupported",
      "grammar",
      "Dynamic operands are outside the authoring grammar.",
    ],
    ["[('x', '=', 1e999)]", "invalid", "number", "Float must be finite."],
    [
      "[('x', '=', '\\q')]",
      "invalid",
      "escape",
      "Unsupported string escape \\q.",
    ],
    [
      "[('x', '=', 1)] trailing",
      "invalid",
      "syntax",
      "Unexpected text after the domain.",
    ],
  ] as const;
  for (const [source, status, _code, message] of examples) {
    expect(parseDomainSource(source)).toMatchObject({
      source,
      status,
      error: {
        kind: status === "invalid" ? "validation" : "unsupported",
        message,
      },
    });
  }
});

/// (partial: precedence, literal distinctions, exact bare-field semantics and bounded subset)
test("expression authoring preserves operand semantics and precedence without evaluation", () => {
  const fetch = vi.spyOn(globalThis, "fetch");
  expect(shape(expression("active or amount > 1 and not name"))).toEqual({
    kind: "or",
    children: [{ kind: "field", path: "active" }, {
      kind: "and",
      children: [{
        kind: "condition",
        path: "amount",
        operator: ">",
        value: integer("1"),
      }, { kind: "not", child: { kind: "field", path: "name" } }],
    }],
  });
  expect(serializeExpressionTree(expression("not (active or name == None)")))
    .toBe("not (active or name == None)");
  expect(shape(expression("amount == 0 or amount == False"))).toEqual({
    kind: "or",
    children: [{
      kind: "condition",
      path: "amount",
      operator: "==",
      value: integer("0"),
    }, {
      kind: "condition",
      path: "amount",
      operator: "==",
      value: { kind: "boolean", value: false },
    }],
  });
  expect(serializeExpressionTree(expression("amount in (1, 2)"))).toBe(
    "amount in (1, 2)",
  );
  expect(serializeExpressionTree(expression("True"))).toBe("True");
  expect(fetch).not.toHaveBeenCalled();
  fetch.mockRestore();
});

test("expression rejects undeclared and relational paths and preserves unsupported text", () => {
  for (
    const source of [
      "context.get('x')",
      "amount + 1",
      "1 < amount",
      "amount < 2 < 3",
      "[x for x in amount]",
    ]
  ) {
    const result = parseExpressionSource(source, ["amount"]);
    expect(result).toMatchObject({
      source,
      status: "unsupported",
      error: {
        kind: "unsupported",
        message: "Expression is outside the authoring grammar.",
      },
    });
  }
  expect(parseExpressionSource("secret", ["amount"])).toMatchObject({
    source: "secret",
    status: "unsupported",
    error: {
      kind: "unsupported",
      message: "Expression field secret is not declared for this editor.",
    },
  });
  expect(parseExpressionSource("partner.name", ["partner.name"])).toMatchObject(
    {
      source: "partner.name",
      status: "unsupported",
      error: {
        kind: "unsupported",
        message: "Expression authoring does not traverse relations.",
      },
    },
  );
});

/// (partial: immutable node replacement, nesting, insertion and pruning)
test("pure tree operations preserve identities, order and prune emptied ancestors", () => {
  const a = condition("a"), b = condition("b"), c = condition("c");
  const root: ConditionNode = { id: "root", kind: "and", children: [a, b] };
  const nested = nestConditionNode(root, "b", "group");
  expect(nested).toEqual({
    id: "root",
    kind: "and",
    children: [a, { id: "group", kind: "or", children: [b] }],
  });
  const appended = appendConditionNode(nested, "group", c);
  expect(appended).toEqual({
    id: "root",
    kind: "and",
    children: [a, { id: "group", kind: "or", children: [b, c] }],
  });
  expect(root.children).toEqual([a, b]);
  expect(removeConditionNode(removeConditionNode(appended, "b"), "c")).toEqual({
    id: "root",
    kind: "and",
    children: [a],
  });
  expect(removeConditionNode(removeConditionNode(root, "a"), "b")).toEqual({
    id: "root",
    kind: "and",
    children: [],
  });
  const replacement = { ...a, path: "name" };
  expect(replaceConditionNode(root, "a", replacement)).toEqual({
    id: "root",
    kind: "and",
    children: [replacement, b],
  });
});

test("tree admission rejects cycles, duplicate IDs, missing targets and output overflow without mutation", () => {
  const cyclic: { id: string; kind: "not"; child?: unknown } = {
    id: "x",
    kind: "not",
  };
  cyclic.child = cyclic;
  expect(() => validateConditionTree(cyclic as ConditionNode)).toThrowError(
    expect.objectContaining({
      kind: "validation",
      message: "Condition tree contains a cycle.",
    }),
  );
  const root: ConditionNode = {
    id: "root",
    kind: "and",
    children: [condition("a")],
  };
  expect(() => appendConditionNode(root, "root", condition("a"))).toThrowError(
    expect.objectContaining({
      kind: "validation",
      message: "Condition node IDs must be unique: a.",
    }),
  );
  expect(() => removeConditionNode(root, "missing")).toThrowError(
    expect.objectContaining({
      kind: "validation",
      message: "Condition node missing does not exist.",
    }),
  );
  expect(() => serializeDomainTree(root, { outputBytes: 2 })).toThrowError(
    expect.objectContaining({
      kind: "validation",
      message: "Authored output exceeds 2 UTF-8 bytes.",
    }),
  );
  expect(root.children).toEqual([condition("a")]);
});

test("codec safety limits admit exact bytes and reject one-over bytes, nodes and depth", () => {
  expect(parseDomainSource("[]", { sourceBytes: 2 }).status).toBe("supported");
  expect(parseDomainSource("[] ", { sourceBytes: 2 })).toMatchObject({
    status: "invalid",
    error: {
      kind: "validation",
      message: "Authored source exceeds 2 UTF-8 bytes.",
    },
  });
  expect(parseDomainSource("[('x', '=', 'é')]", { sourceBytes: 16 }))
    .toMatchObject({
      status: "invalid",
      error: {
        kind: "validation",
        message: "Authored source exceeds 16 UTF-8 bytes.",
      },
    });
  expect(() =>
    validateConditionTree({ id: "r", kind: "not", child: condition("a") }, {
      nodes: 1,
    })
  ).toThrowError(
    expect.objectContaining({
      kind: "validation",
      message: "Condition tree exceeds 1 nodes.",
    }),
  );
  expect(() =>
    validateConditionTree({ id: "r", kind: "not", child: condition("a") }, {
      depth: 1,
    })
  ).toThrowError(
    expect.objectContaining({
      kind: "validation",
      message: "Condition tree exceeds 1 levels.",
    }),
  );
  expect(() => parseDomainSource("[]", { nodes: 1025 })).toThrowError(
    expect.objectContaining({
      kind: "validation",
      message: "Editor nodes limit must be an integer from 1 to 1024.",
    }),
  );
});

test("literal controls share strict conversion without losing numeric identity", () => {
  expect(parseConditionLiteral("9223372036854775807")).toEqual(
    integer("9223372036854775807"),
  );
  expect(parseConditionLiteral("1.0")).toEqual({ kind: "float", value: 1 });
  expect(parseConditionLiteral("None")).toEqual({ kind: "none" });
  expect(serializeConditionLiteral({ kind: "tuple", items: [integer("1")] }))
    .toBe("(1,)");
  expect(() => parseConditionLiteral("9223372036854775808")).toThrowError(
    expect.objectContaining({
      kind: "validation",
      message: "Integer is outside the signed 64-bit range.",
    }),
  );
  expect(() => parseConditionLiteral("01")).toThrowError(
    expect.objectContaining({
      kind: "validation",
      message: "Malformed numeric literal.",
    }),
  );
  expect(() => parseConditionLiteral("True trailing")).toThrowError(
    expect.objectContaining({
      kind: "validation",
      message: "Unexpected text after the literal.",
    }),
  );
});

test("refused source has no tree and safety failures carry an admission marker", () => {
  const sources = [
    "['|']",
    "[('secret', 'any!', [])]",
    "[('x', '=', user.id)]",
    "[('x', '=', 9223372036854775808)]",
  ];
  for (const source of sources) {
    const result = parseDomainSource(source);
    expect(result.source).toBe(source);
    expect(result.status).not.toBe("supported");
    expect(Object.hasOwn(result, "tree")).toBe(false);
  }
  const result = parseDomainSource("[('x', '=', [1, 2, 3])]", { nodes: 3 });
  expect(result).toMatchObject({
    status: "invalid",
    source: "[('x', '=', [1, 2, 3])]",
    error: {
      kind: "validation",
      admission: true,
      message: "Condition tree exceeds 3 nodes.",
    },
  });
  expect(parseExpressionSource("not not active", ["active"], { depth: 2 }))
    .toMatchObject({
      status: "invalid",
      error: {
        kind: "validation",
        admission: true,
        message: "Condition tree exceeds 2 levels.",
      },
    });
  expect(parseDomainSource("['|']")).toMatchObject({
    status: "invalid",
    error: {
      kind: "validation",
      message: "Domain connector | requires two operands.",
    },
  });
});

test("literal collections and trees reject hidden unknowns, wrong types and cyclic values", () => {
  const collection: { kind: "list"; items: unknown[] } = {
    kind: "list",
    items: [],
  };
  collection.items.push(collection);
  expect(() => serializeConditionLiteral(collection as never)).toThrowError(
    expect.objectContaining({
      kind: "validation",
      message: "Condition tree contains a cycle.",
    }),
  );
  expect(() =>
    validateConditionTree({
      id: "r",
      kind: "constant",
      value: false,
      injected: true,
    })
  ).toThrowError(
    expect.objectContaining({
      kind: "validation",
      message: "Unknown condition tree entry key: injected.",
    }),
  );
  expect(() => validateConditionTree({ id: "r", kind: "constant", value: 0 }))
    .toThrowError(
      expect.objectContaining({
        kind: "validation",
        message: "Condition constant must contain a boolean.",
      }),
    );
  expect(() =>
    validateConditionTree({ id: "r", kind: "and", children: [undefined] })
  ).toThrowError(
    expect.objectContaining({
      kind: "validation",
      message: "Condition tree entries must be objects.",
    }),
  );
  expect(() => serializeDomainTree({ id: "r", kind: "field", path: "name" }))
    .toThrowError(
      expect.objectContaining({
        kind: "validation",
        message: "Bare field operands cannot be authored as domains.",
      }),
    );
  expect(() => serializeExpressionTree(condition("r"))).toThrowError(
    expect.objectContaining({
      kind: "unsupported",
      message: "Expression operator = is outside the authoring grammar.",
    }),
  );
});

test("canonical output admits exact bytes, Unicode escaping and preserves false group semantics", () => {
  expect(
    serializeDomainTree({ id: "r", kind: "and", children: [] }, {
      outputBytes: 2,
    }),
  ).toBe("[]");
  expect(serializeExpressionTree({ id: "r", kind: "or", children: [] })).toBe(
    "False",
  );
  expect(parseConditionLiteral("'a\\x41\\u03a9\\U0001f600\\r\\n'")).toEqual({
    kind: "string",
    value: "aAΩ😀\r\n",
  });
  expect(serializeConditionLiteral({ kind: "float", value: -0 })).toBe("-0.0");
  expect(() =>
    serializeConditionLiteral({ kind: "string", value: "é" }, {
      outputBytes: 3,
    })
  ).toThrowError(
    expect.objectContaining({
      kind: "validation",
      message: "Authored output exceeds 3 UTF-8 bytes.",
    }),
  );
});

test("caller tree and literal admission never invokes own accessors", () => {
  const getter = vi.fn(() => "constant");
  const withKind = {
    id: "r",
    get kind() {
      return getter();
    },
    value: true,
  };
  let failure: unknown;
  try {
    validateConditionTree(withKind);
  } catch (error) {
    failure = error;
  }
  expect(getter).not.toHaveBeenCalled();
  expect(failure).toMatchObject({
    kind: "validation",
    message: "Condition tree entry must contain data properties.",
  });
  const idGetter = vi.fn(() => "r");
  const withId = {
    get id() {
      return idGetter();
    },
    kind: "constant",
    value: true,
  };
  failure = undefined;
  try {
    validateConditionTree(withId);
  } catch (error) {
    failure = error;
  }
  expect(idGetter).not.toHaveBeenCalled();
  expect(failure).toMatchObject({
    kind: "validation",
    message: "Condition tree entry must contain data properties.",
  });
  const literalGetter = vi.fn(() => "none");
  failure = undefined;
  try {
    serializeConditionLiteral({
      get kind() {
        return literalGetter();
      },
    } as never);
  } catch (error) {
    failure = error;
  }
  expect(literalGetter).not.toHaveBeenCalled();
  expect(failure).toMatchObject({
    kind: "validation",
    message: "Condition tree entry must contain data properties.",
  });
});

test("collection admission refuses item accessors without invoking them", () => {
  const getter = vi.fn(() => condition("a"));
  const children: ConditionNode[] = [];
  Object.defineProperty(children, "0", { enumerable: true, get: getter });
  expect(() => validateConditionTree({ id: "r", kind: "and", children }))
    .toThrowError(
      expect.objectContaining({
        kind: "validation",
        message: "Condition collections must be dense data arrays.",
      }),
    );
  expect(getter).not.toHaveBeenCalled();
  const literalGetter = vi.fn(() => ({ kind: "none" }));
  const items: unknown[] = [];
  Object.defineProperty(items, "0", { enumerable: true, get: literalGetter });
  expect(() => serializeConditionLiteral({ kind: "list", items } as never))
    .toThrowError(
      expect.objectContaining({
        kind: "validation",
        message: "Condition collections must be dense data arrays.",
      }),
    );
  expect(literalGetter).not.toHaveBeenCalled();
});

test("source and field-list boundary refuses wrong runtime types and sparse choices", () => {
  expect(() => parseDomainSource(undefined as never)).toThrowError(
    expect.objectContaining({
      kind: "validation",
      message: "Authored source must be text.",
    }),
  );
  expect(() => parseExpressionSource(1 as never, ["x"])).toThrowError(
    expect.objectContaining({
      kind: "validation",
      message: "Authored source must be text.",
    }),
  );
  expect(() => parseConditionLiteral(null as never)).toThrowError(
    expect.objectContaining({
      kind: "validation",
      message: "Authored source must be text.",
    }),
  );
  expect(() => parseExpressionSource("x", new Array<string>(1))).toThrowError(
    expect.objectContaining({
      kind: "validation",
      message:
        "Expression fields must be an array of at most 1024 technical names.",
    }),
  );
});

test("domain final admission counts inclusive condition and literal nodes without syntax overhead", () => {
  expect(parseDomainSource("[('x', '=', 1)]", { nodes: 2, depth: 2 }).status)
    .toBe("supported");
  expect(
    parseDomainSource("[('x', 'in', [1, 2])]", { nodes: 4, depth: 3 }).status,
  ).toBe("supported");
  expect(parseDomainSource("[('x', '=', 1)]", { nodes: 1 })).toMatchObject({
    status: "invalid",
    error: {
      kind: "validation",
      admission: true,
      message: "Condition tree exceeds 1 nodes.",
    },
  });
  expect(parseExpressionSource("(((active)))", ["active"], { nodes: 1 }).status)
    .toBe("supported");
});

test("grouping shells do not consume represented literal or expression nodes", () => {
  expect(parseConditionLiteral("((1))", { nodes: 1 })).toEqual(integer("1"));
  expect(
    parseExpressionSource("amount == (1)", ["amount"], { nodes: 2 }).status,
  ).toBe("supported");
  expect(
    parseExpressionSource("amount in (1,)", ["amount"], { nodes: 3 }).status,
  ).toBe("supported");
});

test("expression serialization refuses reserved field identities without emitting source", () => {
  const reserved = [
    "False",
    "None",
    "True",
    "and",
    "as",
    "assert",
    "async",
    "await",
    "break",
    "class",
    "continue",
    "def",
    "del",
    "elif",
    "else",
    "except",
    "finally",
    "for",
    "from",
    "global",
    "if",
    "import",
    "in",
    "is",
    "lambda",
    "nonlocal",
    "not",
    "or",
    "pass",
    "raise",
    "return",
    "try",
    "while",
    "with",
    "yield",
    "__debug__",
  ];
  for (const path of reserved) {
    const nodes: readonly ConditionNode[] = [
      { id: "r", kind: "field", path },
      { id: "r", kind: "condition", path, operator: "==", value: integer("1") },
    ];
    for (const node of nodes) {
      let emitted: string | undefined;
      let failure: unknown;
      try {
        emitted = serializeExpressionTree(node);
      } catch (error) {
        failure = error;
      }
      expect(emitted).toBeUndefined();
      expect(failure).toMatchObject({
        kind: "unsupported",
        message:
          `Expression field ${path} is a reserved identifier and cannot be authored.`,
      });
    }
  }
  expect(
    serializeExpressionTree({ id: "r", kind: "field", path: "class_name" }),
  ).toBe("class_name");
});

test("declared reserved expression fields remain unsupported without becoming constants", () => {
  for (const source of ["None", "class", "__debug__", "for == 1", "is == 1"]) {
    const result = parseExpressionSource(source, [source.split(" ")[0]!]);
    expect(result).toMatchObject({
      status: "unsupported",
      source,
      error: {
        kind: "unsupported",
        message: `Expression field ${
          source.split(" ")[0]
        } is a reserved identifier and cannot be authored.`,
      },
    });
    expect(Object.hasOwn(result, "tree")).toBe(false);
  }
  expect(shape(expression("True"))).toEqual({ kind: "constant", value: true });
  expect(shape(expression("False"))).toEqual({
    kind: "constant",
    value: false,
  });
});

test("n-ary domain groups reparse at their represented node budget", () => {
  for (const kind of ["and", "or"] as const) {
    const tree: ConditionNode = {
      id: "r",
      kind,
      children: [
        { id: "a", kind: "constant", value: true },
        { id: "b", kind: "constant", value: false },
        { id: "c", kind: "constant", value: true },
      ],
    };
    const source = serializeDomainTree(tree, { nodes: 4, depth: 2 });
    const result = parseDomainSource(source, { nodes: 4, depth: 2 });
    expect(result.status).toBe("supported");
    if (result.status === "supported") {
      expect(shape(result.tree)).toEqual(shape(tree));
    }
  }
});

test("shallow domain conditions do not acquire binary prefix-chain depth on round trip", () => {
  const source = `[${
    Array.from(
      { length: 32 },
      (_, index) => `('field_${index}', '=', ${index})`,
    ).join(", ")
  }]`;
  const original = parseDomainSource(source);
  expect(original.status).toBe("supported");
  if (original.status !== "supported") throw new Error(original.error.message);
  const serialized = serializeDomainTree(original.tree);
  const reparsed = parseDomainSource(serialized);
  expect(reparsed.status).toBe("supported");
  if (reparsed.status === "supported") {
    expect(shape(reparsed.tree)).toEqual(shape(original.tree));
  }
});

test("expression precedence parentheses do not consume represented tree depth", () => {
  const tree: ConditionNode = {
    id: "r",
    kind: "not",
    child: {
      id: "group",
      kind: "or",
      children: [{ id: "a", kind: "field", path: "a" }, {
        id: "b",
        kind: "field",
        path: "b",
      }],
    },
  };
  const source = serializeExpressionTree(tree, { nodes: 4, depth: 3 });
  expect(source).toBe("not (a or b)");
  const reparsed = parseExpressionSource(source, ["a", "b"], {
    nodes: 4,
    depth: 3,
  });
  expect(reparsed.status).toBe("supported");
  if (reparsed.status === "supported") {
    expect(shape(reparsed.tree)).toEqual(shape(tree));
  }
});

test("round-trip admission fixes retain deep source and represented-tree safety refusals", () => {
  const source = `${"(".repeat(33)}active${")".repeat(33)}`;
  expect(parseExpressionSource(source, ["active"])).toMatchObject({
    source,
    status: "invalid",
    error: {
      kind: "validation",
      admission: true,
      message: "Expression source exceeds 32 parenthesis levels.",
    },
  });
  const tree: ConditionNode = {
    id: "r",
    kind: "not",
    child: {
      id: "g",
      kind: "or",
      children: [{ id: "a", kind: "field", path: "a" }, {
        id: "b",
        kind: "field",
        path: "b",
      }],
    },
  };
  expect(
    parseExpressionSource(serializeExpressionTree(tree), ["a", "b"], {
      depth: 2,
    }),
  ).toMatchObject({
    status: "invalid",
    error: {
      kind: "validation",
      admission: true,
      message: "Condition tree exceeds 2 levels.",
    },
  });
});

test("maximal mixed NOT and precedence groups round trip within represented depth", () => {
  let tree: ConditionNode = { id: "leaf", kind: "field", path: "a" };
  for (let level = 1; level < 32; level++) {
    tree = level % 2 === 1
      ? {
        id: `n${level}`,
        kind: "or",
        children: [tree, { id: `s${level}`, kind: "field", path: "b" }],
      }
      : { id: `n${level}`, kind: "not", child: tree };
  }
  validateConditionTree(tree, { depth: 32 });
  const source = serializeExpressionTree(tree, { depth: 32 });
  const result = parseExpressionSource(source, ["a", "b"], { depth: 32 });
  expect(result.status).toBe("supported");
  if (result.status === "supported") {
    expect(shape(result.tree)).toEqual(shape(tree));
  }
  expect(parseExpressionSource(source, ["a", "b"], { depth: 31 }))
    .toMatchObject({
      status: "invalid",
      source,
      error: {
        kind: "validation",
        admission: true,
        message: "Condition tree exceeds 31 levels.",
      },
    });
});

test("wide canonical constants fit the bounded domain lexical envelope", () => {
  for (const kind of ["and", "or"] as const) {
    const tree: ConditionNode = {
      id: "r",
      kind,
      children: Array.from(
        { length: 7 },
        (_, index) => ({
          id: `c${index}`,
          kind: "constant" as const,
          value: index % 2 === 0,
        }),
      ),
    };
    const source = serializeDomainTree(tree, { nodes: 8, depth: 2 });
    const result = parseDomainSource(source, { nodes: 8, depth: 2 });
    expect(result.status).toBe("supported");
    if (result.status === "supported") {
      expect(shape(result.tree)).toEqual(shape(tree));
    }
  }
});

test("expression lexical envelope admits 32 parentheses around a depth-32 unary tree", () => {
  let tree: ConditionNode = { id: "leaf", kind: "field", path: "active" };
  for (let level = 1; level < 32; level++) {
    tree = { id: `n${level}`, kind: "not", child: tree };
  }
  const source = `${"(".repeat(32)}${
    serializeExpressionTree(tree, { nodes: 32, depth: 32 })
  }${")".repeat(32)}`;
  const result = parseExpressionSource(source, ["active"], {
    nodes: 32,
    depth: 32,
  });
  expect(result.status).toBe("supported");
  if (result.status === "supported") {
    expect(shape(result.tree)).toEqual(shape(tree));
  }
});

test("authoring codecs refuse noncanonical whitespace with exact source and offset", () => {
  for (const character of ["\u00a0", "\ufeff", "\f", "\v", "\u2028", "\u2029", "\u3000"]) {
    const code = character.codePointAt(0)!.toString(16).toUpperCase().padStart(4, "0");
    const examples = [
      { source: `${character}[]`, offset: 0, parse: parseDomainSource },
      { source: `[${character}('name', '=', 'A')]`, offset: 1, parse: parseDomainSource },
      { source: `[]${character}`, offset: 2, parse: parseDomainSource },
      { source: `${character}active`, offset: 0, parse: (source: string) => parseExpressionSource(source, ["active", "name"]) },
      { source: `active${character}or name`, offset: 6, parse: (source: string) => parseExpressionSource(source, ["active", "name"]) },
      { source: `active${character}`, offset: 6, parse: (source: string) => parseExpressionSource(source, ["active", "name"]) },
    ];
    for (const { source, offset, parse } of examples) {
      expect(parse(source)).toEqual({
        status: "invalid",
        source,
        error: {
          kind: "validation",
          message: `Invalid whitespace U+${code} in authored source.`,
          offset,
        },
      });
    }
  }
});

test("authoring codecs keep ASCII whitespace and Unicode string contents", () => {
  const text = "A\u00a0\ufeff\f\v\u2028\u2029\u3000B";
  for (const whitespace of [" ", "\t", "\r", "\n", " \t\r\n"]) {
    const domainSource = `${whitespace}[${whitespace}('name',${whitespace}'=',${whitespace}'${text}')${whitespace}]${whitespace}`;
    expect(parseDomainSource(domainSource)).toMatchObject({
      status: "supported",
      source: domainSource,
      tree: { kind: "condition", path: "name", value: { kind: "string", value: text } },
    });
    const expressionSource = `${whitespace}name${whitespace}==${whitespace}'${text}'${whitespace}`;
    expect(parseExpressionSource(expressionSource, ["name"])).toMatchObject({
      status: "supported",
      source: expressionSource,
      tree: { kind: "condition", path: "name", value: { kind: "string", value: text } },
    });
  }
});

test("bare boolean domain constants preserve server-admitted meaning through serialization", () => {
  // Canonical server constructors accept Value::Bool (crates/erp-domain/src/domain.rs:560-578),
  // and the expression parser recognizes True/False (crates/erp-expr/src/parser.rs:417-418).
  // This authoring test does not send raw strings as JSON-2 domains or execute them.
  for (const [source, value, serialized] of [
    ["True", true, "[(1, '=', 1)]"],
    ["False", false, "[(0, '=', 1)]"],
  ] as const) {
    const tree = domain(source);
    expect(shape(tree)).toEqual({ kind: "constant", value });
    expect(serializeDomainTree(tree)).toBe(serialized);
    expect(shape(domain(serializeDomainTree(tree)))).toEqual(shape(tree));
  }
});
