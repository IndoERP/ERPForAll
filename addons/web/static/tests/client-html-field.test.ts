/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
import { afterEach, expect, test, vi } from "vitest";
import { RecordForm } from "../src/client/record-form.js";
import { RecordState } from "../src/client/record.js";
import { RpcClient, ClientError } from "../src/client/transport.js";
import { MessageServices } from "../src/client/messages.js";
import type { Field, View } from "../src/client/metadata.js";
import "../src/client/fields.js";
import { renderHtmlField, renderListHtmlField } from "../src/client/html-field.js";
import { decodeAst, parseBoundedJson } from "../../../../web/packages/expr/src/index.js";

const disposers: (() => void)[] = [];
afterEach(() => {
  disposers.splice(0).forEach(stop => stop());
  document.body.replaceChildren();
  vi.restoreAllMocks();
});

function htmlField(extra: Partial<Field> = {}): Field {
  return {
    name: "description",
    kind: "html",
    label: "Description",
    required: false,
    readonly: false,
    selection: null,
    relation: null,
    relationField: null,
    ...extra,
  };
}

async function fixture(
  extra: Partial<Field> = {},
  onchange?: () => Promise<Record<string, unknown>>,
  saved?: (vals: Record<string, unknown>) => Record<string, unknown>[] | Promise<Record<string, unknown>[]>,
  initialValue: unknown = "<p>Initial description</p>",
) {
  const f = htmlField(extra);
  const calls: { method: string; args: Record<string, unknown> }[] = [];
  let stored = initialValue;

  const rpc = new RpcClient(
    async (path, init) => {
      const method = String(path).split("/").at(-1)!;
      const args = JSON.parse(String(init?.body)) as Record<string, unknown>;
      calls.push({ method, args });
      if (method === "onchange") {
        return new Response(JSON.stringify(onchange ? await onchange() : { value: {} }));
      }
      if (method === "web_save") {
        const vals = (args["vals"] as Record<string, unknown>) ?? {};
        if (Object.hasOwn(vals, "description")) stored = vals["description"];
        const record = saved ? await saved(vals) : [{ id: 1, description: stored }];
        return new Response(JSON.stringify(record));
      }
      return new Response(JSON.stringify([{ id: 1, description: stored }]));
    },
    () => {},
  );

  const fields = [f];
  const state = new RecordState(rpc, "test.record", fields, {});
  await state.load(1);
  calls.length = 0;

  const form = document.createElement("form");
  document.body.append(form);
  const view: View = {
    type: "form",
    mode: "record",
    identity: { kind: "stored", id: 1 },
    fields,
    nodes: [{ kind: "root", children: [{ kind: "field", field: f, label: f.label, widget: "html" }] }],
  } as View;

  const report = vi.fn();
  const stage = vi.fn((name: string, value: unknown) => state.change(name, value));
  const messages = new MessageServices(document.body, report);
  disposers.push(() => messages.dispose());
  const surface = new RecordForm({
    form,
    messages,
    state,
    view,
    rpc,
    context: {},
    report,
    run: job => {
      void job().catch(report);
    },
    stage,
    commit: async (name: string) => {
      await state.onchange(name);
      surface.render();
    },
    openRecord: async () => {},
    change: (name, value) => state.change(name, value),
    button: () => {},
  });
  disposers.push(() => surface.dispose());
  surface.render();

  const editorNode = form.querySelector<HTMLElement>(".html-editor")!;
  const editable = form.querySelector<HTMLElement>(".html-editor-content")!;
  return { form, surface, state, calls, report, editorNode, editable, stage };
}

/// (partial: form HTML field stages draft, retains editor handle on render, and commits on blur)
test("HTML field stages immediately, retains its handle on render and commits on blur", async () => {
  const f = await fixture();
  expect(f.editorNode).not.toBeNull();
  expect(f.editable.innerHTML).toBe("<p>Initial description</p>");

  // User types into editable content
  f.editable.focus();
  f.editable.innerHTML = "<p>Initial description with <strong>bold</strong> edit</p>";
  f.editable.dispatchEvent(new InputEvent("input", { bubbles: true }));

  expect(f.state.dirty).toBe(true);
  expect(f.calls).toEqual([]);
  expect(f.state.values["description"]).toBe("<p>Initial description with <strong>bold</strong> edit</p>");

  // Rerender preserves node and state
  f.surface.render();
  expect(f.form.querySelector(".html-editor")).toBe(f.editorNode);
  expect(f.editable.innerHTML).toBe("<p>Initial description with <strong>bold</strong> edit</p>");

  // Blur (focusout to body) commits staged value to onchange
  f.editorNode.dispatchEvent(new FocusEvent("focusout", { bubbles: true, relatedTarget: document.body }));
  await Promise.resolve();
  await Promise.resolve();

  expect(f.calls.map(c => c.method)).toContain("onchange");
  expect(f.report).not.toHaveBeenCalled();
});

