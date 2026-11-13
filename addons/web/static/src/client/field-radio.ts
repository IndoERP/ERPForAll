/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
import type { FieldControl, FieldControlOptions } from "./fields.js";
import { displayValue } from "./fields.js";
import { relationQuery } from "./field-relations.js";
import { el } from "./dom.js";
import { fieldShell } from "./field-shell.js";
import { array, integer, object, text } from "./metadata.js";
import { ClientError } from "./transport.js";

let nextRadioId = 0;
export const RADIO_MANY2ONE_LIMIT = 200;

/**
 * Radio widget renderer for selection and many2one fields.
 * - Read mode: displays label text (field-read-value).
 * - Edit mode: radio group with keyboard arrow navigation, orientation (horizontal/vertical),
 *   and required, readonly, and invalid states.
 */
export function radioControl(options: FieldControlOptions): FieldControl {
  const { field, value } = options;

  // Read mode (e.g. list view): render label text
  if (options.viewContext === "list") {
    const text = displayValue(field, value, "en-US");
    const node = el("span", text, "field-read-value");
    return { node: fieldShell(field, node, options.viewContext).node, dispose: () => {} };
  }

  // Edit mode: radio group
  const id = `field_radio_${++nextRadioId}`;
  const orientation = field.options?.horizontal ? "horizontal" : "vertical";
  const group = el("div", undefined, `field-radio-group o_${orientation} field-radio-${orientation}`);
  group.setAttribute("role", "radiogroup");
  group.setAttribute("aria-label", field.label);
  group.dataset["orientation"] = orientation;
  // Only a view that says `horizontal: false` insists on a column; otherwise a form lays the choices in a row that wraps.
  if (field.options?.horizontal === false) group.dataset["explicitVertical"] = "";
  if (field.required) group.setAttribute("aria-required", "true");

  let currentId: number | null = null;
  let currentName = "";
  if (field.kind === "many2one") {
    if (typeof value === "number" && value > 0) {
      currentId = value;
      currentName = `#${value}`;
    } else if (value && typeof value === "object") {
      const obj = object(value);
      currentId = integer(obj["id"], "relation id");
      currentName = text(obj["display_name"], "relation name");
    }
  }

  const getSelectedKey = (): string | number | null => {
    if (field.kind === "many2one") return currentId;
    const empty = value === false || value === null || value === undefined || value === "";
    return empty ? null : (value as string);
  };

  const shell = fieldShell(field, group, options.viewContext);

  let choices: (readonly [string | number, string])[] = [];
  if (field.selection) {
    choices = [...field.selection];
  } else if (field.kind === "many2one") {
    if (currentId !== null) {
      choices = [[currentId, currentName]];
    }
  }

  function renderChoices(items: readonly (readonly [string | number, string])[]) {
    group.replaceChildren();
    const sel = getSelectedKey();

    for (let i = 0; i < items.length; i++) {
      const [key, labelText] = items[i]!;
      const item = el("div", undefined, "form-check o_radio_item field-radio-item");
      const inputId = `${id}_${key}`;

      const radio = el("input", undefined, "form-check-input o_radio_input");
      radio.type = "radio";
      radio.name = id;
      radio.id = inputId;
      radio.value = String(key);
      radio.dataset["value"] = String(key);
      radio.dataset["index"] = String(i);
      radio.disabled = Boolean(field.readonly);
      radio.checked = sel !== null && String(sel) === String(key);

      radio.addEventListener("change", () => {
        if (field.readonly) return;
        for (const other of group.querySelectorAll<HTMLInputElement>("input[type='radio']")) {
          other.checked = (other === radio);
        }
        shell.clear();
        if (field.kind === "many2one") {
          currentId = typeof key === "number" ? key : Number(key);
          currentName = labelText;
          options.change({ id: currentId, display_name: labelText });
        } else {
          options.change(key);
        }
      });

      const label = el("label", labelText, "form-check-label o_form_label");
      label.htmlFor = inputId;

      item.append(radio, label);
      group.append(item);
    }
  }

  renderChoices(choices);

  // Keyboard navigation: arrow keys cycle between radio choices
  group.addEventListener("keydown", (event: KeyboardEvent) => {
    if (field.readonly) return;
    const inputs = Array.from(group.querySelectorAll<HTMLInputElement>("input[type='radio']:not(:disabled)"));
    if (inputs.length === 0) return;

    const active = inputs.find(r => r.checked) ?? inputs.find(r => r === document.activeElement);
    const currentIndex = active ? inputs.indexOf(active) : -1;
    let nextIndex = -1;

    if (event.key === "ArrowDown" || event.key === "ArrowRight") {
      event.preventDefault();
      nextIndex = currentIndex >= 0 ? (currentIndex + 1) % inputs.length : 0;
    } else if (event.key === "ArrowUp" || event.key === "ArrowLeft") {
      event.preventDefault();
      nextIndex = currentIndex > 0 ? currentIndex - 1 : inputs.length - 1;
    }

    if (nextIndex >= 0) {
      const nextRadio = inputs[nextIndex]!;
      nextRadio.checked = true;
      nextRadio.focus();
      nextRadio.dispatchEvent(new Event("change", { bubbles: true }));
    }
  });

  // We bound the list instead: past RADIO_MANY2ONE_LIMIT choices
  // the job fails loudly naming the field, never a silently truncated choice list. Errors
  // propagate to `options.run`, which reports them on the form.
  if (field.kind === "many2one" && field.relation && options.rpc) {
    const relation = field.relation;
    options.run(async () => {
      const effective = relationQuery(field, options.values ?? {}, options.context, options.fields);
      const result = object(await options.rpc.call(relation, "web_search_read", {
        domain: effective.domain,
        specification: { display_name: {} },
        limit: RADIO_MANY2ONE_LIMIT + 1,
        context: effective.context,
      }), "relation choices");
      const rows = array(result["records"], "relation choice records");
      if (rows.length > RADIO_MANY2ONE_LIMIT) {
        throw new ClientError("validation", `Field ${field.label} has more than ${RADIO_MANY2ONE_LIMIT} ${relation} choices; use the many2one widget instead of radio.`);
      }
      choices = rows.map(r => {
        const rec = object(r);
        return [integer(rec["id"], "id"), text(rec["display_name"], "display_name")] as const;
      });
      renderChoices(choices);
    });
  }

  return {
    node: shell.node,
    dispose: () => {},
  };
}
