/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
import { createFocusTrap, createHotkeyController } from "@erpforall/elang";
import { button, el } from "./dom.js";
import { ClientError } from "./transport.js";

export type DialogCallback = () => void | boolean | Promise<void | boolean>;
export interface DialogHandle { readonly dialog: HTMLDialogElement; close(): void; dispose(): void }
export interface DialogOptions {
  readonly title?: string; readonly body: HTMLElement | string;
  readonly size?: "sm" | "md" | "lg" | "xl"; readonly header?: boolean; readonly footer?: HTMLElement;
  readonly dismiss?: DialogCallback; readonly signal?: AbortSignal;
  readonly onClose?: () => void;
}
export interface AttachDialogOptions {
  readonly dismiss?: DialogCallback; readonly signal?: AbortSignal; readonly onClose?: () => void;
}
export type ConfirmationDecision = "confirm" | "cancel" | "dismiss";
export interface ConfirmationHandle extends DialogHandle { readonly result: Promise<ConfirmationDecision> }
export interface ConfirmationOptions {
  readonly title?: string; readonly message: string; readonly confirm?: DialogCallback;
  readonly cancel?: DialogCallback; readonly dismiss?: DialogCallback;
  readonly confirmLabel?: string; readonly cancelLabel?: string; readonly contentClass?: string;
  /** A destructive confirmation (delete): the confirm action uses the danger style, not the accent fill. */
  readonly destructive?: boolean;
  readonly signal?: AbortSignal;
}
interface Entry { readonly handle: DialogHandle; readonly finish: () => void }
let nextHeading = 0;
const textBytes = (text: string): number => new TextEncoder().encode(text).length;
function boundedText(value: unknown, name: string): asserts value is string {
  if (typeof value !== "string" || textBytes(value) > 8192) throw new ClientError("validation", `Dialog ${name} must be text of at most 8192 bytes.`);
}
/** Shared admission for trusted-code message declarations; classes and arrays are not option records. */
export function plainMessageObject(value: unknown): boolean {
  return value !== null && typeof value === "object" && (Object.getPrototypeOf(value) === Object.prototype || Object.getPrototypeOf(value) === null);
}
export function messageOptions(value: unknown, keys: readonly string[], label: string): void {
  if (!plainMessageObject(value)) throw new ClientError("validation", `${label} options must be a plain object.`);
  for (const key of Reflect.ownKeys(value as object)) if (typeof key !== "string" || !keys.includes(key)) throw new ClientError("validation", `Unknown ${label.toLowerCase()} option: ${String(key)}.`);
}
export function messageSignal(value: unknown): void {
  if (value !== undefined && !(value instanceof AbortSignal)) throw new ClientError("validation", "Dialog signal must be an AbortSignal.");
}
function callbacks(options: object, keys: readonly string[]): void {
  const record = options as Record<string, unknown>;
  for (const key of keys) if (record[key] !== undefined && typeof record[key] !== "function") throw new ClientError("validation", `Dialog ${key} must be a function.`);
}

