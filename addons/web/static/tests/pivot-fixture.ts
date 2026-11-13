/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
// The pivot tests' server: a small table of records and what `formatted_read_grouping_sets` answers for it, computed here
// (never by the code under test). It refuses what the real server refuses:
//   - crates/erp-orm/src/rpc/core.rs (FORMATTED_READ_GROUPING_SETS_PARAMS, MAX_LIST_ENTRIES = 64 sets and aggregates),
//   - crates/erp-orm/src/web/groups.rs (`formatter`: unknown field, a date grouping without a granularity; bucket
//     values `[start, label]`, the `__extra_domain` of a bucket is `>= start` and `< end`, labels `November 2026`, `Q4 2026`, `2026`),
//   - crates/erp-rpc/src/binding.rs (an unknown key is a 422).
import type { PivotCall } from "../src/client/pivot.js";

export interface Rec { id: number; priority: string | false; state: string; author: number | false; size: number; weight: number; published: string | false; name: string }
export const authors = new Map<number, string>([[1, "Ada"], [2, "Bo"]]);
export const records: Rec[] = [
  { id: 1, priority: "normal", state: "draft", author: 1, size: 10, weight: 1234.5, published: "2026-01-15", name: "Alpha" },
  { id: 2, priority: "normal", state: "done", author: 1, size: 5, weight: 0.25, published: "2026-02-10", name: "Beta" },
  { id: 3, priority: "high", state: "draft", author: 2, size: 7, weight: 2, published: "2026-02-20", name: "Gamma" },
  { id: 4, priority: "high", state: "draft", author: 2, size: 3, weight: 1, published: "2026-04-01", name: "Delta" },
  { id: 5, priority: false, state: "done", author: false, size: 1500, weight: 0, published: false, name: "Epsilon" }
];
const columns: Record<string, (rec: Rec) => unknown> = {
  priority: rec => rec.priority, state: rec => rec.state, author_id: rec => rec.author, size: rec => rec.size, weight: rec => rec.weight, published: rec => rec.published, name: rec => rec.name
};
const MONTHS = ["January", "February", "March", "April", "May", "June", "July", "August", "September", "October", "November", "December"];
const pad = (value: number): string => String(value).padStart(2, "0");

function bucket(date: string, interval: string): { start: string; end: string; label: string } {
  const [y, m, d] = date.split("-").map(Number) as [number, number, number];
  const iso = (year: number, month: number, day: number): string => `${String(year).padStart(4, "0")}-${pad(month)}-${pad(day)}`;
  const addMonths = (count: number, first: number): string => { const total = y * 12 + first - 1 + count; return iso(Math.floor(total / 12), (total % 12) + 1, 1); };
  switch (interval) {
    case "month": return { start: iso(y, m, 1), end: addMonths(1, m), label: `${MONTHS[m - 1]} ${y}` };
    case "quarter": { const first = Math.floor((m - 1) / 3) * 3 + 1; return { start: iso(y, first, 1), end: addMonths(3, first), label: `Q${(first - 1) / 3 + 1} ${y}` }; }
    case "year": return { start: iso(y, 1, 1), end: iso(y + 1, 1, 1), label: String(y) };
    case "day": { const next = new Date(Date.UTC(y, m - 1, d + 1)); return { start: iso(y, m, d), end: iso(next.getUTCFullYear(), next.getUTCMonth() + 1, next.getUTCDate()), label: `${pad(d)} ${MONTHS[m - 1]!.slice(0, 3)} ${y}` }; }
    default: throw new Error(`fixture: the ${interval} bucket is not modelled`);
  }
}
type Cond = [string, string, unknown];
function group(rec: Rec, spec: string): { value: unknown; domain: unknown[] } {
  const [name, interval] = spec.split(":") as [string, string | undefined];
  const column = columns[name];
  if (!column) throw new Error(`Invalid field '${name}' on model 't.note'`);
  const raw = column(rec);
  if (name === "published") {
    if (!interval) throw new Error(`Granularity is missing in '${spec}'`);
    if (raw === false) return { value: false, domain: [["published", "=", false]] };
    const { start, end, label } = bucket(raw as string, interval);
    return { value: [start, label], domain: ["&", ["published", ">=", start], ["published", "<", end]] };
  }
  if (interval) throw new Error(`Invalid granularity '${interval}' for '${name}'`);
  if (name === "author_id") return { value: raw === false ? false : [raw, authors.get(raw as number)], domain: [["author_id", "=", raw]] };
  return { value: raw, domain: [[name, "=", raw]] };
}

/** The records a domain keeps (prefix notation, implicit AND; terms `=`, `>=`, `<`, `ilike`). */
export function filterRecords(domain: unknown[], source: readonly Rec[]): Rec[] {
  let at = 0;
  const parse = (): ((rec: Rec) => boolean) => {
    const token = domain[at++];
    if (token === "&" || token === "|") { const left = parse(), right = parse(); return token === "&" ? rec => left(rec) && right(rec) : rec => left(rec) || right(rec); }
    if (token === "!") { const inner = parse(); return rec => !inner(rec); }
    const [name, op, expected] = token as Cond;
    const column = columns[name];
    if (!column) throw new Error(`Invalid field '${name}' on model 't.note'`);
    return rec => {
      const value = column(rec);
      switch (op) {
        case "=": return value === expected;
        case ">=": return typeof value === "string" && value >= (expected as string);
        case "<": return typeof value === "string" && value < (expected as string);
        case "ilike": return typeof value === "string" && value.toLowerCase().includes(String(expected).toLowerCase());
        default: throw new Error(`fixture: operator ${op} is not modelled`);
      }
    };
  };
  const tests: ((rec: Rec) => boolean)[] = [];
  while (at < domain.length) tests.push(parse());
  return source.filter(rec => tests.every(test => test(rec)));
}

