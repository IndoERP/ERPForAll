/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
import { afterEach, beforeEach, expect, test, vi } from "vitest";
import { configureClientTemplates } from "../src/client/registry.js";
import { ErrorDialog } from "../src/client/error-dialog.js";
import { HumanApp } from "../src/client/app.js";

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
    app?.dispose(); app = undefined; delete document.documentElement.dataset["theme"];
    if (modalDescriptor) Object.defineProperty(HTMLDialogElement.prototype, "showModal", modalDescriptor);
    if (closeDescriptor) Object.defineProperty(HTMLDialogElement.prototype, "close", closeDescriptor);
    vi.restoreAllMocks(); vi.unstubAllGlobals(); document.body.replaceChildren(); history.replaceState(null, "", "/");
  });
  const host = (): HTMLElement => {
    const root = document.createElement("div");
    for (const region of ["header", "navigation", "notice", "content"]) { const part = document.createElement("div"); part.setAttribute(`data-client-${region}`, ""); root.append(part); }
    document.body.append(root); return root;
  };
  type Respond = (path: string, body: Record<string, unknown>) => Promise<Response | undefined> | Response | undefined;
  /** Mount the client without waiting for its first page; a fixture may hold any response. */
  const launch = (respond: Respond, resolveTheme = false) => {
    history.replaceState(null, "", "/erp/action-20?id=5&view_type=form");
    const calls: { path: string; body: Record<string, unknown> }[] = [];
    const book = { state: "available" };
    vi.stubGlobal("fetch", vi.fn(async (path: string, init: RequestInit) => {
      const body = JSON.parse(String(init.body)) as Record<string, unknown>;
      calls.push({ path, body });
      const custom = await respond(path, body);
      if (custom) return custom;
      const value = path.endsWith("get_res_users_settings") ? { homemenu_config: null }
        : path.endsWith("erp_load_web_menus") ? { wire_major: 1, registry_version: "r1", roots: [10], nodes: [{ module: null, id: 10, parent_id: null, label: "Books", action_id: 20, children: [], icon: null }] }
        : (path.endsWith("erp_load_web_action") || path.endsWith("erp_load_web_action_views")) ? actionMetadata(path, { wire_major: 1, id: 20, name: "Books", model: "test.book", target: "current", views: [{ id: 1, type: "form" }], domain: null, context: null }, descriptionOf(bookForm))
        : path.endsWith("get_views") ? descriptionOf(bookForm)
        : path.endsWith("erp_normalize_action_result") ? { wire_major: 1, result: { kind: "none" } }
        : path.endsWith("onchange") ? { value: {} }
        : path.endsWith("web_save") ? [{ id: 5, name: (body["vals"] as Record<string, unknown>)["name"] ?? "Dune", ...book }]
        : path.endsWith("/lend") ? (book.state = "lent", true)
        : [{ id: 5, name: "Dune", ...book }];
      return new Response(JSON.stringify(value), { headers: { "Content-Type": "application/json" } });
    }));
    const root = host();
    app = new HumanApp(root, bootstrap);
    return { root, calls, started: app.start(resolveTheme) };
  };
  const start = async (respond: Respond) => {
    const { root, calls, started } = launch(respond);
    await started;
    await vi.waitFor(() => expect(root.querySelector(".form-header")).not.toBeNull());
    return { root, calls };
  };
  const choice = (label: string): void => { [...document.querySelectorAll<HTMLButtonElement>("dialog button")].find(b => b.textContent === label)!.click(); };


const attributeDetails = (extra: Record<string, unknown> = {}) => ({size: "normal", circle: false, classes: [], debounce_ms: 0, block_ui: false, ...extra});
const activeContext = (key: string) => ({schema: 1, nodes: [
  {kind: "literal", value: {kind: "str", value: key}},
  {kind: "literal", value: key === "active_model" ? {kind: "str", value: "other.model"} : {kind: "int", value: "99"}},
  ...(key === "active_ids" ? [{kind: "list", items: [1]}, {kind: "dict", items: [[0, 2]]}] : [{kind: "dict", items: [[0, 1]]}])
]});

const json = (value: unknown) => new Response(JSON.stringify(value), { headers: { "Content-Type": "application/json" } });
const wizard = { kind: "window", name: "Lend book", model: "test.wizard", res_id: null,
  views: [{ id: null, type: "form" }], context: { default_name: "Borrower" } };
const wizardDescription = () => ({ ...descriptionOf([
  { kind: "form", record_mode: "record", children: [1, 2] },
  { ...fieldNode("name"), field: { model: "test.wizard", name: "name" } },
  { kind: "footer", children: [3, 4] }, buttonNode({ name: "confirm_loan", label: "Confirm loan" }),
  { kind: "cancel_button", label: "Cancel wizard", variant: "secondary", modifiers: modifiers() }
]), model: "test.wizard", fields: { "test.wizard": { name: textField("Name", "char", { required: true }) } } });
const setup = async (result: unknown = { kind: "close" }, failure?: string, custom?: (path: string, body: Record<string, unknown>) => Promise<Response> | Response | undefined) => {
  const session = await start((path, body) => {
    const override = custom?.(path, body); if (override) return override;
    if (path.endsWith("/lend")) return json({ action: "open" });
    if (path.endsWith("erp_normalize_action_result")) return json({ wire_major: 1, result: (body["result"] as { action?: string })?.action === "open" ? wizard : result });
    if (path.includes("test.wizard/get_views")) return json(wizardDescription());
    if (path.includes("test.wizard/default_get")) return json({ name: "Borrower" });
    if (path.includes("test.wizard/onchange")) return json({ value: {} });
    if (path.includes("test.wizard/web_save")) return json([{ id: 77, name: "Borrower" }]);
    if (path.endsWith("/confirm_loan")) {
      if (failure === "network") return Promise.reject(new TypeError("lost response"));
      if (failure === "permission") return new Response(JSON.stringify({ name: "erp.exceptions.AccessError", message: "Loan denied." }), {status: 403, headers: { "Content-Type": "application/json" }});
      return json(true);
    }
    return undefined;
  });
  const url = location.href;
  session.root.querySelector<HTMLButtonElement>(".form-header-btn")!.click();
  await vi.waitFor(() => expect(document.querySelector("dialog")?.textContent).toContain("Lend this book?"));
  choice("Lend");
  await vi.waitFor(() => expect(document.querySelector(".wizard-dialog")).not.toBeNull());
  return { ...session, url };
};
const dialogButton = (label: string) => [...document.querySelectorAll<HTMLButtonElement>(".wizard-dialog button")].find(b => b.textContent === label)!;

/// (partial: mounted client with explicit HTTP fixtures; installed backend acceptance remains)
test("method result opens an independent dialog and cancel writes nothing, refreshes caller once", async () => {
  const { root, calls, url } = await setup();
  expect(location.href).toBe(url);
  expect(root.querySelector(".breadcrumb-current")?.textContent).toBe("Dune");
  const defaults = calls.find(c => c.path.includes("test.wizard/default_get"))!;
  expect(defaults.body["context"]).toEqual({ lang: "en_US", tz: "UTC", allowed_company_ids: [1], active_model: "test.book", active_id: 5, active_ids: [5], default_name: "Borrower" });
  const before = calls.filter(c => c.path.includes("test.book/web_read")).length;
  dialogButton("Cancel wizard").click();
  await vi.waitFor(() => expect(document.querySelector(".wizard-dialog")).toBeNull());
  await vi.waitFor(() => expect(calls.filter(c => c.path.includes("test.book/web_read")).length).toBe(before + 1));
  expect(calls.some(c => c.path.includes("test.wizard/web_save") || c.path.endsWith("confirm_loan"))).toBe(false);
});

/// (partial: mounted client with explicit HTTP fixtures; installed backend acceptance remains)
test("confirm saves new wizard before method and closes with one caller reload", async () => {
  const { calls, url } = await setup();
  const before = calls.filter(c => c.path.includes("test.book/web_read")).length;
  dialogButton("Confirm loan").click(); dialogButton("Confirm loan").click();
  await vi.waitFor(() => expect(document.querySelector(".wizard-dialog")).toBeNull());
  const save = calls.findIndex(c => c.path.includes("test.wizard/web_save"));
  const method = calls.findIndex(c => c.path.endsWith("confirm_loan"));
  expect(save).toBeGreaterThan(-1); expect(method).toBeGreaterThan(save);
  expect(calls[method]!.body["ids"]).toEqual([77]);
  expect(calls[method]!.body["context"]).toEqual({lang: "en_US", tz: "UTC", allowed_company_ids: [1], active_model: "test.book", active_id: 5, active_ids: [5], default_name: "Borrower"});
  expect(calls.filter(c => c.path.endsWith("confirm_loan"))).toHaveLength(1);
  await vi.waitFor(() => expect(calls.filter(c => c.path.includes("test.book/web_read")).length).toBe(before + 1));
  expect(location.href).toBe(url);
});

/// (partial: mounted client with explicit HTTP fixtures; button clicked right after field edit runs after commit)
test("Confirm loan clicked right after editing a field commits the field and executes the button", async () => {
  const { calls } = await setup();
  const input = document.querySelector<HTMLInputElement>(".wizard-dialog input[name='name']")!;
  input.value = "Updated Borrower";
  input.dispatchEvent(new Event("input", { bubbles: true }));
  input.dispatchEvent(new Event("change", { bubbles: true }));
  dialogButton("Confirm loan").click();
  await vi.waitFor(() => expect(document.querySelector(".wizard-dialog")).toBeNull());
  const method = calls.find(c => c.path.endsWith("confirm_loan"));
  expect(method).toBeDefined();
  const save = calls.find(c => c.path.includes("test.wizard/web_save"));
  expect(save).toBeDefined();
  expect(save!.body["vals"]).toMatchObject({ name: "Updated Borrower" });
});

/// (partial: mounted client with explicit HTTP fixtures; second change during in-flight commit is queued)
test("a second change during an in-flight commit is queued and runs sequentially", async () => {
  let resolveFirstOnchange!: () => void;
  const gate = new Promise<void>(resolve => { resolveFirstOnchange = resolve; });
  let onchangeCount = 0;
  let ready = false;
  await setup(undefined, undefined, path => {
    if (path.includes("test.wizard/onchange") && ready) {
      onchangeCount++;
      if (onchangeCount === 1) {
        return gate.then(() => json({ value: {} }));
      }
      return json({ value: {} });
    }
    return undefined;
  });
  ready = true;
  const input = document.querySelector<HTMLInputElement>(".wizard-dialog input[name='name']")!;
  input.value = "First Edit";
  input.dispatchEvent(new Event("input", { bubbles: true }));
  input.dispatchEvent(new Event("change", { bubbles: true }));
  await vi.waitFor(() => expect(onchangeCount).toBe(1));

  input.value = "Second Edit";
  input.dispatchEvent(new Event("input", { bubbles: true }));
  input.dispatchEvent(new Event("change", { bubbles: true }));

  resolveFirstOnchange();
  await vi.waitFor(() => expect(onchangeCount).toBe(2));
  expect(presented).toEqual([]);
});