/** Browser-local modal scopes. Native dialog owns background inertness; UI5 owns nested focus. */
export class DialogService {
  private readonly stack: Entry[] = [];
  private disposed = false;
  constructor(private readonly report: (error: unknown) => void, private readonly onActive?: (dialog: HTMLDialogElement | undefined) => void) {}
  active(): HTMLDialogElement | undefined { return this.stack.at(-1)?.handle.dialog; }
  private publishActive(): void { try { this.onActive?.(this.disposed ? undefined : this.active()); } catch (error) { this.report(error); } }
  open(options: DialogOptions): DialogHandle {
    messageOptions(options, ["title", "body", "size", "header", "footer", "dismiss", "signal", "onClose"], "Dialog");
    callbacks(options, ["dismiss", "onClose"]); messageSignal(options.signal);
    if (options.header !== undefined && typeof options.header !== "boolean") throw new ClientError("validation", "Dialog header must be a boolean.");
    if (options.footer !== undefined && !(options.footer instanceof HTMLElement)) throw new ClientError("validation", "Dialog footer must be a trusted element.");
    if (options.title !== undefined) boundedText(options.title, "title");
    if (typeof options.body === "string") boundedText(options.body, "body");
    else if (!(options.body instanceof HTMLElement)) throw new ClientError("validation", "Dialog body must be text or a trusted element.");
    const size = options.size === undefined ? "md" : options.size;
    if (!["sm", "md", "lg", "xl"].includes(size)) throw new ClientError("validation", "Unknown dialog size.");
    const dialog = el("dialog", undefined, "choice-dialog message-dialog");
    dialog.dataset["size"] = size;
    const header = el("header", undefined, "message-dialog-header");
    const heading = el("h2", options.title ?? "Details"); heading.id = `message-dialog-heading-${++nextHeading}`;
    dialog.setAttribute("aria-labelledby", heading.id);
    const close = button("×", () => dismiss(), "message-dialog-close"); close.setAttribute("aria-label", "Close");
    header.append(heading, close);
    const body = el("div", undefined, "message-dialog-body"); body.tabIndex = 0; body.append(options.body);
    if (options.header !== false) dialog.append(header);
    else { dialog.removeAttribute("aria-labelledby"); dialog.setAttribute("aria-label", options.title ?? "Details"); }
    dialog.append(body);
    if (options.footer) { const footer = el("footer", undefined, "message-dialog-footer"); footer.append(options.footer); dialog.append(footer); }
    let handle: DialogHandle;
    let dismissPending = false;
    const dismiss = (): void => {
      if (dismissPending || !dialog.isConnected) return;
      dismissPending = true;
      void (async () => {
        try { if (await options.dismiss?.() !== false) handle.close(); }
        catch (error) { handle.dispose(); this.report(error); }
        finally { dismissPending = false; }
      })();
    };
    handle = this.attach(dialog, { dismiss: () => { dismiss(); return false; }, ...(options.signal ? { signal: options.signal } : {}), ...(options.onClose ? { onClose: options.onClose } : {}) });
    return handle;
  }
  /** Adopt a legacy shell. Call after constructing its content; the returned handle owns removal. */
  attach(dialog: HTMLDialogElement, options: AttachDialogOptions = {}): DialogHandle {
    messageOptions(options, ["dismiss", "signal", "onClose"], "Attached dialog");
    callbacks(options, ["dismiss", "onClose"]); messageSignal(options.signal);
    if (!(dialog instanceof HTMLDialogElement)) throw new ClientError("validation", "Attached dialog must be a native dialog element.");
    if (this.disposed) throw new ClientError("validation", "The dialog service is disposed.");
    if (this.stack.length >= 8) throw new ClientError("validation", "At most 8 dialogs may be open.");
    if (this.stack.some(entry => entry.handle.dialog === dialog)) throw new ClientError("validation", "The dialog is already attached.");
    const priorParent = this.active();
    const opener = document.activeElement instanceof HTMLElement ? document.activeElement : undefined;
    if (priorParent) { priorParent.inert = true; priorParent.setAttribute("inert", ""); priorParent.setAttribute("aria-hidden", "true"); }
    if (!dialog.isConnected) document.body.append(dialog);
    dialog.dataset["messageDialog"] = "";
    let dead = false, dismissPending = false;
    let trap: ReturnType<typeof createFocusTrap> | undefined;
    let hotkeys: ReturnType<typeof createHotkeyController> | undefined;
    const finish = (): void => {
      if (dead) return;
      dead = true;
      const index = this.stack.indexOf(entry);
      for (const child of this.stack.slice(index + 1).reverse()) child.finish();
      if (index >= 0) this.stack.splice(index, 1);
      const active = this.active();
      if (active) { active.inert = false; active.removeAttribute("inert"); active.removeAttribute("aria-hidden"); }
      options.signal?.removeEventListener("abort", finish);
      dialog.removeEventListener("cancel", cancel); dialog.removeEventListener("close", finish); hotkeys?.dispose();
      if (dialog.open) dialog.close();
      dialog.remove(); trap?.dispose(); this.publishActive();
      // Focus returns to the opener (the native dialog does it too; this covers a removed-before-closed shell).
      if (!this.active() && opener?.isConnected) opener.focus();
      document.body.classList.toggle("message-modal-open", document.querySelector("dialog[data-message-dialog]") !== null);
      try { options.onClose?.(); } catch (error) { this.report(error); }
    };
    const dismiss = (): void => {
      if (dead || dismissPending || this.active() !== dialog) return;
      dismissPending = true;
      void (async () => {
        try { if (await options.dismiss?.() !== false) finish(); }
        catch (error) { finish(); this.report(error); }
        finally { dismissPending = false; }
      })();
    };
    const cancel = (event: Event): void => { event.preventDefault(); dismiss(); };
    const handle: DialogHandle = { dialog, close: finish, dispose: finish };
    const entry: Entry = { handle, finish }; this.stack.push(entry);
    dialog.addEventListener("cancel", cancel); dialog.addEventListener("close", finish);
    options.signal?.addEventListener("abort", finish, { once: true });
    try {
      if (!dialog.open) dialog.showModal();
      document.body.classList.add("message-modal-open");
      // A body control marked `autofocus` (the record picker's search box) takes the first focus instead of the
      // header's close button, the trap's default first candidate.
      const preferred = dialog.querySelector<HTMLElement>("[autofocus]:not([disabled])") ?? dialog.querySelector<HTMLElement>(".message-dialog-footer [data-default-action]:not([disabled])");
      trap = createFocusTrap(dialog, { onEscape: dismiss, ...(preferred ? { initialFocus: preferred } : {}) });
      hotkeys = createHotkeyController(dialog, { report: this.report, bindings: () => {
        const usable = [...dialog.querySelectorAll<HTMLButtonElement>(".message-dialog-footer button")].filter(control => !control.hidden && !control.closest("[hidden],[inert]") && getComputedStyle(control).display !== "none");
        const first = usable.find(control => control.hasAttribute("data-default-action")) ?? usable[0];
        return first ? [{kind:"dom",chord:"ctrl+enter",target:first,allowEditable:true}] : [];
      } });
      this.publishActive();
      if (options.signal?.aborted) finish();
    } catch (error) { finish(); throw error; }
    return handle;
  }
  confirm(options: ConfirmationOptions): ConfirmationHandle { return this.confirmation(options, false); }
  alert(options: ConfirmationOptions): ConfirmationHandle { return this.confirmation(options, true); }
  private confirmation(options: ConfirmationOptions, alert: boolean): ConfirmationHandle {
    messageOptions(options, ["title", "message", "confirm", "cancel", "dismiss", "confirmLabel", "cancelLabel", "contentClass", "destructive", "signal"], "Confirmation");
    callbacks(options, ["confirm", "cancel", "dismiss"]); messageSignal(options.signal);
    for (const key of ["title", "confirmLabel", "cancelLabel"] as const) if (options[key] !== undefined) boundedText(options[key], key);
    boundedText(options.message, "message");
    if (options.destructive !== undefined && typeof options.destructive !== "boolean") throw new ClientError("validation", "Dialog destructive must be a boolean.");
    if (options.contentClass !== undefined && (typeof options.contentClass !== "string" || !/^[a-zA-Z0-9_ -]{1,256}$/.test(options.contentClass))) throw new ClientError("validation", "Invalid dialog content class.");
    const body = el("p", options.message);
    const footer = el("div", undefined, "actions");
    let pending = false, settled = false;
    let resolve!: (value: ConfirmationDecision) => void, reject!: (error: unknown) => void;
    const result = new Promise<ConfirmationDecision>((yes, no) => { resolve = yes; reject = no; });
    // Event handlers cannot return a rejection to the browser. Preserve the rejecting result for callers and report it once.
    void result.catch(this.report);
    let handle: DialogHandle;
    const finish = (decision: ConfirmationDecision): void => { if (!settled) { settled = true; resolve(decision); } handle.close(); };
    const run = async (decision: ConfirmationDecision, callback: DialogCallback | undefined): Promise<false> => {
      if (pending || settled) return false;
      pending = true;
      const controls = [...handle.dialog.querySelectorAll<HTMLButtonElement>("button")];
      const previous = controls.map(control => control.disabled); controls.forEach(control => { control.disabled = true; });
      try { if (await callback?.() !== false) finish(decision); }
      catch (error) { settled = true; reject(error); handle.dispose(); }
      finally { pending = false; controls.forEach((control, index) => { control.disabled = previous[index]!; }); }
      return false;
    };
    // The primary (or destructive) action is last, right-aligned; Ctrl+Enter and the initial focus go to it.
    if (!alert || options.cancel) footer.append(button(options.cancelLabel ?? "Cancel", () => { void run("cancel", options.cancel); }));
    const confirmButton = button(options.confirmLabel ?? "OK", () => { void run("confirm", options.confirm); }, options.destructive ? "danger" : "primary");
    confirmButton.dataset["defaultAction"] = "";
    footer.append(confirmButton);
    handle = this.open({ title: options.title ?? (alert ? "Notice" : "Confirm"), body, size: "sm", footer,
      dismiss: () => run("dismiss", options.dismiss ?? options.cancel),
      onClose: () => { if (!settled) { settled = true; resolve("dismiss"); } },
      ...(options.signal ? { signal: options.signal } : {}) });
    if (options.contentClass) handle.dialog.querySelector(".message-dialog-body")?.classList.add(...options.contentClass.split(/\s+/).filter(Boolean));
    return { ...handle, result };
  }
  dispose(): void {
    if (this.disposed) return;
    this.disposed = true;
    for (const entry of [...this.stack].reverse()) entry.finish();
  }
}
