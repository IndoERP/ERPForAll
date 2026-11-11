/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
import { afterEach, beforeEach, describe, expect, test, vi } from "vitest";
import { configureClientTemplates } from "../src/client/registry.js";
import { HumanApp } from "../src/client/app.js";
import { ErrorDialog } from "../src/client/error-dialog.js";
import { buttonHidden, runObjectButton, type ButtonEnvironment } from "../src/client/buttons.js";
import { describeViews, type ButtonNode, type Field } from "../src/client/metadata.js";
import { renderFormLayout } from "../src/client/form-layout.js";

/** Match the bundled navigation RPC while keeping standalone discovery fixtures. */
function actionMetadata(path: string, action: unknown, views: unknown): unknown {
  return path.endsWith("erp_load_web_action_views") ? { action, views } : action;
}

// Names compared with a string: `<name> != '<value>'`, as postorder wire nodes.
const differs = (name: string, value: string) => ({ schema: 1, nodes: [
  { kind: "name", name }, { kind: "literal", value: { kind: "str", value } },
  { kind: "compare", left: 0, ops: ["not_eq"], comparators: [1] }
] });
const modifiers = (invisible: unknown = null) => ({ readonly: null, required: null, invisible });
const buttonNode = (extra: Record<string, unknown> = {}) => ({ kind: "button", name: "lend", button_type: "object", label: "Lend", variant: "primary", confirm: null, modifiers: modifiers(), ...extra });
const textField = (name: string, type = "char", extra: Record<string, unknown> = {}) => ({ type, label: name, required: false, readonly: false, selection: null, relation: null, relation_field: null, ...extra });
const fieldNode = (name: string, widget = "char") => ({ kind: "field", force_save: false, field: { model: "test.book", name }, widget, options: {}, modifiers: modifiers(), label: name });
const formView = (nodes: unknown[]) => ({
  identity: { kind: "stored", id: 1 }, type: "form", schema_major: 1, renderer: "web.form", root: 0, nodes
});
const descriptionOf = (nodes: unknown[]) => ({
  wire_major: 1, registry_version: "r1", description_version: "d1", model: "test.book",
  fields: { "test.book": { name: textField("Name", "char", { required: true }), state: textField("State", "selection", { selection: [["available", "Available"], ["lent", "Lent"]] }) } },
  views: [formView(nodes)], notices: []
});
const decoded = (value: unknown) => value;
const headerForm = (button: unknown = buttonNode({ modifiers: modifiers(differs("state", "available")) })) => [
  { kind: "form", record_mode: "record", children: [1, 3] }, { kind: "header", children: [2] }, button, fieldNode("name")
];

const attributeDetails = (extra: Record<string, unknown> = {}) => ({size: "normal", circle: false, classes: [], debounce_ms: 0, block_ui: false, ...extra});
const activeContext = (key: string) => ({schema: 1, nodes: [
  {kind: "literal", value: {kind: "str", value: key}},
  {kind: "literal", value: key === "active_model" ? {kind: "str", value: "other.model"} : {kind: "int", value: "99"}},
  ...(key === "active_ids" ? [{kind: "list", items: [1]}, {kind: "dict", items: [[0, 2]]}] : [{kind: "dict", items: [[0, 1]]}])
]});