/// (partial: mounted client with explicit HTTP fixtures; failed commit restores input focus)
test("failed field commit restores focus to the editing input", async () => {
  let ready = false;
  await setup(undefined, undefined, path => {
    if (path.includes("test.wizard/onchange") && ready) {
      return new Response(JSON.stringify({ name: "erp.exceptions.UserError", message: "Invalid value." }), {
        status: 400,
        headers: { "Content-Type": "application/json" }
      });
    }
    return undefined;
  });
  ready = true;
  const input = document.querySelector<HTMLInputElement>(".wizard-dialog input[name='name']")!;
  input.focus();
  expect(document.activeElement).toBe(input);
  input.value = "Failing Value";
  input.dispatchEvent(new Event("input", { bubbles: true }));
  input.dispatchEvent(new Event("change", { bubbles: true }));
  // In real browsers, disabling an active element drops focus to body.
  document.body.tabIndex = -1;
  document.body.focus();
  expect(document.activeElement).not.toBe(input);

  await vi.waitFor(() => expect(presented).toContainEqual(expect.objectContaining({ message: "Invalid value." })));
  document.querySelector<HTMLButtonElement>("[data-error-close]")?.click();
  await vi.waitFor(() => expect(document.querySelector(".client-error-dialog")).toBeNull());
  expect(input.disabled).toBe(false);
  const currentInput = document.querySelector<HTMLInputElement>(".wizard-dialog input[name='name']")!;
  await vi.waitFor(() => expect(document.activeElement).toBe(currentInput));
});

/// (partial: mounted client with explicit HTTP fixtures; installed backend acceptance remains)
test("uncertain method outcome keeps dialog read-only and cannot replay", async () => {
  const { calls } = await setup(undefined, "network");
  dialogButton("Confirm loan").click();
  await vi.waitFor(() => expect(document.querySelector(".client-error-dialog")).not.toBeNull());
  expect(document.querySelector(".wizard-dialog")).not.toBeNull();
  expect(dialogButton("Confirm loan").disabled).toBe(true);
  expect(dialogButton("Cancel wizard").disabled).toBe(true);
  expect(calls.filter(c => c.path.endsWith("confirm_loan"))).toHaveLength(1);
  expect(presented).toEqual([expect.objectContaining({kind:"ambiguous",message:"The connection was interrupted. The change may have been saved. Check the record before trying again."})]);
});

/// (partial: mounted client with explicit HTTP fixtures; installed backend acceptance remains)
test("notification text closes the wizard and refreshes caller once", async () => {
  const { calls, root } = await setup({ kind: "notification", title: "Loan", message: "<b>Done</b>", level: "success", then_close: true });
  const before = calls.filter(c => c.path.includes("test.book/web_read")).length;
  dialogButton("Confirm loan").click();
  await vi.waitFor(() => expect(document.querySelector(".wizard-dialog")).toBeNull());
  expect(root.querySelector(".client-notification-title")?.textContent).toBe("Loan");
  expect(root.querySelector(".client-notification-message")?.textContent).toBe("<b>Done</b>");
  expect(root.querySelector("[data-client-notice] b")).toBeNull();
  await vi.waitFor(() => expect(calls.filter(c => c.path.includes("test.book/web_read")).length).toBe(before + 1));
});

test("explicit permission error keeps wizard and edits, allows a corrected operation", async () => {
  const { calls } = await setup(undefined, "permission");
  dialogButton("Confirm loan").click();
  await vi.waitFor(() => expect(document.querySelector(".client-error-dialog")?.textContent).toContain("Loan denied."));
  expect(presented).toEqual([expect.objectContaining({kind:"permission",message:"Loan denied."})]);
  expect(document.querySelector<HTMLInputElement>('.wizard-dialog input[name="name"]')?.value).toBe("Borrower");
  expect(dialogButton("Confirm loan").disabled).toBe(false);
  expect(calls.filter(c => c.path.endsWith("confirm_loan"))).toHaveLength(1);
});

test("failed post-method normalization locks the operation without replay", async () => {
  const { calls } = await setup(undefined, undefined, (path, body) => path.endsWith("erp_normalize_action_result") && body["result"] === true
    ? new Response(JSON.stringify({name:"erp.exceptions.ValidationError",message:"Unsupported result."}), {status:422,headers:{"Content-Type":"application/json"}}) : undefined);
  dialogButton("Confirm loan").click();
  await vi.waitFor(() => expect(document.querySelector(".client-error-dialog")?.textContent).toContain("Unsupported result."));
  expect(dialogButton("Confirm loan").disabled).toBe(true);
  expect(presented).toEqual([expect.objectContaining({kind:"validation",message:"Unsupported result."})]);
  expect(calls.filter(c => c.path.endsWith("confirm_loan"))).toHaveLength(1);
});

/// (partial: mounted client with explicit HTTP fixtures; installed backend acceptance remains)
test("dirty Escape closes the wizard at once, like the reference ERP's dialog dismiss; nothing is saved", async () => {
  const { calls } = await setup();
  const dialog = document.querySelector<HTMLDialogElement>(".wizard-dialog")!;
  dialog.dispatchEvent(new Event("cancel", { cancelable: true }));
  await vi.waitFor(() => expect(dialog.isConnected).toBe(false));
  expect(document.querySelector(".choice-dialog")).toBeNull();
  expect(calls.some(c => c.path.includes("test.wizard/web_save"))).toBe(false);
  expect(calls.some(c => c.path.endsWith("confirm_loan"))).toBe(false);
});

/// (partial: mounted client with explicit HTTP fixtures; installed backend acceptance remains)
test("footer cancel closes the wizard without a save or a method call", async () => {
  const { calls } = await setup();
  const dialog = document.querySelector<HTMLDialogElement>(".wizard-dialog")!;
  await vi.waitFor(() => expect(dialogButton("Cancel wizard").disabled).toBe(false));
  dialogButton("Cancel wizard").click();
  await vi.waitFor(() => expect(dialog.isConnected).toBe(false));
  expect(document.querySelector(".choice-dialog")).toBeNull();
  expect(calls.some(c => c.path.includes("test.wizard/web_save"))).toBe(false);
});

test("disposing app removes wizard and scoped choice, late responses cannot remount", async () => {
  // A button confirmation is the wizard's scoped choice (Escape and Reload never ask).
  const description = wizardDescription();
  description.views[0]!.nodes[3] = buttonNode({ name: "confirm_loan", label: "Confirm loan", confirm: "Lend this book?" });
  const { calls } = await setup(undefined, undefined, path => path.includes("test.wizard/get_views") ? json(description) : undefined);
  await vi.waitFor(() => expect(dialogButton("Confirm loan").disabled).toBe(false));
  dialogButton("Confirm loan").click();
  await vi.waitFor(() => expect(document.querySelector(".choice-dialog")?.textContent).toContain("Lend this book?"));
  app!.dispose();
  await vi.waitFor(() => expect(document.querySelector("dialog")).toBeNull());
  expect(calls.some(c => c.path.includes("test.wizard/web_save"))).toBe(false);
});

test("required field on a hidden notebook page is revealed and method never runs", async () => {
  const description = wizardDescription();
  description.views[0]!.nodes = [
    {kind:"form",record_mode:"record",children:[1,2,7]},
    {...fieldNode("name"),field:{model:"test.wizard",name:"name"}},
    {kind:"notebook",children:[3,5]},
    {kind:"page",name:"first",label:"First",children:[4]},
    {kind:"separator",label:"First page"},
    {kind:"page",name:"required",label:"Required",children:[6]},
    {...fieldNode("reason"),field:{model:"test.wizard",name:"reason"}},
    {kind:"footer",children:[8,9]},buttonNode({name:"confirm_loan",label:"Confirm loan"}),
    {kind:"cancel_button",label:"Cancel wizard",variant:"secondary",modifiers:modifiers()}
  ];
  Object.assign(description.fields["test.wizard"], {reason:textField("Reason","char",{required:true})});
  const { calls } = await setup(undefined, undefined, path => path.includes("test.wizard/get_views") ? json(description) : undefined);
  dialogButton("Confirm loan").click();
  await vi.waitFor(() => expect(document.querySelector('.wizard-dialog [role="tab"][aria-selected="true"]')?.textContent).toBe("Required"));
  expect(calls.some(c => c.path.includes("test.wizard/web_save") || c.path.endsWith("confirm_loan"))).toBe(false);
  expect(document.activeElement).toBe(document.querySelector('.wizard-dialog input[name="reason"]'));
});

test("caller refresh failure surfaces after close and never replays the method", async () => {
  let reads = 0;
  const { calls } = await setup(undefined, undefined, path => path.includes("test.book/web_read") && ++reads > 1
    ? new Response(JSON.stringify({name:"erp.exceptions.AccessError",message:"Caller no longer readable."}),{status:403,headers:{"Content-Type":"application/json"}}) : undefined);
  dialogButton("Confirm loan").click();
  await vi.waitFor(() => expect(document.querySelector(".client-error-dialog")?.textContent).toContain("Caller no longer readable."));
  expect(document.querySelector(".wizard-dialog")).toBeNull();
  expect(presented).toEqual([expect.objectContaining({kind:"permission",message:"Caller no longer readable."})]);
  expect(calls.filter(c => c.path.endsWith("confirm_loan"))).toHaveLength(1);
});

test("an existing wizard loads its identity and a method lock survives read-only reload", async () => {
  const { calls } = await setup(undefined, "network", (path, body) => {
    if (path.endsWith("erp_normalize_action_result") && (body["result"] as {action?:string})?.action === "open") return json({wire_major:1,result:{...wizard,res_id:77}});
    if (path.includes("test.wizard/web_read")) return json([{id:77,name:"Existing"}]);
    return undefined;
  });
  expect(calls.some(c => c.path.includes("test.wizard/default_get"))).toBe(false);
  dialogButton("Confirm loan").click();
  await vi.waitFor(() => expect(document.querySelector(".client-error-dialog")).not.toBeNull());
  choice("Close");
  dialogButton("Reload").click();
  await vi.waitFor(() => expect(calls.filter(c => c.path.includes("test.wizard/web_read"))).toHaveLength(2));
  await vi.waitFor(() => expect(dialogButton("Reload").disabled).toBe(false));
  expect(dialogButton("Confirm loan").disabled).toBe(true);
  expect(calls.filter(c => c.path.endsWith("confirm_loan"))).toHaveLength(1);
});

test("a late metadata response after disposal cannot mount a wizard", async () => {
  let release: (() => void) | undefined;
  const { root } = await start((path) => {
    if (path.endsWith("/lend")) return json(true);
    if (path.endsWith("erp_normalize_action_result")) return json({wire_major:1,result:wizard});
    if (path.includes("test.wizard/get_views")) return new Promise(resolve => { release = () => resolve(json(wizardDescription())); });
    if (path.includes("test.wizard/default_get")) return json({name:"Ada"});
    if (path.includes("test.wizard/onchange")) return json({value:{}});
    return undefined;
  });
  root.querySelector<HTMLButtonElement>(".form-header-btn")!.click();
  await vi.waitFor(() => expect(document.querySelector("dialog")).not.toBeNull()); choice("Lend");
  await vi.waitFor(() => expect(release).toBeDefined());
  app!.dispose(); release!();
  await vi.waitFor(() => expect(document.querySelector(".wizard-dialog")).toBeNull());
});

