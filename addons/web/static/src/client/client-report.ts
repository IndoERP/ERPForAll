/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
import { ClientError, type Fetcher } from "./transport.js";

/** What a report is about (SPEC-ERRTRACE: the kinds `POST /erp/client-error` accepts). */
export type ReportKind = "error" | "unhandledrejection" | "render";
const BUILD = /^[A-Za-z0-9._-]{1,64}$/;
/** Reports a page sends at most; the server limits each session to 10 a minute as well. */
const PAGE_LIMIT = 20;

/**
 * Sends a browser-side failure to the server log, where an administrator finds it next to the
 * calls of the same session. An RPC failure is not sent: the server logged it already, under the
 * Error ID the dialog shows. The report carries the message, the page path and the stack, never
 * request arguments or record data.
 */
export class ClientErrorReporter {
  private sent = 0;
  private readonly build: string;
  constructor(private readonly fetcher: Fetcher, build: string) {
    if (!BUILD.test(build)) throw new ClientError("unsupported", "Invalid application build id. Reload the page.");
    this.build = build;
  }
  report(kind: ReportKind, error: unknown): void {
    if (error instanceof ClientError) return;
    // A page caught in an error loop stops reporting instead of flooding the log; the dialog still shows every failure.
    if (this.sent >= PAGE_LIMIT) return;
    this.sent += 1;
    const message = error instanceof Error ? `${error.name}: ${error.message}` : String(error);
    const stack = error instanceof Error && typeof error.stack === "string" && error.stack ? error.stack.slice(0, 4096) : undefined;
    const body = JSON.stringify({ kind, message: message.slice(0, 1024), path: window.location.pathname.slice(0, 512), build: this.build, ...(stack ? { stack } : {}) });
    void this.fetcher("/erp/client-error", {
      method: "POST", credentials: "same-origin", redirect: "error", cache: "no-store", keepalive: true,
      headers: { "Content-Type": "application/json" }, body,
    }).then(response => {
      // A refused report is not shown to the user (it is about an error they already see); the console keeps it.
      if (response.status !== 204) console.warn(`Error report refused by the server: HTTP ${response.status}`);
    }, (failure: unknown) => console.warn("Error report not sent:", failure));
  }
}
