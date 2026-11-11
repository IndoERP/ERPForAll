/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
import { afterEach, expect, test, vi } from "vitest";
import { createFieldControl } from "../src/client/fields.js";
import { RADIO_MANY2ONE_LIMIT } from "../src/client/field-radio.js";
import type { Field } from "../src/client/metadata.js";
import { RpcClient } from "../src/client/transport.js";

const dummyRpc = new RpcClient(async () => { throw new Error("Unexpected RPC"); }, () => {});

function selectionField(widget = "radio", readonly = false, required = false, options: Record<string, unknown> = {}): Field & { widget: string; options: Record<string, unknown> } {
  return {
    name: "state",
    label: "Status",
    kind: "selection",
    readonly,
    required,
    relation: null,
    relationField: null,
    selection: [
      ["draft", "Draft"],
      ["in_progress", "In Progress"],
      ["done", "Done"],
    ],
    widget,
    options,
  };
}

function many2oneField(widget = "radio", readonly = false, required = false, options: Record<string, unknown> = {}): Field & { widget: string; options: Record<string, unknown> } {
  return {
    name: "partner_id",
    label: "Partner",
    kind: "many2one",
    readonly,
    required,
    relation: "res.partner",
    relationField: null,
    selection: null,
    widget,
    options,
  };
}

afterEach(() => document.body.replaceChildren());

/// (partial: selection radio group renders choices with orientation and target labels)
test("radio group renders labelled inputs targeting each choice and respects orientation", () => {
  // Test vertical orientation (default)
  const fVert = selectionField("radio", false, false, {});
  const handleVert = createFieldControl({
    field: fVert,
    value: "draft",
    rpc: dummyRpc,
    context: {},
    change: vi.fn(),
    run: job => { void job(); },
  });
  document.body.append(handleVert.node);

  const groupVert = handleVert.node.querySelector<HTMLElement>('[role="radiogroup"]')!;
  expect(groupVert).not.toBeNull();
  expect(groupVert.getAttribute("aria-label")).toBe("Status");
  expect(groupVert.classList.contains("o_vertical")).toBe(true);
  expect(groupVert.dataset["orientation"]).toBe("vertical");

  const items = groupVert.querySelectorAll(".o_radio_item");
  expect(items.length).toBe(3);

  const inputs = groupVert.querySelectorAll<HTMLInputElement>("input[type='radio']");
  const labels = groupVert.querySelectorAll<HTMLLabelElement>("label.o_form_label");
  expect(inputs.length).toBe(3);
  expect(labels.length).toBe(3);

  for (let i = 0; i < inputs.length; i++) {
    expect(labels[i]!.htmlFor).toBe(inputs[i]!.id);
    expect(inputs[i]!.name).toBe(inputs[0]!.name);
  }

  expect(labels[0]!.textContent).toBe("Draft");
  expect(labels[1]!.textContent).toBe("In Progress");
  expect(labels[2]!.textContent).toBe("Done");

  expect(inputs[0]!.checked).toBe(true);
  expect(inputs[1]!.checked).toBe(false);
  expect(inputs[2]!.checked).toBe(false);
  handleVert.dispose();

  // Test horizontal orientation
  document.body.replaceChildren();
  const fHoriz = selectionField("radio", false, false, { horizontal: true });
  const handleHoriz = createFieldControl({
    field: fHoriz,
    value: "in_progress",
    rpc: dummyRpc,
    context: {},
    change: vi.fn(),
    run: job => { void job(); },
  });
  document.body.append(handleHoriz.node);

  const groupHoriz = handleHoriz.node.querySelector<HTMLElement>('[role="radiogroup"]')!;
  expect(groupHoriz).not.toBeNull();
  expect(groupHoriz.classList.contains("o_horizontal")).toBe(true);
  expect(groupHoriz.dataset["orientation"]).toBe("horizontal");

  const horizInputs = groupHoriz.querySelectorAll<HTMLInputElement>("input[type='radio']");
  expect(horizInputs[0]!.checked).toBe(false);
  expect(horizInputs[1]!.checked).toBe(true);
  expect(horizInputs[2]!.checked).toBe(false);
  handleHoriz.dispose();
});

/// (partial: radio group unset value selects none and clicking updates selection)
test("unset value selects none and clicking updates selection", () => {
  const change = vi.fn();
  const f = selectionField("radio");
  const handle = createFieldControl({
    field: f,
    value: false,
    rpc: dummyRpc,
    context: {},
    change,
    run: job => { void job(); },
  });
  document.body.append(handle.node);

  const inputs = handle.node.querySelectorAll<HTMLInputElement>("input[type='radio']");
  // Unset value selects none
  for (const input of inputs) {
    expect(input.checked).toBe(false);
  }

  // Click on the third choice ("Done")
  inputs[2]!.click();
  inputs[2]!.dispatchEvent(new Event("change", { bubbles: true }));

  expect(change).toHaveBeenCalledWith("done");
  expect(inputs[2]!.checked).toBe(true);
  expect(inputs[0]!.checked).toBe(false);
  expect(inputs[1]!.checked).toBe(false);

  handle.dispose();
});