/// (partial: save flushes draft and discard resets editor to baseline)
test("HTML field save without blur commits draft before write, and discard drops pending edit", async () => {
  const f = await fixture();
  f.editable.focus();
  f.editable.innerHTML = "<p>Edited content</p>";
  f.editable.dispatchEvent(new InputEvent("input", { bubbles: true }));

  await f.state.save();
  expect(f.calls.map(c => c.method)).toEqual(["onchange", "web_save"]);
  expect(f.state.dirty).toBe(false);

  // Edit again, then discard
  const currentEditable = f.form.querySelector<HTMLElement>(".html-editor-content")!;
  currentEditable.focus();
  currentEditable.innerHTML = "<p>Unsaved edit</p>";
  currentEditable.dispatchEvent(new InputEvent("input", { bubbles: true }));
  expect(f.state.dirty).toBe(true);

  f.state.discard();
  f.surface.render();
  expect(f.state.dirty).toBe(false);
  const resetEditable = f.form.querySelector<HTMLElement>(".html-editor-content")!;
  expect(resetEditable.innerHTML).toBe("<p>Edited content</p>");
});

/// (partial: required HTML field validation)
test("required HTML field validation refuses empty content and clears when populated", async () => {
  const f = await fixture({ required: true });
  expect(f.form.querySelector(".html-editor-error")?.getAttribute("hidden")).not.toBeNull();

  // Clear content
  f.editable.innerHTML = "<p><br></p>";
  f.editable.dispatchEvent(new InputEvent("input", { bubbles: true }));

  // Form-level validation
  const valid = f.surface.valid();
  expect(valid).toBe(false);

  const error = f.form.querySelector(".html-editor-error");
  expect(error?.textContent).toBe("Description is required.");
  expect(error?.hasAttribute("hidden")).toBe(false);

  // Populate content
  f.editable.innerHTML = "<p>Now filled</p>";
  f.editable.dispatchEvent(new InputEvent("input", { bubbles: true }));

  const nowValid = f.surface.valid();
  expect(nowValid).toBe(true);
  expect(error?.hasAttribute("hidden")).toBe(true);
});

/// (partial: dynamic readonly toggles contenteditable and editor controls)
test("HTML field follows dynamic readonly modifiers", async () => {
  const lockedExpr = decodeAst(parseBoundedJson(JSON.stringify({ schema: 1, nodes: [{ kind: "name", name: "locked" }] })));
  const locked: Field = { name: "locked", kind: "boolean", label: "Locked", required: false, readonly: false, selection: null, relation: null, relationField: null };
  const desc = htmlField({ modifiers: { readonly: lockedExpr, required: null, invisible: null } });

  const rpc = new RpcClient(async () => new Response(JSON.stringify([{ id: 1, description: "<p>Text</p>", locked: true }])), () => {});
  const state = new RecordState(rpc, "test.record", [locked, desc], {});
  await state.load(1);

  const form = document.createElement("form");
  document.body.append(form);
  const view = {
    type: "form",
    mode: "record",
    identity: { kind: "stored", id: 1 },
    fields: [locked, desc],
    nodes: [{ kind: "root", children: [{ kind: "field", field: desc, label: desc.label, widget: "html" }] }],
  } as View;

  const report = vi.fn();
  const messages = new MessageServices(document.body, report);
  disposers.push(() => messages.dispose());
  const surface = new RecordForm({
    form,
    messages,
    state,
    view,
    rpc,
    context: {},
    report,
    run: job => { void job().catch(report); },
    stage: (name, value) => state.change(name, value),
    commit: async () => {},
    openRecord: async () => {},
    change: (name, value) => state.change(name, value),
    button: () => {},
  });
  disposers.push(() => surface.dispose());
  surface.render();

  const editable = form.querySelector<HTMLElement>(".html-editor-content")!;
  const boldButton = form.querySelector<HTMLButtonElement>("button[data-html-command='bold']")!;

  expect(editable.getAttribute("contenteditable")).toBe("false");
  expect(editable.getAttribute("aria-readonly")).toBe("true");
  expect(boldButton.disabled).toBe(true);

  // Unlock record
  state.change("locked", false);
  surface.render();

  expect(editable.getAttribute("contenteditable")).toBe("true");
  expect(editable.getAttribute("aria-readonly")).toBe("false");
});

