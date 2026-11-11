/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
import { ClientError, type CallOptions, type ConnectionEvent } from "./transport.js";

export const panelCompatibilityMessage = "The application changed. Reload the page to continue.";
export class PanelCompatibilityError extends ClientError {
  constructor() { super("unsupported", panelCompatibilityMessage); this.name="PanelCompatibilityError"; }
}
type RecoveryStorage = Pick<Storage,"getItem"|"setItem">;
const guard = "erp.panel-sections-v1.reload-consumed";
/** At most one automatic read-metadata recovery per browsing-tab session. There is no reset API. */
export function attemptPanelRecovery(storage:()=>RecoveryStorage, safe:boolean, reload:()=>void):boolean {
  if (!safe) return false;
  try {
    const state=storage();
    if(state.getItem(guard)!==null) return false;
    state.setItem(guard,"1");
    if(state.getItem(guard)!=="1") return false;
    reload();
    return true;
  } catch {
    // Storage/reload failure explicitly selects the caller's visible manual-recovery state.
    // Keep any consumed guard; a failure must never re-arm automatic navigation.
    return false;
  }
}

/** All writes interlock recovery, even when an editor has not created its local state yet.
 * An ambiguous write stays unsafe for the lifetime of this application instance. */
export class PanelRecoverySafety {
  private pending = 0;
  private uncertain = false;
  get safe(): boolean { return this.pending === 0 && !this.uncertain; }
  observe(event: ConnectionEvent, options: CallOptions): void {
    if (!options.write) return;
    if (event === "request") { this.pending++; return; }
    if (this.pending === 0) {
      this.uncertain = true;
      throw new ClientError("server", "Invalid write recovery event sequence.");
    }
    this.pending--;
    if (event === "offline") this.uncertain = true;
  }
}