describe("metadata", () => {
  /// (partial: the consumer admits header object buttons and their invisible expression)
  test("header buttons are decoded with their confirmation and admitted expression", () => {
    const view = describeViews(decoded(descriptionOf(headerForm(buttonNode({ confirm: "Lend this book?", modifiers: modifiers(differs("state", "available")) })))), "test.book", "r1").views.get("form")!;
    const [header] = (view.nodes[0] as { children: { kind: string; children: ButtonNode[] }[] }).children;
    expect(header).toMatchObject({ kind: "header" });
    expect(header!.children[0]).toMatchObject({ kind: "button", name: "lend", label: "Lend", variant: "primary", confirm: "Lend this book?" });
    expect(header!.children[0]!.invisible).not.toBeNull();
  });

  /// (partial: consumer refuses misplaced headers and buttons, non-object buttons and unknown variants)
  test("misplaced or unsupported header nodes are refused", () => {
    const parse = (nodes: unknown[]) => () => describeViews(decoded(descriptionOf(nodes)), "test.book", "r1");
    expect(parse([{ kind: "form", record_mode: "record", children: [1, 2] }, fieldNode("name"), { kind: "header", children: [] }])).toThrow("Invalid form layout hierarchy.");
    expect(parse([{ kind: "form", record_mode: "record", children: [1] }, buttonNode()])).toThrow("Invalid form layout hierarchy.");
    expect(parse([{ kind: "form", record_mode: "record", children: [1] }, { kind: "header", children: [2] }, fieldNode("name")])).toThrow("Header fields require the statusbar widget.");
    expect(parse(headerForm(buttonNode({ button_type: "action" })))).toThrow("Only object buttons are supported (a later release).");
    expect(parse(headerForm(buttonNode({ variant: "neon" })))).toThrow("Unsupported button variant.");
    expect(parse(headerForm(buttonNode({ name: "Lend" })))).toThrow("Invalid button name or label.");
    expect(parse(headerForm(buttonNode({ extra: true })))).toThrow("Unknown view node member: extra.");
    expect(parse(headerForm(buttonNode({ modifiers: { readonly: differs("state", "x"), required: null, invisible: null } })))).toThrow("Dynamic readonly modifiers is not supported");
  });

  test("a malformed invisible expression is refused by the AST decoder", () => {
    const bad = headerForm(buttonNode({ modifiers: modifiers({ schema: 2, nodes: [] }) }));
    expect(() => describeViews(descriptionOf(bad), "test.book", "r1")).toThrow();
    const call = headerForm(buttonNode({ modifiers: modifiers({ schema: 1, nodes: [{ kind: "name", name: "state" }, { kind: "call", func: 0, args: [], keywords: [] }] }) }));
    const view = describeViews(descriptionOf(call), "test.book", "r1").views.get("form")!;
    const node = (view.nodes[0] as { children: { children: ButtonNode[] }[] }).children[0]!.children[0]!;
    // Admitted syntactically; evaluating a call on a selection value fails loudly rather than hiding the button.
    expect(() => buttonHidden(node, [{ name: "state", kind: "selection", label: "State", required: false, readonly: false, selection: [], relation: null, relationField: null }], { state: "x" })).toThrow();
  });

  /// (partial: header buttons are evaluated against the record's current values)
  test("a button is hidden by its expression over the record values", () => {
    const view = describeViews(decoded(descriptionOf(headerForm())), "test.book", "r1").views.get("form")!;
    const node = (view.nodes[0] as { children: { children: ButtonNode[] }[] }).children[0]!.children[0]!;
    const state = { name: "state", kind: "selection", label: "State", required: false, readonly: false, selection: [], relation: null, relationField: null } satisfies Field;
    expect(view.fields.map(f => f.name)).toEqual(["name"]);
    expect(buttonHidden(node, [state], { state: "available" })).toBe(false);
    expect(buttonHidden(node, [state], { state: "lent" })).toBe(true);
    expect(buttonHidden(node, [state], { state: false })).toBe(true);
    expect(() => buttonHidden(node, [], {})).toThrow();
  });

  /// (partial: a one2many holding an unsaved line still evaluates; the line counts with a client id)
  test("an unsaved one2many line does not break button expressions", () => {
    const view = describeViews(decoded(descriptionOf(headerForm())), "test.book", "r1").views.get("form")!;
    const node = (view.nodes[0] as { children: { children: ButtonNode[] }[] }).children[0]!.children[0]!;
    const state = { name: "state", kind: "selection", label: "State", required: false, readonly: false, selection: [], relation: null, relationField: null } satisfies Field;
    const loans = { name: "loan_ids", kind: "one2many", label: "Loans", required: false, readonly: false, selection: [], relation: "test.loan", relationField: "book_id" } satisfies Field;
    const values = { state: "available", loan_ids: [{ id: 3, borrower_id: { id: 7, display_name: "Ani" } }, { __key: "line_1", borrower_id: { id: 8, display_name: "Budi" } }] };
    expect(buttonHidden(node, [state, loans], values)).toBe(false);
    expect(buttonHidden(node, [state, loans], { ...values, state: "lent" })).toBe(true);
    expect(() => buttonHidden(node, [state, loans], { state: "available", loan_ids: [{ borrower_id: false }] }))
      .toThrow("Field loan_ids holds a line without an id or a client key.");
  });
});