test("ambiguous existing-record save requires readback before another write", async () => {
  let saves = 0;
  const { calls } = await setup(undefined, undefined, (path, body) => {
    if (path.endsWith("erp_normalize_action_result") && (body["result"] as {action?:string})?.action === "open") return json({wire_major:1,result:{...wizard,res_id:77}});
    if (path.includes("test.wizard/web_read")) return json([{id:77,name:"Existing"}]);
    if (path.includes("test.wizard/web_save") && ++saves === 1) return new Response(JSON.stringify({message:"Response lost after write."}), {status:500,headers:{"Content-Type":"application/json"}});
    return undefined;
  });
  const input = document.querySelector<HTMLInputElement>('.wizard-dialog input[name="name"]')!;
  input.value = "Changed"; input.dispatchEvent(new Event("change"));
  await vi.waitFor(() => expect(dialogButton("Confirm loan").disabled).toBe(false));
  dialogButton("Confirm loan").click();
  await vi.waitFor(() => expect(document.querySelector(".client-error-dialog")).not.toBeNull());
  expect(dialogButton("Confirm loan").disabled).toBe(true);
  expect(calls.some(c => c.path.endsWith("confirm_loan"))).toBe(false);
  choice("Close"); dialogButton("Reload").click();
  await vi.waitFor(() => expect(dialogButton("Confirm loan").disabled).toBe(false));
  expect(calls.filter(c => c.path.includes("test.wizard/web_read"))).toHaveLength(2);
  expect(calls.filter(c => c.path.includes("test.wizard/web_save"))).toHaveLength(1);
});


test("session expiry during a method disposes every protected scope", async () => {
  const { root, calls } = await setup(undefined, undefined, path => path.endsWith("confirm_loan")
    ? new Response("{}",{status:401,headers:{"Content-Type":"application/json"}}) : undefined);
  dialogButton("Confirm loan").click();
  await vi.waitFor(() => expect(root.textContent).toBe("Session expiredYour session has expired. Sign in again.Sign in"));
  expect(root.querySelectorAll("main")).toHaveLength(1);
  expect(root.querySelector("main h1")?.textContent).toBe("Session expired");
  expect(document.querySelector("dialog")).toBeNull();
  expect(calls.filter(c => c.path.endsWith("confirm_loan"))).toHaveLength(1);
});

test("relational defaults flow through shared initial onchange as ORM commands", async () => {
  const description = wizardDescription();
  Object.assign(description.fields["test.wizard"], {tags:textField("Tags","many2many",{relation:"test.tag"})});
  description.views[0]!.nodes = [
    {kind:"form",record_mode:"record",children:[1,2,3]},
    {...fieldNode("name"),field:{model:"test.wizard",name:"name"}},
    {...fieldNode("tags","many2many"),field:{model:"test.wizard",name:"tags"}},
    {kind:"footer",children:[4,5]},buttonNode({name:"confirm_loan",label:"Confirm loan"}),
    {kind:"cancel_button",label:"Cancel wizard",variant:"secondary",modifiers:modifiers()}
  ];
  const { calls } = await setup(undefined, undefined, path => {
    if (path.includes("test.wizard/get_views")) return json(description);
    if (path.includes("test.wizard/default_get")) return json({name:"Ada",tags:[[6,0,[31]],[4,32,0]]});
    return undefined;
  });
  const onchange = calls.find(c=>c.path.includes("test.wizard/onchange"))!;
  expect((onchange.body["values"] as Record<string,unknown>)["tags"]).toEqual([[6,0,[31,32]]]);
});

test("failed replacement metadata preserves the original scope read-only", async () => {
  let descriptions = 0;
  const { calls } = await setup({...wizard,name:"Next step"}, undefined, path => path.includes("test.wizard/get_views") && ++descriptions > 1
    ? new Response(JSON.stringify({name:"erp.exceptions.AccessError",message:"Next view denied."}),{status:403,headers:{"Content-Type":"application/json"}}) : undefined);
  dialogButton("Confirm loan").click();
  await vi.waitFor(() => expect(presented).toEqual([expect.objectContaining({kind:"permission",message:"Next view denied."})]));
  expect(document.querySelectorAll(".wizard-dialog")).toHaveLength(1);
  expect(document.querySelector(".wizard-heading h2")?.textContent).toBe("Lend book");
  expect(dialogButton("Confirm loan").disabled).toBe(true);
  expect(calls.filter(c => c.path.endsWith("confirm_loan"))).toHaveLength(1);
});

test("a successful next-window result replaces rather than stacks action dialogs", async () => {
  const { calls } = await setup({...wizard,name:"Next step"});
  dialogButton("Confirm loan").click();
  await vi.waitFor(() => expect(document.querySelector(".wizard-heading h2")?.textContent).toBe("Next step"));
  expect(document.querySelectorAll(".wizard-dialog")).toHaveLength(1);
  dialogButton("Cancel wizard").click();
  await vi.waitFor(() => expect(document.querySelector(".wizard-dialog")).toBeNull());
  expect(calls.filter(c => c.path.includes("test.book/web_read"))).toHaveLength(2);
});

test("an explicit close result outside an action dialog is a typed invalid transition", async () => {
  const { root, calls } = await start(path => path.endsWith("erp_normalize_action_result") ? json({wire_major:1,result:{kind:"close"}}) : undefined);
  root.querySelector<HTMLButtonElement>(".form-header-btn")!.click();
  await vi.waitFor(() => expect(document.querySelector(".choice-dialog")).not.toBeNull()); choice("Lend");
  await vi.waitFor(() => expect(presented).toEqual([expect.objectContaining({kind:"validation",message:"Invalid transition: no action dialog to close."})]));
  expect(document.querySelector(".wizard-dialog")).toBeNull();
  expect(calls.filter(c => c.path.endsWith("/lend"))).toHaveLength(1);
});

test("navigating away discards an open dirty wizard without asking or saving", async () => {
  const { root, calls } = await setup();
  const dialog = document.querySelector<HTMLDialogElement>(".wizard-dialog")!;
  root.querySelector<HTMLButtonElement>(".brand")!.click();
  await vi.waitFor(() => expect(dialog.isConnected).toBe(false));
  expect(document.querySelector(".choice-dialog")).toBeNull();
  expect(calls.some(c => c.path.includes("test.wizard/web_save"))).toBe(false);
  expect(calls.some(c => c.path.endsWith("confirm_loan"))).toBe(false);
});

test("a nonclosing notification is announced as text inside the active dialog", async () => {
  await setup({kind:"notification",title:"Heads up",message:"<b>Still open</b>",level:"warning",then_close:false});
  dialogButton("Confirm loan").click();
  await vi.waitFor(()=>expect(document.querySelector('.wizard-dialog [data-wizard-notice]')?.textContent).toBe("Heads up: <b>Still open</b>"));
  expect(document.querySelector('.wizard-dialog [data-wizard-notice]')?.getAttribute("role")).toBe("alert");
  expect(document.querySelector('.wizard-dialog [data-wizard-notice] b')).toBeNull();
});

for (const failure of ["method", "normalization"] as const) {
  test(`uncertain root ${failure} blocks navigation and stays locked after readback`, async () => {
    const { root, calls } = await start(path => {
      if (failure==="method" && path.endsWith("/lend")) return Promise.reject(new TypeError("lost root method response"));
      if (failure==="normalization" && path.endsWith("erp_normalize_action_result")) return new Response(JSON.stringify({name:"erp.exceptions.ValidationError",message:"Root result refused."}),{status:422,headers:{"Content-Type":"application/json"}});
      return undefined;
    });
    const url=location.href;
    root.querySelector<HTMLButtonElement>(".form-header-btn")!.click();
    await vi.waitFor(()=>expect(document.querySelector(".choice-dialog")).not.toBeNull()); choice("Lend");
    await vi.waitFor(()=>expect(document.querySelector(".client-error-dialog")).not.toBeNull());
    expect(presented).toEqual([expect.objectContaining(failure==="method"
      ? {kind:"ambiguous",message:"The connection was interrupted. The change may have been saved. Check the record before trying again."}
      : {kind:"validation",message:"Root result refused."})]);
    choice("Close"); root.querySelector<HTMLButtonElement>(".brand")!.click();
    await vi.waitFor(()=>expect(root.querySelector(".client-notification-message")?.textContent).toBe("The action outcome is unknown. This form is read-only; reloading cannot authorize another attempt."));
    expect(root.querySelector(".record-form")).not.toBeNull();
    expect(location.href).toBe(url);
    root.querySelector<HTMLButtonElement>(".form-reload-btn")!.click();
    await vi.waitFor(()=>expect(calls.filter(c=>c.path.includes("test.book/web_read"))).toHaveLength(2));
    expect([...root.querySelectorAll<HTMLButtonElement>(".form-header-btn")].every(b=>b.disabled)).toBe(true);
    expect(root.querySelector<HTMLInputElement>('input[name="name"]')?.disabled).toBe(true);
    expect(calls.filter(c=>c.path.endsWith("/lend"))).toHaveLength(1);
  });
}

test("wizard header has the title and a close icon; reload is hidden while nothing is uncertain", async () => {
  await setup();
  const heading = document.querySelector<HTMLElement>(".wizard-dialog .wizard-heading")!;
  expect([...heading.querySelectorAll("button")].map(b => b.getAttribute("aria-label"))).toEqual(["Close"]);
  const row = document.querySelector<HTMLElement>(".wizard-dialog .wizard-status-row")!;
  expect(row.hidden).toBe(true);
  expect(row.contains(dialogButton("Reload"))).toBe(true);
  heading.querySelector<HTMLButtonElement>("button")!.click();
  await vi.waitFor(() => expect(document.querySelector(".wizard-dialog")).toBeNull());
});

const related = {kind:"current_window",name:"Related loans",model:"test.loan",views:[{id:null,type:"list"},{id:null,type:"form"}],domain:[["name","=","Visible"]],context:{default_name:"Visible"}};
const relatedDescription = () => ({wire_major:1,registry_version:"r1",description_version:"loan1",model:"test.loan",
  fields:{"test.loan":{name:textField("Name","char",{required:true})}},notices:[],views:[
    {identity:{kind:"stored",id:31},type:"list",schema_major:1,renderer:"web.list",root:0,nodes:[{kind:"list",children:[1]},{...fieldNode("name"),field:{model:"test.loan",name:"name"}}]},
    {...formView([{kind:"form",record_mode:"record",children:[1]},{...fieldNode("name"),field:{model:"test.loan",name:"name"}}]),identity:{kind:"stored",id:32}}
  ]});
