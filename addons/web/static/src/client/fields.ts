/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
import { array, integer, object, text, type Field } from "./metadata.js";
import { button, el, labelled } from "./dom.js";
import { icon } from "./icons.js";
import { registerFieldWidget, type WidgetHandle } from "./registry.js";
import { ClientError, RpcClient } from "./transport.js";
import { absentPlaceholderInput } from "./field-options.js";
import { createDateFieldControl } from "./field-dates.js";
import type { EditorLocale } from "./datetime-values.js";
import { renderCodeField } from "./code-field.js";
import { renderHtmlField, renderListHtmlField } from "./html-field.js";
import type { PopoverService } from "./popover.js";
import { createAutocomplete, type AutocompleteOption } from "./autocomplete.js";
import type { EditorEnvironment } from "./editor-types.js";
// Web-owned named widgets register with the builtins, so every form that loads fields sees them.
import "./menu-icon-picker.js";
import "./preference-choice.js";
import { monetaryControl, type CurrencyDisplayCache } from "./field-currency.js";
import { relationQuery } from "./field-relations.js";
import { fieldShell } from "./field-shell.js";
import { scalarControl } from "./field-scalars.js";
import { radioControl } from "./field-radio.js";
import { priorityControl } from "./priority.js";
import { colorControl, colorPickerControl } from "./color.js";
import { copyClipboardControl } from "./copy-clipboard.js";
import type { NumericLocale } from "./numeric-values.js";
import { withRelationPicker } from "./field-picker.js";
import "./x2many.js";
import "./statusbar.js";

export function displayValue(field: Field, value: unknown, locale: string): string {
  if (value === null || value === undefined || value === false && field.kind !== "boolean") return "";
  if (field.kind === "boolean") return value ? "Yes" : "No";
  if (field.selection) return field.selection.find(([key]) => key === value)?.[1] ?? String(value);
  if (field.kind === "one2many") {
    const count = array(value, "lines").length;
    return count === 1 ? "1 record" : `${new Intl.NumberFormat(locale).format(count)} records`;
  }
  if (field.relation) {
    const name = (item: unknown): string => typeof item === "number" ? `#${item}` : text(object(item)["display_name"], "relation name");
    return field.kind === "many2many" ? array(value, "relations").map(name).join(", ") : name(value);
  }
  if (typeof value === "number") return new Intl.NumberFormat(locale).format(value);
  if (typeof value === "string") return value;
  throw new Error(`Unsupported value for ${field.name}.`);
}

export interface FieldControl extends WidgetHandle {}
/** Explicit display and overlay ownership for the direct date/time control adapter. */
export interface FieldEditorEnvironment {
  locale: EditorLocale; popovers: PopoverService; signal?: AbortSignal;
  report(error: unknown): void;
}
export interface FieldControlOptions {
  viewContext?: "form" | "list" | "card";
  field: Field; value: unknown; rpc: RpcClient; context: Record<string, unknown>;
  change(value: unknown): void;
  changeValues?(changes: Readonly<Record<string, unknown>>): void;
  fields?: readonly Field[];
  run(job: () => Promise<void>): void;
  openRecord?: (model: string, id: number) => Promise<void>;
  /** Date/time needs its explicit locale; editable relational controls need message services. */
  editor?: EditorEnvironment | FieldEditorEnvironment;
  numericLocale?: NumericLocale;
  currencies?: CurrencyDisplayCache;
  values?: Readonly<Record<string, unknown>>;
}

/** Node-only date/time callers bind cleanup to their owner signal; use createFieldControl
 * when the caller instead retains an explicit disposer. */
export function fieldControl(options: FieldControlOptions): HTMLElement {
  const { field, editor } = options;
  if (field.kind === "date" || field.kind === "datetime") {
    if (!editor || !("locale" in editor)) throw new ClientError("unsupported", "Date/time fields require an editor locale and the shared popover service.");
    if (!(editor.signal instanceof AbortSignal)) throw new ClientError("validation", "The node-only date/time field helper requires an owner AbortSignal; use createFieldControl for explicit disposal.");
  }
  return createFieldControl(options).node;
}

