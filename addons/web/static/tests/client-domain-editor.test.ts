/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
import { afterEach, beforeEach, expect, test, vi } from "vitest";
import { MessageServices } from "../src/client/messages.js";
import { createDomainEditor } from "../src/client/domain-editor.js";
import type { Field } from "../src/client/metadata.js";
import type {
  CodecResult,
  ConditionNode,
  TreeLimits,
} from "../src/client/condition-tree.js";
let messages: MessageServices | undefined;
beforeEach(() => {
  Object.defineProperty(HTMLDialogElement.prototype, "showModal", {
    configurable: true,
    value(this: HTMLDialogElement) {
      this.open = true;
    },
  });
  Object.defineProperty(HTMLDialogElement.prototype, "close", {
    configurable: true,
    value(this: HTMLDialogElement) {
      this.open = false;
    },
  });
});
afterEach(() => {
  messages?.dispose();
  document.body.replaceChildren();
});
function environment() {
  messages?.dispose();
  messages = new MessageServices(document.body, vi.fn());
  return { messages, report: vi.fn() };
}
const field = (name: string, kind: Field["kind"]): Field => ({
  name,
  label: name,
  kind,
  required: false,
  readonly: false,
  relation: null,
  relationField: null,
  selection: null,
});
const fields = () =>
  new Map([["name", field("name", "char")], [
    "active",
    field("active", "boolean"),
  ]]);
const raw = (node: HTMLElement) =>
  node.querySelector<HTMLTextAreaElement>("[data-editor-source]")!;
const reset = (node: HTMLElement) =>
  node.querySelector<HTMLButtonElement>("[data-editor-reset]")!;
const archived = (node: HTMLElement) =>
  node.querySelector<HTMLInputElement>("[data-domain-archived]")!;
