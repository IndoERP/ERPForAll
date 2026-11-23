/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
// The page the shell shows in its content area when the server refuses the window action a route names.
import { button, el } from "./dom.js";
import { Registry } from "./registry.js";
import { ClientError, type RpcClient } from "./transport.js";
import type { Route } from "./route.js";

/** What a registered refused-action page receives. It never carries the refused action or the server's message. */
export interface RefusedActionEnvironment {
  readonly rpc: RpcClient;
  /** Shell navigation, through the shell's guards; does nothing once the page was left. */
  navigate(route: Route): Promise<void>;
}
export interface RefusedActionHandle { readonly element: HTMLElement; dispose(): void }
/** Replaces the body of the refused-action page (for example to point to a self-service entry). At most one is registered. */
export interface RefusedActionPage { create(environment: RefusedActionEnvironment): RefusedActionHandle }

export const REFUSED_ACTION_TEXT = "You do not have access to this page.";

export const refusedActionPages = new Registry<RefusedActionPage>("refused action page");
/** Register the body of the refused-action page. A second page, under any key, fails: two addons cannot both own it. */
export function registerRefusedActionPage(key: string, page: RefusedActionPage): void {
  const existing = refusedActionPages.entries()[0];
  if (existing) throw new ClientError("validation", `A refused action page is already registered: ${existing[0]}.`);
  refusedActionPages.add(key, page);
}

/** A failure that means the user may not open the action: the server's 403 (`AccessError`). */
export function isActionRefusal(error: unknown): boolean {
  return error instanceof ClientError && error.kind === "permission";
}

/** Raised by the shell's navigation when the action load was refused; never leaves the shell. */
export class RefusedAction extends Error {
  constructor() { super(REFUSED_ACTION_TEXT); this.name = "RefusedAction"; }
}

export interface RefusedActionMount { readonly element: HTMLElement; dispose(): void }

/**
 * The content of the refused-action page: the registered page when there is one, else the fixed text and a
 * "Go to home" button. A registered page that throws is reported and replaced by the fixed page.
 */
export function mountRefusedActionPage(options: {
  rpc: RpcClient;
  navigate(route: Route): Promise<void>;
  goHome(): void;
  report(error: unknown): void;
}): RefusedActionMount {
  let disposed = false;
  const environment: RefusedActionEnvironment = {
    rpc: options.rpc,
    navigate: async route => { if (!disposed) await options.navigate(route); },
  };
  const registered = refusedActionPages.entries()[0];
  if (registered) {
    try {
      const handle = registered[1].create(environment);
      if (!(handle?.element instanceof HTMLElement) || typeof handle.dispose !== "function") {
        throw new ClientError("unsupported", `The refused action page ${registered[0]} returned no element.`);
      }
      const element = el("section", undefined, "refused-action");
      element.append(handle.element);
      return { element, dispose: () => { if (disposed) return; disposed = true; handle.dispose(); } };
    } catch (error) {
      options.report(error);
    }
  }
  const element = el("section", undefined, "refused-action");
  element.setAttribute("role", "alert");
  element.append(el("p", REFUSED_ACTION_TEXT, "refused-action-text"),
    button("Go to home", () => { if (!disposed) options.goHome(); }, "primary refused-action-home"));
  return { element, dispose: () => { disposed = true; } };
}