/** What `formatted_read_grouping_sets` answers: one list of groups per set, each with its group values, `__extra_domain`, `__count` and aggregates. */
export function answer(body: Record<string, unknown>, source: readonly Rec[] = records): unknown[][] {
  const known = ["domain", "grouping_sets", "aggregates", "order", "context"];
  const unknown = Object.keys(body).find(key => !known.includes(key));
  if (unknown) throw new Error(`422: got an unexpected keyword argument '${unknown}'`);
  for (const required of ["domain", "grouping_sets"]) if (!(required in body)) throw new Error(`422: missing a required argument: '${required}'`);
  if (!Array.isArray(body["domain"])) throw new Error("422: domain must be a list");
  const sets = body["grouping_sets"];
  if (!Array.isArray(sets) || !sets.every(set => Array.isArray(set) && set.every(name => typeof name === "string"))) throw new Error("422: grouping_sets must be a list of lists of strings");
  const aggregates = (body["aggregates"] ?? []) as string[];
  if (!Array.isArray(aggregates) || !aggregates.every(name => typeof name === "string")) throw new Error("422: aggregates must be a list of strings");
  if (sets.length > 64) throw new Error(`422: 'grouping_sets' accepts at most 64 sets, got ${sets.length}`);
  if (aggregates.length > 64) throw new Error(`422: 'aggregates' accepts at most 64 entries, got ${aggregates.length}`);
  for (const spec of aggregates) if (spec !== "__count" && !/^(size|weight):sum$/.test(spec)) throw new Error(`422: invalid aggregate '${spec}'`);
  const kept = filterRecords(body["domain"], source);
  return (sets as string[][]).map(set => {
    const groups = new Map<string, { recs: Rec[]; values: unknown[]; domains: unknown[][] }>();
    for (const rec of kept) {
      const found = set.map(spec => group(rec, spec));
      const key = JSON.stringify(found.map(item => item.value));
      const entry = groups.get(key) ?? { recs: [], values: found.map(item => item.value), domains: found.map(item => item.domain) };
      entry.recs.push(rec); groups.set(key, entry);
    }
    if (!set.length && !groups.size) groups.set("[]", { recs: [], values: [], domains: [] });
    // `__extra_domain`: the group's conditions joined by AND (prefix `&` for each join), as the server writes them.
    const rows = [...groups.values()].map(entry => {
      const terms = entry.domains.flatMap(domain => domain);
      const joined = entry.domains.length > 1 ? [...Array<string>(entry.domains.length - 1).fill("&"), ...terms] : terms;
      const row: Record<string, unknown> = { __extra_domain: joined };
      set.forEach((spec, index) => { row[spec] = entry.values[index]; });
      for (const spec of aggregates) row[spec] = spec === "__count" ? entry.recs.length : entry.recs.reduce((total, rec) => total + (rec[spec.split(":")[0] as "size" | "weight"] ?? 0), 0);
      return row;
    });
    // Server order: by the grouping values in the order of the specs, empty values last.
    // Selections sort by their declared sequence (the field's selection list), not by their key.
    const sequence: Record<string, string[]> = { priority: ["normal", "high"], state: ["draft", "done"] };
    const valueOf = (row: Record<string, unknown>, spec: string): string | number | null => {
      const value = row[spec];
      if (value === false) return null;
      return sequence[spec] ? sequence[spec]!.indexOf(value as string) : (Array.isArray(value) ? value[0] : value) as string | number;
    };
    return rows.sort((a, b) => {
      for (const spec of set) {
        const left = valueOf(a, spec), right = valueOf(b, spec);
        if (left === right) continue;
        if (left === null) return 1;
        if (right === null) return -1;
        return left < right ? -1 : 1;
      }
      return 0;
    });
  });
}

export interface Request { groupingSets: string[][]; aggregates: string[]; domain: unknown[] }
export function recorder(source: readonly Rec[] = records): { call: PivotCall; requests: Request[] } {
  const requests: Request[] = [];
  return { requests, call: async request => { requests.push(request); return answer({ domain: request.domain, grouping_sets: request.groupingSets, aggregates: request.aggregates }, source); } };
}
/** A call whose answers are held until released: for the tests of stale answers and the queue. */
export function gated(source: () => readonly Rec[]): { call: PivotCall; requests: Request[]; release(index: number): void; overlapped(): boolean } {
  const requests: Request[] = [], opens: (() => void)[] = [];
  let inFlight = 0, overlap = false;
  return {
    requests, overlapped: () => overlap, release: index => opens[index]!(),
    call: async request => {
      requests.push(request); inFlight++; if (inFlight > 1) overlap = true;
      await new Promise<void>(resolve => { opens[requests.length - 1] = resolve; });
      inFlight--;
      return answer({ domain: request.domain, grouping_sets: request.groupingSets, aggregates: request.aggregates }, source());
    }
  };
}