/// (partial: radio group keyboard arrow navigation cycles through options)
test("keyboard arrows cycle through options with wrapping", () => {
  const change = vi.fn();
  const f = selectionField("radio");
  const handle = createFieldControl({
    field: f,
    value: "draft",
    rpc: dummyRpc,
    context: {},
    change,
    run: job => { void job(); },
  });
  document.body.append(handle.node);

  const group = handle.node.querySelector<HTMLElement>('[role="radiogroup"]')!;
  const inputs = group.querySelectorAll<HTMLInputElement>("input[type='radio']");
  inputs[0]!.focus();

  // ArrowDown advances from draft (0) to in_progress (1)
  group.dispatchEvent(new KeyboardEvent("keydown", { key: "ArrowDown", bubbles: true }));
  expect(change).toHaveBeenCalledWith("in_progress");
  expect(inputs[1]!.checked).toBe(true);

  // ArrowRight advances from in_progress (1) to done (2)
  group.dispatchEvent(new KeyboardEvent("keydown", { key: "ArrowRight", bubbles: true }));
  expect(change).toHaveBeenCalledWith("done");
  expect(inputs[2]!.checked).toBe(true);

  // ArrowDown wraps from done (2) back to draft (0)
  group.dispatchEvent(new KeyboardEvent("keydown", { key: "ArrowDown", bubbles: true }));
  expect(change).toHaveBeenCalledWith("draft");
  expect(inputs[0]!.checked).toBe(true);

  // ArrowUp wraps from draft (0) back to done (2)
  group.dispatchEvent(new KeyboardEvent("keydown", { key: "ArrowUp", bubbles: true }));
  expect(change).toHaveBeenCalledWith("done");
  expect(inputs[2]!.checked).toBe(true);

  // ArrowLeft moves from done (2) back to in_progress (1)
  group.dispatchEvent(new KeyboardEvent("keydown", { key: "ArrowLeft", bubbles: true }));
  expect(change).toHaveBeenCalledWith("in_progress");
  expect(inputs[1]!.checked).toBe(true);

  handle.dispose();
});

/// (partial: radio group required and readonly states)
test("readonly disables inputs and required marks the radiogroup", () => {
  // Readonly disables inputs
  const change = vi.fn();
  const fReadonly = selectionField("radio", true, false);
  const handleReadonly = createFieldControl({
    field: fReadonly,
    value: "draft",
    rpc: dummyRpc,
    context: {},
    change,
    run: job => { void job(); },
  });
  document.body.append(handleReadonly.node);

  const inputs = handleReadonly.node.querySelectorAll<HTMLInputElement>("input[type='radio']");
  for (const input of inputs) {
    expect(input.disabled).toBe(true);
  }

  // Clicking disabled radio does not trigger change
  inputs[1]!.click();
  expect(change).not.toHaveBeenCalled();

  // Keyboard navigation when readonly does not trigger change
  const group = handleReadonly.node.querySelector<HTMLElement>('[role="radiogroup"]')!;
  group.dispatchEvent(new KeyboardEvent("keydown", { key: "ArrowDown", bubbles: true }));
  expect(change).not.toHaveBeenCalled();
  handleReadonly.dispose();

  // Required attribute
  document.body.replaceChildren();
  const fRequired = selectionField("radio", false, true);
  const handleRequired = createFieldControl({
    field: fRequired,
    value: false,
    rpc: dummyRpc,
    context: {},
    change: vi.fn(),
    run: job => { void job(); },
  });
  document.body.append(handleRequired.node);

  const groupReq = handleRequired.node.querySelector<HTMLElement>('[role="radiogroup"]')!;
  expect(groupReq.getAttribute("aria-required")).toBe("true");
  expect(handleRequired.node.classList.contains("field-required")).toBe(true);
  handleRequired.dispose();
});

/// (partial: radio field read mode displays label text)
test("read mode in list context displays formatted label text without radio inputs", () => {
  // Selection field in list view
  const fSel = selectionField("radio", true);
  const handleSel = createFieldControl({
    field: fSel,
    value: "in_progress",
    rpc: dummyRpc,
    context: {},
    change: vi.fn(),
    run: job => { void job(); },
    viewContext: "list",
  });
  document.body.append(handleSel.node);

  expect(handleSel.node.querySelector("input[type='radio']")).toBeNull();
  expect(handleSel.node.querySelector('[role="radiogroup"]')).toBeNull();
  const readValue = handleSel.node.querySelector(".field-read-value")!;
  expect(readValue).not.toBeNull();
  expect(readValue.textContent).toBe("In Progress");
  handleSel.dispose();

  // Unset selection in list view
  document.body.replaceChildren();
  const handleUnset = createFieldControl({
    field: fSel,
    value: false,
    rpc: dummyRpc,
    context: {},
    change: vi.fn(),
    run: job => { void job(); },
    viewContext: "list",
  });
  document.body.append(handleUnset.node);
  expect(handleUnset.node.querySelector(".field-read-value")!.textContent).toBe("");
  handleUnset.dispose();
});