function type(node: HTMLElement, value: string) {
  raw(node).value = value;
  raw(node).dispatchEvent(new Event("input", { bubbles: true }));
}
/// (partial: exact raw callbacks, explicit reset and readonly/debug branches)
test("domain raw edits retain exact source and status and reset only on explicit request", async () => {
  const change = vi.fn(),
    editor = createDomainEditor({
      environment: environment(),
      label: "Filter",
      model: "res.partner",
      value: "[('name', 'child_of', 1)]",
      fields,
      debug: true,
      change,
    });
  document.body.append(editor.node);
  await editor.ready;
  expect(change).not.toHaveBeenCalled();
  expect(editor.node.dataset["state"]).toBe("unsupported");
  const source = "[('name', '=', context.get('name'))]\n";
  type(editor.node, source);
  await vi.waitFor(() => expect(change).toHaveBeenCalledTimes(1));
  expect(change.mock.calls[0]![0]).toMatchObject({
    source,
    status: "unsupported",
    error: {
      kind: "unsupported",
      message: "Dynamic operands are outside the authoring grammar.",
    },
  });
  await vi.waitFor(() =>
    expect(editor.node.getAttribute("aria-busy")).toBe("false")
  );
  reset(editor.node).click();
  await vi.waitFor(() => expect(change).toHaveBeenCalledTimes(2));
  expect(change.mock.calls[1]![0]).toMatchObject({
    source: "[]",
    status: "supported",
  });
  expect(archived(editor.node).checked).toBe(false);
  editor.dispose();
});
test("domain readonly debug source is visible only for unsupported input and cannot reset", async () => {
  for (const value of ["[]", "[('name', 'child_of', 1)]"]) {
    const editor = createDomainEditor({
      environment: environment(),
      label: "Filter",
      model: "res.partner",
      value,
      fields,
      readonly: true,
      debug: true,
      change: vi.fn(),
    });
    await editor.ready;
    expect(reset(editor.node)).toBeNull();
    if (value === "[]") expect(raw(editor.node)).toBeNull();
    else expect(raw(editor.node).readOnly).toBe(true);
    editor.dispose();
  }
});
/// (partial: exact archived marker position, duplicates and explicit active preservation)
test("archived inclusion extracts only whole or top AND markers and repeated toggles preserve filters", async () => {
  const change = vi.fn(),
    editor = createDomainEditor({
      environment: environment(),
      label: "Filter",
      model: "res.partner",
      value:
        "[('active', 'in', [True, False]), ('active', 'in', [True, False]), ('name', '=', 'A'), ('active', '=', False)]",
      fields,
      debug: true,
      change,
    });
  await editor.ready;
  expect(archived(editor.node).checked).toBe(true);
  archived(editor.node).checked = false;
  archived(editor.node).dispatchEvent(new Event("change"));
  await vi.waitFor(() => expect(change).toHaveBeenCalledTimes(1));
  expect(change.mock.calls[0]![0].source).toBe(
    "['&', ('name', '=', 'A'), ('active', '=', False)]",
  );
  await vi.waitFor(() =>
    expect(editor.node.getAttribute("aria-busy")).toBe("false")
  );
  archived(editor.node).checked = true;
  archived(editor.node).dispatchEvent(new Event("change"));
  await vi.waitFor(() => expect(change).toHaveBeenCalledTimes(2));
  expect(change.mock.calls[1]![0].source.match(/'in'/g)).toHaveLength(1);
  for (
    const source of [
      "['|', ('active', 'in', [True, False]), ('name', '=', 'A')]",
      "['!', ('active', 'in', [True, False])]",
      "[('active', 'in', [False, True])]",
    ]
  ) {
    await editor.setValue(source);
    expect(archived(editor.node).checked).toBe(false);
  }
  expect(change).toHaveBeenCalledTimes(2);
  editor.dispose();
});
test("archive checkbox requires caller-authorized boolean active metadata", async () => {
  for (
    const metadata of [
      new Map<string, Field>(),
      new Map([["active", field("active", "char")]]),
    ]
  ) {
    const editor = createDomainEditor({
      environment: environment(),
      label: "Filter",
      model: "res.partner",
      value: "[]",
      fields: () => metadata,
      change: vi.fn(),
    });
    await editor.ready;
    expect(archived(editor.node)).toBeNull();
    editor.dispose();
  }
});
test("failed callbacks preserve represented source and are not replayed by replacement", async () => {
  const env = environment(),
    failure = new Error("Caller refused draft"),
    change = vi.fn().mockRejectedValue(failure);
  const editor = createDomainEditor({
    environment: env,
    label: "Filter",
    model: "res.partner",
    value: "[]",
    fields,
    debug: true,
    change,
  });
  await editor.ready;
  type(editor.node, "[('name', '=', 'new')]");
  await vi.waitFor(() => expect(env.report).toHaveBeenCalledWith(failure));
  expect(raw(editor.node).value).toBe("[]");
  expect(editor.node.textContent).toContain("Caller refused draft");
  await editor.setValue("[('name', '=', 'owner')]");
  expect(change).toHaveBeenCalledTimes(1);
  expect(raw(editor.node).value).toBe("[('name', '=', 'owner')]");
  editor.dispose();
});
test("parent replacement and disposal suppress pending callback completion", async () => {
  let finish!: () => void;
  const change = vi.fn(() =>
    new Promise<void>((resolve) => {
      finish = resolve;
    })
  );
  const editor = createDomainEditor({
    environment: environment(),
    label: "Filter",
    model: "res.partner",
    value: "[]",
    fields,
    debug: true,
    change,
  });
  await editor.ready;
  type(editor.node, "[('name', '=', 'draft')]");
  await vi.waitFor(() => expect(change).toHaveBeenCalledTimes(1));
  await editor.setValue("[('name', '=', 'owner')]");
  finish();
  await Promise.resolve();
  await Promise.resolve();
  expect(raw(editor.node).value).toBe("[('name', '=', 'owner')]");
  editor.dispose();
  type(editor.node, "[]");
  expect(change).toHaveBeenCalledTimes(1);
});
test("domain options reject unknown keys and wrong policies before creating controls", () => {
  const options = {
    environment: environment(),
    label: "Filter",
    model: "res.partner",
    value: "[]",
    fields,
    change: vi.fn(),
  };
  expect(() =>
    createDomainEditor(Object.assign({}, options, { unexpected: true }))
  ).toThrowError(
    expect.objectContaining({
      kind: "validation",
      message: "Unknown domain editor option: unexpected.",
    }),
  );
  const wrongPolicy = Object.assign({}, options, { debug: "yes" });
  // @ts-expect-error Runtime callers must also receive a precise policy refusal.
  expect(() => createDomainEditor(wrongPolicy)).toThrowError(
    expect.objectContaining({
      kind: "validation",
      message: "Invalid domain editor policy.",
    }),
  );
});
test("domain authoring composes rules and archive state with unsupported reset and readonly debug policies", async () => {
  const execute = vi.spyOn(globalThis, "eval"),
    network = vi.spyOn(globalThis, "fetch");
  try {
    const change = vi.fn(),
      editor = createDomainEditor({
        environment: environment(),
        label: "Filter",
        model: "res.partner",
        value: "[('name', '=', 'A'), ('active', 'in', [True, False])]",
        fields,
        debug: true,
        change,
      });
    document.body.append(editor.node);
    await editor.ready;
    expect(editor.node.querySelector(".editor-tree")).not.toBeNull();
    expect(archived(editor.node).checked).toBe(true);
    const input = editor.node.querySelector<HTMLInputElement>(
      "[data-tree-value]",
    )!;
    input.value = "B";
    input.dispatchEvent(new Event("change"));
    await vi.waitFor(() => expect(change).toHaveBeenCalledTimes(1));
    expect(change.mock.calls[0]![0].source).toBe(
      "['&', ('name', '=', 'B'), ('active', 'in', [True, False])]",
    );
    await editor.setValue("[('name', 'child_of', 1)]");
    expect(editor.node.dataset["state"]).toBe("unsupported");
    expect(editor.node.querySelector("[role=alert]")?.textContent).toBeTruthy();
    expect(raw(editor.node).readOnly).toBe(false);
    await vi.waitFor(() =>
      expect(editor.node.getAttribute("aria-busy")).toBe("false")
    );
    reset(editor.node).click();
    await vi.waitFor(() => expect(change).toHaveBeenCalledTimes(2));
    expect(change.mock.calls[1]![0].source).toBe("[]");
    editor.dispose();
    const readonly = createDomainEditor({
      environment: environment(),
      label: "Readonly filter",
      model: "res.partner",
      value: "[('name', 'child_of', 1)]",
      fields,
      readonly: true,
      debug: true,
      change,
    });
    await readonly.ready;
    expect(raw(readonly.node).readOnly).toBe(true);
    expect(reset(readonly.node)).toBeNull();
    readonly.dispose();
    expect(execute).not.toHaveBeenCalled();
    expect(network).not.toHaveBeenCalled();
  } finally {
    execute.mockRestore();
    network.mockRestore();
  }
});
test("raw source and output admission refusals retain source without empty-domain callbacks", async () => {
  for (
    const [limits, message] of [
      [{ sourceBytes: 8 }, "Authored source exceeds 8 UTF-8 bytes."],
      [{ outputBytes: 8 }, "Authored output exceeds 8 UTF-8 bytes."],
      [{ nodes: 2 }, "Condition tree exceeds 2 nodes."],
      [{ depth: 2 }, "Condition tree exceeds 2 levels."],
    ] as const
  ) {
    const env = environment(),
      change = vi.fn(),
      editor = createDomainEditor({
        environment: env,
        label: "Filter",
        model: "res.partner",
        value: "[]",
        fields,
        debug: true,
        limits,
        change,
      });
    await editor.ready;
    type(editor.node, "[('name', 'in', [[['a long draft']]])]");
    await vi.waitFor(() =>
      expect(env.report).toHaveBeenCalledWith(
        expect.objectContaining({ kind: "validation", message }),
      )
    );
    expect(change).not.toHaveBeenCalled();
    expect(raw(editor.node).value).toBe("[]");
    editor.dispose();
  }
});
test("native raw textarea identity and caret survive accepted typing", async () => {
  const change = vi.fn(),
    editor = createDomainEditor({
      environment: environment(),
      label: "Filter",
      model: "res.partner",
      value: "[]",
      fields,
      debug: true,
      change,
    });
  await editor.ready;
  const control = raw(editor.node);
  control.value = "[('name', '=', 'A')]";
  control.setSelectionRange(3, 3);
  control.dispatchEvent(new Event("input"));
  await vi.waitFor(() => expect(change).toHaveBeenCalledTimes(1));
  expect(raw(editor.node)).toBe(control);
  expect(control.selectionStart).toBe(3);
  editor.dispose();
});
test("accepted raw input never removes the live textarea from its mounted editor", async () => {
  const change = vi.fn(),
    editor = createDomainEditor({
      environment: environment(),
      label: "Filter",
      model: "res.partner",
      value: "[]",
      fields,
      debug: true,
      change,
    });
  document.body.append(editor.node);
  await editor.ready;
  const control = raw(editor.node), removed: Node[] = [];
  const observer = new MutationObserver((records) => {
    for (const record of records) removed.push(...record.removedNodes);
  });
  observer.observe(editor.node, { childList: true, subtree: true });
  try {
    control.focus();
    control.value = "[('name', '=', 'A')]";
    control.dispatchEvent(new Event("input"));
    await vi.waitFor(() =>
      expect(editor.node.getAttribute("aria-busy")).toBe("false")
    );
    expect(change).toHaveBeenCalledTimes(1);
    expect(removed.some((node) => node === control || node.contains(control)))
      .toBe(false);
    expect(raw(editor.node)).toBe(control);
    expect(document.activeElement).toBe(control);
  } finally {
    observer.disconnect();
    editor.dispose();
  }
});

