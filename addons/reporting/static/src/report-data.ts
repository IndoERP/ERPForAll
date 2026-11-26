/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
// The wire of `reporting.report` (design REPORTING.CORE §6, §9): decoders that refuse a malformed
// answer by name, and the pure operations on the loaded line list (fold, unfold, load more).
export type CellKind = "monetary" | "decimal" | "integer" | "percent" | "date" | "text" | "empty";
export type LineKind = "section" | "group" | "detail" | "total" | "more";
/** `tone` marks a cell the reader must notice (an overdue date); `currency` is the id whose symbol a monetary cell shows. */
export interface Cell { readonly drillable?: boolean; readonly tone?: "danger"; readonly kind: CellKind; readonly value: string | null; readonly scale: number; readonly currency: number | null; readonly zero: boolean }
export interface Currency { readonly symbol: string; readonly position: "before" | "after" }
export interface Line {
  readonly id: string; readonly parent: string | null; readonly name: string; readonly level: number; readonly kind: LineKind;
  readonly unfoldable: boolean; unfolded: boolean; readonly cells: readonly Cell[]; readonly caret: readonly string[];
  readonly remaining: number | null; readonly nextOffset: number | null;
}
export interface Column { readonly group: number; readonly name: string; readonly label: string; readonly kind: CellKind; readonly sortable: boolean }
export interface Period { readonly from: string | null; readonly to: string }
export interface Filters {
  readonly partnerLedger: boolean;
  readonly generalLedger: boolean;
  readonly serverSearch: boolean;
  readonly dateMode: "range" | "single"; readonly currentOnly: boolean; readonly comparison: boolean; readonly multiCompany: boolean; readonly unfoldAll: boolean;
  readonly hideZero: "never" | "optional" | "by_default"; readonly searchBar: boolean; readonly maxSearchChars: number; readonly totalsBelow: boolean;
}
/** Apply report filters to the loaded tree. Unknown folded or paged amounts stay reachable. */
export function visibleLines(lines: Line[], options: Options, serverSearch = false): Line[] {
  const hideZero = options["hide_zero"] === true;
  const search = !serverSearch && typeof options["search"] === "string" ? options["search"].toLowerCase() : "";
  if (!hideZero && !search) return lines;

  const matched = new Set<string>(), nonzero = new Set<string>();
  for (const line of lines) {
    if (line.kind === "more") continue;
    if (line.name.toLowerCase().includes(search) || (line.parent !== null && matched.has(line.parent))) {
      matched.add(line.id);
    }
  }
  // Descendant matches keep their path. A sibling of a match stays hidden.
  for (let i = lines.length - 1; i >= 0; i--) {
    const line = lines[i]!;
    const blankCells = line.cells.every(cell => cell.kind === "empty" || cell.kind === "text" || cell.kind === "date" || cell.value === null);
    if (line.kind === "more" || (line.unfoldable && !line.unfolded && blankCells) || line.cells.some(cell => !cell.zero)) {
      nonzero.add(line.id);
    }
    if (line.parent !== null) {
      if (nonzero.has(line.id)) nonzero.add(line.parent);
      if (matched.has(line.id)) matched.add(line.parent);
    }
  }
  return lines.filter(line => {
    const id = line.kind === "more" && line.parent !== null ? line.parent : line.id;
    return (!hideZero || nonzero.has(id)) && (!search || matched.has(id));
  });
}

/** The options object as the server resolved it; the screen edits it and sends it back whole (design §5). */
export type Options = Record<string, unknown>;
export interface Loaded { readonly options: Options; readonly lines: Line[]; readonly computedAt: string; readonly hidden: readonly string[] }

const KINDS: readonly string[] = ["monetary", "decimal", "integer", "percent", "date", "text", "empty"];
const LINE_KINDS: readonly string[] = ["section", "group", "detail", "total", "more"];