const relatedSetup = (failure = false) => start((path,body) => {
  if(path.endsWith("/lend")) return json({open_related:true});
  if(path.endsWith("erp_normalize_action_result")) return json({wire_major:1,result:related});
  if(path.includes("test.loan/get_views")) return failure
    ? new Response(JSON.stringify({name:"erp.exceptions.AccessError",message:"Related loans denied."}),{status:403,headers:{"Content-Type":"application/json"}})
    : json(relatedDescription());
  if(path.includes("test.loan/web_search_read")) return json({length:41,records:[{id:Number(body["offset"]) + 31,name:"Visible"}]});
  if(path.includes("test.loan/web_read")) return json([{id:(body["ids"] as number[])[0],name:"Visible"}]);
  if(path.includes("test.loan/default_get")) return json({name:"Visible"});
  return undefined;
});
const openRelated = async (root: HTMLElement) => {
  await vi.waitFor(()=>expect(root.querySelector<HTMLButtonElement>(".form-header-btn")!.disabled).toBe(false));
  root.querySelector<HTMLButtonElement>(".form-header-btn")!.click();
  await vi.waitFor(()=>expect(document.querySelector("dialog")?.textContent).toContain("Lend this book?"));
  choice("Lend");
};

/// (partial: valid dirty save and single method invocation)
/// (partial: paging, record/default context; category integration is a separate lane)
/// (partial: same-page back/forward retains scope)
test("current-list result saves once, carries literal scope through pages/forms/new and history", async () => {
  const {root,calls} = await relatedSetup();
  const name = root.querySelector<HTMLInputElement>('input[name="name"]')!;
  name.value="Edited source"; name.dispatchEvent(new Event("change",{bubbles:true}));
  await vi.waitFor(()=>expect(calls.some(call=>call.path.endsWith("onchange"))).toBe(true));
  await openRelated(root);
  await vi.waitFor(()=>expect(root.querySelector(".list-table")).not.toBeNull());
  expect(calls.filter(call=>call.path.endsWith("/lend"))).toHaveLength(1);
  expect(calls.filter(call=>call.path.endsWith("web_save"))).toHaveLength(1);
  expect(calls.findIndex(call=>call.path.endsWith("web_save"))).toBeLessThan(calls.findIndex(call=>call.path.endsWith("/lend")));
  const actionId = new URL(location.href).searchParams.get("method_action");
  expect(actionId).not.toBeNull();
  const scope = {lang:"en_US",tz:"UTC",allowed_company_ids:[1],active_model:"test.book",active_id:5,active_ids:[5],default_name:"Visible"};
  const requests = () => calls.filter(call=>call.path.includes("test.loan/web_search_read"));
  expect(requests()[0]!.body["domain"]).toEqual(related.domain);
  expect(requests()[0]!.body["context"]).toEqual(scope);
  expect(calls.find(call=>call.path.includes("test.loan/get_views"))!.body["context"]).toEqual(scope);
  root.querySelector<HTMLButtonElement>('.pager-next')!.click();
  await vi.waitFor(()=>expect(requests().some(call=>call.body["offset"]===40)).toBe(true));
  expect(requests().every(call=>JSON.stringify(call.body["domain"])===JSON.stringify(related.domain))).toBe(true);
  root.querySelector<HTMLButtonElement>('.list-table tbody button.link')!.click();
  await vi.waitFor(()=>expect(root.querySelector('input[name="name"]')).not.toBeNull());
  expect(new URL(location.href).searchParams.get("method_action")).toBe(actionId);
  expect(calls.find(call=>call.path.includes("test.loan/web_read"))!.body["context"]).toEqual(scope);
  history.back();
  await vi.waitFor(()=>expect(root.querySelector(".list-table")).not.toBeNull());
  expect(new URL(location.href).searchParams.get("method_action")).toBe(actionId);
  expect(requests().at(-1)!.body["domain"]).toEqual(related.domain);
  history.forward();
  await vi.waitFor(()=>expect(root.querySelector('input[name="name"]')).not.toBeNull());
  expect(new URL(location.href).searchParams.get("method_action")).toBe(actionId);
  [...root.querySelectorAll<HTMLButtonElement>("button")].find(button=>button.textContent==="New")!.click();
  await vi.waitFor(()=>expect(calls.some(call=>call.path.includes("test.loan/default_get"))).toBe(true));
  expect(calls.find(call=>call.path.includes("test.loan/default_get"))!.body["context"]).toEqual(scope);
  expect(calls.filter(call=>call.path.endsWith("/lend"))).toHaveLength(1);
});

/// (partial: a fresh page refuses a copied or reloaded identity)
test("a copied method-action URL in a fresh page fails loudly without opening an unfiltered destination", async () => {
  const {root,calls}=await relatedSetup();
  await openRelated(root);
  await vi.waitFor(()=>expect(root.querySelector(".list-table")).not.toBeNull());
  const url=location.href;
  const before=calls.filter(call=>call.path.includes("test.loan/web_search_read")).length;
  app!.dispose(); root.remove();
  const fresh=host();
  history.replaceState(null,"",url);
  app=new HumanApp(fresh,bootstrap);
  app.run(()=>app!.start());
  await vi.waitFor(()=>expect(presented.some(error=>error instanceof Error && error.message==="This related action has expired. Open its originating menu again.")).toBe(true));
  expect(fresh.querySelector(".list-table")).toBeNull();
  expect(fresh.querySelector<HTMLAnchorElement>(".method-action-recovery")!.getAttribute("href")).toBe("/erp/action-20");
  expect(calls.filter(call=>call.path.includes("test.loan/web_search_read"))).toHaveLength(before);
});

/// (partial: failed destination preserves interactive source without replay)
test("failed current-list destination retains an interactive source and never repeats its method", async () => {
  const {root,calls} = await relatedSetup(true);
  const url=location.href;
  await openRelated(root);
  await vi.waitFor(()=>expect(presented.some(error=>error instanceof Error && error.message==="Related loans denied.")).toBe(true));
  expect(location.href).toBe(url);
  expect(root.querySelector('input[name="name"]')).not.toBeNull();
  expect(root.querySelector<HTMLButtonElement>(".form-header-btn")!.disabled).toBe(false);
  expect(calls.filter(call=>call.path.endsWith("/lend"))).toHaveLength(1);
  expect(calls.filter(call=>call.path.includes("test.book/web_read")).length).toBeGreaterThan(1);
});

/// (partial: global pending guard and duplicate activation)
test("a pending related action blocks other navigation and duplicate method activation", async () => {
  let release: (response:Response)=>void = ()=>{ throw new Error("method did not start"); };
  const response = new Promise<Response>(resolve=>{release=resolve;});
  const {root,calls}=await start(path=>{
    if(path.endsWith("/lend")) return response;
    if(path.endsWith("erp_normalize_action_result")) return json({wire_major:1,result:related});
    if(path.includes("test.loan/get_views")) return json(relatedDescription());
    if(path.includes("test.loan/web_search_read")) return json({length:1,records:[{id:31,name:"Visible"}]});
    return undefined;
  });
  const url=location.href;
  await openRelated(root);
  await vi.waitFor(()=>expect(calls.filter(call=>call.path.endsWith("/lend"))).toHaveLength(1));
  root.querySelector<HTMLButtonElement>(".form-header-btn")!.click();
  root.querySelector<HTMLElement>('[data-menu-id="10"]')!.click();
  expect(location.href).toBe(url);
  expect(calls.filter(call=>call.path.endsWith("/lend"))).toHaveLength(1);
  release(json({open_related:true}));
  await vi.waitFor(()=>expect(root.querySelector(".list-table")).not.toBeNull());
  expect(calls.filter(call=>call.path.endsWith("/lend"))).toHaveLength(1);
});

/// (partial: list-only results offer no unavailable form/create transition)
test("a list-only current action does not offer record links or a New form", async () => {
  const {root}=await start(path=>{
    if(path.endsWith("/lend")) return json({open_related:true});
    if(path.endsWith("erp_normalize_action_result")) return json({wire_major:1,result:{...related,views:[related.views[0]]}});
    if(path.includes("test.loan/get_views")) return json(relatedDescription());
    if(path.includes("test.loan/web_search_read")) return json({length:1,records:[{id:31,name:"Visible"}]});
    return undefined;
  });
  await openRelated(root);
  await vi.waitFor(()=>expect(root.querySelector(".list-table")).not.toBeNull());
  expect(root.querySelector(".list-table button.link")).toBeNull();
  expect([...root.querySelectorAll<HTMLButtonElement>("button")].some(button=>button.textContent==="New")).toBe(false);
});


/// (partial: mounted wizard cancellation identity with equal explicit captions)
test("equal wizard confirmation captions preserve cancellation separately from acceptance", async () => {
  const description = wizardDescription();
  description.views[0]!.nodes[3] = buttonNode({name: "confirm_loan", label: "Confirm loan", confirm: "Proceed?", details: attributeDetails({confirm_label: "Same", cancel_label: "Same"})});
  const {calls} = await setup(undefined, undefined, path => path.includes("test.wizard/get_views") ? json(description) : undefined);
  dialogButton("Confirm loan").click();
  await vi.waitFor(() => expect(document.querySelectorAll(".choice-dialog .message-dialog-footer button")).toHaveLength(2));
  const choices = [...document.querySelectorAll<HTMLButtonElement>(".choice-dialog .message-dialog-footer button")];
  expect(choices.map(button => button.textContent)).toEqual(["Same", "Same"]); choices[0]!.click();
  await vi.waitFor(() => expect(document.querySelector(".choice-dialog")).toBeNull());
  await vi.waitFor(() => expect(dialogButton("Confirm loan")?.disabled).toBe(false));
  expect(calls.filter(call => call.path.includes("test.wizard/web_save") || call.path.endsWith("/confirm_loan"))).toHaveLength(0);
  dialogButton("Confirm loan").click();
  await vi.waitFor(() => expect(document.querySelectorAll(".choice-dialog .message-dialog-footer button")).toHaveLength(2));
  document.querySelector<HTMLButtonElement>(".choice-dialog .message-dialog-footer [data-default-action]")!.click();
  await vi.waitFor(() => expect(calls.filter(call => call.path.endsWith("/confirm_loan"))).toHaveLength(1));
  expect(calls.filter(call => call.path.includes("test.wizard/web_save"))).toHaveLength(1);
});