async function editFlatConjunction(
  count: number,
  limits?: TreeLimits,
): Promise<void> {
  const env = environment();
  const change = vi.fn<(result: CodecResult) => void>();
  const value = `[${
    Array.from(
      { length: count },
      (_, index) => `('name', '=', 'value${index}')`,
    ).join(", ")
  }]`;
  const editor = createDomainEditor({
    environment: env,
    label: "Flat conjunction",
    model: "res.partner",
    value,
    fields,
    ...(limits ? { limits } : {}),
    change,
  });
  document.body.append(editor.node);
  try {
    await editor.ready;
    expect(editor.node.dataset["state"]).toBe("supported");
    expect(editor.node.querySelectorAll("[data-tree-value]")).toHaveLength(
      count,
    );
    expect(change).not.toHaveBeenCalled();
    const control = editor.node.querySelector<HTMLInputElement>(
      "input[data-tree-value]",
    )!;
    control.value = "changed";
    control.dispatchEvent(new Event("change"));
    await vi.waitFor(() => expect(change).toHaveBeenCalledTimes(1));
    await vi.waitFor(() => {
      expect(editor.node.getAttribute("aria-busy")).toBe("false");
      expect(
        editor.node.querySelector(".editor-tree")?.getAttribute("aria-busy"),
      ).toBe("false");
    });
    const authored = change.mock.calls[0]![0];
    expect(authored.status).toBe("supported");
    if (authored.status !== "supported") {
      throw new Error(authored.error.message);
    }
    const conditions: ConditionNode[] = [];
    const visit = (node: ConditionNode): void => {
      if (node.kind === "and") node.children.forEach(visit);
      else conditions.push(node);
    };
    visit(authored.tree);
    expect(conditions).toHaveLength(count);
    expect(
      conditions.map((node) =>
        node.kind === "condition"
          ? { path: node.path, operator: node.operator, value: node.value }
          : node
      ),
    ).toEqual(
      Array.from(
        { length: count },
        (_, index) => ({
          path: "name",
          operator: "=",
          value: {
            kind: "string",
            value: index === 0 ? "changed" : `value${index}`,
          },
        }),
      ),
    );
    expect(
      [...editor.node.querySelectorAll("[role=alert]")].map((alert) =>
        alert.textContent?.trim()
      ).filter(Boolean),
    ).toEqual([]);
    expect(env.report).not.toHaveBeenCalled();
    expect(change).toHaveBeenCalledTimes(1);
  } finally {
    editor.dispose();
  }
}

