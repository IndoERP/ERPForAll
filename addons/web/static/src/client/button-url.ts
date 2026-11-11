/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
import { createButtonControl } from "./button-control.js";
import { parseRoute, type Route } from "./route.js";
import { ClientError } from "./transport.js";

/** Internal adapter only: the server wire does not yet carry URL button declarations. */
export interface UrlButtonEnvironment {
  readonly baseUrl: string;
  navigate(route: Route): Promise<void>;
  /**
   * Caller opens synchronously from activation. With `noopener` the browser's open steps always return no window
   * handle, even when the tab opened, so the result cannot report success or a blocked popup; a blocked popup is
   * shown by the browser's own popup notice. Exceptions thrown by the opener still reach the caller's report.
   */
  openExternal(url: string, target: "_blank", features: "noopener,noreferrer"): void;
}

/** Validate before side effects. Same-origin navigation retains router validation and the caller's dirty guard. */
export async function runUrlButton(href: string, env: UrlButtonEnvironment): Promise<void> {
  if (typeof href !== "string" || !href.trim() || href.length > 4096) throw new ClientError("validation", "Invalid button URL: expected nonempty text of at most 4096 characters.");
  let url: URL, base: URL;
  try { base = new URL(env.baseUrl); url = new URL(href, base); }
  catch { throw new ClientError("validation", "Invalid button URL."); }
  if (!["http:", "https:"].includes(url.protocol) || url.username || url.password) throw new ClientError("validation", "Invalid button URL: only HTTP(S) without credentials is supported.");
  if (url.origin === base.origin) await env.navigate(parseRoute(url));
  else env.openExternal(url.href, "_blank", "noopener,noreferrer");
}

/** One native button, with explicit navigation/report ownership rather than ordinary anchor navigation. */
export function createUrlButton(options: {
  readonly label: string; readonly href: string; readonly environment: UrlButtonEnvironment;
  report(error: unknown): void;
}): HTMLButtonElement {
  let pending = false;
  const control = createButtonControl({ label: options.label, action: () => {
    if (pending) return;
    pending = true; control.disabled = true; control.setAttribute("aria-busy", "true");
    void runUrlButton(options.href, options.environment).catch(options.report).finally(() => {
      pending = false; control.disabled = false; control.removeAttribute("aria-busy");
    });
  } });
  return control;
}