/// (partial: mounted title-only wizard object/action defaults and keyboard decisions)
test("title-only wizard controls retain named confirmation defaults without overrides", async () => {
  for (const kind of ["object", "action"]) {
    const description = wizardDescription();
    description.views[0]!.nodes[3] = kind === "object" ? buttonNode({name: "confirm_loan", label: "", confirm: "Proceed?", details: attributeDetails({title: "Run wizard operation"})})
      : {kind: "action_button", target: {kind: "action", id: 21}, label: "", variant: "primary", confirm: "Proceed?", modifiers: modifiers(), details: attributeDetails({title: "Run wizard operation"})};
    const {calls, root} = await setup(undefined, undefined, (path, body) => {
      if (path.includes("test.wizard/get_views")) return json(description);
      if ((path.endsWith("erp_load_web_action") || path.endsWith("erp_load_web_action_views")) && (body["ids"] as number[] | undefined)?.[0] === 21) return json(actionMetadata(path, {wire_major: 1, id: 21, name: "Books", model: "test.book", target: "current", views: [{id: 1, type: "form"}], domain: null, context: null}, descriptionOf(bookForm)));
      return undefined;
    });
    const launch = (): HTMLButtonElement => document.querySelector<HTMLButtonElement>('.wizard-dialog button[aria-label="Run wizard operation"]')!;
    expect(launch()).not.toBeNull(); launch().click();
    await vi.waitFor(() => expect(document.querySelector(".choice-dialog")).not.toBeNull());
    let dialog = document.querySelector<HTMLDialogElement>(".choice-dialog")!;
    expect(document.getElementById(dialog.getAttribute("aria-labelledby")!)?.textContent).toBe("Run wizard operation");
    expect(dialog.querySelector(".message-dialog-footer [data-default-action]")?.textContent).toBe("Run wizard operation");
    expect(document.activeElement).toBe(dialog.querySelector(".message-dialog-footer [data-default-action]"));
    dialog.dispatchEvent(new KeyboardEvent("keydown", {key: "Escape", bubbles: true, cancelable: true}));
    await vi.waitFor(() => expect(dialog.isConnected).toBe(false));
    expect(calls.filter(call => call.path.includes("test.wizard/web_save") || call.path.endsWith("/confirm_loan"))).toHaveLength(0);
    await vi.waitFor(() => expect(launch()?.disabled).toBe(false)); launch().click();
    await vi.waitFor(() => expect(document.querySelector(".choice-dialog")).not.toBeNull());
    dialog = document.querySelector<HTMLDialogElement>(".choice-dialog")!;
    dialog.dispatchEvent(new KeyboardEvent("keydown", {key: "Enter", ctrlKey: true, bubbles: true, cancelable: true}));
    await vi.waitFor(() => expect(calls.some(call => kind === "object" ? call.path.endsWith("/confirm_loan") : call.path.endsWith("erp_load_web_action") && (call.body["ids"] as number[] | undefined)?.[0] === 21)).toBe(true));
    expect(calls.filter(call => call.path.includes("test.wizard/web_save"))).toHaveLength(1);
    app!.dispose(); app = undefined; root.remove(); presented.length = 0;
  }
});

/// (partial: mounted new wizard refuses literal active-identity replacement before method dispatch)
test("button active identities cannot replace the caller context of a newly saved wizard", async () => {
  for (const key of ["active_model", "active_id", "active_ids"]) {
    const description = wizardDescription();
    description.views[0]!.nodes[3] = buttonNode({name: "confirm_loan", label: "Confirm loan", details: attributeDetails({context: activeContext(key)})});
    const {calls, root} = await setup(undefined, undefined, path => path.includes("test.wizard/get_views") ? json(description) : undefined);
    dialogButton("Confirm loan").click();
    await vi.waitFor(() => expect(presented).toContainEqual(expect.objectContaining({kind: "validation", message: `Button context cannot replace protected key '${key}'.`})));
    expect(calls.filter(call => call.path.includes("test.wizard/web_save"))).toHaveLength(1);
    expect(calls.filter(call => call.path.endsWith("/confirm_loan"))).toHaveLength(0);
    expect(document.querySelector(".wizard-dialog")).not.toBeNull();
    app!.dispose(); app = undefined; root.remove(); presented.length = 0;
  }
});

/// (partial: mounted title-only cancel closes without save or method dispatch)
test("a title-only cancel closes the mounted wizard without saving or calling a method", async () => {
  const description = wizardDescription();
  description.views[0]!.nodes[4] = {kind: "cancel_button", label: "", variant: "secondary", modifiers: modifiers(), details: attributeDetails({title: "Close dialog", icon: "lucide:refresh-cw"})};
  const {calls} = await setup(undefined, undefined, path => path.includes("test.wizard/get_views") ? json(description) : undefined);
  const cancel = document.querySelector<HTMLButtonElement>('.wizard-dialog button[aria-label="Close dialog"]')!;
  expect(cancel).not.toBeNull(); expect(cancel.type).toBe("button"); cancel.focus(); expect(document.activeElement).toBe(cancel); cancel.click();
  await vi.waitFor(() => expect(document.querySelector(".wizard-dialog")).toBeNull());
  expect(calls.filter(call => call.path.includes("test.wizard/web_save") || call.path.endsWith("/confirm_loan"))).toHaveLength(0);
});

/// (partial: wizard code commit retains ordinary-field operation locks through Save)
test("wizard keeps ordinary controls disabled while a code draft commits and saves", async () => {
  const d = wizardDescription();
  Object.assign(d.fields["test.wizard"], { source: textField("Source", "text") });
  const nodes = d.views[0]!.nodes as Record<string, unknown>[];
  nodes[0]!["children"] = [1, 5, 2];
  nodes.push({ ...fieldNode("source", "code"), field: { model: "test.wizard", name: "source" }, options: { mode: "xml" } });
  let releaseChange!: () => void, releaseSave!: () => void, changes = 0, writes = 0, committing = false;
  const changing = new Promise<void>(resolve => { releaseChange = resolve; });
  const saving = new Promise<void>(resolve => { releaseSave = resolve; });
  await setup({ kind: "close" }, undefined, (path, body) => {
    if (path.includes("test.wizard/get_views")) return json(d);
    if (path.includes("test.wizard/default_get")) return json({ name: "Borrower", source: "<form/>" });
    if (path.includes("test.wizard/onchange")) { changes++; if ((body["field_names"] as string[]).includes("source")) { committing = true; return changing.then(() => json({ value: {} })); } return json({ value: {} }); }
    if (path.includes("test.wizard/web_save")) { writes++; return saving.then(() => json([{ id: 77, name: "Borrower", source: "draft" }])); }
    return undefined;
  });
  const { default: ace } = await import("ace-builds"); ace.edit(document.querySelector<HTMLElement>(".wizard-dialog .ace_editor")!).setValue("draft", -1); await Promise.resolve();
  dialogButton("Confirm loan").click(); await vi.waitFor(() => expect(committing).toBe(true));
  releaseChange(); await vi.waitFor(() => expect(writes).toBe(1));
  expect(document.querySelector<HTMLInputElement>(".wizard-dialog input[name='name']")?.disabled).toBe(true);
  releaseSave(); await vi.waitFor(() => expect(document.querySelector(".wizard-dialog")).toBeNull()); expect(presented).toEqual([]);
});

/// (partial: shortcut-started wizard rejection after app disposal)
test("wizard shortcut failures remain visible after app disposal",async()=>{
  let reject!:(error:unknown)=>void;const pending=new Promise<Response>((_resolve,no)=>{reject=no;});
  const d=wizardDescription();d.views[0]!.nodes[3]=buttonNode({name:"confirm_loan",label:"Confirm loan",details:attributeDetails({hotkey:"x"})});
  const {calls}=await setup(undefined,undefined,(path)=>path.includes("test.wizard/get_views")?json(d):path.endsWith("/confirm_loan")?pending:undefined);
  const report=vi.fn();vi.stubGlobal("reportError",report);
  const control=dialogButton("Confirm loan");await vi.waitFor(()=>expect(control.disabled).toBe(false));control.focus();
  control.dispatchEvent(new KeyboardEvent("keydown",{key:"x",code:"KeyX",altKey:true,bubbles:true,cancelable:true}));
  await vi.waitFor(()=>expect(calls.filter(call=>call.path.endsWith("/confirm_loan"))).toHaveLength(1));
  app!.dispose();reject(new TypeError("lost response"));
  await vi.waitFor(()=>expect(report).toHaveBeenCalledExactlyOnceWith(expect.objectContaining({kind:"ambiguous",message:"The connection was interrupted. The change may have been saved. Check the record before trying again."})));
  expect(document.querySelector("dialog")).toBeNull();vi.unstubAllGlobals();
});

// My Preferences: the user menu opens the
// preferences window action as the action dialog on the current user's record.
const preferencesAction = { type: "ir.actions.act_window", id: 7, name: "My Preferences", res_model: "res.users", target: "new", view_mode: "form", views: [[9, "form"]] };
const preferencesWindow = { kind: "window", name: "My Preferences", model: "res.users", res_id: 2, views: [{ id: 9, type: "form" }], context: {} };
const userField = (name: string, widget = "char") => ({ ...fieldNode(name, widget), field: { model: "res.users", name } });
const preferencesDescription = () => ({ ...descriptionOf([
  { kind: "form", record_mode: "record", children: [1, 2, 3] },
  userField("name"), userField("lang", "web.preference_choice"),
  { kind: "footer", children: [4, 5] },
  { ...buttonNode({ name: "preference_save", label: "Update Preferences" }), details: attributeDetails({ hotkey: "q" }) },
  { kind: "cancel_button", label: "Cancel", variant: "secondary", modifiers: modifiers(), details: attributeDetails({ hotkey: "x" }) }
]), model: "res.users", fields: { "res.users": { name: textField("Name", "char", { required: true }), lang: textField("Language", "char", { required: true }) } } });
const languageChoices = [["en_US", "English (US)"], ["id_ID", "Indonesian"]];
const preferencesFixtures = (path: string, body: Record<string, unknown>): Response | undefined => {
  if (path.endsWith("res.users/action_get")) return json(preferencesAction);
  if (path.endsWith("erp_normalize_action_result")) {
    const result = body["result"] as { type?: string };
    return json({ wire_major: 1, result: result.type === "ir.actions.act_window" ? preferencesWindow : { kind: "close" } });
  }
  if (path.endsWith("res.users/get_views")) return json(preferencesDescription());
  if (path.endsWith("res.users/web_read")) return json([{ id: 2, name: "Operator", lang: "en_US" }]);
  if (path.endsWith("res.users/onchange")) return json({ value: {} });
  if (path.endsWith("res.partner/fields_get")) return json(Object.fromEntries((body["allfields"] as string[]).map(name => [name, { selection: name === "lang" ? languageChoices : [] }])));
  if (path.endsWith("res.users/read")) return json([{ id: 2, color_scheme: "system" }]);
  if (path.endsWith("res.users/web_save")) return json([{ id: 2, name: "Operator", lang: (body["vals"] as Record<string, unknown>)["lang"] ?? "en_US" }]);
  if (path.endsWith("res.users/preference_save")) return json({ type: "ir.actions.act_window_close" });
  return undefined;
};
const openPreferences = async () => {
  const session = await start(preferencesFixtures);
  const reload = vi.fn();
  app!.reloadPage = reload;
  const entry = session.root.querySelector<HTMLButtonElement>(".user-dropdown-preferences")!;
  entry.click();
  await vi.waitFor(() => expect(document.querySelector(".wizard-dialog")).not.toBeNull());
  return { ...session, entry, reload };
};