/** Builds a control and returns the cleanup for any listener it attached outside its own subtree. */
export function createFieldControl(options: FieldControlOptions): FieldControl {
  const { field, value } = options;
  if (field.widget === "monetary" || field.kind === "monetary" && field.widget !== "float") return monetaryControl(options);
  if (field.kind === "date" || field.kind === "datetime") return createDateFieldControl(options);
  if (field.widget === "badge" && field.kind === "many2one") return scalarControl({ ...options, field: { ...field, kind: "char", readonly: true }, value: displayValue(field, value, "en-US") });
  if (field.widget === "radio") return radioControl(options);
  if (field.widget === "priority") return priorityControl(options);
  if (field.widget === "color") return colorControl(options);
  if (field.widget === "color_picker") return colorPickerControl(options);
  if (field.widget === "CopyClipboardChar" || field.widget === "CopyClipboardText" || field.widget === "CopyClipboardURL") return copyClipboardControl(options);
  if (field.kind === "many2one") return many2oneControl(options);
  if (field.kind === "many2many") return many2manyControl(options);
  return scalarControl(options);
}

function markRequired(wrapper: HTMLElement, label: string): void {
  wrapper.classList.add("field-required");
  const text = wrapper.querySelector(":scope > span > label") ?? wrapper.querySelector(":scope > span");
  if (!text) throw new ClientError("server", "A required field is missing its label.");
  const marker = el("span", "*", "field-required-marker"); marker.setAttribute("aria-hidden", "true");
  text.replaceChildren(label, " ", marker);
}

/** Relational adapters retain their existing seven visible records and quick-create protocol. */
const SEARCH_LIMIT = 7;
let relationInputIdentity = 0;
interface RelationAutocompleteOptions {
  field: Field; input: HTMLInputElement; host: HTMLElement; rpc: RpcClient;
  context: Record<string, unknown>; run(job: () => Promise<void>): void;
  exclude(): ReadonlySet<number>; pick(item: { id: number; display_name: string; [key: string]: unknown }): void;
  representedValue(): string;
  editor: EditorEnvironment | undefined;
  values?: Readonly<Record<string, unknown>>;
  fields?: readonly Field[];
}
function relationAutocomplete(options: RelationAutocompleteOptions): ReturnType<typeof createAutocomplete> {
  const { field, input, host, rpc, context, run, exclude, pick, editor, representedValue } = options;
  if (!editor) throw new ClientError("unsupported", "Editable relational fields require message services.");
  let disposed = false;
  const effective = relationQuery(field, options.values ?? {}, context, options.fields);
  // The bound is the server's integer option admission (0..=4096).
  const threshold = field.options?.search_threshold ?? 0, limit = SEARCH_LIMIT;
  if (!Number.isSafeInteger(threshold) || threshold < 0 || threshold > 4096) throw new ClientError("server", "Invalid relation search threshold.");
  // The picker button is appended by withRelationPicker inside the many2one buttons; the typed text travels in
  // data-query, which its click handler reads once.
  const searchMoreOption = (query: string): AutocompleteOption | undefined => {
    const button = field.kind === "many2one" ? host.querySelector<HTMLButtonElement>(".relation-search-more") : null;
    return button ? { kind: "option", label: "Search more…", select: () => { button.dataset["query"] = query; button.click(); delete button.dataset["query"]; } } : undefined;
  };
  const complete = createAutocomplete({ input, owner: host, alignTo: host, environment: editor, debounceMs: 150, autoSelect: true, focusFirst: false, representedValue,
    sources: [{ options: (query, signal) => new Promise<readonly AutocompleteOption[]>((resolve, reject) => {
      const typed = [...query].length;
      if (typed < threshold) { const more = searchMoreOption(query); resolve([{ kind: "status", label: `Start typing ${threshold - typed} more character${threshold - typed === 1 ? "" : "s"}…` }, ...(more ? [more] : [])]); return; }
      run(async () => {
        try {
          const skip = exclude();
          const rows = array(await rpc.call(field.relation!, "web_name_search", {
            name: query, domain: effective.domain, specification: { display_name: {}, ...(field.options?.color_field ? { [field.options.color_field.name]: {} } : {}) }, limit: limit + 1 + skip.size, context: effective.context
          }, { signal }), "relation search");
          if (disposed || signal.aborted) { resolve([]); return; }
          const items: AutocompleteOption[] = rows.map(row => {
            const record = object(row);
            return { id: integer(record["id"], "relation id"), display_name: text(record["display_name"], "relation name"), ...(field.options?.color_field ? { [field.options.color_field.name]: record[field.options.color_field.name] } : {}) };
          }).filter(item => !skip.has(item.id)).slice(0, limit).map(item => ({ kind: "option", label: item.display_name, select: () => pick(item) }));
          if (items.length > 0) { const more = searchMoreOption(query); if (more) items.push(more); }
          if (query !== "" && !field.options?.no_create && !field.options?.no_quick_create && field.options?.create !== false) items.push({ kind: "option", label: `Create "${query}"`, select: () => {
            run(async () => {
              const created = array(await rpc.call(field.relation!, "name_create", { name: query, context: effective.context }, { write: true }), "created record");
              if (created.length !== 2) throw new ClientError("server", "Invalid quick-create response.");
              // name_create returns only (id, name): a new tag has no colour until it is read back.
              const choice = { id: integer(created[0], "created record id"), display_name: text(created[1], "created record name"), ...(field.options?.color_field ? { [field.options.color_field.name]: false } : {}) };
              if (!disposed && !complete.isDisposed() && !editor.signal?.aborted && host.isConnected) pick(choice);
            });
          } });
          resolve(items);
        } catch (error) { reject(error); }
      });
    }) }] });
  return { ...complete, dispose: () => { if (disposed) return; disposed = true; complete.dispose(); } };
}

