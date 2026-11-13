/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
import { editorEnvironment, editorOptions, editorText, type EditorEnvironment } from "./editor-types.js";
import { ClientError } from "./transport.js";
export type EditorCloseReason = "select" | "cancel" | "dispose";
export interface EditorOverlayOptions { anchor: HTMLElement; content: HTMLElement; environment: EditorEnvironment; title: string; touch?: boolean; alignTo?: HTMLElement; onClose?: (reason: EditorCloseReason) => void }
export interface EditorOverlay { node: HTMLElement; mode: "popover" | "sheet"; close(reason?: EditorCloseReason, restoreFocus?: boolean): void; dispose(): void }
/** Open caller-owned trusted content through the application's shared overlay services. */
export function openEditorOverlay(options: EditorOverlayOptions): EditorOverlay {
  editorOptions(options, ["anchor", "content", "environment", "title", "touch", "alignTo", "onClose"], "Editor overlay");
  editorEnvironment(options.environment); editorText(options.title, "Editor overlay title");
  if (!(options.anchor instanceof HTMLElement) || !options.anchor.isConnected || !(options.content instanceof HTMLElement) || options.content.ownerDocument !== options.anchor.ownerDocument) throw new ClientError("validation", "Editor overlay requires a connected anchor and content in the same document.");
  if (options.touch !== undefined && typeof options.touch !== "boolean") throw new ClientError("validation", "Editor overlay touch must be a boolean.");
  if (options.onClose !== undefined && typeof options.onClose !== "function") throw new ClientError("validation", "Editor overlay onClose must be a function.");
  const view = options.anchor.ownerDocument.defaultView!;
  const touch = options.touch === undefined ? typeof view.matchMedia === "function" && view.matchMedia("(pointer: coarse)").matches && view.innerWidth <= 700 : options.touch;
  const mode = touch ? "sheet" : "popover";
  let reason: EditorCloseReason = "cancel", restore = true, closed = false;
  let inner: { close(): void } | undefined;
  const removed = new MutationObserver(() => { if (!options.anchor.isConnected) close("dispose", false); });
  const finish = (): void => {
    if (closed) return; closed = true;
    removed.disconnect(); options.environment.signal?.removeEventListener("abort", abort);
    if (options.environment.signal?.aborted) reason = "dispose";
    try { options.onClose?.(reason); } catch (error) { options.environment.report(error); }
    if (restore && reason !== "dispose" && options.anchor.isConnected && !options.anchor.closest('[hidden],[inert],[aria-hidden="true"]') && !options.anchor.matches(":disabled")) options.anchor.focus();
  };
  const close = (why: EditorCloseReason = "cancel", restoreFocus = true): void => {
    if (closed) return;
    if (!["select", "cancel", "dispose"].includes(why) || typeof restoreFocus !== "boolean") throw new ClientError("validation", "Invalid editor overlay close policy.");
    reason = why; restore = restoreFocus; inner?.close(); finish();
  };
  const abort = (): void => close("dispose", false);
  if (options.environment.signal?.aborted) {
    reason = "dispose"; finish();
    return { node: options.content, mode, close, dispose: abort };
  }
  let node: HTMLElement;
  if (touch) {
    const dialog = options.environment.messages.dialogs.open({ title: options.title, body: options.content, size: "md", onClose: finish });
    node = dialog.dialog; node.classList.add("editor-sheet"); inner = dialog;
  } else {
    const popover = options.environment.messages.popovers.open(options.anchor, options.content, { role: "dialog", restoreFocus: false, ...(options.alignTo ? { alignTo: options.alignTo } : {}), onClose: finish });
    node = popover.node; node.classList.add("editor-popover"); node.setAttribute("aria-label", options.title); inner = popover;
  }
  removed.observe(options.anchor.ownerDocument.documentElement, { subtree: true, childList: true });
  options.environment.signal?.addEventListener("abort", abort, { once: true });
  return { node, mode, close, dispose: abort };
}