describe("layout", () => {
  /// (partial: header renders a labelled toolbar of focusable buttons; line forms show none)
  test("header buttons render in a toolbar before the content and are native buttons", () => {
    const view = describeViews(decoded(descriptionOf(headerForm(buttonNode()))), "test.book", "r1").views.get("form")!;
    const form = document.createElement("form");
    const rendered: string[] = [];
    renderFormLayout(form, view.nodes, (node, parent) => { const input = document.createElement("input"); input.name = node.field.name; parent.append(input); }, new Map(), (node, parent) => {
      const control = document.createElement("button"); control.type = "button"; control.textContent = node.label; parent.append(control); rendered.push(node.name);
    });
    const bar = form.firstElementChild!;
    expect(bar.querySelector(".form-header-actions")?.getAttribute("role")).toBe("toolbar");
    expect(bar.querySelector(".form-header-actions")?.getAttribute("aria-label")).toBe("Record actions");
    expect(bar.querySelector("button")?.tabIndex).toBe(0);
    expect(form.children[1]?.tagName).toBe("INPUT");
    expect(rendered).toEqual(["lend"]);
    const lineForm = document.createElement("form");
    renderFormLayout(lineForm, view.nodes, () => undefined, new Map());
    expect(lineForm.querySelector(".form-header")).toBeNull();
  });
});

describe("click flow", () => {
  const node: ButtonNode = { kind: "button", name: "lend", label: "Lend", variant: "primary", confirm: null, invisible: null };
  const harness = (over: Partial<ButtonEnvironment> = {}) => {
    const log: string[] = [];
    const environment: ButtonEnvironment = {
      model: "test.book", context: { lang: "en_US" }, id: () => 5, dirty: () => false,
      save: async () => { log.push("save"); },
      call: async (model, method, args) => { log.push(`call ${model}.${method} ${JSON.stringify(args)}`); return true; },
      reload: async () => { log.push("reload"); },
      confirm: async () => { log.push("confirm"); return true; },
      ...over
    };
    return { log, environment };
  };

  /// (partial: a clean saved record calls the method on its id, then reloads)
  test("a clean record calls the method with its id and reloads", async () => {
    const { log, environment } = harness();
    await runObjectButton(node, environment);
    expect(log).toEqual(['call test.book.lend {"ids":[5],"context":{"lang":"en_US"}}', "reload"]);
  });

  /// (partial: dirty or new record is saved before the method call)
  test("a dirty or new record is saved first", async () => {
    let dirty = true;
    const dirtyRun = harness({ dirty: () => dirty, save: async () => { dirty = false; } });
    await runObjectButton(node, dirtyRun.environment);
    expect(dirtyRun.log[0]).toContain("call test.book.lend");
    let id: number | undefined;
    const created = harness({ id: () => id, save: async () => { id = 9; } });
    await runObjectButton(node, created.environment);
    expect(created.log[0]).toContain('"ids":[9]');
  });

  /// (partial: a failed or refused save stops the click)
  test("a failing or validation-refused save never reaches the method", async () => {
    const failing = harness({ dirty: () => true, save: async () => { throw new Error("Name is required."); } });
    await expect(runObjectButton(node, failing.environment)).rejects.toThrow("Name is required.");
    expect(failing.log).toEqual([]);
    const refused = harness({ dirty: () => true, save: async () => undefined });
    await runObjectButton(node, refused.environment);
    expect(refused.log).toEqual([]);
    const unsaved = harness({ id: () => undefined, save: async () => undefined });
    await runObjectButton(node, unsaved.environment);
    expect(unsaved.log).toEqual([]);
  });

  test("a declined confirmation does nothing and a method error skips the reload", async () => {
    const declined = harness({ confirm: async () => false });
    await runObjectButton({ ...node, confirm: "Lend it?" }, declined.environment);
    expect(declined.log).toEqual([]);
    const failing = harness({ call: async () => { throw new Error("Already lent."); } });
    await expect(runObjectButton(node, failing.environment)).rejects.toThrow("Already lent.");
    expect(failing.log).toEqual([]);
  });
});