function combobox({ field, className, placeholder, values, fields }: Pick<FieldControlOptions, "field" | "values" | "fields"> & { className: string; placeholder: string }): HTMLInputElement {
  const input = el("input", undefined, className);
  input.type = "text";
  const dependency = field.options?.placeholder_field;
  const supplied = dependency ? values?.[dependency.name] : undefined;
  if (dependency && supplied !== false && supplied !== null && !absentPlaceholderInput({ name: dependency.name, values, fields }) && typeof supplied !== "string") throw new ClientError("server", "Invalid relation placeholder field value.");
  input.placeholder = dependency ? typeof supplied === "string" ? supplied : "" : field.options?.placeholder ?? placeholder;
  input.name = field.name;
  input.autocomplete = "off";
  input.setAttribute("role", "combobox");
  input.setAttribute("aria-autocomplete", "list");
  input.setAttribute("aria-expanded", "false");
  input.setAttribute("aria-label", `Search ${field.label}`);
  return input;
}

function many2oneControl({ field, value, rpc, context, change: changed, run, openRecord, editor: owner, values, fields: authorizedFields, viewContext }: FieldControlOptions): FieldControl {
  const editor = owner && "messages" in owner ? owner : undefined;
  let currentId: number | null = null;
  let currentName = "";

  if (typeof value === "number" && value > 0) {
    currentId = value;
    currentName = `#${value}`;
  } else if (value && typeof value === "object") {
    const obj = object(value);
    currentId = integer(obj["id"], "relation id");
    currentName = text(obj["display_name"], "relation name");
  }

  if (field.readonly) {
    const area = el(viewContext === "list" ? "span" : "div", undefined, "many2one-readonly");
    if (currentId !== null) {
      if (openRecord && !field.options?.no_open) {
        const id = currentId;
        const open = button(currentName, () => run(() => openRecord(field.relation!, id)), "many2one-open-btn link-button");
        open.setAttribute("aria-label", `Open ${field.label}: ${currentName}`);
        area.append(open);
      } else area.append(el("span", currentName, "many2one-value"));
    } else {
      area.append(el("span", "—", "muted"));
    }
    if (viewContext === "list") return { node: fieldShell(field, area, viewContext).node, dispose: () => {} };
    const wrapper = labelled(field.label + (field.required ? " *" : ""), area);
    wrapper.classList.add("field-readonly");
    if (field.required) markRequired(wrapper, field.label);
    return { node: wrapper, dispose: () => {} };
  }

  const area = el("div", undefined, "many2one-control");
  const input = combobox({ field, className: "many2one-input", placeholder: `Search ${field.label}…`, values, fields: authorizedFields });
  input.id = `relation-input-${++relationInputIdentity}`;
  input.required = field.required;
  input.value = currentName;

  const buttonsDiv = el("div", undefined, "many2one-buttons");
  const clearBtn = button("", () => {
    currentId = null;
    currentName = "";
    input.value = "";
    clearBtn.style.display = "none";
    if (openBtn) openBtn.style.display = "none";
    complete.close();
    changed(false);
    input.focus();
  }, "many2one-clear-btn");
  clearBtn.append(icon("x"));
  clearBtn.setAttribute("aria-label", "Clear selection");
  clearBtn.style.display = currentId !== null ? "inline-flex" : "none";

  const openBtn = openRecord && !field.options?.no_open ? button("", () => run(async () => {
    const id = currentId;
    if (id === null) throw new ClientError("validation", "Choose a related record first.");
    await openRecord(field.relation!, id);
  }), "many2one-open-btn") : undefined;
  if (openBtn) {
    const openIcon = icon("arrow-left"); openIcon.style.transform = "rotate(135deg)"; openIcon.style.transformOrigin = "center";
    openBtn.append(openIcon);
    openBtn.setAttribute("aria-label", `Open ${field.label}`);
    openBtn.style.display = currentId === null ? "none" : "inline-flex";
    buttonsDiv.append(openBtn);
  }
  buttonsDiv.append(clearBtn);

  area.append(input);
  const complete = relationAutocomplete({ field, input, host: area, rpc, context, run, exclude: () => new Set(), representedValue: () => currentName, editor, values, fields: authorizedFields, pick: item => {
    currentId = item.id;
    currentName = item.display_name;
    input.value = item.display_name;
    clearBtn.style.display = "inline-flex";
    if (openBtn) openBtn.style.display = "inline-flex";
    changed(item);
  } });
  // Focus
  // must not search: a re-render after a pick refocuses the input and would reopen the list.
  input.addEventListener("focus", () => input.select());
  input.addEventListener("click", () => complete.toggle(input.value === currentName ? "" : input.value.trim()));

  area.append(input, buttonsDiv);
  // The input moves into a touch sheet. A wrapping label would then retarget the
  // in-progress native activation to Clear; an explicit association keeps its identity.
  const wrapper = el("div", undefined, "field-label"); wrapper.setAttribute("role", "group");
  const caption = el("span"); caption.id = `${input.id}-label`;
  const label = el("label", field.label); label.htmlFor = input.id; caption.append(label);
  wrapper.setAttribute("aria-labelledby", caption.id); wrapper.append(caption, area);
  if (field.required) markRequired(wrapper, field.label);
  return { node: wrapper, dispose: complete.dispose };
}

