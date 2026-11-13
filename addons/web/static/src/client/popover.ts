/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
import { createHotkeyBoundary } from "@erpforall/elang";
import { reportHotkeyError } from "../../../../../web/src/runtime/hotkeys.js";
import { el } from "./dom.js";
import { ClientError } from "./transport.js";

export type PopoverCloseReason = "explicit" | "escape" | "outside" | "navigation" | "abort" | "anchor-removed" | "dispose";
export interface PopoverOptions {
  placement?: "top" | "bottom" | "left" | "right";
  role?: string;
  dismissOnOutside?: boolean;
  restoreFocus?: boolean;
  presentation?: "anchored" | "sheet";
  /** Anchored list that lines up with this element: same left edge, never narrower (a field's suggestion list). */
  alignTo?: HTMLElement;
  signal?: AbortSignal;
  /** Called once after all listeners and DOM have been released. */
  onClose?: (reason: PopoverCloseReason) => void;
}
export interface PopoverHandle { node: HTMLElement; close(reason?: PopoverCloseReason): void }
let popoverIdentity = 0;
// Keyboard ownership is document-wide, like the UI5 focus-trap stack; each service still owns disposal.
const documentPopovers = new WeakMap<Document, PopoverHandle[]>();
const PLACEMENTS = ["top", "bottom", "left", "right"] as const;
const ROLES = ["dialog", "tooltip", "menu", "listbox", "region", "status"];