/// (partial: mounted client with explicit HTTP fixtures; installed backend acceptance is crates/erp-cli/tests/user_preferences.rs)
test("the user menu lists My Preferences above Sign out and opens the dialog on the current user", async () => {
  const { root, calls, entry } = await openPreferences();
  const menu = [...root.querySelector(".user-dropdown")!.children];
  expect(entry.textContent).toBe("My preferences");
  expect(menu.indexOf(entry)).toBeGreaterThan(-1);
  expect(menu.indexOf(entry)).toBeLessThan(menu.findIndex(node => node.tagName === "FORM"));
  expect(root.querySelector(".user-dropdown")!.classList.contains("open")).toBe(false);
  const normalize = calls.find(c => c.path.endsWith("erp_normalize_action_result"))!;
  expect(normalize.body["result"]).toEqual({ type: "ir.actions.act_window", name: preferencesAction.name, res_model: "res.users", target: "new", views: preferencesAction.views, res_id: bootstrap.identity.uid, context: {} });
  expect(calls.find(c => c.path.endsWith("res.users/get_views"))!.body["views"]).toEqual([[9, "form"]]);
  expect(calls.find(c => c.path.endsWith("res.users/web_read"))!.body["ids"]).toEqual([2]);
  expect(document.querySelector(".wizard-dialog h2")?.textContent).toBe("My Preferences");
});

const held = () => { let release!: () => void; const gate = new Promise<void>(resolve => { release = resolve; }); return { gate, release }; };
const menusOf = () => json({ wire_major: 1, registry_version: "r1", roots: [10], nodes: [{ module: null, id: 10, parent_id: null, label: "Books", action_id: 20, children: [], icon: null }] });

/// (partial: mounted client with explicit HTTP fixtures; chosen before the first page has started loading)
test("My Preferences chosen while the menus load still opens after the first page starts", async () => {
  const menus = held(), answer = held();
  let pageStarted = false;
  const { root, calls, started } = launch((path, body) => {
    if (path.endsWith("erp_load_web_menus")) return menus.gate.then(menusOf);
    if (path.includes("erp_load_web_action")) { pageStarted = true; return undefined; }
    if (path.endsWith("res.users/action_get")) return answer.gate.then(() => preferencesFixtures(path, body));
    return preferencesFixtures(path, body);
  });
  const entry = root.querySelector<HTMLButtonElement>(".user-dropdown-preferences");
  expect(entry).not.toBeNull();
  entry!.click();
  await vi.waitFor(() => expect(calls.some(c => c.path.endsWith("res.users/action_get"))).toBe(true));
  // the first navigation starts while the preferences action is still being read
  menus.release();
  await vi.waitFor(() => expect(pageStarted).toBe(true));
  answer.release();
  await started;
  await vi.waitFor(() => expect(document.querySelector(".wizard-dialog h2")?.textContent).toBe("My Preferences"));
  await vi.waitFor(() => expect(root.querySelector(".form-header")).not.toBeNull());
  expect(calls.filter(c => c.path.endsWith("res.users/action_get"))).toHaveLength(1);
  expect(presented).toEqual([]);
});

/// (partial: mounted client with explicit HTTP fixtures; chosen while the first page loads)
test("My Preferences chosen while the first page loads opens once that page is shown", async () => {
  const page = held();
  let pageStarted = false;
  const { root, calls, started } = launch((path, body) => {
    if (path.includes("erp_load_web_action")) { pageStarted = true; return page.gate.then(() => undefined); }
    return preferencesFixtures(path, body);
  });
  await vi.waitFor(() => expect(pageStarted).toBe(true));
  root.querySelector<HTMLButtonElement>(".user-dropdown-preferences")!.click();
  await Promise.resolve();
  expect(calls.some(c => c.path.endsWith("res.users/action_get"))).toBe(false);
  page.release();
  await started;
  await vi.waitFor(() => expect(document.querySelector(".wizard-dialog h2")?.textContent).toBe("My Preferences"));
  await vi.waitFor(() => expect(root.querySelector(".form-header")).not.toBeNull());
  expect(calls.filter(c => c.path.endsWith("res.users/action_get"))).toHaveLength(1);
  expect(presented).toEqual([]);
});

/// (partial: mounted client with explicit HTTP fixtures; the real list comes from res.partner.fields_get)
test("the language picker offers the server's choices and keeps the current value", async () => {
  const { calls } = await openPreferences();
  await vi.waitFor(() => expect(document.querySelectorAll(".wizard-dialog select option").length).toBe(languageChoices.length + 1));
  const select = document.querySelector<HTMLSelectElement>(".wizard-dialog select")!;
  // the selection control adds its own empty option in front of the server's list
  expect([...select.options].filter(option => option.value !== "").map(option => [option.value, option.textContent])).toEqual(languageChoices);
  expect(select.value).toBe("en_US");
  expect(select.disabled).toBe(false);
  const requests = calls.filter(c => c.path.endsWith("res.partner/fields_get"));
  expect(requests.map(c => c.body["allfields"])).toEqual([["lang"]]);
  expect(requests[0]!.body["attributes"]).toEqual(["selection"]);
});

/// (partial: mounted client with explicit HTTP fixtures)
test("Update Preferences saves, runs the method once, closes the dialog and reloads the page", async () => {
  const { calls, reload } = await openPreferences();
  await vi.waitFor(() => expect(document.querySelectorAll(".wizard-dialog select option").length).toBe(languageChoices.length + 1));
  const select = document.querySelector<HTMLSelectElement>(".wizard-dialog select")!;
  select.value = "id_ID";
  select.dispatchEvent(new Event("change", { bubbles: true }));
  await vi.waitFor(() => expect(dialogButton("Update Preferences").disabled).toBe(false));
  dialogButton("Update Preferences").click();
  await vi.waitFor(() => expect(document.querySelector(".wizard-dialog")).toBeNull());
  const save = calls.findIndex(c => c.path.endsWith("res.users/web_save"));
  const method = calls.findIndex(c => c.path.endsWith("res.users/preference_save"));
  expect(save).toBeGreaterThan(-1);
  expect(method).toBeGreaterThan(save);
  expect((calls[save]!.body["vals"] as Record<string, unknown>)["lang"]).toBe("id_ID");
  expect(calls[method]!.body["ids"]).toEqual([2]);
  expect(calls.filter(c => c.path.endsWith("res.users/preference_save"))).toHaveLength(1);
  expect(reload).toHaveBeenCalledTimes(1);
});

/// (partial: mounted client with explicit HTTP fixtures)
test("Cancel and Escape close the dialog without a save, a method call or a reload", async () => {
  const first = await openPreferences();
  await vi.waitFor(() => expect(dialogButton("Cancel").disabled).toBe(false));
  dialogButton("Cancel").click();
  await vi.waitFor(() => expect(document.querySelector(".wizard-dialog")).toBeNull());
  expect(first.reload).not.toHaveBeenCalled();
  first.root.querySelector<HTMLButtonElement>(".user-dropdown-preferences")!.click();
  await vi.waitFor(() => expect(document.querySelector(".wizard-dialog")).not.toBeNull());
  document.querySelector<HTMLDialogElement>(".wizard-dialog")!.dispatchEvent(new Event("cancel", { cancelable: true }));
  await vi.waitFor(() => expect(document.querySelector(".wizard-dialog")).toBeNull());
  expect(first.calls.some(c => c.path.endsWith("res.users/web_save") || c.path.endsWith("res.users/preference_save"))).toBe(false);
  expect(first.reload).not.toHaveBeenCalled();
});

/// (partial: mounted client with explicit HTTP fixtures)
test("a refused save keeps the dialog open and does not reload", async () => {
  const session = await start((path, body) => path.endsWith("res.users/web_save")
    ? new Response(JSON.stringify({ name: "erp.exceptions.ValidationError", message: "Wrong value for res.users.lang: 'id_ID'" }), { status: 422, headers: { "Content-Type": "application/json" } })
    : preferencesFixtures(path, body));
  const reload = vi.fn();
  app!.reloadPage = reload;
  session.root.querySelector<HTMLButtonElement>(".user-dropdown-preferences")!.click();
  await vi.waitFor(() => expect(document.querySelector(".wizard-dialog")).not.toBeNull());
  await vi.waitFor(() => expect(document.querySelectorAll(".wizard-dialog select option").length).toBe(languageChoices.length + 1));
  const select = document.querySelector<HTMLSelectElement>(".wizard-dialog select")!;
  select.value = "id_ID";
  select.dispatchEvent(new Event("change", { bubbles: true }));
  await vi.waitFor(() => expect(dialogButton("Update Preferences").disabled).toBe(false));
  dialogButton("Update Preferences").click();
  await vi.waitFor(() => expect(document.querySelector(".client-error-dialog")?.textContent).toContain("Wrong value for res.users.lang: 'id_ID'"));
  expect(document.querySelector(".wizard-dialog")).not.toBeNull();
  expect(session.calls.some(c => c.path.endsWith("res.users/preference_save"))).toBe(false);
  expect(reload).not.toHaveBeenCalled();
});

/// (partial: mounted client with explicit HTTP fixtures)
test("a method answer that leaves the dialog open does not reload the page", async () => {
  const session = await start((path, body) => {
    if (path.endsWith("res.users/preference_save")) return json({ type: "notify" });
    if (path.endsWith("erp_normalize_action_result") && (body["result"] as { type?: string }).type === "notify") {
      return json({ wire_major: 1, result: { kind: "notification", title: "Heads up", message: "Not done", level: "warning", then_close: false } });
    }
    return preferencesFixtures(path, body);
  });
  const reload = vi.fn();
  app!.reloadPage = reload;
  session.root.querySelector<HTMLButtonElement>(".user-dropdown-preferences")!.click();
  await vi.waitFor(() => expect(document.querySelector(".wizard-dialog")).not.toBeNull());
  await vi.waitFor(() => expect(dialogButton("Update Preferences").disabled).toBe(false));
  dialogButton("Update Preferences").click();
  await vi.waitFor(() => expect(session.calls.filter(c => c.path.endsWith("res.users/preference_save"))).toHaveLength(1));
  await vi.waitFor(() => expect(dialogButton("Update Preferences").disabled).toBe(false));
  expect(document.querySelector(".wizard-dialog")).not.toBeNull();
  expect(reload).not.toHaveBeenCalled();
});

const profileDescription = () => {
  const field = (name: string, widget = "char") => ({ ...fieldNode(name, widget), field: { model: "res.users", name } });
  return { ...descriptionOf([
    { kind: "form", record_mode: "record", children: [1, 7] },
    { kind: "sheet", children: [2] },
    { kind: "title", children: [3, 5, 6] },
    { kind: "heading", level: 1, children: [4] },
    field("name"), field("email", "email"), field("phone", "phone"),
    { kind: "footer", children: [8, 9] },
    { ...buttonNode({ name: "preference_save", label: "Update Preferences" }) },
    { kind: "cancel_button", label: "Cancel", variant: "secondary", modifiers: modifiers() }
  ]), model: "res.users", fields: { "res.users": { name: textField("Name"), email: textField("Email"), phone: textField("Phone") } } };
};
const profileFixtures = (scheme: unknown) => (path: string, body: Record<string, unknown>): Response | undefined => {
  if (path.endsWith("res.users/get_views")) return json(profileDescription());
  if (path.endsWith("res.users/web_read")) return json([{ id: 2, name: "Operator", email: "op@example.com", phone: "+62 21 555 0100" }]);
  if (path.endsWith("res.users/read")) return json([{ id: 2, color_scheme: scheme }]);
  if (path.endsWith("erp_describe")) return json({ wire_major: 1, registry_version: "r1", scope: body["scope"], declarations: [] });
  return preferencesFixtures(path, body);
};

