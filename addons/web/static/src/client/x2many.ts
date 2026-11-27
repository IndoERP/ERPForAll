/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
import { passwordResetRows } from "./password-editor.js";
import { t } from "./translate.js";
import { actionButton, button, el } from "./dom.js";
import { renderFormLayout } from "./form-layout.js";
import { icon } from "./icons.js";
import { array, integer, object, type Description, type Field, type Node, type Subview, type View } from "./metadata.js";
import { applyOnchangeUpdate, lineFields, lineOnchangeValues, linePolicy, newLineKey, parentOnchangeValues, parseOnchangeWarning, specification, touchNewLine, type Line, type OnchangeWarning } from "./record.js";
import { effectiveNodes } from "./field-modifiers.js";
import { CurrencyDisplayCache } from "./field-currency.js";
import { constantRelationContext, relationQuery } from "./field-relations.js";
import { resolveFieldWidget, registerFieldWidget, type WidgetEnvironment, type WidgetHandle, type WidgetDraft } from "./registry.js";
import { ClientError } from "./transport.js";
import { columnChoices } from "./column-choices.js";
import { resizeColumns } from "./columns.js";

/** Column widths of each sub-list, kept across the redraws a line change causes (one map per field's sub-view). */
const subListWidths = new WeakMap<Subview, Map<string, number>>();

/** How many line-form levels below the opened form get their one2many views attached. A deeper
 *  one2many keeps the name-only list. */
export const SUBVIEW_DEPTH = 3;

/** The views a one2many names for its lines in its constant context (`list_view_ref`, `form_view_ref`), merged into the
 *  context of the comodel's view request: two fields on one comodel (invoice lines and journal items) show different columns. */
export type ViewRefs = Readonly<Record<string, string>>;
function viewRefs(field: Field): ViewRefs | undefined {
  const context = constantRelationContext(field), refs: Record<string, string> = {};
  for (const key of ["list_view_ref", "form_view_ref"]) { const value = context?.[key]; if (typeof value === "string" && value) refs[key] = value; }
  return Object.keys(refs).length ? refs : undefined;
}

/**
 *  One request per comodel; a comodel
 *  that cannot be described leaves its fields with `subviewError`, which the widget shows.
 *  The comodel's own line form is attached the same way, so a line opened in a dialog shows its
 *  one2many with the comodel's list columns. Recursion stops at `SUBVIEW_DEPTH` and at a model
 *  already on the chain (`res.partner` lines inside a partner): such a one2many keeps the
 *  name-only list, and the read specification stays finite. 
 */
