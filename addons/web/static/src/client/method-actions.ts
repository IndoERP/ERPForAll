/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
import type { ActionResult } from "../generated/human-ui.js";
import type { Route } from "./route.js";
import { ClientError, newRequestId } from "./transport.js";

export type CurrentWindow = Extract<ActionResult, { kind: "current_window" }>;
export interface MethodAction {
  readonly result: CurrentWindow;
  readonly context: Record<string, unknown>;
  readonly sourceCompanyId?: number;
  readonly origin: Route;
  /** The source record's name for the breadcrumb back to it ("Products › Mouse › Product Variants"). */
  readonly originLabel: string;
}
export class ExpiredMethodAction extends ClientError {
  constructor() { super("validation", "This related action has expired. Open its originating menu again."); }
}

/** Largest method action kept, as JSON bytes. */
export const MAX_METHOD_ACTION_BYTES = 32768;
/** Most method actions kept. */
export const MAX_METHOD_ACTIONS = 64;

/** Bounded state of this page. URLs/history reference identities, never action payloads.
 * An unknown/evicted identity is an explicit error; it must never open an unfiltered list.
 * `keep` hears every entry added and removed: the tab's store keeps them for a reload, and the server admits a kept
 * entry again before it opens. `clear()` (changed metadata, the page's end) drops only this page's copies, so it does
 * not tell `keep`: a kept entry still opens after the server admits it again under the new metadata. */
export class MethodActions {
  private readonly page = newRequestId();
  private next = 0;
  private readonly entries = new Map<string, MethodAction>();
  constructor(private readonly keep?: { save(id: string, entry: MethodAction): void; remove(id: string): void }) {}
  add(entry: MethodAction): string {
    if (!Number.isSafeInteger(this.next + 1)) throw new ClientError("unsupported", "Method-action identities are exhausted. Reload the page.");
    const id = `${this.page}:${++this.next}`;
    this.restore(id, entry);
    return id;
  }
  /** Puts an entry back under its identity (one admitted again after a reload). */
  restore(id: string, entry: MethodAction): void {
    if (new TextEncoder().encode(JSON.stringify(entry)).length > MAX_METHOD_ACTION_BYTES) throw new ClientError("server", "Method-action state is too large.");
    this.entries.delete(id);
    this.entries.set(id, structuredClone(entry));
    if (this.entries.size > MAX_METHOD_ACTIONS) {
      const oldest = this.entries.keys().next().value;
      if (oldest !== undefined) this.entries.delete(oldest);
    }
    this.keep?.save(id, entry);
  }
  get(id: string): MethodAction {
    const entry = this.entries.get(id);
    if (!entry) throw new ExpiredMethodAction();
    return entry;
  }
  remove(id: string): void { this.entries.delete(id); this.keep?.remove(id); }
  clear(): void { this.entries.clear(); }
}