export function obj(value: unknown, what: string): Record<string, unknown> {
  if (typeof value !== "object" || value === null || Array.isArray(value)) throw new Error(`The server answered ${what} in an unknown form.`);
  return value as Record<string, unknown>;
}
function arr(value: unknown, what: string): unknown[] {
  if (!Array.isArray(value)) throw new Error(`The server answered ${what} in an unknown form.`);
  return value;
}
function str(value: unknown, what: string): string {
  if (typeof value !== "string") throw new Error(`The server answered ${what} in an unknown form.`);
  return value;
}
function num(value: unknown, what: string): number {
  if (typeof value !== "number" || !Number.isInteger(value)) throw new Error(`The server answered ${what} in an unknown form.`);
  return value;
}
function bool(value: unknown, what: string): boolean {
  if (typeof value !== "boolean") throw new Error(`The server answered ${what} in an unknown form.`);
  return value;
}
const orNull = <T>(value: unknown, read: (v: unknown) => T): T | null => value === null || value === undefined ? null : read(value);

function cell(raw: unknown): Cell {
  const c = obj(raw, "a cell"), kind = str(c["kind"], "a cell kind");
  if (!KINDS.includes(kind)) throw new Error(`The server sent a cell of unknown kind '${kind}'.`);
  const value = orNull(c["value"], v => str(v, "a cell value"));
  if (c["tone"] !== undefined && c["tone"] !== "danger") throw new Error(`The server sent a cell of unknown tone '${String(c["tone"])}'.`);
  return {
    ...(c["drillable"] === undefined ? {} : { drillable: bool(c["drillable"], "whether a cell has source records") }), ...(c["tone"] === undefined ? {} : { tone: "danger" as const }),
    kind: kind as CellKind, value, scale: num(c["scale"], "a cell scale"), currency: orNull(c["currency"], v => num(v, "a cell currency")), zero: bool(c["zero"], "a cell")
  };
}
export function line(raw: unknown): Line {
  const l = obj(raw, "a line"), kind = str(l["kind"], "a line kind");
  if (!LINE_KINDS.includes(kind)) throw new Error(`The server sent a line of unknown kind '${kind}'.`);
  return {
    id: str(l["id"], "a line id"), parent: orNull(l["parent"], v => str(v, "a line parent")), name: str(l["name"], "a line name"),
    level: num(l["level"], "a line level"), kind: kind as LineKind, unfoldable: bool(l["unfoldable"], "a line"), unfolded: bool(l["unfolded"], "a line"),
    cells: arr(l["cells"], "line cells").map(cell), caret: arr(l["caret"], "line caret items").map(item => str(item, "a caret item")),
    remaining: orNull(l["remaining"], v => num(v, "a remaining count")), nextOffset: orNull(l["next_offset"], v => num(v, "a next offset"))
  };
}
export function filters(raw: unknown): Filters {
  const f = obj(raw, "the filters"), mode = str(f["date_mode"], "the date mode"), hide = str(f["hide_zero"], "the hide-zero mode");
  if (mode !== "range" && mode !== "single" || hide !== "never" && hide !== "optional" && hide !== "by_default") throw new Error("The server sent unknown filter modes.");
  const currentOnly = f["current_only"] === undefined ? false : bool(f["current_only"], "the current position filter");
  // Older reporting servers use the original wire bound of 256 Unicode characters.
  const maxSearchChars = f["max_search_chars"] === undefined ? 256 : num(f["max_search_chars"], "the search limit");
  if (!Number.isSafeInteger(maxSearchChars) || maxSearchChars < 1) throw new Error("The report search limit is invalid.");
  const provider = f["provider"] === undefined ? undefined : str(f["provider"], "the report provider");
  return { dateMode: mode, partnerLedger: provider === "account_reporting.partner_ledger", generalLedger: provider === "account_reporting.general_ledger", serverSearch: f["server_search"] === undefined ? false : bool(f["server_search"], "the provider search option"), currentOnly, maxSearchChars, hideZero: hide, comparison: bool(f["comparison"], "the filters"), multiCompany: bool(f["multi_company"], "the filters"),
    unfoldAll: bool(f["unfold_all"], "the filters"), searchBar: bool(f["search_bar"], "the filters"), totalsBelow: bool(f["totals_below"], "the filters") };
}
/** Only the accounting Partner Ledger owns the reconciliation control. */
export function validateProviderOptions(options: Options, filters: Filters): void {
  if (filters.generalLedger) {
    const provider = obj(options["provider"], "the General Ledger options");
    if ("fiscal_opening" in provider) bool(provider["fiscal_opening"], "the fiscal opening option");
  }
  if (filters.partnerLedger) {
    bool(obj(options["provider"], "the Partner Ledger options")["unreconciled"], "the unreconciled filter");
  }
}
export function decodeOptions(raw: unknown): { name: string; options: Options; filters: Filters } {
  const a = obj(raw, "the report options");
  const options = obj(a["options"], "the options"), controls = filters(a["filters"]);
  validateProviderOptions(options, controls);
  return { name: str(a["name"], "the report name"), options, filters: controls };
}
export function decodeLines(raw: unknown): Loaded {
  const a = obj(raw, "the report lines");
  return { options: obj(a["options"], "the options"), lines: arr(a["lines"], "the lines").map(line), computedAt: str(a["computed_at"], "the computation time"),
    hidden: arr(a["hidden_lines"], "the hidden lines").map(name => str(name, "a hidden line name")) };
}
export function decodeExpand(raw: unknown): Line[] { return arr(obj(raw, "an unfold")["lines"], "the lines").map(line); }