/// (partial: mounted client; the dialog's title block is drawn as a profile header)
test("My Preferences draws the user's initial, the heading and an icon on the e-mail and phone rows", async () => {
  const session = await start(profileFixtures("system"));
  session.root.querySelector<HTMLButtonElement>(".user-dropdown-preferences")!.click();
  await vi.waitFor(() => expect(document.querySelector(".wizard-dialog .form-title-profile")).not.toBeNull());
  const block = document.querySelector<HTMLElement>(".wizard-dialog .form-title-profile")!;
  expect(block.querySelector(".user-avatar")!.getAttribute("data-letter")).toBe("O");
  expect(block.querySelector<HTMLInputElement>("h1 input")!.value).toBe("Operator");
  expect([...block.querySelectorAll(".form-title-row > svg")].map(svg => svg.getAttribute("data-icon"))).toEqual(["mail", "phone"]);
  expect([...block.querySelectorAll<HTMLInputElement>(".form-title-row input")].map(input => input.value)).toEqual(["op@example.com", "+62 21 555 0100"]);
});

/// (partial: mounted client; the saved scheme is read and applied when the application starts)
test("the saved colour scheme is applied at start, and a failed read is reported without stopping the start", async () => {
  const dark = launch(profileFixtures("dark"), true);
  await dark.started;
  expect(document.documentElement.dataset["theme"]).toBe("dark");
  expect(dark.calls.find(c => c.path.endsWith("res.users/read"))!.body).toMatchObject({ ids: [2], fields: ["color_scheme"] });
  app?.dispose(); app = undefined; delete document.documentElement.dataset["theme"]; document.body.replaceChildren();
  const broken = launch(profileFixtures("sepia"), true);
  await broken.started;
  await vi.waitFor(() => expect(broken.root.querySelector(".form-header")).not.toBeNull());
  expect(document.documentElement.dataset["theme"]).toBeUndefined();
  await vi.waitFor(() => expect(presented.some(error => (error as Error).message === "Invalid colour scheme.")).toBe(true));
});

/// (partial: mounted client; a refused e-mail keeps its message visible in the profile header)
test("an invalid e-mail in the profile header shows its message, not only a red border", async () => {
  vi.spyOn(HTMLInputElement.prototype, "validationMessage", "get").mockReturnValue("Enter a valid e-mail address.");
  const session = await start(profileFixtures("system"));
  session.root.querySelector<HTMLButtonElement>(".user-dropdown-preferences")!.click();
  await vi.waitFor(() => expect(document.querySelector(".wizard-dialog .form-title-profile")).not.toBeNull());
  const input = document.querySelector<HTMLInputElement>(".wizard-dialog .form-title-row input[type=email]")!;
  input.value = "foo"; input.dispatchEvent(new Event("input", { bubbles: true }));
  const error = document.querySelector<HTMLElement>(".wizard-dialog .form-title-row .field-error")!;
  expect(error.hidden).toBe(false);
  expect(error.textContent).toBe("Enter a valid e-mail address.");
  expect(input.getAttribute("aria-invalid")).toBe("true");
  // not the visually hidden caption: the hiding rule is written for the caption only, and the caption stays out of its reach
  expect(error.matches(".field-label > span:not(.field-error):not(.field-help)")).toBe(false);
  expect(getComputedStyle(error).clipPath).not.toBe("inset(50%)");
});

/// (partial: mounted client; Update Preferences applies the new scheme before the reload)
test("Update Preferences applies the saved colour scheme and then reloads the page", async () => {
  const session = await start(profileFixtures("light"));
  const reload = vi.fn(() => { expect(document.documentElement.dataset["theme"]).toBe("light"); });
  app!.reloadPage = reload;
  session.root.querySelector<HTMLButtonElement>(".user-dropdown-preferences")!.click();
  await vi.waitFor(() => expect(document.querySelector(".wizard-dialog .form-title-profile")).not.toBeNull());
  await vi.waitFor(() => expect(dialogButton("Update Preferences").disabled).toBe(false));
  dialogButton("Update Preferences").click();
  await vi.waitFor(() => expect(reload).toHaveBeenCalledTimes(1));
  expect(session.calls.filter(c => c.path.endsWith("res.users/read"))).toHaveLength(1);
});

const altKey = (key: string): KeyboardEvent => new KeyboardEvent("keydown", { key, code: `Key${key.toUpperCase()}`, altKey: true, bubbles: true, cancelable: true });

/// (partial: mounted client with explicit HTTP fixtures; hotkey ownership is SPEC-BTN /)
test("the footer hotkeys work in the dialog: Alt+x cancels, Alt+q updates", async () => {
  const cancelled = await openPreferences();
  await vi.waitFor(() => expect(dialogButton("Cancel").disabled).toBe(false));
  document.querySelector(".wizard-dialog")!.dispatchEvent(altKey("x"));
  await vi.waitFor(() => expect(document.querySelector(".wizard-dialog")).toBeNull());
  expect(cancelled.calls.some(c => c.path.endsWith("res.users/web_save") || c.path.endsWith("res.users/preference_save"))).toBe(false);
  cancelled.root.querySelector<HTMLButtonElement>(".user-dropdown-preferences")!.click();
  await vi.waitFor(() => expect(document.querySelector(".wizard-dialog")).not.toBeNull());
  await vi.waitFor(() => expect(dialogButton("Update Preferences").disabled).toBe(false));
  document.querySelector(".wizard-dialog")!.dispatchEvent(altKey("q"));
  await vi.waitFor(() => expect(cancelled.reload).toHaveBeenCalledTimes(1));
  expect(cancelled.calls.filter(c => c.path.endsWith("res.users/preference_save"))).toHaveLength(1);
});

/// (partial: mounted client with explicit HTTP fixtures; the retained-value rule)
test("a stored value the server no longer lists stays visible and selected", async () => {
  const session = await start((path, body) => path.endsWith("res.users/web_read") ? json([{ id: 2, name: "Operator", lang: "xx_XX" }]) : preferencesFixtures(path, body));
  session.root.querySelector<HTMLButtonElement>(".user-dropdown-preferences")!.click();
  await vi.waitFor(() => expect(document.querySelector(".wizard-dialog")).not.toBeNull());
  await vi.waitFor(() => expect(document.querySelectorAll(".wizard-dialog select option").length).toBe(languageChoices.length + 2));
  const select = document.querySelector<HTMLSelectElement>(".wizard-dialog select")!;
  expect(select.value).toBe("xx_XX");
  expect([...select.options].map(option => option.value)).toEqual(["", "en_US", "id_ID", "xx_XX"]);
});

// Password changes: from My Preferences, the identity
// confirmation, then the own-password form. Secrets reach only their managed operations; nothing is saved.
const windowOf = (model: string, name: string, res_id: number | null) => ({ kind: "window", name, model, res_id, views: [{ id: null, type: "form" }], context: {} });
const secretField = (model: string, name: string) => ({ ...fieldNode(name, "password"), field: { model, name } });
const secretForm = (model: string, names: string[], method: string, label: string) => ({ ...descriptionOf([
  { kind: "form", record_mode: "record", children: [...names.map((_, i) => i + 1), names.length + 1] },
  ...names.map(name => secretField(model, name)),
  { kind: "footer", children: [names.length + 2, names.length + 3] },
  buttonNode({ name: method, label }),
  { kind: "cancel_button", label: "Cancel", variant: "secondary", modifiers: modifiers() }
]), model, fields: { [model]: Object.fromEntries(names.map(name => [name, textField(name, "char", { required: true })])) } });
const passwordFixtures = (options: { ownAnswers?: ("identity" | "close" | "lost")[] } = {}) => {
  let opened = 0;
  const ownAnswers = [...(options.ownAnswers ?? ["close"])];
  const respond = (path: string, body: Record<string, unknown>): Promise<Response> | Response | undefined => {
    if (path.endsWith("res.users/get_views")) {
      const description = preferencesDescription();
      const nodes = description.views[0]!.nodes as unknown[];
      (nodes[3] as { children: number[] }).children.push(nodes.length);
      nodes.push(buttonNode({ name: "preference_change_password", label: "Change password", variant: "secondary" }));
      return json(description);
    }
    if (path.endsWith("res.users/preference_change_password")) {
      opened++;
      return json(opened === 1 ? { type: "ir.actions.act_window", res_model: "res.users.identitycheck" } : { type: "ir.actions.act_window", res_model: "change.password.own", res_id: 9 });
    }
    if (path.endsWith("erp_normalize_action_result")) {
      const result = body["result"] as { type?: string; res_model?: string };
      const normalized = result.type !== "ir.actions.act_window" ? { kind: "close" }
        : result.res_model === "res.users.identitycheck" ? windowOf("res.users.identitycheck", "Confirm your identity", null)
        : result.res_model === "change.password.own" ? windowOf("change.password.own", "Change password", 9) : preferencesWindow;
      return json({ wire_major: 1, result: normalized });
    }
    if (path.endsWith("res.users.identitycheck/get_views")) return json(secretForm("res.users.identitycheck", ["password"], "run_check", "Confirm"));
    if (path.endsWith("change.password.own/get_views")) return json(secretForm("change.password.own", ["new_password", "confirm_password"], "change_password", "Change password"));
    if (path.endsWith("res.users.identitycheck/onchange")) return json({ value: {} });
    if (path.endsWith("res.users.identitycheck/default_get")) return json({});
    if (path.endsWith("change.password.own/web_read")) return json([{ id: 9, new_password: false, confirm_password: false }]);
    if (path.endsWith("change.password.own/change_password")) {
      const answer = ownAnswers.shift() ?? "close";
      if (answer === "lost") return Promise.reject(new TypeError("lost response")) as unknown as Response;
      return json(answer === "identity" ? { type: "ir.actions.act_window", res_model: "res.users.identitycheck" } : { type: "ir.actions.act_window_close" });
    }
    if (path.endsWith("/run_check")) return json({ type: "ir.actions.act_window_close" });
    if (path.includes("identitycheck/web_save") || path.includes("change.password.own/web_save") || path.includes("identitycheck/onchange") && (body["field_names"] as unknown[]).length)
      return new Response(JSON.stringify({ name: "erp.exceptions.ValidationError", message: "Passwords must be submitted with the dedicated password operation." }), { status: 422, headers: { "Content-Type": "application/json" } });
    return preferencesFixtures(path, body);
  };
  return respond;
};
const typeSecret = (name: string, value: string): void => {
  const input = document.querySelector<HTMLInputElement>(`.wizard-dialog input[name='${name}']`)!;
  expect(input.type).toBe("password");
  input.value = value; input.dispatchEvent(new Event("input", { bubbles: true })); input.dispatchEvent(new Event("change", { bubbles: true }));
};
const heading = () => document.querySelector(".wizard-dialog h2")?.textContent;