/// (partial: many2one radio group loads and selects relational choices)
test("many2one radio group fetches relational choices and updates value", async () => {
  const records = [
    { id: 10, display_name: "Acme Corp" },
    { id: 20, display_name: "Globex Inc" },
  ];
  const requests: { url: string; body: Record<string, unknown> }[] = [];
  const rpc = new RpcClient(async (input, init) => {
    requests.push({ url: String(input), body: JSON.parse(String(init?.body ?? "{}")) as Record<string, unknown> });
    return new Response(JSON.stringify({ length: records.length, records }), {
      headers: { "Content-Type": "application/json" },
    });
  }, () => {});

  const change = vi.fn();
  const f = many2oneField("radio");
  const handle = createFieldControl({
    field: f,
    value: { id: 10, display_name: "Acme Corp" },
    rpc,
    context: {},
    change,
    run: job => { void job(); },
  });
  document.body.append(handle.node);

  // Initial render has the current record
  const initialInputs = handle.node.querySelectorAll<HTMLInputElement>("input[type='radio']");
  expect(initialInputs.length).toBeGreaterThanOrEqual(1);
  expect(initialInputs[0]!.checked).toBe(true);

  // Wait for RPC to load choices
  await vi.waitFor(() => {
    const inputs = handle.node.querySelectorAll<HTMLInputElement>("input[type='radio']");
    expect(inputs.length).toBe(2);
  });

  const inputs = handle.node.querySelectorAll<HTMLInputElement>("input[type='radio']");
  const labels = handle.node.querySelectorAll<HTMLLabelElement>("label.o_form_label");

  expect(labels[0]!.textContent).toBe("Acme Corp");
  expect(labels[1]!.textContent).toBe("Globex Inc");
  expect(requests).toHaveLength(1);
  expect(requests[0]!.url).toMatch(/web_search_read$/);
  expect(requests[0]!.body["limit"]).toBe(RADIO_MANY2ONE_LIMIT + 1);
  expect(requests[0]!.body["specification"]).toEqual({ display_name: {} });
  expect(inputs[0]!.checked).toBe(true);
  expect(inputs[1]!.checked).toBe(false);

  // Click second choice
  inputs[1]!.click();
  inputs[1]!.dispatchEvent(new Event("change", { bubbles: true }));

  expect(change).toHaveBeenCalledWith({ id: 20, display_name: "Globex Inc" });
  expect(inputs[1]!.checked).toBe(true);
  expect(inputs[0]!.checked).toBe(false);

  handle.dispose();

  // Read mode for many2one in list view
  document.body.replaceChildren();
  const handleRead = createFieldControl({
    field: f,
    value: { id: 20, display_name: "Globex Inc" },
    rpc,
    context: {},
    change: vi.fn(),
    run: job => { void job(); },
    viewContext: "list",
  });
  document.body.append(handleRead.node);
  expect(handleRead.node.querySelector(".field-read-value")!.textContent).toBe("Globex Inc");
  handleRead.dispose();
});

/// (partial: many2one radio refuses an over-bound choice list instead of truncating)
test("many2one radio fails loudly past the choice bound and keeps no partial list", async () => {
  const records = Array.from({ length: RADIO_MANY2ONE_LIMIT + 1 }, (_, i) => ({ id: i + 1, display_name: `Partner ${i + 1}` }));
  const rpc = new RpcClient(async () => new Response(JSON.stringify({ length: records.length, records }), {
    headers: { "Content-Type": "application/json" },
  }), () => {});
  let failure: unknown;
  const handle = createFieldControl({
    field: many2oneField("radio"),
    value: { id: 1, display_name: "Partner 1" },
    rpc,
    context: {},
    change: vi.fn(),
    run: job => { job().catch(error => { failure = error; }); },
  });
  document.body.append(handle.node);
  await vi.waitFor(() => expect(failure).toBeDefined());
  expect(String((failure as Error).message)).toContain(`more than ${RADIO_MANY2ONE_LIMIT}`);
  expect(String((failure as Error).message)).toContain("Partner");
  // Only the current value stays; the oversized list is never rendered.
  expect(handle.node.querySelectorAll("input[type='radio']")).toHaveLength(1);
  handle.dispose();
});

/// (partial: many2one radio choice-load failure reaches the run reporter)
test("many2one radio choice-load failure propagates to run instead of being swallowed", async () => {
  const rpc = new RpcClient(async () => new Response(JSON.stringify({ error: { message: "denied" } }), {
    status: 403, headers: { "Content-Type": "application/json" },
  }), () => {});
  let failure: unknown;
  const handle = createFieldControl({
    field: many2oneField("radio"),
    value: false,
    rpc,
    context: {},
    change: vi.fn(),
    run: job => { job().catch(error => { failure = error; }); },
  });
  document.body.append(handle.node);
  await vi.waitFor(() => expect(failure).toBeDefined());
  expect(handle.node.querySelectorAll("input[type='radio']")).toHaveLength(0);
  handle.dispose();
});