/** Browser-local, owner-disposable anchored overlays; content is supplied by trusted code. */
export class PopoverService {
  private readonly active = new Set<PopoverHandle>();
  private dead = false;
  constructor(private readonly host: HTMLElement, private readonly report: (error: unknown) => void = reportHotkeyError) {
    if (!(host instanceof HTMLElement) || !host.isConnected) throw new ClientError("validation", "Popover host must be connected.");
  }
  open(anchor: HTMLElement, content: HTMLElement, options: PopoverOptions = {}): PopoverHandle {
    if (this.dead) throw new ClientError("validation", "Popover service is disposed.");
    if (!(anchor instanceof HTMLElement) || !anchor.isConnected || anchor.ownerDocument !== this.host.ownerDocument) throw new ClientError("validation", "Popover anchor must be connected inside its document.");
    if (!(content instanceof HTMLElement) || content.ownerDocument !== anchor.ownerDocument) throw new ClientError("validation", "Popover content must be an element in the anchor document.");
    if (options === null || typeof options !== "object" || Object.keys(options).some(key => !["placement", "role", "dismissOnOutside", "restoreFocus", "presentation", "alignTo", "signal", "onClose"].includes(key))) throw new ClientError("validation", "Invalid popover options.");
    const placement = options.placement === undefined ? "bottom" : options.placement;
    if (!PLACEMENTS.includes(placement)) throw new ClientError("validation", "Invalid popover placement.");
    if (options.role !== undefined && !ROLES.includes(options.role)) throw new ClientError("validation", "Invalid popover role.");
    if (options.alignTo !== undefined && !(options.alignTo instanceof HTMLElement)) throw new ClientError("validation", "Popover alignTo must be an element.");
    if (options.presentation !== undefined && options.presentation !== "anchored" && options.presentation !== "sheet") throw new ClientError("validation", "Invalid popover presentation.");
    if (options.dismissOnOutside !== undefined && typeof options.dismissOnOutside !== "boolean" || options.restoreFocus !== undefined && typeof options.restoreFocus !== "boolean" || options.signal !== undefined && !(options.signal instanceof AbortSignal) || options.onClose !== undefined && typeof options.onClose !== "function") throw new ClientError("validation", "Invalid popover options.");
    if (this.active.size >= 16) throw new ClientError("validation", "Popover limit of 16 reached.");
    const node = el("div", undefined, "message-popover"); node.id = `message-popover-${++popoverIdentity}`;
    node.dataset["placement"] = placement; node.dataset["presentation"] = options.presentation ?? "anchored"; node.setAttribute("role", options.role ?? "dialog"); node.style.position = "fixed";
    const modal = anchor.closest("dialog[open]");
    // Native modal descendants must enter the top layer: translated, scrolling dialog shells
    // establish a fixed containing block and otherwise clip a viewport-positioned calendar.
    if (modal && (typeof node.showPopover !== "function" || typeof node.hidePopover !== "function")) throw new ClientError("unsupported", "Popovers inside dialogs require native popover support.");
    if (modal) {
      node.setAttribute("popover", "manual"); node.style.inset = "auto"; node.style.right = "auto"; node.style.bottom = "auto"; node.style.margin = "0";
      if (options.presentation !== "sheet") node.style.width = "max-content";
    }
    node.append(content);
    if ((options.role ?? "dialog") !== "tooltip") node.setAttribute("aria-label", (anchor.getAttribute("aria-label") ?? anchor.textContent?.trim()) || "More information");
    const doc = anchor.ownerDocument, view = doc.defaultView!;
    const prior = doc.activeElement;
    let closed = false, nativeOpen = false;
    const fit = (): void => {
      if (closed) return;
      if (!anchor.isConnected || !node.isConnected) { handle.close("anchor-removed"); return; }
      const aligned = options.alignTo?.getBoundingClientRect();
      if (aligned) node.style.minWidth = `${aligned.width}px`;
      const target = anchor.getBoundingClientRect(), box = node.getBoundingClientRect();
      const viewport = view.visualViewport;
      const width = viewport?.width ?? view.innerWidth, height = viewport?.height ?? view.innerHeight;
      const originLeft = viewport?.offsetLeft ?? 0, originTop = viewport?.offsetTop ?? 0;
      let left: number, top: number;
      if (options.presentation === "sheet") {
        left = originLeft + 8; top = originTop + Math.max(8, height - box.height - 8);
        node.style.width = `${Math.max(0, width - 16)}px`;
      } else if (placement === "top" || placement === "bottom") {
        left = aligned ? aligned.left : target.left + (target.width - box.width) / 2;
        top = placement === "top" ? target.top - box.height - 8 : (aligned ?? target).bottom + (aligned ? 2 : 8);
      } else {
        left = placement === "left" ? target.left - box.width - 8 : target.right + 8;
        top = target.top + (target.height - box.height) / 2;
      }
      node.style.maxWidth = `${Math.max(0, width - 16)}px`;
      node.style.maxHeight = `${Math.max(0, height - 16)}px`;
      node.style.left = `${Math.max(originLeft + 8, Math.min(left, originLeft + width - box.width - 8))}px`;
      node.style.top = `${Math.max(originTop + 8, Math.min(top, originTop + height - box.height - 8))}px`;
    };
    const outside = (event: Event): void => {
      if (options.dismissOnOutside === false || documentPopovers.get(doc)?.at(-1) !== handle) return;
      const target = event.target;
      if (target instanceof Node && !node.contains(target) && !anchor.contains(target)) handle.close("outside");
    };
    let hotkeys: ReturnType<typeof createHotkeyBoundary> | undefined;
    const removed = new MutationObserver(() => { if (!anchor.isConnected || !node.isConnected) handle.close("anchor-removed"); });
    const resize = typeof ResizeObserver === "function" ? new ResizeObserver(fit) : undefined;
    const abort = (): void => handle.close("abort");
    const handle: PopoverHandle = { node, close: (reason = "explicit") => {
      if (!["explicit", "escape", "outside", "navigation", "abort", "anchor-removed", "dispose"].includes(reason)) throw new ClientError("validation", "Invalid popover close reason.");
      if (closed) return; closed = true;
      removed.disconnect(); resize?.disconnect(); options.signal?.removeEventListener("abort", abort);
      doc.removeEventListener("pointerdown", outside); hotkeys?.dispose();
      view.removeEventListener("resize", fit); view.removeEventListener("scroll", fit, true);
      view.visualViewport?.removeEventListener("resize", fit); view.visualViewport?.removeEventListener("scroll", fit);
      const errors: unknown[] = [];
      if (nativeOpen) { nativeOpen = false; try { node.hidePopover(); } catch (error) { errors.push(error); } }
      node.remove(); this.active.delete(handle);
      const stack = documentPopovers.get(doc), index = stack?.indexOf(handle) ?? -1;
      if (stack && index >= 0) { stack.splice(index, 1); if (!stack.length) documentPopovers.delete(doc); }
      if (options.restoreFocus && (reason === "explicit" || reason === "escape") && prior instanceof HTMLElement && prior.isConnected) prior.focus();
      try { options.onClose?.(reason); } catch (error) { errors.push(error); }
      if (errors.length === 1) throw errors[0];
      if (errors.length > 1) throw new AggregateError(errors, "Popover close failed.");
    } };
    if (options.signal?.aborted) { closed = true; return handle; }
    (modal ?? this.host).append(node); this.active.add(handle);
    const stack = documentPopovers.get(doc) ?? []; stack.push(handle); documentPopovers.set(doc, stack);
    if (modal) {
      try { node.showPopover(); nativeOpen = true; }
      catch (error) { try { handle.close("dispose"); } catch (cleanup) { throw new AggregateError([error, cleanup], "Popover opening and cleanup failed."); } throw error; }
    }
    fit(); if (closed) return handle;
    removed.observe(doc.documentElement, { childList: true, subtree: true }); resize?.observe(anchor); resize?.observe(node);
    doc.addEventListener("pointerdown", outside);
    try { if (options.role !== "tooltip" && options.role !== "status") hotkeys = createHotkeyBoundary(node, { report: this.report, onEscape: () => handle.close("escape") }); }
    catch (error) { handle.close("dispose"); throw error; }
    view.addEventListener("resize", fit); view.addEventListener("scroll", fit, true);
    view.visualViewport?.addEventListener("resize", fit); view.visualViewport?.addEventListener("scroll", fit);
    options.signal?.addEventListener("abort", abort, { once: true });
    return handle;
  }
  private closeAll(reason: PopoverCloseReason): void {
    const errors: unknown[] = [];
    for (const handle of [...this.active].reverse()) { try { handle.close(reason); } catch (error) { errors.push(error); } }
    if (errors.length) throw new AggregateError(errors, "Popover close callbacks failed.");
  }
  navigation(): void { this.closeAll("navigation"); }
  dispose(): void { if (this.dead) return; this.dead = true; this.closeAll("dispose"); }
}