/// (partial: list cells render safe read-only plain text from inert template)
test("list HTML field renders safe read-only plain text without evaluating scripts", () => {
  const field = htmlField();

  // Standard markup
  const standardHandle = renderListHtmlField({
    field,
    value: "<p>Hello <strong>world</strong>!</p>",
    rpc: {} as RpcClient,
    context: {},
    change: () => {},
    run: () => {},
    report: () => {},
  });
  expect(standardHandle.node.className).toContain("list-html");
  expect(standardHandle.node.textContent).toBe("Hello world!");
  expect(standardHandle.node.getAttribute("aria-label")).toBe("Description");

  // Adversarial script tags are not executed; template parses safely
  const scriptHandle = renderListHtmlField({
    field,
    value: "<p>Text <script>window.__pwned = true;</script>more</p>",
    rpc: {} as RpcClient,
    context: {},
    change: () => {},
    run: () => {},
    report: () => {},
  });
  expect(scriptHandle.node.textContent).toBe("Text more");
  expect((window as unknown as Record<string, unknown>)["__pwned"]).toBeUndefined();

  // Empty, null and false render empty text
  for (const emptyVal of [false, null, undefined, ""]) {
    const handle = renderListHtmlField({
      field,
      value: emptyVal,
      rpc: {} as RpcClient,
      context: {},
      change: () => {},
      run: () => {},
      report: () => {},
    });
    expect(handle.node.textContent).toBe("");
  }

  // Non-string, non-null values throw ClientError server
  expect(() =>
    renderListHtmlField({
      field,
      value: 12345,
      rpc: {} as RpcClient,
      context: {},
      change: () => {},
      run: () => {},
      report: () => {},
    })
  ).toThrow(ClientError);
});

/// (partial: refuses non-html field)
test("renderHtmlField refuses non-html field kind", () => {
  const invalidField: Field = {
    name: "notes",
    kind: "text",
    label: "Notes",
    required: false,
    readonly: false,
    selection: null,
    relation: null,
    relationField: null,
  };

  expect(() =>
    renderHtmlField({
      field: invalidField,
      value: "plain text",
      rpc: {} as RpcClient,
      context: {},
      draft: { stage: () => {}, commit: () => {} },
      change: () => {},
      run: () => {},
      report: () => {},
    })
  ).toThrow(ClientError);
});

/// (partial: server-sanitized output read back after save)
test("server-sanitized value is reflected back after save, and server error surfaces", async () => {
  // Server cleans markup during save
  const f = await fixture(
    {},
    undefined,
    () => [{ id: 1, description: "<p>Clean text</p>" }],
  );

  f.editable.focus();
  f.editable.innerHTML = "<p>Draft proposed text</p>";
  f.editable.dispatchEvent(new InputEvent("input", { bubbles: true }));

  await f.state.save();

  // Server-sanitized value is shown back
  const editorContent = f.form.querySelector<HTMLElement>(".html-editor-content")!;
  expect(editorContent.innerHTML).toBe("<p>Clean text</p>");

  // Server error surfaces typed failure
  const failingRpc = new RpcClient(
    async () =>
      new Response(JSON.stringify({ message: "Server sanitizer refused oversized markup." }), {
        status: 400,
        headers: { "Content-Type": "application/json" },
      }),
    () => {},
  );
  const failingState = new RecordState(failingRpc, "test.record", [htmlField()], {});
  await expect(failingState.load(1)).rejects.toMatchObject({
    kind: "validation",
    message: "Server sanitizer refused oversized markup.",
  });
});

/// (partial: a host without message services or reporter is refused, not given a silent stub)
test("renderHtmlField refuses a host without message services or an error reporter", () => {
  const base = {
    field: htmlField(), value: "<p>x</p>", rpc: {} as RpcClient, context: {},
    draft: { stage: () => {}, commit: () => {} }, change: () => {}, run: () => {},
  };
  expect(() => renderHtmlField({ ...base, report: () => {} })).toThrow(/message services/);
  const messages = new MessageServices(document.body, () => {});
  disposers.push(() => messages.dispose());
  expect(() => renderHtmlField({ ...base, messages } as never)).toThrow(/error reporter/);
});