export async function attachSubviews(description: Description, view: View, load: (model: string, refs?: ViewRefs) => Promise<Description>,
  chain: ReadonlySet<string> = new Set([description.model]), byModel = new Map<string, Promise<Description>>(), depth = 0): Promise<void> {
  // Every comodel of one level (and each one's own lines) is requested together, not one after another.
  await Promise.all(view.fields.map(async field => {
    if (field.kind !== "one2many" || field.relation === null) return;
    const relation = field.relation, refs = viewRefs(field), key = refs ? `${relation} ${JSON.stringify(refs)}` : relation;
    let comodel = byModel.get(key);
    if (!comodel) { comodel = load(relation, refs); byModel.set(key, comodel); }
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
  }));
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
  if (environment.model === "change.password.wizard" && field.name === "user_ids" && field.relation === "change.password.user") {
    if (field.readonly || !subview?.form) throw new ClientError("validation", field.subviewError || "Password reset fields are unavailable.");
    return passwordResetRows(items, subview.form.fields);
  }
  const editable = !field.readonly && subview?.form !== undefined;
  const searchField = constantRelationContext(field)?.["inline_search_field"];

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
  let disposed = false;
  const scroller = el("div", undefined, "x2many-scroll");
  const scope = environment.columnScope && JSON.stringify([environment.columnScope, field.name, subview?.model, viewRefs(field)]);
  const choices = columnChoices(subview?.list ?? [], scope, environment.messages?.popovers, () => drawTable(), environment.report);
  function drawTable(): void {
    for (const stop of cellStops.splice(0)) stop();
    const columns = choices.visible();
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
    choices.attach(headRow.cells[headRow.cells.length - 1]);
    head.append(headRow);
    const body = el("tbody");
    const width = (columns.length || 1) + (editable ? 1 : 0);
    const searchColumn = columns.find(column => column.name === searchField && column.kind === "many2one" && !column.readonly);
    if (!items.length && !(editable && searchColumn)) {
      const row = el("tr", undefined, "x2many-empty"), cell = el("td", field.readonly ? t("No lines.") : t("No lines yet."), "muted");
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
        } else if (editable && searchColumn && subview && line["__key"] && (column.kind === "float" || column.kind === "integer") && (!column.widget || column.widget === column.kind) && !column.readonly) {
          const policy = linePolicy(subview, line);
          const writable = policy.find(candidate => candidate.name === column.name && !candidate.readonly && !candidate.invisible);
          const control = resolveFieldWidget(column.kind, "list").render({
            field: { ...column, readonly: !writable }, value: line[column.name], values: line,
            fields: policy, viewContext: "list", numericLocale: environment.numericLocale,
            rpc: environment.rpc, context: environment.context, run, report: environment.report,
            change: next => {
              if (!writable) throw new ClientError("validation", `${column.label} is read-only.`);
              if (disposed || !cell.isConnected || environment.signal?.aborted) return;
              run(async () => {
                const lineContext = relationQuery(field, environment.values ?? {}, environment.context, environment.fields ?? [], "context").context;
                const formFields = lineFields(subview).filter(candidate => candidate.name !== field.relationField);
                const draft: Line = { ...line, [column.name]: next };
                const touched = [column.name];
                touchNewLine(String(line["__key"]), touched);
                const outcome = await runLineOnchange(environment, subview, formFields, draft, [column.name], lineContext);
                if (disposed || !cell.isConnected || environment.signal?.aborted) return;
                if (outcome.warning) environment.messages?.warning(outcome.warning);
                if (outcome.changedKeys.length) touchNewLine(String(line["__key"]), outcome.changedKeys);
                change(items.map((item, i) => i === index ? outcome.values : item));
              });
            }
          });
          cell.classList.add("x2many-inline-number");
          cell.append(control.node); cellStops.push(control.dispose);
        } else {
          // The comodel's list widgets (color swatch, priority stars, badge, tags, percentage, favorite), read-only, the
          // same adapters the main list uses, so a line reads the same in both places.
          const handle = resolveFieldWidget(column.widget ?? column.kind, "list").render({
            field: { ...column, readonly: true }, value: line[column.name], values: line, ...(field.relation === null ? {} : { model: field.relation }), fields: readonlyColumns, viewContext: "list",
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
        row.addEventListener("keydown", event => { if (event.target === row && (event.key === "Enter" || event.key === " ")) { event.preventDefault(); open(); } });
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
    if (editable && subview) {
      const footer = table.createTFoot(), cell = footer.insertRow().insertCell();
      cell.colSpan = width; cell.className = "x2many-entry";
      const actions = el("div", undefined, "x2many-entry-actions");
      const detail = (): void => run(() => openLine(environment, items, null));
      const add = button(t("Add a line"), searchColumn ? () => run(startSearch) : detail, "x2many-add link-button");
      add.prepend(icon("plus"));
      actions.append(add);
      if (searchColumn) {
        const form = button("", detail, "x2many-detail-add");
        form.append(icon("panels-top-left"));
        form.title = "Open line form";
        form.setAttribute("aria-label", form.title);
        actions.append(form);
      }
      cell.append(actions);
      async function startSearch(): Promise<void> {
        if (!subview || !searchColumn || add.disabled) return;
        add.disabled = true;
        try {
          const lineContext = relationQuery(field, environment.values ?? {}, environment.context, environment.fields ?? [], "context").context;
          const values = await readLineDefaults(environment, subview, lineContext);
          if (!cell.isConnected || environment.signal?.aborted) return;
          const policy = linePolicy(subview, values);
          const permitted = policy.find(candidate => candidate.name === searchColumn.name && !candidate.readonly && !candidate.invisible);
          if (!permitted) { await openLine(environment, items, null); return; }
          const entry = el("div", undefined, "x2many-inline-search");
          let entryCancelled = false;
          const picker = resolveFieldWidget("many2one", "form").render({
            field: permitted, model: subview.model, fields: policy, values, value: values[permitted.name],
            context: lineContext, rpc: environment.rpc, run, report: environment.report, signal: environment.signal,
            messages: environment.messages,
            change: selected => {
              if (!selected || entryCancelled || disposed || !entry.isConnected) return;
              run(async () => {
                const formFields = lineFields(subview).filter(candidate => candidate.name !== field.relationField);
                const key = newLineKey();
                touchNewLine(key, [permitted.name]);
                const proposed: Line = { ...values, [permitted.name]: selected, __key: key };
                const outcome = await runLineOnchange(environment, subview, formFields, proposed, [permitted.name], lineContext);
                if (entryCancelled || disposed || !entry.isConnected || environment.signal?.aborted) return;
                if (outcome.warning) environment.messages?.warning(outcome.warning);
                if (outcome.changedKeys.length) touchNewLine(key, outcome.changedKeys);
                const updated = outcome.values;
                if (missingLineField(subview, updated)) {
                  reset(); await openLine(environment, items, null, updated); return;
                }
                reset(); change([...items, updated]);
              });
            }
          });
          const stop = (): void => { entryCancelled = true; picker.dispose(); };
          const reset = (): void => {
            entryCancelled = true;
            const index = cellStops.indexOf(stop);
            if (index >= 0) cellStops.splice(index, 1);
            stop(); entry.remove(); actions.hidden = false; add.focus();
          };
          cellStops.push(stop);
          const cancel = button("", reset, "x2many-entry-cancel");
          cancel.append(icon("x"));
          cancel.title = "Cancel line entry";
          cancel.setAttribute("aria-label", cancel.title);
          entry.append(picker.node, cancel);
          entry.addEventListener("keydown", event => {
            if (event.key === "Escape") { event.preventDefault(); event.stopPropagation(); reset(); }
            if (event.key === "Enter" && event.target instanceof HTMLInputElement) event.preventDefault();
          });
          actions.hidden = true; cell.append(entry); picker.node.querySelector("input")?.focus();
        } finally { add.disabled = false; }
      }
    }
    // The main list's width rules (kind, arch width, even fill, resize); measured once the form attaches the table.
    if (columns.length && subview) {
      const widths = subListWidths.get(subview) ?? new Map<string, number>(); subListWidths.set(subview, widths);
      for (const th of [...headRow.cells].slice(0, columns.length)) th.classList.add("x2many-column");
      resizeColumns(table, columns, widths, { selector: false, actions: editable });
    }
    scroller.replaceChildren(table);
  }
  drawTable();
  wrapper.append(scroller);
  return { node: wrapper, dispose: () => { disposed = true; choices.dispose(); for (const stop of cellStops.splice(0)) stop(); } };
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

async function readLineDefaults(environment: WidgetEnvironment, subview: Subview, context: Record<string, unknown>): Promise<Line> {
  const fields = lineFields(subview).filter(field => field.name !== environment.field.relationField);
  const defaults = object(await environment.rpc.call(subview.model, "default_get", { fields: fields.map(field => field.name), context }), "line defaults");
  return Object.fromEntries(fields.filter(field => Object.hasOwn(defaults, field.name)).map(field => [field.name, lineDefault(field, defaults[field.name])]));
}

/** Run `<line_model>/onchange` with virtual parent header values so line computes and `#[onchange]`
 *  methods update the line live before Save. */
async function runLineOnchange(
  environment: WidgetEnvironment,
  subview: Subview,
  formFields: readonly Field[],
  line: Line,
  fieldNames: readonly string[],
  lineContext: Record<string, unknown>,
): Promise<{ values: Line; changedKeys: string[]; warning: OnchangeWarning | undefined }> {
  const payload = lineOnchangeValues(formFields, line);
  const inverse = environment.field.relationField;
  if (inverse) {
    if (environment.fields?.length && environment.values) {
      payload[inverse] = parentOnchangeValues(environment.fields, environment.values);
    } else if (environment.values && typeof environment.values["id"] === "number" && Number.isSafeInteger(environment.values["id"]) && environment.values["id"] > 0) {
      payload[inverse] = environment.values["id"];
    }
  }
  const id = line["id"];
  const ids = typeof id === "number" && Number.isSafeInteger(id) && id > 0 ? [id] : [];
  const response = await environment.rpc.call(subview.model, "onchange", {
    ids,
    values: payload,
    field_names: [...fieldNames],
    fields_spec: specification(formFields),
    context: lineContext,
  }, { signal: environment.signal });
  const result = object(response, "onchange response");
  const update = object(result["value"], "onchange values");
  const applied = applyOnchangeUpdate(formFields, line, update, fieldNames.length === 0);
  const warning = parseOnchangeWarning(result["warning"]);
  return { values: applied.values, changedKeys: applied.keys, warning };
}

function missingLineField(subview: Subview, values: Line): Field | undefined {
  return linePolicy(subview, values).find(field => {
    if (!field.required || field.readonly || field.invisible || field.kind === "boolean") return false;
    const value = values[field.name];
    return value === undefined || value === null || value === false || value === "" || Array.isArray(value) && value.length === 0;
  });
}

const nativeLineScalars = new Set(["char", "text", "integer", "float", "monetary"]);

function lineDependency(value: unknown, name: string): boolean {
  if (value === null || typeof value !== "object") return false;
  return "name" in value && value.name === name || "nodes" in value && Array.isArray(value.nodes)
    && value.nodes.some(node => node?.kind === "name" && node.name === name);
}

/** Custom widgets and declarations that read a record value still get a fresh line presentation. */
function presentationReads(nodes: readonly Node[], name: string): boolean {
  return nodes.some(node => {
    if (node.kind === "field") {
      const { field, widget } = node;
      return widget !== field.kind && widget !== "many2many_tags"
        || Object.values(field.modifiers ?? {}).some(expression => lineDependency(expression, name))
        || Object.values(field.options ?? {}).some(option => lineDependency(option, name));
    }
    if (node.kind === "widget") return true;
    if ((node.kind === "page" || node.kind === "ribbon") && lineDependency(node.invisible, name)) return true;
    return "children" in node && presentationReads(node.children, name);
  });
}

/**
 *  Save & New keeps the dialog open for the next line; every saved line reaches the parent
 *  record in one change when the dialog closes. 
 */
async function openLine(environment: WidgetEnvironment, items: readonly Line[], index: number | null, initial?: Line): Promise<void> {
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
  let lineEpoch = 0;
  let pendingChange: Promise<void> | undefined;

  const load = async (): Promise<void> => {
    lineEpoch += 1;
    pendingChange = undefined;
    changed = new Set();
    lineContext = relationQuery(field, environment.values ?? {}, context, environment.fields ?? [], "context").context;
    if (current !== null) { values = { ...working[current]! }; return; }
    if (initial !== undefined) {
      const key = typeof initial["__key"] === "string" ? initial["__key"] : newLineKey();
      values = { ...initial, __key: key };
      initial = undefined;
      return;
    }
    const defaults = await readLineDefaults(environment, subview, lineContext);
    if (signal?.aborted) return;
    const key = newLineKey();
    const outcome = await runLineOnchange(environment, subview, formFields, { ...defaults, __key: key }, [], lineContext);
    if (signal?.aborted) return;
    values = { ...outcome.values, __key: key };
    if (outcome.changedKeys.length) touchNewLine(key, outcome.changedKeys);
    if (outcome.warning) environment.messages?.warning(outcome.warning);
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
    lineEpoch += 1;
    pendingChange = undefined;
    signal?.removeEventListener("abort", abort);
    closed = true; disposeWidgets(); dialog.close(); dialog.remove();
    if (committed && !discard) change(working);
    else if (prior instanceof HTMLElement && prior.isConnected) prior.focus();
  };

  const applyLineChange = (names: readonly string[], patch: Readonly<Record<string, unknown>>, scalar?: string): void => {
    values = { ...values, ...patch };
    for (const name of names) changed.add(name);
    if (typeof values["__key"] === "string") touchNewLine(values["__key"], names);
    const source = scalar === undefined ? [] : widgetHandles.filter(item => item.name === scalar);
    if (scalar !== undefined && source.length === 1 && !presentationReads(form.nodes, scalar)) {
      // A plain input already displays its edit. Replacing the line on blur can detach
      // the next field before that pointer or Tab interaction has reached it.
      source[0]!.represented = values[scalar];
      source[0]!.handle.node.classList.remove("field-dirty");
    } else render();
    const epoch = lineEpoch;
    const priorWork = pendingChange;
    const work = (async () => {
      if (priorWork) { try { await priorWork; } catch {} }
      if (closed || signal?.aborted || epoch !== lineEpoch) return;
      const sent: Line = { ...values };
      const outcome = await runLineOnchange(environment, subview, formFields, sent, names, lineContext);
      if (closed || signal?.aborted || epoch !== lineEpoch) return;
      // Merge server-derived keys without overwriting fields edited locally while this request was in flight.
      const merged: Line = { ...values };
      const appliedKeys: string[] = [];
      for (const key of outcome.changedKeys) {
        if (Object.is(values[key], sent[key]) && JSON.stringify(values[key]) !== JSON.stringify(outcome.values[key])) {
          merged[key] = outcome.values[key];
          appliedKeys.push(key);
        }
      }
      values = merged;
      for (const key of appliedKeys) changed.add(key);
      if (typeof values["__key"] === "string" && appliedKeys.length) touchNewLine(values["__key"], appliedKeys);
      if (outcome.warning) environment.messages?.warning(outcome.warning);
      if (appliedKeys.length) render();
    })();
    const tracked = work.finally(() => { if (pendingChange === tracked) pendingChange = undefined; });
    void tracked.catch(() => {});
    pendingChange = tracked;
    run(() => work);
  };

  const flushLineChange = async (): Promise<void> => {
    if (pendingChange) await pendingChange;
  };

  let submitting = false;
  const commit = (): void => {
    const absent = missingLineField(subview, values);
    if (absent) { body.reportValidity(); throw new ClientError("validation", `${absent.label} is required.`); }
    if (!body.reportValidity()) throw new ClientError("validation", "Correct the invalid line fields before saving.");
    committed = true;
    if (current === null) {
      const key = typeof values["__key"] === "string" ? values["__key"] : newLineKey();
      touchNewLine(key, changed);
      working = [...working, { ...values, __key: key }];
    } else {
      const old = working[current]!;
      if (old["id"] === undefined && typeof old["__key"] === "string") touchNewLine(old["__key"], changed);
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
    const represented = new Map(widgetHandles.map(item => [item.name, item.represented]));
    for (const control of body.querySelectorAll<HTMLInputElement | HTMLTextAreaElement>("input,textarea")) {
      if (control.name && Object.is(represented.get(control.name), values[control.name])
          && (control.closest(".field-dirty,.field-invalid") || !control.validity.valid)) {
        drafts.set(control.name, { value: control.value, message: control.validationMessage });
      }
    }
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
        columnScope: environment.columnScope && JSON.stringify([environment.columnScope, field.name, subview.model, form.id]),
        value: values[node.field.name], values, ...(field.relation === null ? {} : { model: field.relation }), fields: policy.map(candidate => ({ ...candidate, readonly: readonly || candidate.readonly })), numericLocale: environment.numericLocale, currencies,
        changeValues: changes => {
          for (const name of Object.keys(changes)) if (!policy.some(field => field.name === name && !field.readonly) || readonly) throw new ClientError("validation", "Readonly line fields cannot be changed.");
          applyLineChange(Object.keys(changes), changes);
        },
        rpc, context: lineContext, run, report, signal, messages: environment.messages, editorLocale: environment.editorLocale,
        ...(environment.views ? { views: environment.views } : {}),
        ...(environment.openRecord ? { openRecord: async (model: string, id: number) => {
          if (closed || signal?.aborted || submitting) return;
          submitting = true;
          try {
            // The line's own edits reach the parent form first, then the dialog closes: leaving the form asks about them.
            await flushLineChange();
            if (closed || signal?.aborted) return;
            if (changed.size && !readonly) commit();
            close();
            await environment.openRecord!(model, id);
          } finally { submitting = false; }
        } } : {}),
        change: next => {
          const scalar = node.widget === node.field.kind && nativeLineScalars.has(node.widget) ? node.field.name : undefined;
          applyLineChange([node.field.name], { [node.field.name]: next }, scalar);
        }
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
  const submitOnce = (job: () => Promise<void>): void => {
    act(async () => {
      if (submitting || closed || signal?.aborted) return;
      submitting = true;
      try { await job(); } finally { submitting = false; }
    });
  };
  const buttons = (): void => {
    actions.replaceChildren();
    if (readonly) { actions.append(button("Close", () => close(), "primary")); return; }
    // Destructive first (pushed left), the primary action last.
    if (current !== null) {
      actions.append(actionButton("Delete", () => { const at = current!; working = working.filter((_, i) => i !== at); committed = true; close(); }, "danger"));
    }
    actions.append(actionButton("Discard", () => close()));
    if (current === null) actions.append(button("Save & New", () => submitOnce(async () => { await flushLineChange(); if (closed || signal?.aborted) return; commit(); current = null; await load(); if (closed || signal?.aborted) return; render(); body.querySelector<HTMLElement>("input,select,textarea")?.focus(); })));
    actions.append(button("Save & Close", () => submitOnce(async () => { await flushLineChange(); if (closed || signal?.aborted) return; commit(); close(); }), "primary"));
  };
  render();
  buttons();
  dialog.addEventListener("cancel", event => { event.preventDefault(); close(); });
  body.addEventListener("submit", event => { event.preventDefault(); if (!readonly) submitOnce(async () => { await flushLineChange(); if (closed || signal?.aborted) return; commit(); close(); }); });
  signal?.addEventListener("abort", abort, { once: true });
  document.body.append(dialog);
  dialog.showModal();
  body.querySelector<HTMLElement>("input:not([disabled]),select:not([disabled]),textarea:not([disabled])")?.focus();
}

registerFieldWidget("one2many", { kinds: ["one2many"], render: one2manyField });
