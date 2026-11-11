/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
import { button, el } from "./dom.js";
import { absentPlaceholderInput } from "./field-options.js";
import { fieldShell } from "./field-shell.js";
import { ClientError } from "./transport.js";
import { parseNumeric, formatNumeric, percentFromFraction, fractionFromPercent } from "./numeric-values.js";
import type { FieldControl, FieldControlOptions } from "./fields.js";

/**
 * Link target for a typed URL, e-mail or phone value, or `null` when the value has no safe link. Values are user
 * data, so this never throws: an unsafe value renders as plain text. URL values without an `http(s)://` or
 * `ftp(s)://` prefix get `http://`, so `javascript:x` becomes a harmless host name.
 */
export function safeLink(widget: string, value: string): string | null {
  if (/\p{Cc}/u.test(value)) return null;
  if (widget === "email") return `mailto:${encodeURIComponent(value)}`;
  if (widget === "phone") return `tel:${encodeURIComponent(value)}`;
  const href = /^(?:(?:ftp|http)s?:\/)?\//i.test(value) ? value : `http://${value}`;
  let url: URL;
  try { url = new URL(href); } catch { return null; }
  return ["http:", "https:", "ftp:"].includes(url.protocol) ? href : null;
}
/** Scalar adapters never save independently; invalid drafts retain their represented value. */
export function scalarControl(options: FieldControlOptions): FieldControl {
  const { field, value, change } = options, config = field.options ?? {}, widget = field.widget ?? field.kind;
  const wrap = (control: HTMLElement) => fieldShell(field, control, options.viewContext);
  const compactRead = options.viewContext === "list" && field.readonly;
  if ((field.kind === "boolean" || widget === "badge" || widget === "label_selection") && (config.placeholder !== undefined || config.placeholder_field !== undefined)) throw new ClientError("validation", `Widget '${widget}' does not support placeholder options.`);
  let placeholder = config.placeholder ?? "";
  if (config.placeholder_field) {
    const supplied = options.values?.[config.placeholder_field.name];
    if (supplied === false || supplied === null || absentPlaceholderInput({ name: config.placeholder_field.name, values: options.values, fields: options.fields })) placeholder = "";
    else if (typeof supplied === "string") placeholder = supplied;
    else throw new ClientError("server", "Invalid scalar placeholder field value.");
  }

  if (config.type !== undefined && config.type !== "text" && config.type !== "number") throw new ClientError("unsupported", "Numeric input type must be text or number. Other input types require a later release.");
  const numericLocale = config.type === "number" ? { decimal_point: ".", thousands_sep: "", grouping: [] } : options.numericLocale;
  const unset = value === null || value === undefined || value === false;
  if (!unset && (field.kind === "char" || field.kind === "text") && typeof value !== "string") throw new ClientError("server", `Invalid text value for ${field.name}.`);
  if (!unset && (field.kind === "integer" || field.kind === "float" || field.kind === "monetary") && (typeof value !== "number" || !Number.isFinite(value) || Math.abs(value) > Number.MAX_SAFE_INTEGER || field.kind === "integer" && !Number.isSafeInteger(value))) throw new ClientError("server", `Invalid numeric value for ${field.name}.`);
  const empty = value === null || value === undefined || value === false && field.kind !== "boolean" || value === "" && (field.kind === "char" || field.kind === "text");
  const text = empty ? "" : String(value);
  if (field.kind === "boolean") {
    if (typeof value !== "boolean" && value !== null && value !== undefined) throw new ClientError("server", `Invalid boolean value for ${field.name}.`);
    if (compactRead) {
      const symbolic = widget === "boolean_favorite" || widget === "boolean_icon";
      const glyph = widget === "boolean_favorite" ? value === true ? "★" : "☆" : widget === "boolean_icon" ? config.icon ?? "●" : value === true ? "Yes" : "No";
      const read = el("span", glyph, "field-read-value");
      if (symbolic) { read.setAttribute("role", "img"); read.setAttribute("aria-label", value === true ? "Yes" : "No"); read.classList.toggle("field-boolean-off", value !== true); }
      if (symbolic && config.icon_class !== undefined) {
        const tokens = config.icon_class.split(/\s+/).filter(Boolean);
        if (!tokens.length || tokens.length > 8 || tokens.some(token => !/^[a-zA-Z_][a-zA-Z0-9_-]{0,63}$/.test(token))) throw new ClientError("validation", "Invalid boolean icon classes.");
        read.classList.add(...tokens);
      }
      return { node: wrap(read).node, dispose: () => {} };
    }
    if (widget === "boolean_favorite" || widget === "boolean_icon") {
      let current = value === true;
      const control = button(widget === "boolean_favorite" ? "★" : config.icon ?? "●", () => { if (!field.readonly) { current = !current; control.setAttribute("aria-pressed", String(current)); change(current); } }, "field-boolean-button");
      if (config.icon_class !== undefined) {
        const tokens = config.icon_class.split(/\s+/).filter(Boolean);
        if (!tokens.length || tokens.length > 8 || tokens.some(token => !/^[a-zA-Z_][a-zA-Z0-9_-]{0,63}$/.test(token))) throw new ClientError("validation", "Invalid boolean icon classes.");
        control.classList.add(...tokens);
      }
      control.disabled = field.readonly; control.setAttribute("aria-label", field.label); control.setAttribute("aria-pressed", String(current));
      return { node: wrap(control).node, dispose: () => {} };
    }
    const input = el("input", undefined, widget === "boolean_checkbox" ? "boolean-checkbox" : "boolean-toggle"); input.type = "checkbox"; input.name = field.name; input.checked = value === true; input.disabled = field.readonly;
    input.addEventListener("change", () => { if (!field.readonly) change(input.checked); });
    return { node: wrap(input).node, dispose: () => {} };
  }
  if (field.selection) {
    let choices = [...field.selection];
    if (widget === "filterable_selection") {
      const supplied = config.whitelist_fname ? options.values?.[config.whitelist_fname.name] : config.whitelisted_values;
      if (supplied !== undefined && (!Array.isArray(supplied) || supplied.some(key => typeof key !== "string"))) throw new ClientError("server", "Invalid selection whitelist.");
      const allowed = supplied as string[] | undefined;
      choices = choices.filter(([key]) => key === value || (allowed ? allowed.includes(key) : !config.blacklisted_values?.includes(key)));
    }
    const label = empty ? "" : field.selection.find(([key]) => key === value)?.[1];
    if (label === undefined) throw new ClientError("server", `Unknown selection value for ${field.name}.`);
    if (field.readonly || widget === "badge" || widget === "label_selection") return { node: wrap(el("span", label, widget === "badge" || widget === "label_selection" ? "field-badge" : "field-read-value")).node, dispose: () => {} };
    const select = el("select", undefined, "styled-select"); select.name = field.name; select.required = field.required;
    select.append(new Option(placeholder, "")); for (const [key, label] of choices) select.append(new Option(label, key)); select.value = empty ? "" : String(value);
    const shell = wrap(select);
    select.addEventListener("change", () => { if (!select.checkValidity()) { shell.invalid(`${field.label} is required.`); return; } shell.clear(); change(select.value === "" ? false : select.value); });
    if (widget === "badges_selection") {
      const badges = el("div", undefined, "field-badge-choices"); badges.setAttribute("role", "group"); badges.setAttribute("aria-label", field.label);
      const limit = config.badge_limit ?? 5;
      for (const [key, label] of choices.slice(0, limit)) { const pick = button(label, () => change(key === value && !field.required ? false : key), "field-badge"); pick.setAttribute("aria-pressed", String(key === value)); badges.append(pick); }
      shell.node.replaceChild(badges, select);
      if (choices.length > limit) { select.setAttribute("aria-label", `More ${field.label} choices`); badges.append(select); }
    }
    return { node: shell.node, dispose: () => {} };
  }
  const numeric = field.kind === "integer" || field.kind === "float" || field.kind === "monetary";
  const percentage = widget === "percentage";
  if (percentage && field.kind !== "integer" && field.kind !== "float") throw new ClientError("validation", `Widget 'percentage' supports integer and float fields, not ${field.kind}.`);
  const numericKind = field.kind === "integer" ? "integer" : "float";
  if (percentage && !numericLocale) throw new ClientError("server", "Percentage fields need the numeric locale.");
  if (numeric && numericLocale) {
    try {
      formatNumeric(false, numericKind, numericLocale);
      // A stored value whose percent leaves the safe range is shown as an invalid field, not a thrown render.
      if (percentage && typeof value === "number") percentFromFraction(value);
    }
    catch (error) {
      if (!(error instanceof ClientError)) throw error;
      if (compactRead) { const shell = wrap(el("span", text, "field-read-value")); shell.invalid(error.message); return { node: shell.node, dispose: () => {} }; }
      const input = el("input"); input.name = field.name; input.value = text; input.setAttribute("aria-disabled", "true");
      input.addEventListener("input", () => { input.value = text; });
      const shell = wrap(input); shell.invalid(error.message); return { node: shell.node, dispose: () => {} };
    }
  }

  if ((field.readonly || widget === "badge" || widget === "label_selection") && widget !== "password" && !config.password) {
    let shown = text;
    if (numeric && !empty && numericLocale) {
      if (typeof value !== "number") throw new ClientError("server", `Invalid numeric value for ${field.name}.`);
      // Currency display precision does not relax integer-backed edit admission.
      shown = percentage ? formatNumeric(percentFromFraction(value), "float", numericLocale, config.digits?.[1]) : formatNumeric(value, widget === "monetary" ? "float" : numericKind, numericLocale, config.digits?.[1]);
      if (config.hide_trailing_zeros && shown.includes(numericLocale.decimal_point)) {
        const at = shown.lastIndexOf(numericLocale.decimal_point);
        const fraction = shown.slice(at + numericLocale.decimal_point.length).replace(/0+$/, "");
        shown = shown.slice(0, at) + (fraction ? numericLocale.decimal_point + fraction : "");
      }
    }
    if (percentage && !empty && !config.no_symbol) shown += "%";
    const href = ["url", "email", "phone"].includes(widget) && text ? safeLink(widget, text) : null;
    const node = href !== null ? el("a", config.text ?? shown, "field-link") : el("span", shown, widget === "badge" || widget === "label_selection" ? "field-badge" : "field-read-value");
    if (node instanceof HTMLAnchorElement && href !== null) { node.href = href; node.rel = "noopener noreferrer"; }
    return { node: wrap(node).node, dispose: () => {} };
  }
  const secret = widget === "password" || config.password === true;
  if (compactRead && secret) {
    const content = el("span", undefined, "field-password-read"), shown = el("span", empty ? "" : "••••••••", "field-read-value"); content.append(shown);
    let revealed = false;
    const reveal = button("Show", () => { revealed = !revealed; shown.textContent = revealed ? text : empty ? "" : "••••••••"; reveal.textContent = revealed ? "Hide" : "Show"; reveal.setAttribute("aria-pressed", String(revealed)); }, "field-password-reveal");
    reveal.setAttribute("aria-label", `Reveal ${field.label}`); reveal.setAttribute("aria-pressed", "false");
    if (!empty) content.append(reveal);
    return { node: wrap(content).node, dispose: () => { shown.textContent = empty ? "" : "••••••••"; revealed = false; } };
  }
  const input = field.kind === "text" && !secret ? el("textarea") : el("input"); input.name = field.name; input.disabled = field.readonly; input.required = field.required;
  input.placeholder = placeholder;
  if (input instanceof HTMLInputElement) {
    input.type = secret ? "password" : widget === "email" ? "email" : widget === "phone" ? "tel" : numeric && (config.type === "number" || !numericLocale) ? "number" : "text";
    if (numeric) { input.inputMode = numericKind === "integer" ? "numeric" : "decimal"; if (input.type === "number" && numericKind === "float") input.step = "any"; }
    if (config.autocomplete !== undefined) input.autocomplete = config.autocomplete as AutoFill;
  }
  if (config.max_length !== undefined) input.maxLength = config.max_length;
  if (input instanceof HTMLTextAreaElement && config.rows !== undefined) input.rows = config.rows;
  input.value = numeric && numericLocale && !empty && typeof value === "number" ? percentage ? formatNumeric(percentFromFraction(value), "float", numericLocale, config.digits?.[1]) : formatNumeric(value, numericKind, numericLocale) : text;
  const shell = wrap(input); let represented = input.value;
  shell.node.classList.toggle("field-empty", empty);
  const validate = (commit: boolean): void => {
    shell.clear(); shell.node.classList.toggle("field-dirty", input.value !== represented);
    try {
      if (!input.checkValidity()) throw new ClientError("validation", input.validationMessage);
      if (numeric && input.value === represented) return;
      if (percentage) {
        // The percent symbol is optional on input; the draft is a float percent, the stored value its fraction.
        const parsed = input.value.replace(/\s*%\s*$/, "");
        if (parsed.trim() === "" && input.value.trim() !== "") throw new ClientError("validation", "Enter a percentage before the % symbol.");
        if (!numericLocale) throw new ClientError("server", "Percentage input needs the numeric locale.");
        const percent = parseNumeric(parsed, "float", numericLocale);
        const fraction = percent === false ? false : fractionFromPercent(percent);
        if (fraction !== false && numericKind === "integer" && !Number.isSafeInteger(fraction)) throw new ClientError("validation", "Enter a whole percentage that is a multiple of 100 for an integer field.");
        if (commit && !field.readonly) { change(fraction); represented = input.value; }
        return;
      }
      const next = numeric ? numericLocale ? parseNumeric(input.value, numericKind, numericLocale) : input.value === "" ? false : Number(input.value) : input.value === "" ? false : input.value;
      if (numeric && next !== false && (typeof next !== "number" || !Number.isFinite(next) || numericKind === "integer" && !Number.isSafeInteger(next))) throw new ClientError("validation", "Enter a valid number.");
      if (commit && !field.readonly) { change(next); represented = input.value; }
    } catch (error) { if (!(error instanceof ClientError)) throw error; shell.invalid(error.message); }
  };
  input.addEventListener("input", () => validate(false)); input.addEventListener("change", () => validate(true));
  input.addEventListener("keydown", event => {
    if (!(event instanceof KeyboardEvent)) return;
    if (event.key === "Escape") { input.value = represented; shell.clear(); shell.node.classList.remove("field-dirty"); }
    else if (numeric && (event.key === "Enter" || event.key === "Tab")) { if (event.key === "Enter") event.preventDefault(); validate(true); }
  });
  let reset = (): void => {};
  if (secret && input instanceof HTMLInputElement) {
    const reveal = button("Show", () => { const visible = input.type === "password"; input.type = visible ? "text" : "password"; reveal.textContent = visible ? "Hide" : "Show"; reveal.setAttribute("aria-pressed", String(visible)); }, "field-password-reveal");
    reveal.setAttribute("aria-label", `Reveal ${field.label}`); reveal.setAttribute("aria-pressed", "false"); shell.node.append(reveal);
    reset = () => { input.type = "password"; reveal.setAttribute("aria-pressed", "false"); };
  }
  const href = ["url", "email", "phone"].includes(widget) && text ? safeLink(widget, text) : null;
  if (href !== null) { const link = el("a", `Open ${field.label}`, "field-link-action"); link.href = href; link.rel = "noopener noreferrer"; shell.node.append(link); }
  return { node: shell.node, dispose: reset,
    captureDraft: () => input.value !== represented || !input.validity.valid ? { value: input.value, message: input.validationMessage } : undefined,
    restoreDraft: draft => {
      input.value = draft.value; input.dispatchEvent(new Event("input"));
      if (draft.message && input.validity.valid) shell.invalid(draft.message);
    },
  };
}
