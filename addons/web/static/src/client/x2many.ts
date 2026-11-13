/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
import { actionButton, button, el } from "./dom.js";
import { renderFormLayout } from "./form-layout.js";
import { icon } from "./icons.js";
import { array, integer, object, type Description, type Field, type Node, type Subview, type View } from "./metadata.js";
import { lineFields, linePolicy, newLineKey, type Line } from "./record.js";
import { effectiveNodes } from "./field-modifiers.js";
import { CurrencyDisplayCache } from "./field-currency.js";
import { relationQuery } from "./field-relations.js";
import { resolveFieldWidget, registerFieldWidget, type WidgetEnvironment, type WidgetHandle, type WidgetDraft } from "./registry.js";
import { ClientError } from "./transport.js";

/** How many line-form levels below the opened form get their one2many views attached. A deeper
 *  one2many keeps the name-only list. */
export const SUBVIEW_DEPTH = 3;

/**
 *  One request per comodel; a comodel
 *  that cannot be described leaves its fields with `subviewError`, which the widget shows.
 *  The comodel's own line form is attached the same way, so a line opened in a dialog shows its
 *  one2many with the comodel's list columns. Recursion stops at `SUBVIEW_DEPTH` and at a model
 *  already on the chain (`res.partner` lines inside a partner): such a one2many keeps the
 *  name-only list, and the read specification stays finite. 
 */
export async function attachSubviews(description: Description, view: View, load: (model: string) => Promise<Description>,
  chain: ReadonlySet<string> = new Set([description.model]), byModel = new Map<string, Promise<Description>>(), depth = 0): Promise<void> {
  for (const field of view.fields) {
    if (field.kind !== "one2many" || field.relation === null) continue;
    const relation = field.relation;
    let comodel = byModel.get(relation);
    if (!comodel) { comodel = load(relation); byModel.set(relation, comodel); }
    let subview: Subview | undefined, error: string | undefined;
    try {
      const target = await comodel;
      const list = target.views.get("list"), form = target.views.get("form");
      if (!list && !form) throw new ClientError("unsupported", `${relation} has no list or form view.`);
      const inverse = (candidate: Field): boolean => candidate.name === field.relationField;
      subview = {
        model: relation,
        list: (list?.fields ?? form?.fields ?? []).filter(candidate => !inverse(candidate)),
        form: form?.mode === "record" ? form : undefined,
        fields: target.fields
      };
      if (subview.form && depth + 1 < SUBVIEW_DEPTH && !chain.has(relation))
        await attachSubviews(target, subview.form, load, new Set([...chain, relation]), byModel, depth + 1);
    } catch (failure) {
      error = failure instanceof Error ? failure.message : "The lines cannot be described.";
    }
    const occurrences: Field[] = [];
    const collect = (nodes: readonly Node[]): void => { for (const node of nodes) {
      if (node.kind === "field" && node.field.name === field.name) occurrences.push(node.field);
      else if ("children" in node) collect(node.children);
    } };
    collect(view.nodes);
    for (const target of [...occurrences, ...view.fields.filter(candidate => candidate.name === field.name)]) {
      if (!target) continue;
      if (subview) target.subview = subview;
      if (error !== undefined) target.subviewError = error;
    }
  }
}

function lineList(value: unknown): Line[] {
  return value === undefined || value === false ? [] : array(value, "one2many lines").map(line => object(line, "one2many line"));
}
function lineLabel(line: Line, subview: Subview | undefined): string {
  if (typeof line["display_name"] === "string" && line["display_name"]) return line["display_name"];
  const first = subview?.list.find(field => field.kind === "char");
  const name = first ? line[first.name] : undefined;
  return typeof name === "string" && name ? name : line["id"] === undefined ? "New line" : `#${String(line["id"])}`;
}

/** `one2many`: the lines as an embedded list. A row opens the line in a dialog; "Add a line"
 *  opens an empty one; the bin removes the line. Lines are written with the parent record. */
