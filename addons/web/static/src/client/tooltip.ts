/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
import { el } from "./dom.js";
import { type PopoverHandle, PopoverService } from "./popover.js";
import { ClientError } from "./transport.js";

export interface TooltipOptions { text?: string; content?: () => HTMLElement; delay?: number; touch?: boolean }
/** Scoped pointer/touch help. Keyboard-triggered parity is not inferred from pointer observations. */
export class TooltipService {
  private readonly detachments = new Set<() => void>();
  private dead = false;
  constructor(private readonly popovers: PopoverService, private readonly report: (error: unknown) => void) {
    if (typeof report !== "function") throw new ClientError("validation", "Tooltip service requires an owner error reporter.");
  }
  attach(anchor: HTMLElement, options: TooltipOptions): () => void {
    if (this.dead) throw new ClientError("validation", "Tooltip service is disposed.");
    if (!(anchor instanceof HTMLElement) || !anchor.isConnected) throw new ClientError("validation", "Tooltip anchor must be connected.");
    if (options === null || typeof options !== "object" || Object.keys(options).some(key => !["text", "content", "delay", "touch"].includes(key))) throw new ClientError("validation", "Invalid tooltip options.");
    if (options.text === undefined && options.content === undefined) throw new ClientError("validation", "Tooltip requires text or trusted content.");
    if (options.text !== undefined && (typeof options.text !== "string" || new TextEncoder().encode(options.text).length > 4096)) throw new ClientError("validation", "Tooltip text must be a string of at most 4096 bytes.");
    if (options.content !== undefined && typeof options.content !== "function" || options.touch !== undefined && typeof options.touch !== "boolean") throw new ClientError("validation", "Invalid tooltip options.");
    const delay = options.delay === undefined ? 400 : options.delay;
    if (typeof delay !== "number" || !Number.isFinite(delay) || delay < 0 || delay > 60000) throw new ClientError("validation", "Tooltip delay must be finite and between 0 and 60000 milliseconds.");
    let timer: ReturnType<typeof setTimeout> | undefined, current: PopoverHandle | undefined, detached = false;
    const described = anchor.getAttribute("aria-describedby");
    const restoreDescription = (): void => {
      if (described === null) anchor.removeAttribute("aria-describedby"); else anchor.setAttribute("aria-describedby", described);
    };
    const close = (): void => { clearTimeout(timer); timer = undefined; const view = current; current = undefined; view?.close(); restoreDescription(); };
    const schedule = (): void => {
      close();
      if (detached || !anchor.isConnected || !options.content && options.text?.trim() === anchor.textContent?.trim()) return;
      timer = setTimeout(() => {
        timer = undefined;
        if (detached || !anchor.isConnected) return;
        try {
          const content = options.content ? options.content() : el("span", options.text);
          // A trusted factory may synchronously dispose its owner or detach its target.
          if (this.dead || detached || !anchor.isConnected) return;
          current = this.popovers.open(anchor, content, { role: "tooltip", dismissOnOutside: false, onClose: () => { current = undefined; restoreDescription(); } });
          current.node.classList.add("message-tooltip");
          anchor.setAttribute("aria-describedby", [described, current.node.id].filter(value => value !== null && value !== "").join(" "));
        } catch (error) {
          // attach() returned before this timer ran: the owner reporter is the failure channel.
          close();
          if (!this.dead && !detached && anchor.isConnected) this.report(error);
        }
      }, delay);
    };
    const removed = new MutationObserver(() => { if (!anchor.isConnected) detach(); });
    anchor.addEventListener("mouseenter", schedule); anchor.addEventListener("mouseleave", close); anchor.addEventListener("click", close);
    if (options.touch !== false) { anchor.addEventListener("touchstart", schedule); anchor.addEventListener("touchend", close); anchor.addEventListener("touchcancel", close); }
    removed.observe(anchor.ownerDocument.documentElement, { childList: true, subtree: true });
    const detach = (): void => {
      if (detached) return; detached = true; close(); removed.disconnect();
      anchor.removeEventListener("mouseenter", schedule); anchor.removeEventListener("mouseleave", close); anchor.removeEventListener("click", close);
      anchor.removeEventListener("touchstart", schedule); anchor.removeEventListener("touchend", close); anchor.removeEventListener("touchcancel", close);
      this.detachments.delete(detach);
    };
    this.detachments.add(detach); return detach;
  }
  dispose(): void { if (this.dead) return; this.dead = true; for (const detach of [...this.detachments]) detach(); }
}
