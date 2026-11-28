/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
import { actionResult } from "./actions.js";
import { MAX_METHOD_ACTIONS, MAX_METHOD_ACTION_BYTES, type MethodAction } from "./method-actions.js";
import type { BreadcrumbItem } from "./navigation.js";
import { parseRoute, routeUrl, type Route } from "./route.js";

/** The tab's navigation store: one session-storage key (`erp.session.*`, so sign-out, expiry and the sign-in page erase
 *  it with the rest of the tab's protected state). It holds, per history entry, the breadcrumb trail as it was drawn
 *  (titles and addresses the user saw), and each related list a button opened (the list the server admitted for it, the
 *  address and title of the page it was opened from). What it reads back is untrusted: checked entry by entry, never
 *  evaluated, and every entry is loaded again through the normal, server-checked navigation when the user goes to it; a
 *  related list is admitted again by the server before it opens. */
export const NAVIGATION_KEY = "erp.session.navigation";
/** Most history entries whose trail is kept; the oldest drop first. */
const MAX_ENTRIES = 64;
/** Largest stored text, in UTF-16 units; a larger value is not read. */
const MAX_STORED = 1024 * 1024;
const MAX_TITLE = 512;

interface Stored { readonly id: string; readonly title: string; readonly url: string; readonly form: boolean }
interface StoredAction { readonly result: unknown; readonly context: Record<string, unknown>; readonly origin: string; readonly label: string; readonly sourceCompanyId?: number }

const MAX_CONTEXT_TEXT = 4096;
const scalar = (value: unknown): boolean => typeof value === "string" && value.length <= MAX_CONTEXT_TEXT
  || typeof value === "number" && Number.isFinite(value) && (!Number.isInteger(value) || Number.isSafeInteger(value));
/** A value of a kept list's context, admitted by type: null, a boolean, a text, a number (whole numbers in the safe
 *  range), or a flat list of numbers or texts (ids, names). Anything else (a nested object) is not one this client keeps. */
const contextValue = (value: unknown): boolean => value === null || typeof value === "boolean" || scalar(value)
  || Array.isArray(value) && value.length <= 1000 && value.every(scalar);
const CONTEXT_KEY = /^[A-Za-z_][A-Za-z0-9_]{0,127}$/;
const title = (value: unknown): value is string => typeof value === "string" && !!value.trim() && value.length <= MAX_TITLE && !/[\u0000-\u001f\u007f]/u.test(value);
/** A page address this client writes: same origin, a route the parser admits, an action. */
function address(value: unknown): Route | undefined {
  if (typeof value !== "string" || !value.startsWith("/") || value.startsWith("//")) return undefined;
  try {
    const route = parseRoute(new URL(value, window.location.origin));
    return route.action === undefined ? undefined : route;
  } catch { return undefined; }
}

/** A trail entry read back from storage, or `undefined` when it is not one this client writes. */
function crumb(raw: unknown): BreadcrumbItem | undefined {
  if (typeof raw !== "object" || raw === null || Array.isArray(raw)) return undefined;
  const { id, title: name, url, form } = raw as Record<string, unknown>;
  if (typeof id !== "string" || !/^[A-Za-z0-9:_-]{1,256}$/.test(id) || !title(name) || typeof form !== "boolean") return undefined;
  const route = address(url);
  return route && { id, title: name, route, url: routeUrl(route), isFormView: form };
}

/** A related list read back from storage, or `undefined` when it is not one this client writes. The list itself goes
 *  through the same decoder as a server answer; the server admits it again before it opens. */
function methodAction(raw: unknown): MethodAction | undefined {
  if (typeof raw !== "object" || raw === null || Array.isArray(raw) || JSON.stringify(raw).length > MAX_METHOD_ACTION_BYTES) return undefined;
  const { result, context, origin, label, sourceCompanyId } = raw as Record<string, unknown>;
  if (sourceCompanyId !== undefined && (typeof sourceCompanyId !== "number" || !Number.isSafeInteger(sourceCompanyId) || sourceCompanyId <= 0)) return undefined;
  const route = address(origin);
  if (!route || !title(label) || typeof context !== "object" || context === null || Array.isArray(context) || Object.keys(context).length > 64) return undefined;
  // Each value goes into the server's admission and every request of the list: only the kinds this client writes there.
  if (!Object.entries(context).every(([key, value]) => CONTEXT_KEY.test(key) && contextValue(value))) return undefined;
  try {
    const decoded = actionResult({ wire_major: 1, result });
    return decoded.kind === "current_window" ? { result: decoded, context: context as Record<string, unknown>, origin: route, originLabel: label,
      ...(sourceCompanyId === undefined ? {} : { sourceCompanyId }) } : undefined;
  } catch { return undefined; }
}
const METHOD_ACTION_ID = /^[0-9a-f]{8}-[0-9a-f]{4}-4[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}:[1-9][0-9]{0,15}$/;

export class NavigationStore {
  private trails = new Map<number, readonly BreadcrumbItem[]>();
  private readonly actions = new Map<string, MethodAction>();
  private failed = false;
  /** Set when the session ends: the tab's protected state is erased, and nothing is written back after that. */
  private ended = false;