test("structured editing preserves all 32 shallow AND conditions through source serialization", async () => {
  await editFlatConjunction(32);
});

test("structured editing preserves a flat conjunction at its lowered tree node limit", async () => {
  await editFlatConjunction(3, { nodes: 7 });
});

function pendingOwner() {
  const calls: { resolve(): void; reject(error: unknown): void }[] = [];
  const change = vi.fn((_result: CodecResult) => new Promise<void>((resolve, reject) => {
    calls.push({ resolve, reject });
  }));
  return { calls, change };
}
test("raw domain drafts stay editable and coalesce to the latest source after a pending owner settles", async () => {
  const owner = pendingOwner(), env = environment();
  const editor = createDomainEditor({ environment: env, label: "Filter", model: "res.partner", value: "[]", fields, debug: true, change: owner.change });
  document.body.append(editor.node);
  await editor.ready;
  const textarea = raw(editor.node);
  type(editor.node, "[('name', '=', 'A')]");
  await vi.waitFor(() => expect(owner.change).toHaveBeenCalledTimes(1));
  expect(textarea.readOnly).toBe(false);
  type(editor.node, "[('name', '=', 'B')]");
  type(editor.node, "[('name', '=', 'C')]");
  expect(owner.change).toHaveBeenCalledTimes(1);
  owner.calls[0]!.resolve();
  await vi.waitFor(() => expect(owner.change).toHaveBeenCalledTimes(2));
  expect(owner.change.mock.calls.map(([result]) => result.source)).toEqual(["[('name', '=', 'A')]", "[('name', '=', 'C')]"]);
  expect(raw(editor.node)).toBe(textarea);
  expect(textarea.value).toBe("[('name', '=', 'C')]");
  owner.calls[1]!.resolve();
  await vi.waitFor(() => expect(editor.node.getAttribute("aria-busy")).toBe("false"));
  expect(textarea.value).toBe("[('name', '=', 'C')]");
  expect(env.report).not.toHaveBeenCalled();
  editor.dispose();
});
test("a rejected raw owner reports once while a newer user draft is retained and published", async () => {
  const owner = pendingOwner(), env = environment(), failure = new Error("Owner refused A");
  const editor = createDomainEditor({ environment: env, label: "Filter", model: "res.partner", value: "[]", fields, debug: true, change: owner.change });
  await editor.ready;
  type(editor.node, "[('name', '=', 'A')]");
  await vi.waitFor(() => expect(owner.change).toHaveBeenCalledTimes(1));
  type(editor.node, "[('name', '=', 'C')]");
  owner.calls[0]!.reject(failure);
  await vi.waitFor(() => expect(owner.change).toHaveBeenCalledTimes(2));
  expect(raw(editor.node).value).toBe("[('name', '=', 'C')]");
  expect(env.report).toHaveBeenCalledExactlyOnceWith(failure);
  expect(owner.change.mock.calls[1]![0].source).toBe("[('name', '=', 'C')]");
  owner.calls[1]!.resolve();
  await vi.waitFor(() => expect(editor.node.getAttribute("aria-busy")).toBe("false"));
  expect(env.report).toHaveBeenCalledTimes(1);
  editor.dispose();
});
test("owner replacement discards old queued drafts and serializes a new generation draft", async () => {
  const owner = pendingOwner(), env = environment();
  const editor = createDomainEditor({ environment: env, label: "Filter", model: "res.partner", value: "[]", fields, debug: true, change: owner.change });
  await editor.ready;
  type(editor.node, "[('name', '=', 'old A')]");
  await vi.waitFor(() => expect(owner.change).toHaveBeenCalledTimes(1));
  type(editor.node, "[('name', '=', 'old B')]");
  await editor.setValue("[('name', '=', 'owner')]");
  type(editor.node, "[('name', '=', 'new C')]");
  expect(owner.change).toHaveBeenCalledTimes(1);
  owner.calls[0]!.resolve();
  await vi.waitFor(() => expect(owner.change).toHaveBeenCalledTimes(2));
  expect(owner.change.mock.calls[1]![0].source).toBe("[('name', '=', 'new C')]");
  expect(raw(editor.node).value).toBe("[('name', '=', 'new C')]");
  owner.calls[1]!.resolve();
  await vi.waitFor(() => expect(editor.node.getAttribute("aria-busy")).toBe("false"));
  expect(env.report).not.toHaveBeenCalled();
  editor.dispose();
});
test("abort discards queued raw drafts and suppresses a late rejected callback", async () => {
  const owner = pendingOwner(), env = environment(), abort = new AbortController();
  const editor = createDomainEditor({ environment: { ...env, signal: abort.signal }, label: "Filter", model: "res.partner", value: "[]", fields, debug: true, change: owner.change });
  await editor.ready;
  type(editor.node, "[('name', '=', 'A')]");
  await vi.waitFor(() => expect(owner.change).toHaveBeenCalledTimes(1));
  type(editor.node, "[('name', '=', 'B')]");
  abort.abort();
  const before = editor.node.innerHTML;
  owner.calls[0]!.reject(new Error("Late rejection"));
  await Promise.resolve(); await Promise.resolve(); await Promise.resolve();
  expect(owner.change).toHaveBeenCalledTimes(1);
  expect(env.report).not.toHaveBeenCalled();
  expect(editor.node.innerHTML).toBe(before);
});
test("over-limit input preserves an admitted pending raw draft without a callback", async () => {
  const owner = pendingOwner(), env = environment();
  const editor = createDomainEditor({ environment: env, label: "Filter", model: "res.partner", value: "[]", fields, debug: true, limits: { sourceBytes: 64 }, change: owner.change });
  await editor.ready;
  type(editor.node, "[('name', '=', 'A')]");
  await vi.waitFor(() => expect(owner.change).toHaveBeenCalledTimes(1));
  type(editor.node, "[('name', '=', 'B')]");
  type(editor.node, "x".repeat(65));
  expect(raw(editor.node).value).toBe("[('name', '=', 'B')]");
  expect(env.report).toHaveBeenCalledExactlyOnceWith(expect.objectContaining({ kind: "validation", message: "Authored source exceeds 64 UTF-8 bytes." }));
  owner.calls[0]!.resolve();
  await vi.waitFor(() => expect(owner.change).toHaveBeenCalledTimes(2));
  expect(owner.change.mock.calls[1]![0].source).toBe("[('name', '=', 'B')]");
  owner.calls[1]!.resolve();
  await vi.waitFor(() => expect(editor.node.getAttribute("aria-busy")).toBe("false"));
  editor.dispose();
});
test("rejected domain tree edits have exactly one owning alert and one report", async () => {
  const env = environment(), failure = new Error("Owner refused tree edit"), change = vi.fn().mockRejectedValue(failure);
  const editor = createDomainEditor({ environment: env, label: "Filter", model: "res.partner", value: "[('name', '=', 'before')]", fields, debug: true, change });
  document.body.append(editor.node);
  await editor.ready;
  editor.node.querySelector<HTMLButtonElement>('[data-tree-action="nest"]')!.click();
  await vi.waitFor(() => expect(env.report).toHaveBeenCalled());
  expect(env.report).toHaveBeenCalledExactlyOnceWith(failure);
  expect(editor.node.querySelectorAll('[role="alert"]')).toHaveLength(1);
  expect(raw(editor.node).value).toBe("[('name', '=', 'before')]");
  expect(change).toHaveBeenCalledTimes(1);
  editor.dispose();
});