/// (partial: mounted client with explicit HTTP fixtures; the confirmation resubmits the kept operation; installed acceptance is crates/erp-cli/tests/password_change.rs)
/// (partial: mounted client with explicit HTTP fixtures; no password field is saved or sent to onchange)
test("Change password confirms the identity, sends the kept operation again and submits the own form", async () => {
  const session = await start(passwordFixtures());
  const reload = vi.fn(); app!.reloadPage = reload;
  session.root.querySelector<HTMLButtonElement>(".user-dropdown-preferences")!.click();
  await vi.waitFor(() => expect(heading()).toBe("My Preferences"));
  await vi.waitFor(() => expect(dialogButton("Change password").disabled).toBe(false));
  dialogButton("Change password").click();
  await vi.waitFor(() => expect(heading()).toBe("Confirm your identity"));
  await vi.waitFor(() => expect(dialogButton("Confirm").disabled).toBe(false));
  typeSecret("password", "current-secret");
  await vi.waitFor(() => expect(dialogButton("Confirm").disabled).toBe(false));
  dialogButton("Confirm").click();
  await vi.waitFor(() => expect(heading()).toBe("Change password"));
  const calls = session.calls;
  const check = calls.filter(c => c.path.endsWith("res.users.identitycheck/run_check"));
  expect(check).toHaveLength(1);
  expect(check[0]!.body["password"]).toBe("current-secret");
  expect(check[0]!.body["ids"]).toBeUndefined();
  const opens = calls.filter(c => c.path.endsWith("res.users/preference_change_password"));
  expect(opens.map(c => c.body["ids"])).toEqual([[2], [2]]);
  await vi.waitFor(() => expect(dialogButton("Change password").disabled).toBe(false));
  typeSecret("new_password", "next-secret");
  await vi.waitFor(() => expect(dialogButton("Change password").disabled).toBe(false));
  typeSecret("confirm_password", "next-secret");
  await vi.waitFor(() => expect(dialogButton("Change password").disabled).toBe(false));
  dialogButton("Change password").click();
  await vi.waitFor(() => expect(document.querySelector(".wizard-dialog")).toBeNull());
  const change = calls.filter(c => c.path.endsWith("change.password.own/change_password"));
  expect(change.map(c => [c.body["ids"], c.body["new_password"], c.body["confirm_password"]])).toEqual([[[9], "next-secret", "next-secret"]]);
  // no secret left through a save or an onchange
  const leaked = calls.filter(c => JSON.stringify(c.body).includes("-secret") && !c.path.endsWith("/run_check") && !c.path.endsWith("change.password.own/change_password"));
  expect(leaked.map(c => c.path)).toEqual([]);
  expect(calls.some(c => c.path.includes("identitycheck/web_save") || c.path.includes("change.password.own/web_save"))).toBe(false);
  expect(reload).not.toHaveBeenCalled();
  expect(presented).toEqual([]);
});

/// (partial: mounted client with explicit HTTP fixtures; a refused confirmation keeps the dialog and sends nothing again)
test("a refused identity confirmation keeps the dialog open and does not resend the operation", async () => {
  const fixtures = passwordFixtures();
  const session = await start((path, body) => path.endsWith("/run_check")
    ? new Response(JSON.stringify({ name: "erp.exceptions.AccessDenied", message: "Access Denied" }), { status: 403, headers: { "Content-Type": "application/json" } })
    : fixtures(path, body));
  session.root.querySelector<HTMLButtonElement>(".user-dropdown-preferences")!.click();
  await vi.waitFor(() => expect(dialogButton("Change password").disabled).toBe(false));
  dialogButton("Change password").click();
  await vi.waitFor(() => expect(heading()).toBe("Confirm your identity"));
  await vi.waitFor(() => expect(dialogButton("Confirm").disabled).toBe(false));
  typeSecret("password", "wrong-secret");
  await vi.waitFor(() => expect(dialogButton("Confirm").disabled).toBe(false));
  dialogButton("Confirm").click();
  await vi.waitFor(() => expect(presented).toHaveLength(1));
  expect(heading()).toBe("Confirm your identity");
  expect(session.calls.filter(c => c.path.endsWith("res.users/preference_change_password"))).toHaveLength(1);
});

const openIdentity = async (fixtures: ReturnType<typeof passwordFixtures>) => {
  const session = await start(fixtures);
  session.root.querySelector<HTMLButtonElement>(".user-dropdown-preferences")!.click();
  await vi.waitFor(() => expect(dialogButton("Change password").disabled).toBe(false));
  dialogButton("Change password").click();
  await vi.waitFor(() => expect(heading()).toBe("Confirm your identity"));
  await vi.waitFor(() => expect(dialogButton("Confirm").disabled).toBe(false));
  return session;
};
const confirmIdentity = async () => {
  typeSecret("password", "current-secret");
  await vi.waitFor(() => expect(dialogButton("Confirm").disabled).toBe(false));
  dialogButton("Confirm").click();
};
const submitOwn = async () => {
  await vi.waitFor(() => expect(heading()).toBe("Change password"));
  await vi.waitFor(() => expect(dialogButton("Change password").disabled).toBe(false));
  typeSecret("new_password", "next-secret");
  await vi.waitFor(() => expect(dialogButton("Change password").disabled).toBe(false));
  typeSecret("confirm_password", "next-secret");
  await vi.waitFor(() => expect(dialogButton("Change password").disabled).toBe(false));
  dialogButton("Change password").click();
};

/// (partial: mounted client with explicit HTTP fixtures; a click right after typing, whose blur commits the field, still confirms)
test("Confirm clicked right after typing the password sends run_check once", async () => {
  const session = await openIdentity(passwordFixtures());
  // A browser fires the field's change event when the click blurs it, then the click: no wait in between.
  typeSecret("password", "current-secret");
  dialogButton("Confirm").click();
  await vi.waitFor(() => expect(heading()).toBe("Change password"));
  expect(session.calls.filter(c => c.path.endsWith("/run_check")).map(c => c.body["password"])).toEqual(["current-secret"]);
});

/// (partial: mounted client with explicit HTTP fixtures; typing both passwords and clicking at once submits the own form)
test("Change password clicked right after typing both passwords submits them", async () => {
  const session = await openIdentity(passwordFixtures());
  typeSecret("password", "current-secret");
  dialogButton("Confirm").click();
  await vi.waitFor(() => expect(heading()).toBe("Change password"));
  await vi.waitFor(() => expect(dialogButton("Change password").disabled).toBe(false));
  typeSecret("new_password", "next-secret");
  typeSecret("confirm_password", "next-secret");
  dialogButton("Change password").click();
  await vi.waitFor(() => expect(document.querySelector(".wizard-dialog")).toBeNull());
  const change = session.calls.filter(c => c.path.endsWith("change.password.own/change_password"));
  expect(change.map(c => [c.body["new_password"], c.body["confirm_password"]])).toEqual([["next-secret", "next-secret"]]);
});

/// (partial: mounted client with explicit HTTP fixtures; Enter in the identity form presses Confirm and never saves)
test("Enter in the identity confirmation sends run_check and never a save", async () => {
  const session = await openIdentity(passwordFixtures());
  typeSecret("password", "current-secret");
  await vi.waitFor(() => expect(dialogButton("Confirm").disabled).toBe(false));
  document.querySelector<HTMLFormElement>(".wizard-dialog form")!.dispatchEvent(new Event("submit", { bubbles: true, cancelable: true }));
  await vi.waitFor(() => expect(heading()).toBe("Change password"));
  expect(session.calls.filter(c => c.path.endsWith("/run_check"))).toHaveLength(1);
  expect(session.calls.some(c => c.path.endsWith("/web_save"))).toBe(false);
});

/// (partial: mounted client with explicit HTTP fixtures; an expired window on submission asks again and resends the same submission once)
test("a submission answered with the identity confirmation is sent again exactly once after confirming", async () => {
  const session = await openIdentity(passwordFixtures({ ownAnswers: ["identity", "close"] }));
  await confirmIdentity();
  await submitOwn();
  await vi.waitFor(() => expect(heading()).toBe("Confirm your identity"));
  await vi.waitFor(() => expect(dialogButton("Confirm").disabled).toBe(false));
  await confirmIdentity();
  const sent = () => session.calls.filter(c => c.path.endsWith("change.password.own/change_password"));
  // the dialog closes before the kept operation is sent again: wait for that call, then settle so a stray third send shows up
  await vi.waitFor(() => expect(sent()).toHaveLength(2));
  await vi.waitFor(() => expect(document.querySelector(".wizard-dialog")).toBeNull());
  await new Promise(resolve => setTimeout(resolve, 50));
  const changes = sent();
  expect(changes).toHaveLength(2);
  expect(changes[1]!.body).toEqual(changes[0]!.body);
  expect(session.calls.filter(c => c.path.endsWith("/run_check"))).toHaveLength(2);
  expect(presented).toEqual([]);
});

/// (partial: mounted client with explicit HTTP fixtures; an interrupted resubmission is reported and never repeated)
test("an interrupted resubmission is reported as unknown and not sent a third time", async () => {
  const session = await openIdentity(passwordFixtures({ ownAnswers: ["identity", "lost", "close"] }));
  await confirmIdentity();
  await submitOwn();
  await vi.waitFor(() => expect(heading()).toBe("Confirm your identity"));
  await vi.waitFor(() => expect(dialogButton("Confirm").disabled).toBe(false));
  await confirmIdentity();
  await vi.waitFor(() => expect(presented).toContainEqual(expect.objectContaining({ kind: "ambiguous" })));
  await new Promise(resolve => setTimeout(resolve, 50));
  expect(session.calls.filter(c => c.path.endsWith("change.password.own/change_password"))).toHaveLength(2);
  // the read-only outcome: the app holds the uncertain-method state, so it counts as dirty and refuses to leave
  const state = app as unknown as { uncertainMethod: boolean; isDirty(): boolean };
  expect(state.uncertainMethod).toBe(true);
  expect(state.isDirty()).toBe(true);
});

/// Second review pass: the way back from a related form keeps the related list identity and the origin record crumb.
test("a form opened from a related list keeps the origin crumb and returns to the same related list", async () => {
  const {root} = await relatedSetup();
  await openRelated(root);
  await vi.waitFor(()=>expect(root.querySelector(".list-table")).not.toBeNull());
  const actionId = new URL(location.href).searchParams.get("method_action");
  expect(actionId).not.toBeNull();
  root.querySelector<HTMLButtonElement>('.list-table tbody button.link')!.click();
  await vi.waitFor(()=>expect(root.querySelector('input[name="name"]')).not.toBeNull());
  expect(root.querySelector(".breadcrumb-origin")).not.toBeNull();
  root.querySelector<HTMLButtonElement>(".breadcrumb-back")!.click();
  await vi.waitFor(()=>expect(root.querySelector(".list-table")).not.toBeNull());
  expect(new URL(location.href).searchParams.get("method_action")).toBe(actionId);
});