describe("application", () => {
  const bootstrap = {
    wire_major: 1, registry_version: "r1", asset_version: "a1", identity: { uid: 2, name: "Operator", internal: true },
    companies: [{ id: 1, name: "Company" }], current_company_id: 1, allowed_company_ids: [1], active_company_ids: [1],
    locale: { lang: "en_US", tz: "UTC", date_format: "%m/%d/%Y", time_format: "%H:%M:%S" }, csrf: { logout: "test-token-not-secret" }
  };
  const bookForm = [
    { kind: "form", record_mode: "record", children: [1, 5, 6] },
    { kind: "header", children: [2, 3, 4] },
    buttonNode({ confirm: "Lend this book?", modifiers: modifiers(differs("state", "available")) }),
    buttonNode({ name: "give_back", label: "Return", variant: "secondary", modifiers: modifiers({ schema: 1, nodes: [{ kind: "name", name: "state" }, { kind: "literal", value: { kind: "str", value: "lent" } }, { kind: "compare", left: 0, ops: ["not_eq"], comparators: [1] }] }) }),
    buttonNode({ name: "hold", label: "Hold", variant: "danger", modifiers: modifiers({ schema: 1, nodes: [{ kind: "literal", value: { kind: "bool", value: true } }] }) }),
    fieldNode("name"),
    fieldNode("state", "selection")
  ];
  let app: HumanApp | undefined;
  const presented: unknown[] = [];
  const modalDescriptor = Object.getOwnPropertyDescriptor(HTMLDialogElement.prototype, "showModal");
  const closeDescriptor = Object.getOwnPropertyDescriptor(HTMLDialogElement.prototype, "close");
  beforeEach(() => {
    presented.length = 0;
    const show = ErrorDialog.prototype.show;
    vi.spyOn(ErrorDialog.prototype, "show").mockImplementation(function(this: ErrorDialog, error) { presented.push(error); show.call(this, error); });
    Object.defineProperty(HTMLDialogElement.prototype, "showModal", { configurable: true, value(this: HTMLDialogElement) { this.open = true; } });
    Object.defineProperty(HTMLDialogElement.prototype, "close", { configurable: true, value(this: HTMLDialogElement) { this.open = false; } });
    configureClientTemplates([{ name: "web.HumanControlPanel", render: () => {
      const root = document.createElement("section");
      root.setAttribute("data-control-panel", "");
      for (const key of ["breadcrumbs", "search", "actions", "extension", "tools"]) { const part = document.createElement("div"); part.setAttribute(`data-control-${key}`, ""); root.append(part); }
      return root;
    } }]);
  });
  afterEach(() => {
    app?.dispose(); app = undefined;
    if (modalDescriptor) Object.defineProperty(HTMLDialogElement.prototype, "showModal", modalDescriptor);
    if (closeDescriptor) Object.defineProperty(HTMLDialogElement.prototype, "close", closeDescriptor);
    vi.restoreAllMocks(); vi.unstubAllGlobals(); document.body.replaceChildren(); history.replaceState(null, "", "/");
  });
  const host = (): HTMLElement => {
    const root = document.createElement("div");
    for (const region of ["header", "navigation", "notice", "content"]) { const part = document.createElement("div"); part.setAttribute(`data-client-${region}`, ""); root.append(part); }
    document.body.append(root); return root;
  };
  const start = async (respond: (path: string, body: Record<string, unknown>) => Promise<Response> | Response | undefined, nodes: unknown[] = bookForm, fresh = false) => {
    history.replaceState(null, "", fresh ? "/erp/action-20?view_type=form" : "/erp/action-20?id=5&view_type=form");
    const calls: { path: string; body: Record<string, unknown> }[] = [];
    const book = { state: "available" };
    vi.stubGlobal("fetch", vi.fn(async (path: string, init: RequestInit) => {
      const body = JSON.parse(String(init.body)) as Record<string, unknown>;
      calls.push({ path, body });
      const custom = respond(path, body);
      if (custom) return custom;
      const value = path.endsWith("erp_load_web_menus") ? { wire_major: 1, registry_version: "r1", roots: [10], nodes: [{ id: 10, parent_id: null, label: "Books", action_id: 20, children: [], icon: null }] }
        : (path.endsWith("erp_load_web_action") || path.endsWith("erp_load_web_action_views")) ? actionMetadata(path, { wire_major: 1, id: 20, name: "Books", model: "test.book", target: "current", views: [{ id: 1, type: "form" }], domain: null, context: null }, descriptionOf(nodes))
        : path.endsWith("get_views") ? descriptionOf(nodes)
        : path.endsWith("erp_normalize_action_result") ? { wire_major: 1, result: { kind: "none" } }
        : path.endsWith("onchange") ? { value: {} }
        : path.endsWith("web_save") ? [{ id: 5, name: (body["vals"] as Record<string, unknown>)["name"] ?? "Dune", ...book }]
        : path.endsWith("/lend") ? (book.state = "lent", true)
        : [{ id: 5, name: "Dune", ...book }];
      return new Response(JSON.stringify(value), { headers: { "Content-Type": "application/json" } });
    }));
    const root = host();
    app = new HumanApp(root, bootstrap);
    await app.start();
    await vi.waitFor(() => expect(root.querySelector(".form-header")).not.toBeNull());
    return { root, calls };
  };
  const buttons = (root: HTMLElement): string[] => [...root.querySelectorAll(".form-header-btn")].map(b => b.textContent ?? "");
  const choice = (label: string): void => { [...document.querySelectorAll<HTMLButtonElement>("dialog button")].find(b => b.textContent === label)!.click(); };

  test("phone header re-evaluates visible action order while status fields stay separate", async () => {
    vi.spyOn(window, "innerWidth", "get").mockReturnValue(390);
    const nodes = [
      { kind: "form", record_mode: "record", children: [1, 6] },
      { kind: "header", children: [2, 3, 4, 5, 7] },
      buttonNode({ name: "hidden", label: "Hidden", modifiers: modifiers({ schema: 1, nodes: [{ kind: "literal", value: {kind: "bool", value: true} }] }) }),
      buttonNode({ confirm: null, modifiers: modifiers(differs("state", "available")) }),
      buttonNode({ name: "give_back", label: "Return", modifiers: modifiers(differs("state", "lent")) }),
      buttonNode({ name: "hold", label: "Hold" }),
      fieldNode("name"), { ...fieldNode("state", "statusbar"), options: {visible: null} }
    ];
    const {root} = await start(() => undefined, nodes);
    const group = (): HTMLElement => root.querySelector(".form-header-actions")!;
    expect([...group().querySelectorAll(":scope > button")].map(control => control.textContent)).toEqual(["Lend", "More actions"]);
    expect(root.querySelector(".form-header > .form-statusbar")).not.toBeNull();
    expect(group().querySelector(".form-statusbar")).toBeNull();
    group().querySelector<HTMLButtonElement>(".form-header-btn")!.click();
    await vi.waitFor(() => expect([...group().querySelectorAll(":scope > button")].map(control => control.textContent)).toEqual(["Return", "More actions"]));
    expect(buttons(root)).toEqual(["Return", "Hold"]);
    expect(root.querySelector('.form-statusbar [aria-current="step"]')?.textContent).toBe("Lent");
  });

  /// (partial: header renders only buttons whose expression leaves them visible)
  test("visible buttons render as native focusable buttons in a labelled toolbar", async () => {
    const { root } = await start(() => undefined);
    expect(buttons(root)).toEqual(["Lend"]);
    const bar = root.querySelector(".form-header")!;
    expect(bar.querySelector(".form-header-actions")?.getAttribute("role")).toBe("toolbar");
    const lend = bar.querySelector<HTMLButtonElement>("button")!;
    expect(lend.type).toBe("button");
    expect(lend.disabled).toBe(false);
    lend.focus();
    expect(document.activeElement).toBe(lend);
  });

  /// (partial: confirm dialog, method call on the record id, reload, expression re-evaluated)
  test("a confirmed click calls the method, reloads and re-evaluates visibility", async () => {
    const { root, calls } = await start(() => undefined);
    root.querySelector<HTMLButtonElement>(".form-header-btn")!.click();
    await vi.waitFor(() => expect(document.querySelector("dialog")?.textContent).toContain("Lend this book?"));
    expect(calls.some(call => call.path.endsWith("/lend"))).toBe(false);
    choice("Lend");
    await vi.waitFor(() => expect(buttons(root)).toEqual(["Return"]));
    const lendAt = calls.findIndex(call => call.path === "/erp/json/2/test.book/lend");
    expect(calls[lendAt]!.body["ids"]).toEqual([5]);
    expect(calls.slice(lendAt + 1).some(call => call.path.endsWith("web_read"))).toBe(true);
    expect(document.querySelector("dialog")).toBeNull();
  });

  test("cancelling the confirmation makes no call", async () => {
    const { root, calls } = await start(() => undefined);
    root.querySelector<HTMLButtonElement>(".form-header-btn")!.click();
    await vi.waitFor(() => expect(document.querySelector("dialog")).not.toBeNull());
    choice("Cancel");
    await vi.waitFor(() => expect(document.querySelector("dialog")).toBeNull());
    expect(calls.some(call => call.path.endsWith("/lend") || call.path.endsWith("web_save"))).toBe(false);
  });

  /// (partial: edited form saves before the method is called)
  test("a dirty record is saved before the method call", async () => {
    const { root, calls } = await start(() => undefined);
    const input = root.querySelector<HTMLInputElement>('input[name="name"]')!;
    input.value = "Dune Messiah"; input.dispatchEvent(new Event("change"));
    await vi.waitFor(() => expect(root.querySelector<HTMLInputElement>('input[name="name"]')?.disabled).toBe(false));
    root.querySelector<HTMLButtonElement>(".form-header-btn")!.click();
    await vi.waitFor(() => expect(document.querySelector("dialog")).not.toBeNull());
    choice("Lend");
    await vi.waitFor(() => expect(buttons(root)).toEqual(["Return"]));
    const order = calls.map(call => call.path.split("/").pop());
    expect(order.indexOf("web_save")).toBeGreaterThan(-1);
    expect(order.indexOf("web_save")).toBeLessThan(order.indexOf("lend"));
  });

  /// (partial: a refused method call shows the shared error dialog and re-enables the buttons)
  test("a server error opens the error dialog and the buttons work again", async () => {
    const { root } = await start(path => path.endsWith("/lend")
      ? new Response(JSON.stringify({ name: "erp.exceptions.UserError", message: "Already lent." }), { status: 422, headers: { "Content-Type": "application/json" } }) : undefined);
    root.querySelector<HTMLButtonElement>(".form-header-btn")!.click();
    await vi.waitFor(() => expect(document.querySelector("dialog")).not.toBeNull());
    choice("Lend");
    await vi.waitFor(() => expect(document.querySelector(".client-error-dialog")?.textContent).toContain("Already lent."));
    expect(buttons(root)).toEqual(["Lend"]);
    expect([...root.querySelectorAll<HTMLButtonElement>(".form-header-btn")].every(b => !b.disabled)).toBe(true);
  });

  /// (partial: every header button is disabled while the call is in flight)
  test("buttons are disabled while the call runs", async () => {
    let release: (() => void) | undefined;
    const { root } = await start(path => path.endsWith("/lend")
      ? new Promise<Response>(resolve => { release = () => resolve(new Response("true", { headers: { "Content-Type": "application/json" } })); }) : undefined);
    root.querySelector<HTMLButtonElement>(".form-header-btn")!.click();
    await vi.waitFor(() => expect(document.querySelector("dialog")).not.toBeNull());
    choice("Lend");
    await vi.waitFor(() => expect(release).toBeDefined());
    expect([...root.querySelectorAll<HTMLButtonElement>(".form-header-btn")].every(b => b.disabled)).toBe(true);
    release!();
    await vi.waitFor(() => expect([...root.querySelectorAll<HTMLButtonElement>(".form-header-btn")].some(b => b.disabled)).toBe(false));
  });

  /// (partial: mounted main-owner cancellation identity with equal captions)
  test("equal default cancel and accept captions never turn the second choice into a write", async () => {
    const nodes = headerForm(buttonNode({confirm: "Proceed?", details: attributeDetails({confirm_label: "Cancel"})}));
    const {root, calls} = await start(() => undefined, nodes);
    const input = root.querySelector<HTMLInputElement>('input[name="name"]')!;
    input.value = "Edited"; input.dispatchEvent(new Event("change"));
    await vi.waitFor(() => expect(root.querySelector<HTMLInputElement>('input[name="name"]')?.disabled).toBe(false));
    root.querySelector<HTMLButtonElement>(".form-header-btn")!.click();
    await vi.waitFor(() => expect(document.querySelectorAll(".choice-dialog .message-dialog-footer button")).toHaveLength(2));
    const choices = [...document.querySelectorAll<HTMLButtonElement>(".choice-dialog .message-dialog-footer button")];
    expect(choices.map(button => button.textContent)).toEqual(["Cancel", "Cancel"]);
    choices[1]!.click();
    await vi.waitFor(() => expect(document.querySelector(".choice-dialog")).toBeNull());
    await vi.waitFor(() => expect(root.querySelector<HTMLButtonElement>(".form-header-btn")?.disabled).toBe(false));
    expect(calls.filter(call => call.path.endsWith("web_save") || call.path.endsWith("/lend"))).toHaveLength(0);
    root.querySelector<HTMLButtonElement>(".form-header-btn")!.click();
    await vi.waitFor(() => expect(document.querySelectorAll(".choice-dialog .message-dialog-footer button")).toHaveLength(2));
    document.querySelector<HTMLButtonElement>(".choice-dialog .message-dialog-footer button")!.click();
    await vi.waitFor(() => expect(calls.filter(call => call.path.endsWith("/lend"))).toHaveLength(1));
    expect(calls.filter(call => call.path.endsWith("web_save"))).toHaveLength(1);
  });

  /// (partial: mounted title-only object/action confirmation names and keyboard routes)
  test("title-only main controls keep named confirmation defaults and keyboard decisions", async () => {
    for (const kind of ["object", "action"]) {
      const node = kind === "object" ? buttonNode({label: "", confirm: "Proceed?", details: attributeDetails({title: "Run operation"})})
        : {kind: "action_button", target: {kind: "action", id: 21}, label: "", variant: "primary", confirm: "Proceed?", modifiers: modifiers(), details: attributeDetails({title: "Run operation"})};
      const {root, calls} = await start((path, body) => (path.endsWith("erp_load_web_action") || path.endsWith("erp_load_web_action_views")) && (body["ids"] as number[] | undefined)?.[0] === 21
        ? new Response(JSON.stringify(actionMetadata(path, {wire_major: 1, id: 21, name: "Books", model: "test.book", target: "current", views: [{id: 1, type: "form"}], domain: null, context: null}, descriptionOf(headerForm(node)))), {headers: {"Content-Type": "application/json"}}) : undefined, headerForm(node));
      const launch = root.querySelector<HTMLButtonElement>(".form-header-btn")!;
      expect(launch.getAttribute("aria-label")).toBe("Run operation"); launch.click();
      await vi.waitFor(() => expect(document.querySelector(".choice-dialog")).not.toBeNull());
      let dialog = document.querySelector<HTMLDialogElement>(".choice-dialog")!;
      expect(document.getElementById(dialog.getAttribute("aria-labelledby")!)?.textContent).toBe("Run operation");
      expect(dialog.querySelector(".message-dialog-footer button")?.textContent).toBe("Run operation");
      expect(document.activeElement).toBe(dialog.querySelector(".message-dialog-footer button"));
      dialog.dispatchEvent(new KeyboardEvent("keydown", {key: "Escape", bubbles: true, cancelable: true}));
      await vi.waitFor(() => expect(dialog.isConnected).toBe(false));
      expect(calls.filter(call => call.path.endsWith("/lend") || (call.path.endsWith("erp_load_web_action") && (call.body["ids"] as number[] | undefined)?.[0] === 21))).toHaveLength(0);
      await vi.waitFor(() => expect(root.querySelector<HTMLButtonElement>(".form-header-btn")?.disabled).toBe(false));
      root.querySelector<HTMLButtonElement>(".form-header-btn")!.click();
      await vi.waitFor(() => expect(document.querySelector(".choice-dialog")).not.toBeNull());
      dialog = document.querySelector<HTMLDialogElement>(".choice-dialog")!;
      dialog.dispatchEvent(new KeyboardEvent("keydown", {key: "Enter", ctrlKey: true, bubbles: true, cancelable: true}));
      await vi.waitFor(() => expect(calls.some(call => kind === "object" ? call.path.endsWith("/lend") : call.path.endsWith("erp_load_web_action") && (call.body["ids"] as number[] | undefined)?.[0] === 21)).toBe(true));
      app!.dispose(); app = undefined; root.remove();
    }
  });

  /// (partial: mounted new main caller refuses literal active-identity replacement after save)
  test("button active identities cannot replace a newly saved main caller before method dispatch", async () => {
    for (const key of ["active_model", "active_id", "active_ids"]) {
      const nodes = headerForm(buttonNode({details: attributeDetails({context: activeContext(key)})}));
      const {root, calls} = await start(path => path.endsWith("default_get") ? new Response(JSON.stringify({name: "New book", state: "available"}), {headers: {"Content-Type": "application/json"}}) : undefined, nodes, true);
      root.querySelector<HTMLButtonElement>(".form-header-btn")!.click();
      await vi.waitFor(() => expect(presented).toContainEqual(expect.objectContaining({kind: "validation", message: `Button context cannot replace protected key '${key}'.`})));
      expect(calls.filter(call => call.path.endsWith("web_save"))).toHaveLength(1);
      expect(calls.filter(call => call.path.endsWith("/lend"))).toHaveLength(0);
      app!.dispose(); app = undefined; root.remove(); presented.length = 0;
    }
  });

  /// (partial: mounted legacy object context remains unchanged without button context)
  test("a legacy main button preserves its established locale and company context", async () => {
    const {root, calls} = await start(() => undefined, headerForm(buttonNode()));
    root.querySelector<HTMLButtonElement>(".form-header-btn")!.click();
    await vi.waitFor(() => expect(calls.some(call => call.path.endsWith("/lend"))).toBe(true));
    expect(calls.find(call => call.path.endsWith("/lend"))!.body["context"]).toEqual({lang: "en_US", tz: "UTC", allowed_company_ids: [1]});
  });

});

