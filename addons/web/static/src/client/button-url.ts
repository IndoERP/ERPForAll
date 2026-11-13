/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
import { createButtonControl } from "./button-control.js";
import { parseRoute, type Route } from "./route.js";
import { ClientError } from "./transport.js";

/** Internal adapter only: the server wire does not yet carry URL button declarations. */
export interface UrlButtonEnvironment {
  readonly baseUrl: string;
  navigate(route: Route): Promise<void>;
  /** A same-origin address that is not a client route (a document, a download, the documentation): loaded as a page. */
  load?(url: string): Promise<void> | void;
  /**
   * Caller opens synchronously from activation. With `noopener` the browser's open steps always return no window
   * handle, even when the tab opened, so the result cannot report success or a blocked popup; a blocked popup is
   * shown by the browser's own popup notice. Exceptions thrown by the opener still reach the caller's report.
   */
  openExternal(url: string, target: "_blank", features: "noopener,noreferrer"): void;
}

/** The paths `parseRoute` reads: the client's own routes. Anything else on the origin is a page the server serves. */
const CLIENT_ROUTE = /^\/(?:erp|odoo|web)$|^\/(?:erp|odoo)\/action-/;

/** Validate before side effects. Same-origin navigation retains router validation and the caller's dirty guard. */
export async function runUrlButton(href: string, env: UrlButtonEnvironment): Promise<void> {
  if (typeof href !== "string" || !href.trim() || href.length > 4096) throw new ClientError("validation", "Invalid button URL: expected nonempty text of at most 4096 characters.");
  let url: URL, base: URL;
  try { base = new URL(env.baseUrl); url = new URL(href, base); }
  catch { throw new ClientError("validation", "Invalid button URL."); }
  if (!["http:", "https:"].includes(url.protocol) || url.username || url.password) throw new ClientError("validation", "Invalid button URL: only HTTP(S) without credentials is supported.");
  if (url.origin === base.origin) {
    if (env.load && !CLIENT_ROUTE.test(url.pathname)) await env.load(url.href);
    else await env.navigate(parseRoute(url));
  }
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

/**
 * Open a link supplied by an addon provider (menu, palette, debug item): scheme and credentials are validated by
 * `runUrlButton`; same-origin paths use the host router when it gave one, external ones open with `noopener,noreferrer`.
 */
export function openProviderUrl(href: string, options: { navigate?: (route: Route) => Promise<void>; loadPath?: (url: string) => Promise<void>; newTab?: boolean } = {}): Promise<void> {
  const open = (url: string): void => { window.open(url, "_blank", "noopener,noreferrer"); };
  return runUrlButton(href, {
    baseUrl: window.location.href,
    navigate: async route => {
      if (options.newTab) open(new URL(href, window.location.href).href);
      else if (options.navigate) await options.navigate(route);
      else window.location.assign(new URL(href, window.location.href).href);
    },
    // Not a client route: a new tab, or a page load after the host agreed to leave the form.
    load: async url => {
      if (options.newTab) open(url);
      else if (options.loadPath) await options.loadPath(url);
      else window.location.assign(url);
    },
    openExternal: url => open(url)
  });
}