export function one2manyField(environment: WidgetEnvironment): WidgetHandle {
  const { field, value, change, run } = environment;
  const subview = field.subview, items = lineList(value);
  const editable = !field.readonly && subview?.form !== undefined;
  const columns: Field[] = subview?.list.filter(field => !field.invisible) ?? [];

  const wrapper = el("div", undefined, "x2many-field");
  wrapper.setAttribute("role", "group");
  const nolabel = field.options?.nolabel === true;
  if (!nolabel && field.label.trim()) {
    const caption = el("span", field.label + (field.required ? " *" : ""), "x2many-caption");
    caption.id = `x2many-${field.name}-${Math.random().toString(36).slice(2, 8)}`;
    wrapper.setAttribute("aria-labelledby", caption.id);
    wrapper.append(caption);
  } else {
    wrapper.setAttribute("aria-label", field.label + (field.required ? " *" : ""));
  }
  if (field.required) wrapper.classList.add("field-required");
  if (field.readonly) wrapper.classList.add("field-readonly");
  if (field.subviewError !== undefined) wrapper.append(el("p", `These lines cannot be shown: ${field.subviewError}`, "x2many-note"));

  // Every field of the line model, read-only: a cell widget may depend on a field that is not a column (a date range's
  // end, a currency), as the main list passes the whole view.
  const readonlyColumns = subview ? [...subview.fields.values()].map(candidate => ({ ...candidate, readonly: true })) : [];
  const lineCurrencies = new CurrencyDisplayCache(), cellStops: (() => void)[] = [];
  const scroller = el("div", undefined, "x2many-scroll");
  const table = el("table", undefined, "x2many-table");
  const head = el("thead"), headRow = el("tr");
  if (columns.length) for (const column of columns) headRow.append(el("th", column.label, column.kind === "integer" || column.kind === "float" || column.kind === "monetary" ? "numeric" : column.kind === "boolean" ? "x2many-boolean" : undefined));
  else headRow.append(el("th", "Name"));
  if (editable) {
    const th = el("th", undefined, "x2many-actions");
    th.scope = "col";
    th.setAttribute("aria-label", "Actions");
    th.append(el("span", "Actions", "visually-hidden"));
    headRow.append(th);
  }
  head.append(headRow);
  const body = el("tbody");
  const width = (columns.length || 1) + (editable ? 1 : 0);
  if (!items.length) {
    const row = el("tr", undefined, "x2many-empty"), cell = el("td", field.readonly ? "No lines." : "No lines yet.", "muted");
    cell.colSpan = width; row.append(cell); body.append(row);
  }
  items.forEach((line, index) => {
    const row = el("tr", undefined, "x2many-row");
    if (line["id"] === undefined || line["__changed"] !== undefined) row.classList.add("x2many-row-dirty");
    if (columns.length) for (const column of columns) {
      const numeric = column.kind === "integer" || column.kind === "float" || column.kind === "monetary";
      const cell = el("td", undefined, numeric ? "numeric" : column.kind === "boolean" ? "x2many-boolean" : undefined);
      if (column.widget === "password" || column.options?.password === true) {
        // Same masking as the main list and the form; reveal stays on those surfaces.
        const raw = line[column.name];
        cell.textContent = raw === false || raw === null || raw === undefined || raw === "" ? "" : "••••••••";
      } else {
        // The comodel's list widgets (color swatch, priority stars, badge, tags, percentage, favorite), read-only, the
        // same adapters the main list uses, so a line reads the same in both places.
        const handle = resolveFieldWidget(column.widget ?? column.kind, "list").render({
          field: { ...column, readonly: true }, value: line[column.name], values: line, fields: readonlyColumns, viewContext: "list",
          rpc: environment.rpc, context: environment.context, currencies: lineCurrencies, numericLocale: environment.numericLocale,
          editorLocale: environment.editorLocale, messages: environment.messages, run, report: environment.report, signal: environment.signal,
          ...(environment.openRecord ? { openRecord: environment.openRecord } : {}),
          change: () => { throw new ClientError("validation", "Line cells are edited in the line form."); },
        });
        cellStops.push(handle.dispose);
        cell.append(handle.node);
      }
      row.append(cell);
    } else row.append(el("td", lineLabel(line, subview)));
    if (subview?.form) {
      row.tabIndex = 0;
      row.classList.add("x2many-row-open");
      row.setAttribute("aria-label", `${editable ? "Edit" : "Open"} ${lineLabel(line, subview)}`);
      const open = (): void => run(() => openLine(environment, items, index));
      // A link or control inside a cell (a URL, a related record) keeps its own action.
      row.addEventListener("click", event => { if (!(event.target instanceof HTMLElement && event.target.closest(".x2many-remove,a[href],button:not([disabled]),input:not([disabled]),select,textarea"))) open(); });
      row.addEventListener("keydown", event => { if (event.key === "Enter" || event.key === " ") { event.preventDefault(); open(); } });
    }
    if (editable) {
      const cell = el("td", undefined, "x2many-actions");
      const remove = button("", () => change(items.filter((_, i) => i !== index)), "x2many-remove icon-button");
      remove.append(icon("trash"));
      remove.setAttribute("aria-label", `Remove ${lineLabel(line, subview)}`);
      cell.append(remove); row.append(cell);
    }
    body.append(row);
  });
  table.append(head, body);
  scroller.append(table);
  wrapper.append(scroller);
  if (editable) {
    const add = button("Add a line", () => run(() => openLine(environment, items, null)), "x2many-add link-button");
    wrapper.append(add);
  }
  return { node: wrapper, dispose: () => { for (const stop of cellStops.splice(0)) stop(); } };
}