export function mainGroup(options: Options): number {
  const comparison = options["comparison"];
  const oldestFirst = typeof comparison === "object" && comparison !== null && (comparison as Record<string, unknown>)["order"] === "ascending";
  return oldestFirst ? periods(options).length - 1 : 0;
}

export function columns(options: Options): Column[] {
  return arr(options["columns"], "the columns").map(raw => {
    const c = obj(raw, "a column"), kind = str(c["kind"], "a column kind");
    if (!KINDS.includes(kind)) throw new Error(`The server sent a column of unknown kind '${kind}'.`);
    return { group: num(c["group"], "a column group"), name: str(c["name"], "a column name"), label: str(c["label"], "a column label"), kind: kind as CellKind, sortable: bool(c["sortable"], "a column") };
  });
}
export function periods(options: Options): Period[] {
  return arr(options["column_groups"], "the column groups").map(raw => {
    const p = obj(raw, "a column group");
    return { from: orNull(p["from"], v => str(v, "a period start")), to: str(p["to"], "a period end") };
  });
}
export const unfolded = (options: Options): string[] => arr(options["unfolded"], "the unfolded lines").map(id => str(id, "an unfolded line id"));

/** Extra successful pages per parent. Older servers omit the field. */
export function loadedPages(options: Options): Record<string, number> | undefined {
  if (options["loaded_pages"] === undefined) return undefined;
  const pages = obj(options["loaded_pages"], "the loaded pages");
  return Object.fromEntries(Object.entries(pages).map(([id, count]) => {
    if (typeof count !== "number" || !Number.isSafeInteger(count) || count <= 0) throw new Error("The server sent an invalid loaded page count.");
    return [id, count];
  }));
}

/** The lines below `id`: ids are the parent's id plus a segment, so a prefix with the separator is exact. */
const below = (id: string): string => `${id}|`;

/** `id` folded: its descendants (and the totals and load-more rows among them) leave the list and the unfolded set. */
export function fold(lines: Line[], opened: readonly string[], id: string): { lines: Line[]; unfolded: string[] } {
  const kept = lines.filter(l => !l.id.startsWith(below(id)));
  const self = kept.find(l => l.id === id);
  if (!self) throw new Error("This line is no longer in the report. Refresh the report.");
  self.unfolded = false;
  return { lines: kept, unfolded: opened.filter(open => open !== id && !open.startsWith(below(id))) };
}
/** `children` follow `id` right away (an unfold). */
export function unfold(lines: Line[], opened: readonly string[], id: string, children: readonly Line[]): { lines: Line[]; unfolded: string[] } {
  const at = lines.findIndex(l => l.id === id);
  const self = lines[at];
  if (!self) throw new Error("This line is no longer in the report. Refresh the report.");
  self.unfolded = true;
  const next = [...lines]; next.splice(at + 1, 0, ...children);
  return { lines: next, unfolded: opened.includes(id) ? [...opened] : [...opened, id] };
}
/** The load-more row `id` replaced by the next page. */
export function loadMore(lines: Line[], id: string, page: readonly Line[]): Line[] {
  const at = lines.findIndex(l => l.id === id);
  if (at < 0 || lines[at]?.kind !== "more") throw new Error("This page is no longer in the report. Refresh the report.");
  const next = [...lines]; next.splice(at, 1, ...page);
  return next;
}

