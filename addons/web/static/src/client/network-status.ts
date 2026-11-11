/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
import { el } from "./dom.js";
import { ClientError, type CallOptions, type ConnectionEvent } from "./transport.js";

export type ConnectionState = "online" | "offline" | "syncing" | "error";
const labels: Readonly<Record<ConnectionState, string>> = {
  online: "Connected", offline: "Offline — check your connection. Changes are not retried automatically.",
  syncing: "Reconnecting — checking the server…", error: "Unable to reconnect. Check your connection and try a safe read again.",
};
/** Presentation only. Its owner reports actual transport/recovery state; this class sends no requests. */
export class NetworkStatus {
  readonly node = el("p", undefined, "client-network-status");
  private dead = false;
  private reads = 0;
  private recoveryState: "offline" | "error" = "offline";
  constructor(host: HTMLElement) { this.node.setAttribute("role", "status"); this.node.setAttribute("aria-live", "polite"); this.node.setAttribute("aria-atomic", "true"); host.append(this.node); this.set("online"); }
  set(state: ConnectionState): void {
    if (this.dead) throw new ClientError("validation", "Network status is disposed.");
    if (!Object.hasOwn(labels, state)) throw new ClientError("validation", "Invalid connection state.");
    this.node.dataset["state"] = state; this.node.hidden = state === "online"; this.node.textContent = labels[state];
  }
  /** Paired transport events settle safe-read cancellation without claiming a successful connection. */
  observe(event: ConnectionEvent, options: CallOptions): void {
    const state = this.node.dataset["state"];
    if (event === "request") {
      if (!options.write) {
        this.reads++;
        if (state !== "online") {
          if (state === "offline" || state === "error") this.recoveryState = state;
          this.set("syncing");
        }
      }
      return;
    }
    if (!options.write) this.reads--;
    if (event === "offline") {
      this.recoveryState = "offline";
      this.set(this.reads > 0 ? "syncing" : "offline");
    } else if (!options.write && event === "online") this.set("online");
    else if (state === "syncing" && !options.write) {
      if (event === "error") this.recoveryState = "error";
      if (this.reads === 0) this.set(this.recoveryState);
    }
  }
  dispose(): void { if (this.dead) return; this.dead = true; this.node.remove(); }
}