/** Normalize a line default as the line form's widgets hold values. */
function lineDefault(field: Field, value: unknown): unknown {
  if (field.kind !== "many2many") return value;
  const items = value === false || value === undefined ? [] : array(value, "relation defaults");
  if (!items.some(Array.isArray)) return items.map(id => integer(id, "default relation id"));
  let ids: number[] = [];
  for (const raw of items) {
    const command = array(raw, "relation command");
    if (command[0] === 5) ids = [];
    else if (command[0] === 6) ids = array(command[2], "relation IDs").map(id => integer(id, "relation id"));
    else if (command[0] === 4) ids.push(integer(command[1], "relation id"));
    else throw new ClientError("unsupported", "Unsupported default for a line relation.");
  }
  return ids;
}

/**
 *  Save & New keeps the dialog open for the next line; every saved line reaches the parent
 *  record in one change when the dialog closes. 
 */
async function openLine(environment: WidgetEnvironment, items: readonly Line[], index: number | null): Promise<void> {
  const { field, rpc, context, change, run, report, signal } = environment;
  if (signal?.aborted) return;
  const subview = field.subview, form = subview?.form;
  if (!subview || !form) throw new ClientError("unsupported", `${field.label}: no line form is available.`);
  const readonly = field.readonly;
  const formFields = lineFields(subview).filter(candidate => candidate.name !== field.relationField);
  let working = [...items];
  let committed = false;
  let current = index;
  let values: Line = {};
  let changed = new Set<string>();

  // Every line's widgets get the field's context (default_*, lang, company keys); the domain is never evaluated here.
  let lineContext = context;

  const load = async (): Promise<void> => {
    changed = new Set();
    lineContext = relationQuery(field, environment.values ?? {}, context, environment.fields ?? [], "context").context;
    if (current !== null) { values = { ...working[current]! }; return; }
    const defaults = object(await rpc.call(subview.model, "default_get", { fields: formFields.map(candidate => candidate.name), context: lineContext }), "line defaults");
    values = {};
    for (const candidate of formFields) if (Object.hasOwn(defaults, candidate.name)) values[candidate.name] = lineDefault(candidate, defaults[candidate.name]);
  };
  await load();
  if (signal?.aborted) return;

  const prior = document.activeElement;
  const dialog = el("dialog", undefined, "choice-dialog message-dialog x2many-dialog");
  dialog.dataset["size"] = "lg";
  const heading = el("h2", undefined, "x2many-dialog-title");
  heading.id = `x2many-dialog-${field.name}`;
  dialog.setAttribute("aria-labelledby", heading.id);
  const body = el("form", undefined, "record-form x2many-form");
  body.noValidate = true;
  const actions = el("div", undefined, "actions");
  // The shared dialog anatomy (dialog-service.ts): header with the title and a close button, body, footer.
  const header = el("header", undefined, "message-dialog-header");
  const headerClose = button("×", () => close(), "message-dialog-close"); headerClose.setAttribute("aria-label", "Close");
  header.append(heading, headerClose);
  const bodyWrap = el("div", undefined, "message-dialog-body"); bodyWrap.append(body);
  const footer = el("footer", undefined, "message-dialog-footer"); footer.append(actions);
  dialog.append(header, bodyWrap, footer);
  let disposers: (() => void)[] = [];
  let widgetHandles: { name: string; represented: unknown; handle: WidgetHandle }[] = [];
  const disposeWidgets = (): void => { for (const stop of disposers) stop(); disposers = []; widgetHandles = []; };
  let closed = false;
  const abort = (): void => close(true);
  const close = (discard = false): void => {
    if (closed) return;
    signal?.removeEventListener("abort", abort);
    closed = true; disposeWidgets(); dialog.close(); dialog.remove();
    if (committed && !discard) change(working);
    else if (prior instanceof HTMLElement && prior.isConnected) prior.focus();
  };

  const missing = (): Field | undefined => linePolicy(subview, values).find(candidate => {
    if (!candidate.required || candidate.readonly || candidate.invisible || readonly || candidate.kind === "boolean") return false;
    const v = values[candidate.name];
    return v === undefined || v === null || v === false || v === "" || Array.isArray(v) && v.length === 0;
  });
  const commit = (): void => {
    const absent = missing();
    if (absent) { body.reportValidity(); throw new ClientError("validation", `${absent.label} is required.`); }
    if (!body.reportValidity()) throw new ClientError("validation", "Correct the invalid line fields before saving.");
    committed = true;
    if (current === null) working = [...working, { ...values, __key: newLineKey() }];
    else {
      const old = working[current]!;
      const before = old["__changed"] === undefined ? [] : array(old["__changed"], "changed line fields");
      const line: Line = { ...old, ...values };
      if (old["id"] !== undefined && changed.size) line["__changed"] = [...new Set([...before, ...changed])];
      working = working.map((candidate, i) => i === current ? line : candidate);
    }
  };

  const currencies = new CurrencyDisplayCache();
  const render = (): void => {
    if (closed || signal?.aborted) return;
    const focused = document.activeElement instanceof HTMLElement && dialog.contains(document.activeElement)
      ? (document.activeElement as HTMLInputElement).name || undefined : undefined;
    const drafts = new Map<string, WidgetDraft>(), managed = new Set<string>();
    for (const control of body.querySelectorAll<HTMLInputElement | HTMLTextAreaElement>("input,textarea")) if (control.name && (control.closest(".field-dirty,.field-invalid") || !control.validity.valid)) drafts.set(control.name, { value: control.value, message: control.validationMessage });
    for (const { name, represented, handle } of widgetHandles) if (Object.is(represented, values[name])) {
      const draft = handle.captureDraft?.(); if (draft) { drafts.set(name, draft); managed.add(name); }
    }
    const nodes = effectiveNodes(form.nodes, formFields, values);
    const policy = linePolicy(subview, values);
    disposeWidgets();
    body.replaceChildren();
    heading.textContent = current === null ? `Create ${field.label}` : `Open: ${field.label}`;
    const renderField = (node: Extract<Node, { kind: "field" }>, parent: HTMLElement): void => {
      if (node.field.name === field.relationField) return;
      const handle = resolveFieldWidget(node.widget, "form").render({
        field: { ...node.field, label: node.label, readonly: readonly || node.field.readonly || node.field.kind === "one2many" },
        value: values[node.field.name], values, fields: policy.map(candidate => ({ ...candidate, readonly: readonly || candidate.readonly })), numericLocale: environment.numericLocale, currencies,
        changeValues: changes => {
          for (const name of Object.keys(changes)) if (!policy.some(field => field.name === name && !field.readonly) || readonly) throw new ClientError("validation", "Readonly line fields cannot be changed.");
          values = { ...values, ...changes }; for (const name of Object.keys(changes)) changed.add(name); render();
        },
        rpc, context: lineContext, run, report, signal, messages: environment.messages, editorLocale: environment.editorLocale,
        change: next => { values = { ...values, [node.field.name]: next }; changed.add(node.field.name); render(); }
      });
      widgetHandles.push({ name: node.field.name, represented: values[node.field.name], handle });
      if (managed.has(node.field.name)) handle.restoreDraft?.(drafts.get(node.field.name)!);
      parent.append(handle.node);
      disposers.push(handle.dispose);
    };
    const layout = renderFormLayout(body, nodes, renderField, new Map());
    disposers.push(layout.dispose);
    for (const control of body.querySelectorAll<HTMLInputElement | HTMLTextAreaElement>("input,textarea")) if (!control.disabled && !managed.has(control.name) && drafts.has(control.name)) {
      const draft = drafts.get(control.name)!; control.value = draft.value; control.dispatchEvent(new Event("input"));
      if (draft.message && control.validity.valid) { control.setCustomValidity(draft.message); control.setAttribute("aria-invalid", "true"); const error = control.closest(".datetime-editor,.field-shell")?.querySelector<HTMLElement>(".editor-validation,.field-error"); if (error) { error.textContent = draft.message; error.hidden = false; } }
    }
    if (focused) body.querySelector<HTMLElement>(`[name="${CSS.escape(focused)}"]`)?.focus();
  };

  const act = (job: () => Promise<void> | void): void => {
    if (closed || signal?.aborted) return;
    try {
      const result = job();
      if (result instanceof Promise) result.catch(report);
    } catch (error) { report(error); }
  };
  const buttons = (): void => {
    actions.replaceChildren();
    if (readonly) { actions.append(button("Close", () => close(), "primary")); return; }
    // Destructive first (pushed left), the primary action last.
    if (current !== null) {
      actions.append(actionButton("Delete", () => { const at = current!; working = working.filter((_, i) => i !== at); committed = true; close(); }, "danger"));
    }
    actions.append(actionButton("Discard", () => close()));
    if (current === null) actions.append(button("Save & New", () => act(async () => { commit(); current = null; await load(); render(); body.querySelector<HTMLElement>("input,select,textarea")?.focus(); })));
    actions.append(button("Save & Close", () => act(() => { commit(); close(); }), "primary"));
  };
  render();
  buttons();
  dialog.addEventListener("cancel", event => { event.preventDefault(); close(); });
  body.addEventListener("submit", event => { event.preventDefault(); if (!readonly) act(() => { commit(); close(); }); });
  signal?.addEventListener("abort", abort, { once: true });
  document.body.append(dialog);
  dialog.showModal();
  body.querySelector<HTMLElement>("input:not([disabled]),select:not([disabled]),textarea:not([disabled])")?.focus();
}

registerFieldWidget("one2many", { kinds: ["one2many"], render: one2manyField });