test("a reentrant owner replacement cannot commit its obsolete raw callback", async () => {
  const env = environment();
  let editor!: ReturnType<typeof createDomainEditor>;
  const change = vi.fn(async (_result: CodecResult) => {
    if (change.mock.calls.length === 1) {
      await editor.setValue("[('name', '=', 'replacement')]");
      type(editor.node, "[('name', '=', 'new generation')]");
    }
  });
  editor = createDomainEditor({ environment: env, label: "Filter", model: "res.partner", value: "[]", fields, debug: true, change });
  await editor.ready;
  type(editor.node, "[('name', '=', 'obsolete')]");
  await vi.waitFor(() => expect(change).toHaveBeenCalledTimes(2));
  await vi.waitFor(() => expect(editor.node.getAttribute("aria-busy")).toBe("false"));
  expect(change.mock.calls[1]![0].source).toBe("[('name', '=', 'new generation')]");
  expect(raw(editor.node).value).toBe("[('name', '=', 'new generation')]");
  expect(env.report).not.toHaveBeenCalled();
  editor.dispose();
});
test("disposal prevents a queued raw draft from publishing after active callback success", async () => {
  const owner = pendingOwner(), env = environment();
  const editor = createDomainEditor({ environment: env, label: "Filter", model: "res.partner", value: "[]", fields, debug: true, change: owner.change });
  await editor.ready;
  type(editor.node, "[('name', '=', 'A')]");
  await vi.waitFor(() => expect(owner.change).toHaveBeenCalledTimes(1));
  type(editor.node, "[('name', '=', 'B')]");
  editor.dispose();
  const before = editor.node.innerHTML;
  owner.calls[0]!.resolve();
  await Promise.resolve(); await Promise.resolve(); await Promise.resolve();
  expect(owner.change).toHaveBeenCalledTimes(1);
  expect(env.report).not.toHaveBeenCalled();
  expect(editor.node.innerHTML).toBe(before);
});
test("a queued raw draft waits for the rejected tree operation to report before redraw", async () => {
  const env = environment(), failure = new Error("Owner refused tree A");
  let reject!: (reason: unknown) => void;
  const change = vi.fn((_result: CodecResult): void | Promise<void> => {
    if (change.mock.calls.length === 1) return new Promise<void>((_resolve, fail) => { reject = fail; });
    return undefined;
  });
  const editor = createDomainEditor({ environment: env, label: "Filter", model: "res.partner", value: "[('name', '=', 'before')]", fields, debug: true, change });
  document.body.append(editor.node);
  await editor.ready;
  editor.node.querySelector<HTMLButtonElement>('[data-tree-action="nest"]')!.click();
  await vi.waitFor(() => expect(change).toHaveBeenCalledTimes(1));
  type(editor.node, "[('name', '=', 'raw B')]");
  reject(failure);
  await vi.waitFor(() => expect(change).toHaveBeenCalledTimes(2));
  await vi.waitFor(() => expect(editor.node.getAttribute("aria-busy")).toBe("false"));
  expect(env.report).toHaveBeenCalledExactlyOnceWith(failure);
  expect(change.mock.calls[1]![0].source).toBe("[('name', '=', 'raw B')]");
  expect(raw(editor.node).value).toBe("[('name', '=', 'raw B')]");
  editor.dispose();
});

