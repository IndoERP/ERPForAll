/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
import { icon } from "./icons.js";
import { ClientError } from "./transport.js";
import type { GroupableField } from "./search/types.js";

const MAX_GROUPS = 10_000;
const MAX_DOMAIN_TOKENS = 256;
const MAX_DOMAIN_DEPTH = 8;
const MAX_LABEL = 4096;
const invalid = (what: string): never => { throw new ClientError("server", `Invalid ${what}.`); };

/** One group of a one-field grouping, as `web_read_group` answers it. */
export interface ListGroup {
  /** Stable for one value (the open/closed memory of the list keys on it). */
  key: string;
  label: string;
  count: number;
  /** The domain that selects the group's records, to add to the list's own. */
  domain: unknown[];
}

/** The label of a group: what the user reads on the row. An unset value reads "None". */
export function groupLabel(field: GroupableField, value: unknown): string {
  switch (field.kind) {
    case "boolean": return value === true ? "Yes" : "No";
    case "selection": {
      if (value === false) return "None";
      const found = field.selection?.find(([key]) => key === value);
      return found ? found[1] : invalid(`selection value of ${field.name}`);
    }
    case "many2one": {
      if (value === false) return "None";
      if (!Array.isArray(value) || value.length !== 2 || !Number.isSafeInteger(value[0]) || typeof value[1] !== "string" && value[1] !== false) return invalid(`group value of ${field.name}`);
      return value[1] === false || value[1] === "" ? `#${value[0]}` : value[1];
    }
    case "integer": {
      if (value === false) return "None";
      return Number.isSafeInteger(value) ? String(value) : invalid(`group value of ${field.name}`);
    }
    case "char": {
      if (value === false || value === "") return "None";
      return typeof value === "string" ? value : invalid(`group value of ${field.name}`);
    }
  }
}

const depth = (value: unknown, level = 0): number => !Array.isArray(value) ? level : level >= MAX_DOMAIN_DEPTH ? level + 1 : Math.max(level + 1, ...value.map(item => depth(item, level + 1)));

/** Validate a `web_read_group` answer for a grouping by `field`: closed set of keys, checked values and counts.
 *  `limit` is the page size that was asked for; more groups than that is refused. */
export function decodeGroups(raw: unknown, field: GroupableField, limit = MAX_GROUPS): { groups: ListGroup[]; length: number } {
  if (!raw || typeof raw !== "object" || Array.isArray(raw)) return invalid("group answer");
  const answer = raw as Record<string, unknown>;
  if (Object.keys(answer).some(key => key !== "groups" && key !== "length")) return invalid("group answer");
  const rows = answer["groups"], length = answer["length"];
  if (!Array.isArray(rows) || rows.length > Math.min(limit, MAX_GROUPS) || typeof length !== "number" || !Number.isSafeInteger(length) || length < rows.length) return invalid("group answer");
  const seen = new Set<string>();
  const groups = rows.map((item): ListGroup => {
    if (!item || typeof item !== "object" || Array.isArray(item)) return invalid("group");
    const row = item as Record<string, unknown>;
    if (Object.keys(row).some(key => key !== field.name && key !== "__count" && key !== "__extra_domain")) return invalid("group property");
    const count = row["__count"], domain = row["__extra_domain"];
    if (typeof count !== "number" || !Number.isSafeInteger(count) || count < 0) return invalid("group count");
    if (!Array.isArray(domain) || domain.length > MAX_DOMAIN_TOKENS || depth(domain) > MAX_DOMAIN_DEPTH) return invalid("group domain");
    const value = row[field.name];
    // A linked record is the same group when its name changes: key on the id alone.
    const key = JSON.stringify(field.kind === "many2one" && Array.isArray(value) ? value[0] : value);
    if (seen.has(key)) return invalid("group list: duplicate group");
    seen.add(key);
    const label = groupLabel(field, value);
    if (label.length > MAX_LABEL) return invalid("group label");
    return { key, label, count, domain };
  });
  return { groups, length };
}

export interface GroupRow {
  row: HTMLTableRowElement;
  toggle: HTMLButtonElement;
  setExpanded(open: boolean): void;
  /** True while the group's records are being read: `aria-busy` on the toggle. */
  setBusy(busy: boolean): void;
  isExpanded(): boolean;
}

/** The row that heads a group: one toggle button (Enter and Space press it, ArrowRight opens, ArrowLeft closes)
 *  that carries the label, the count and `aria-expanded`. `columns` is the table's column count. */
export function createGroupRow(config: { group: ListGroup; columns: number; onToggle: () => void }): GroupRow {
  const { group, columns, onToggle } = config;
  const row = document.createElement("tr");
  row.className = "list-group-row";
  row.dataset["groupKey"] = group.key;
  const cell = document.createElement("td");
  cell.colSpan = columns;
  const toggle = document.createElement("button");
  toggle.type = "button";
  toggle.className = "list-group-toggle";
  toggle.setAttribute("aria-expanded", "false");
  // The accessible name starts with the visible label (WCAG 2.5.3) and states the count in words.
  toggle.setAttribute("aria-label", `${group.label}, ${group.count} ${group.count === 1 ? "record" : "records"}`);
  const label = document.createElement("span");
  label.className = "list-group-label";
  label.textContent = group.label;
  label.title = group.label;
  const count = document.createElement("span");
  count.className = "list-group-count";
  count.textContent = `(${group.count})`;
  toggle.append(icon("chevron-right"), label, count);
  toggle.addEventListener("click", event => { event.stopPropagation(); onToggle(); });
  toggle.addEventListener("keydown", event => {
    if (event.key === "ArrowRight" && toggle.getAttribute("aria-expanded") === "false" || event.key === "ArrowLeft" && toggle.getAttribute("aria-expanded") === "true") {
      event.preventDefault(); onToggle();
    }
  });
  // The whole row is the target, as the record rows are.
  row.addEventListener("click", event => { if (event.target === row || event.target === cell) onToggle(); });
  cell.append(toggle);
  row.append(cell);
  return {
    row, toggle,
    setExpanded: open => { toggle.setAttribute("aria-expanded", String(open)); row.classList.toggle("list-group-open", open); },
    setBusy: busy => { if (busy) toggle.setAttribute("aria-busy", "true"); else toggle.removeAttribute("aria-busy"); },
    isExpanded: () => toggle.getAttribute("aria-expanded") === "true"
  };
}
