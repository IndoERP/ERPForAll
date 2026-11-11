/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
import type { ActionResult } from "../generated/human-ui.js";
import type { Route } from "./route.js";
import { ClientError } from "./transport.js";

export type CurrentWindow = Extract<ActionResult, { kind: "current_window" }>;
export interface MethodAction {
  readonly result: CurrentWindow;
  readonly context: Record<string, unknown>;
  readonly origin: Route;
  /** The source record's name for the breadcrumb back to it ("Products › Mouse › Product Variants"). */
  readonly originLabel: string;
}
export class ExpiredMethodAction extends ClientError {
  constructor() { super("validation", "This related action has expired. Open its originating menu again."); }
}

/** Bounded page-lifetime state. URLs/history reference identities, never action payloads.
 * An unknown/evicted identity is an explicit error; it must never open an unfiltered list. */
export class MethodActions {
  private readonly page = crypto.randomUUID();
  private next = 0;
  private readonly entries = new Map<string, MethodAction>();
  add(entry: MethodAction): string {
    if (!Number.isSafeInteger(this.next + 1)) throw new ClientError("unsupported", "Method-action identities are exhausted. Reload the page.");
    if (new TextEncoder().encode(JSON.stringify(entry)).length > 32768) throw new ClientError("server", "Method-action state is too large.");
    const id = `${this.page}:${++this.next}`;
    this.entries.set(id, structuredClone(entry));
    if (this.entries.size > 64) {
      const oldest = this.entries.keys().next().value;
      if (oldest !== undefined) this.entries.delete(oldest);
    }
    return id;
  }
  get(id: string): MethodAction {
    const entry = this.entries.get(id);
    if (!entry) throw new ExpiredMethodAction();
    return entry;
  }
  remove(id: string): void { this.entries.delete(id); }
  clear(): void { this.entries.clear(); }
}