/// (partial: queued raw drafts survive archive-triggered tree replacement)
test("raw drafts publish after a second tree edit following archive replacement of a busy tree", async () => {
  const env = environment(), owner = pendingOwner();
  const related = new Map([["name", field("name", "char")]]);
  const root = fields();
  root.set("partner_id", { ...field("partner_id", "many2one"), relation: "res.partner.related" });
  let holdMetadata = false;
  const metadataWaiters: (() => void)[] = [];
  const metadata = vi.fn((model: string): ReadonlyMap<string, Field> | Promise<ReadonlyMap<string, Field>> => {
    if (model === "res.partner") return root;
    if (!holdMetadata) return related;
    return new Promise((resolve) => metadataWaiters.push(() => resolve(related)));
  });
  const releaseMetadata = () => {
    holdMetadata = false;
    for (const release of metadataWaiters.splice(0)) release();
  };
  const editor = createDomainEditor({ environment: env, label: "Filter", model: "res.partner", value: "[('partner_id.name', '=', 'before')]", fields: metadata, debug: true, change: owner.change });
  document.body.append(editor.node);
  try {
    await editor.ready;
    const textarea = raw(editor.node);
    editor.node.querySelector<HTMLButtonElement>('[data-tree-action="nest"]')!.click();
    await vi.waitFor(() => expect(owner.change).toHaveBeenCalledTimes(1));
    holdMetadata = true;
    owner.calls[0]!.resolve();
    await vi.waitFor(() => {
      expect(editor.node.getAttribute("aria-busy")).toBe("false");
      expect(editor.node.querySelector('.editor-tree')!.getAttribute("aria-busy")).toBe("true");
      expect(metadataWaiters.length).toBeGreaterThan(0);
    });
    const firstDraft = "[('partner_id.name', '=', 'first raw')]";
    type(editor.node, firstDraft);
    expect(owner.change).toHaveBeenCalledTimes(1);
    const checkbox = archived(editor.node);
    expect(checkbox.disabled).toBe(false);
    checkbox.checked = true;
    checkbox.dispatchEvent(new Event("change", { bubbles: true }));
    await vi.waitFor(() => expect(owner.change).toHaveBeenCalledTimes(2));
    releaseMetadata();
    owner.calls[1]!.resolve();
    await vi.waitFor(() => expect(owner.change).toHaveBeenCalledTimes(3));
    expect(owner.change.mock.calls[2]![0].source).toBe(firstDraft);
    owner.calls[2]!.resolve();
    await vi.waitFor(() => expect(editor.node.getAttribute("aria-busy")).toBe("false"));

    editor.node.querySelector<HTMLButtonElement>('[data-tree-action="nest"]')!.click();
    await vi.waitFor(() => expect(owner.change).toHaveBeenCalledTimes(4));
    const secondDraft = "[('partner_id.name', '=', 'second raw')]";
    type(editor.node, secondDraft);
    expect(owner.change).toHaveBeenCalledTimes(4);
    owner.calls[3]!.resolve();
    await vi.waitFor(() => expect(owner.change).toHaveBeenCalledTimes(5));
    expect(owner.change.mock.calls[4]![0].source).toBe(secondDraft);
    owner.calls[4]!.resolve();
    await vi.waitFor(() => expect(editor.node.getAttribute("aria-busy")).toBe("false"));
    expect(raw(editor.node)).toBe(textarea);
    expect(textarea.value).toBe(secondDraft);
    expect(env.report).not.toHaveBeenCalled();
  } finally {
    releaseMetadata();
    editor.dispose();
    for (const call of owner.calls) call.resolve();
  }
});
