/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
import { messageOptions } from "./dialog-service.js";
import { el } from "./dom.js";
import { ClientError } from "./transport.js";

export interface DropzoneOptions {
  readonly target: HTMLElement;
  readonly onDrop: (event: DragEvent) => void;
  readonly signal?: AbortSignal;
  readonly disabled?: () => boolean;
}
export interface DropzoneHandle {
  readonly node: HTMLElement;
  /** Re-read the target rectangle after a caller-owned layout patch. */
  refresh(): void;
  dispose(): void;
}

/** Visual target alignment and delegated file drops only; the owner supplies a native chooser and preflight. */
export function createDropzone(options: DropzoneOptions): DropzoneHandle {
  messageOptions(options, ["target", "onDrop", "signal", "disabled"], "Dropzone");
  if (!(options.target instanceof HTMLElement)) throw new ClientError("validation", "Dropzone target must be an HTMLElement.");
  if (typeof options.onDrop !== "function") throw new ClientError("validation", "Dropzone onDrop must be a function.");
  if (options.signal !== undefined && !(options.signal instanceof AbortSignal)) throw new ClientError("validation", "Dropzone signal must be an AbortSignal.");
  if (options.disabled !== undefined && typeof options.disabled !== "function") throw new ClientError("validation", "Dropzone disabled must be a function.");

  const { target, signal } = options;
  const node = el("div", "Release to choose files", "file-dropzone");
  // The host's labelled chooser is the accessible alternative. This decorative
  // overlay adds no second focus target or unsupported keyboard drop promise.
  node.setAttribute("aria-hidden", "true");
  node.style.position = "fixed";
  let dead = false, depth = 0;
  const setDragging = (inside: boolean): void => {
    node.dataset["dragging"] = String(inside);
    node.style.pointerEvents = inside ? "auto" : "none";
  };
  const reset = (): void => { depth = 0; setDragging(false); };
  reset();
  const refresh = (): void => {
    if (dead) return;
    const rectangle = target.getBoundingClientRect();
    node.style.left = `${rectangle.left}px`; node.style.top = `${rectangle.top}px`;
    node.style.width = `${rectangle.width}px`; node.style.height = `${rectangle.height}px`;
  };
  const seen = new WeakSet<Event>();
  const accept = (event: DragEvent): boolean => {
    // If an owner places the overlay within its target, its bubbling event still
    // belongs to one drop interaction rather than two independent callbacks.
    if (dead || seen.has(event)) return false;
    seen.add(event);
    if (options.disabled?.()) { reset(); return false; }
    const data = event.dataTransfer;
    return data !== null && data !== undefined && (Array.from(data.types).includes("Files") || data.files.length > 0);
  };
  const enter = (event: DragEvent): void => {
    if (!accept(event)) return;
    event.preventDefault(); depth++; refresh(); setDragging(true);
  };
  const over = (event: DragEvent): void => {
    if (!accept(event)) return;
    event.preventDefault(); refresh(); setDragging(true);
  };
  const leave = (event: DragEvent): void => {
    if (!accept(event)) return;
    depth = Math.max(0, depth - 1); if (depth === 0) reset();
  };
  const drop = (event: DragEvent): void => {
    if (!accept(event)) return;
    event.preventDefault(); reset(); options.onDrop(event);
  };
  const view = target.ownerDocument.defaultView;
  const document = target.ownerDocument;
  const observer = typeof ResizeObserver === "undefined" ? undefined : new ResizeObserver(refresh);
  const dispose = (): void => {
    if (dead) return; dead = true; reset(); node.remove(); observer?.disconnect();
    for (const surface of [target, node]) {
      surface.removeEventListener("dragenter", enter); surface.removeEventListener("dragover", over);
      surface.removeEventListener("dragleave", leave); surface.removeEventListener("drop", drop);
    }
    view?.removeEventListener("resize", refresh); view?.removeEventListener("scroll", refresh, true);
    document.removeEventListener("dragend", reset); document.removeEventListener("drop", reset);
    signal?.removeEventListener("abort", dispose);
  };
  if (signal?.aborted) { dispose(); return { node, refresh, dispose }; }
  for (const surface of [target, node]) {
    surface.addEventListener("dragenter", enter); surface.addEventListener("dragover", over);
    surface.addEventListener("dragleave", leave); surface.addEventListener("drop", drop);
  }
  view?.addEventListener("resize", refresh); view?.addEventListener("scroll", refresh, true);
  document.addEventListener("dragend", reset); document.addEventListener("drop", reset);
  signal?.addEventListener("abort", dispose, { once: true }); observer?.observe(target); refresh();
  return { node, refresh, dispose };
}