/** The next sort of `label` after a click: none, descending, ascending, none (design §12). */
export function nextOrder(options: Options, label: string): { column: string; direction: "desc" | "asc" } | null {
  const order = options["order"];
  const current = order && typeof order === "object" ? order as Record<string, unknown> : null;
  if (!current || current["column"] !== label) return { column: label, direction: "desc" };
  return current["direction"] === "desc" ? { column: label, direction: "asc" } : null;
}

const separators = (lang: string): { group: string; decimal: string } => {
  let parts: Intl.NumberFormatPart[];
  try { parts = new Intl.NumberFormat(lang.replace("_", "-")).formatToParts(1234567.5); } catch { parts = new Intl.NumberFormat("en").formatToParts(1234567.5); }
  return { group: parts.find(p => p.type === "group")?.value ?? ",", decimal: parts.find(p => p.type === "decimal")?.value ?? "." };
};
/** A decimal string shown in the user's locale without passing through a binary float (design §6: exact numbers). */
export function formatCell(c: Cell, lang: string, currencies?: ReadonlyMap<number, Currency>): string {
  if (c.value === null || c.kind === "empty") return "";
  if (c.kind === "date") return formatDate(c.value, lang, { year: "numeric", month: "2-digit", day: "2-digit" });
  if (c.kind !== "monetary" && c.kind !== "decimal" && c.kind !== "integer" && c.kind !== "percent") return c.value;
  const match = /^(-?)(\d+)(?:\.(\d+))?$/.exec(c.value);
  if (!match) return c.value;
  const { group, decimal } = separators(lang);
  const whole = match[2]!.replace(/\B(?=(\d{3})+(?!\d))/g, group);
  const number = `${match[1]}${whole}${match[3] ? decimal + match[3] : ""}${c.kind === "percent" ? " %" : ""}`;
  // A currency whose symbol has not arrived (or may not be read) shows the bare amount, never a guessed symbol.
  const currency = c.kind === "monetary" && c.currency !== null ? currencies?.get(c.currency) : undefined;
  if (!currency) return number;
  return currency.position === "before" ? `${currency.symbol}\u00a0${number}` : `${number}\u00a0${currency.symbol}`;
}
/** The currencies of `lines` that `known` does not hold yet. */
export function missingCurrencies(lines: readonly Line[], known: ReadonlyMap<number, Currency>): number[] {
  const ids = new Set<number>();
  for (const line of lines) for (const c of line.cells) if (c.currency !== null && !known.has(c.currency)) ids.add(c.currency);
  return [...ids];
}
/** The answer of a currency read (`symbol`, `position`), refused by name when malformed. */
export function decodeCurrencies(raw: unknown): Map<number, Currency> {
  const out = new Map<number, Currency>();
  for (const item of arr(raw, "the currencies")) {
    const row = obj(item, "a currency"), position = row["position"];
    if (position !== "before" && position !== "after") throw new Error("The server sent a currency with an unknown symbol position.");
    out.set(num(row["id"], "a currency id"), { symbol: str(row["symbol"], "a currency symbol"), position });
  }
  return out;
}
export function formatDate(iso: string, lang: string, style: Intl.DateTimeFormatOptions = { dateStyle: "medium" }): string {
  const date = new Date(`${iso}T00:00:00Z`);
  if (Number.isNaN(date.getTime())) return iso;
  try { return new Intl.DateTimeFormat(lang.replace("_", "-"), { ...style, timeZone: "UTC" }).format(date); } catch { return iso; }
}
/** The server's UTC computation time (`YYYY-MM-DD HH:MM:SS`) in the user's language and time zone; an unreadable value is shown as sent. */
export function formatStamp(utc: string, lang: string, tz: string): string {
  const date = new Date(`${utc.replace(" ", "T")}Z`);
  if (Number.isNaN(date.getTime())) return utc;
  try { return new Intl.DateTimeFormat(lang.replace("_", "-"), { dateStyle: "medium", timeStyle: "short", timeZone: tz }).format(date); } catch { return utc; }
}