describe("wizard footer grammar", () => {
  const cancel = (extra: Record<string, unknown> = {}) => ({ kind: "cancel_button", label: "Cancel", variant: "secondary", modifiers: modifiers(), ...extra });
  const footerForm = (footerChildren: unknown[], formChildren = [1, 2]) => [
    { kind: "form", record_mode: "record", children: formChildren }, fieldNode("name"), { kind: "footer", children: footerChildren.map((_, i) => i + 3) }, ...footerChildren
  ];
  const parse = (nodes: unknown[]) => () => describeViews(decoded(descriptionOf(nodes)), "test.book", "r1");

  /// (partial: consumer admits a last-child footer of object and cancel buttons)
  test("a last footer with object and cancel buttons is decoded and the main form skips it", () => {
    const view = describeViews(decoded(descriptionOf(footerForm([buttonNode({ name: "confirm", variant: "primary" }), cancel()]))), "test.book", "r1").views.get("form")!;
    const [root] = view.nodes as { children: { kind: string; children?: { kind: string }[] }[] }[];
    expect(root!.children.map(child => child.kind)).toEqual(["field", "footer"]);
    expect(root!.children[1]!.children!.map(child => child.kind)).toEqual(["button", "cancel_button"]);
    const host = document.createElement("div");
    renderFormLayout(host, view.nodes, () => undefined, new Map(), () => { throw new Error("main form rendered a footer button"); });
    expect(host.querySelector(".form-header")).toBeNull();
  });

  /// (partial: consumer refuses misplaced, duplicate and malformed footer nodes)
  test("misplaced footers, cancel outside a footer and unknown members are refused", () => {
    const hierarchy = "Invalid form layout hierarchy.";
    expect(parse(footerForm([cancel()], [2, 1]))).toThrow(hierarchy);
    expect(parse([{ kind: "form", record_mode: "record", children: [1] }, cancel()])).toThrow(hierarchy);
    expect(parse([{ kind: "form", record_mode: "record", children: [1, 3] }, { kind: "header", children: [2] }, cancel(), fieldNode("name")])).toThrow(hierarchy);
    expect(parse([{ kind: "form", record_mode: "record", children: [1] }, { kind: "footer", children: [2] }, fieldNode("name")])).toThrow(hierarchy);
    expect(parse([{ kind: "form", record_mode: "record", children: [1] }, { kind: "sheet", children: [2] }, { kind: "footer", children: [] }])).toThrow(hierarchy);
    expect(parse(footerForm([cancel(), cancel()]))).toThrow("A footer allows at most one cancel button.");
    expect(parse(footerForm([cancel({ name: "lend" })]))).toThrow("Unknown view node member: name.");
    expect(parse(footerForm([cancel({ variant: "neon" })]))).toThrow("Unsupported button variant.");
    expect(parse(footerForm([cancel({ label: " " })]))).toThrow("Invalid button name or label.");
    expect(parse(footerForm([cancel({ modifiers: { readonly: differs("state", "x"), required: null, invisible: null } })]))).toThrow("Dynamic readonly modifiers is not supported");
  });
});