function many2manyControl({ field, value, rpc, context, change: changed, run, editor: owner, values: recordValues, openRecord, fields: authorizedFields, viewContext }: FieldControlOptions): FieldControl {
  if (field.widget === "many2many_tags_avatar") throw new ClientError("unsupported", "Avatar tags are not supported.");
  const editor = owner && "messages" in owner ? owner : undefined;
  const values = value ? array(value, "relations") : [];
  const idOf = (item: unknown): number => typeof item === "number" ? integer(item, "relation id") : integer(object(item)["id"], "relation id");
  const nameOf = (item: unknown): string => typeof item === "number" ? `#${item}` : text(object(item)["display_name"], "relation name");
  const area = el(viewContext === "list" ? "span" : "div", undefined, "many2many-tags");
  const tags = el(viewContext === "list" ? "span" : "div", undefined, "tag-list");
  const limit = field.options?.tag_limit ?? values.length;
  let expanded = false;
  for (const [index, item] of values.entries()) {
    const tag = el("span", undefined, "relation-chip");
    if (field.options?.color_field) {
      if (typeof item !== "object" || item === null) throw new ClientError("server", "Missing tag color data.");
      const color = object(item)[field.options.color_field.name];
      if (color !== false && color !== null) {
        if (field.widget === "many2many_tags_color_dot") {
          if (typeof color !== "string" || !/^#(?:[0-9a-f]{3}|[0-9a-f]{4}|[0-9a-f]{6}|[0-9a-f]{8})$/i.test(color)) throw new ClientError("server", "Invalid tag color.");
          const dot = el("span", undefined, "relation-tag-dot"); dot.dataset["color"] = color; dot.style.backgroundColor = color; dot.setAttribute("aria-hidden", "true"); tag.append(dot);
        } else { if (typeof color !== "number" || !Number.isSafeInteger(color) || color < 0 || color > 11) throw new ClientError("server", "Invalid tag color."); tag.dataset["tagColor"] = String(color); }
      }
    }
    tag.append(el("span", nameOf(item), "relation-chip-name"));
    if (field.options?.can_open && openRecord) { const name = tag.querySelector(".relation-chip-name")!; const open = button(nameOf(item), () => run(() => openRecord(field.relation!, idOf(item))), "link-button"); name.replaceWith(open); }

    if (!field.readonly) {
      const remove = button("×", () => changed(values.filter(candidate => candidate !== item)), "relation-chip-remove");
      remove.setAttribute("aria-label", `Remove ${nameOf(item)}`);
      tag.append(remove);
    }
    if (index >= limit) tag.hidden = true;
    tags.append(tag);
  }
  if (values.length > limit) {
    const more = button(`Show ${values.length - limit} more`, () => { expanded = !expanded; for (const [index, tag] of [...tags.querySelectorAll<HTMLElement>(".relation-chip")].entries()) tag.hidden = !expanded && index >= limit; more.textContent = expanded ? "Show fewer" : `Show ${values.length - limit} more`; more.setAttribute("aria-expanded", String(expanded)); });
    more.setAttribute("aria-expanded", "false"); area.append(more);
  }
  area.append(tags);
  let dispose = (): void => {};
  if (field.readonly) {
    if (!values.length) tags.append(el("span", "—", "muted"));
  } else {
    const input = combobox({ field, className: "many2many-input", placeholder: values.length ? "" : `Add ${field.label}…`, values: recordValues, fields: authorizedFields });
    input.setAttribute("aria-required", String(field.required));
    input.setCustomValidity(field.required && values.length === 0 ? `${field.label} is required.` : "");
    tags.append(input);
    const complete = relationAutocomplete({ field, input, host: area, rpc, context, run, exclude: () => new Set(values.map(idOf)), representedValue: () => "", editor, values: recordValues, fields: authorizedFields, pick: item => {
      changed([...values.filter(old => idOf(old) !== item.id), item]);
    } });
    input.addEventListener("keydown", event => {
      if (event.key === "Backspace" && input.value === "" && values.length) {
        event.preventDefault();
        changed(values.slice(0, -1));
      }
    });
    input.addEventListener("click", () => complete.toggle(input.value.trim()));
    area.addEventListener("click", event => {
      if (event.target === area || event.target === tags) { input.focus(); complete.toggle(input.value.trim()); }
    });
    tags.append(input);

    dispose = complete.dispose;
  }
  if (viewContext === "list") return { node: fieldShell(field, area, viewContext).node, dispose };
  // A group, not a <label>: a click inside a label is forwarded to its first control, which here
  // is a tag's remove button.
  const wrapper = el("div", undefined, "field-label");
  const caption = el("span", field.label + (field.required ? " *" : ""));
  caption.id = `${field.name}-tags-${Math.random().toString(36).slice(2, 8)}`;
  wrapper.setAttribute("role", "group");
  wrapper.setAttribute("aria-labelledby", caption.id);
  wrapper.append(caption, area);
  if (field.required) markRequired(wrapper, field.label);
  if (field.readonly) wrapper.classList.add("field-readonly");
  return { node: wrapper, dispose };
}

// Builtin controls use the same registration contract as trusted addon widgets.
for (const kind of ["char", "text", "boolean", "integer", "float", "monetary", "date", "datetime", "selection", "many2one", "many2many"] as const) {
  registerFieldWidget(kind, { kinds: kind === "char" ? ["char", "text"] : kind === "float" ? ["float", "monetary"] : kind === "monetary" ? ["integer", "float", "monetary"] : [kind], render: environment =>
    withRelationPicker(createFieldControl({
      viewContext: environment.viewContext, field: { ...environment.field, widget: environment.field.widget ?? kind }, value: environment.value, numericLocale: environment.numericLocale, currencies: environment.currencies, values: environment.values, fields: environment.fields, changeValues: environment.changeValues, rpc: environment.rpc, context: environment.context,
      change: environment.change, run: environment.run,
      ...(environment.openRecord ? { openRecord: environment.openRecord } : {}),
      ...(environment.messages ? { editor: environment.field.kind === "date" || environment.field.kind === "datetime"
        ? environment.editorLocale ? {
          locale: environment.editorLocale, popovers: environment.messages.popovers, signal: environment.signal, report: environment.report,
        } : undefined
        : { messages: environment.messages, report: environment.report, ...(environment.signal ? { signal: environment.signal } : {}) },
      } : {}),
    }), environment) });
}

for (const [widget, kinds] of Object.entries({ password: ["char", "text"], email: ["char"], phone: ["char"], url: ["char"], percentage: ["integer", "float"], boolean_checkbox: ["boolean"], boolean_toggle: ["boolean"], boolean_favorite: ["boolean"], boolean_icon: ["boolean"], badge: ["selection", "char", "many2one"], label_selection: ["selection"], badges_selection: ["selection"], filterable_selection: ["selection"], radio: ["selection", "many2one"], priority: ["selection"], color: ["char"], color_picker: ["integer"], CopyClipboardChar: ["char"], CopyClipboardText: ["text"], CopyClipboardURL: ["char"], many2many_tags: ["many2many"], many2many_tags_avatar: ["many2many"], many2many_tags_color_dot: ["many2many"] } as const)) {
  registerFieldWidget(widget, { kinds, render: environment => createFieldControl({ viewContext: environment.viewContext, field: { ...environment.field, widget }, value: environment.value, rpc: environment.rpc, context: environment.context, numericLocale: environment.numericLocale, currencies: environment.currencies, values: environment.values, fields: environment.fields, changeValues: environment.changeValues, change: environment.change, run: environment.run, openRecord: environment.openRecord, ...(environment.messages ? { editor: { messages: environment.messages, report: environment.report, signal: environment.signal } } : {}) }) });
}

registerFieldWidget("list.one2many", { kinds: ["one2many"], render: environment => {
  const node = el("span", displayValue(environment.field, environment.value, environment.editorLocale?.lang.replaceAll("_", "-") ?? "en-US"), "field-read-value");
  node.setAttribute("aria-label", environment.field.label); return { node, dispose: () => {} };
} });

registerFieldWidget("daterange", { kinds: ["date", "datetime"], render: environment => createFieldControl({
  viewContext: environment.viewContext, field: { ...environment.field, widget: "daterange" }, value: environment.value, values: environment.values, fields: environment.fields,
  change: environment.change, changeValues: environment.changeValues, rpc: environment.rpc, context: environment.context, run: environment.run,
  ...(environment.messages && environment.editorLocale ? { editor: { locale: environment.editorLocale, popovers: environment.messages.popovers, signal: environment.signal, report: environment.report } } : {}),
}) });

// Additive registration: code editing is available only for text fields.
registerFieldWidget("code", { kinds: ["text"], render: renderCodeField });
// List cells have no RecordForm draft lifecycle: they show the source as read-only text (lead r1 delta MUST-1).
registerFieldWidget("list.code", { kinds: ["text"], render: environment => {
  const value = environment.value;
  if (value !== false && value !== null && value !== undefined && typeof value !== "string") throw new ClientError("server", `Code field ${environment.field.label} must contain text.`);
  const node = el("span", typeof value === "string" ? value : "", "field-read-value list-code");
  node.setAttribute("aria-label", environment.field.label); return { node, dispose: () => {} };
} });

// Additive registration: html rich-text editing is available for html fields (W-29).
registerFieldWidget("html", { kinds: ["html"], render: renderHtmlField });
// List cells have no RecordForm draft lifecycle: they display safe read-only plain text.
registerFieldWidget("list.html", { kinds: ["html"], render: renderListHtmlField });