  /** `report` hears once when the browser refuses to keep the store (disabled storage, full quota). */
  constructor(private readonly uid: number, private readonly cap: number, private readonly report: (error: unknown) => void,
    private readonly storage: () => Storage = () => window.sessionStorage) {
    // A request that fails after expiry or sign-out (a catch that drops its related list) must not write the store again.
    window.addEventListener("erp:session-ended", () => { this.ended = true; this.trails.clear(); this.actions.clear(); }, { once: true });
    let text: string | null = null;
    try { text = this.storage().getItem(NAVIGATION_KEY); } catch (error) { this.refused(error); }
    if (text === null) return;
    let data: unknown;
    try { data = text.length <= MAX_STORED ? JSON.parse(text) : undefined; } catch { data = undefined; }
    const value = typeof data === "object" && data !== null && !Array.isArray(data) ? data as Record<string, unknown> : {};
    // Another user's trail (another sign-in in this tab's session) is never shown.
    if (value["v"] !== 1 || value["uid"] !== uid || !Array.isArray(value["trails"])) { this.write(); return; }
    for (const pair of value["trails"].slice(-MAX_ENTRIES)) {
      if (!Array.isArray(pair) || pair.length !== 2 || !Number.isSafeInteger(pair[0]) || pair[0] < 0 || !Array.isArray(pair[1])) continue;
      const items = (pair[1] as unknown[]).slice(-cap).map(crumb).filter((item): item is BreadcrumbItem => item !== undefined);
      if (items.length) this.trails.set(pair[0] as number, items);
    }
    for (const pair of Array.isArray(value["actions"]) ? value["actions"].slice(-MAX_METHOD_ACTIONS) : []) {
      if (!Array.isArray(pair) || pair.length !== 2 || typeof pair[0] !== "string" || !METHOD_ACTION_ID.test(pair[0])) continue;
      const entry = methodAction(pair[1]);
      if (entry) this.actions.set(pair[0], entry);
    }
  }

  /** The related list `id` kept by this tab, as it was before a reload; the caller has the server admit it again. */
  action(id: string): MethodAction | undefined {
    return this.actions.get(id);
  }

  saveAction(id: string, entry: MethodAction): void {
    this.actions.delete(id);
    this.actions.set(id, entry);
    while (this.actions.size > MAX_METHOD_ACTIONS) this.actions.delete(this.actions.keys().next().value!);
    this.write();
  }

  removeAction(id: string): void {
    if (this.actions.delete(id)) this.write();
  }

  /** The trail kept for history entry `index`, when its last entry is the page `route` opens (same action, same related
   *  list, same record): a trail kept for another page is not shown with this one. The search part of a list's address is
   *  left out: the list rewrites it in place without keeping its trail again. */
  trail(index: number, route: Route): readonly BreadcrumbItem[] | undefined {
    const items = this.trails.get(index), top = items?.at(-1)?.route;
    return top && top.action === route.action && top.method_action === route.method_action && top.id === route.id ? items : undefined;
  }

  /** The history index a page load without one starts from: past every entry this tab keeps a trail for, so the older
   *  entries (Back after a full page load, as the company switcher does) keep theirs. */
  nextIndex(): number {
    return this.trails.size ? Math.max(...this.trails.keys()) + 1 : 0;
  }

  /** Keeps `items` (the trail on screen) for history entry `index`. */
  saveTrail(index: number, items: readonly BreadcrumbItem[]): void {
    this.trails.delete(index);
    this.trails.set(index, items.slice(-this.cap).map(item => ({ id: item.id, title: item.title, route: { ...item.route }, url: item.url, isFormView: item.isFormView === true })));
    while (this.trails.size > MAX_ENTRIES) this.trails.delete(this.trails.keys().next().value!);
    this.write();
  }

  private write(): void {
    if (this.ended) return;
    const trails = [...this.trails].map(([index, items]) => [index, items.map((item): Stored => ({ id: item.id, title: item.title, url: item.url, form: item.isFormView === true }))]);
    const actions = [...this.actions].map(([id, entry]) => [id, { result: entry.result, context: entry.context, origin: routeUrl(entry.origin), label: entry.originLabel,
      ...(entry.sourceCompanyId === undefined ? {} : { sourceCompanyId: entry.sourceCompanyId }) } satisfies StoredAction]);
    let text = JSON.stringify({ v: 1, uid: this.uid, trails, actions });
    // The oldest related lists, then the oldest trails, give way until the store fits its bound. Each entry's own length
    // (and its comma) is taken off, so the whole store is written out once more, not once per entry.
    if (text.length > MAX_STORED) {
      let excess = text.length - MAX_STORED;
      while (excess > 0 && trails.length + actions.length > 1) {
        // The comma goes with the entry only while another one stays in the same array.
        if (actions.length) { excess -= JSON.stringify(actions.shift()).length + (actions.length ? 1 : 0); this.actions.delete(this.actions.keys().next().value!); }
        else { excess -= JSON.stringify(trails.shift()).length + (trails.length ? 1 : 0); this.trails.delete(this.trails.keys().next().value!); }
      }
      text = JSON.stringify({ v: 1, uid: this.uid, trails, actions });
    }
    try { this.storage().setItem(NAVIGATION_KEY, text); } catch (error) { this.refused(error); }
  }

  private refused(error: unknown): void {
    if (this.failed) return;
    this.failed = true;
    this.report(error);
  }
}
