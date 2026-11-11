/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
import { button, el, errorMessage } from "./dom.js";
import { messageOptions, type ConfirmationHandle } from "./dialog-service.js";
import { FileUpload, type UploadOperation, type UploadTransport } from "./file-upload.js";
import { MessageServices } from "./messages.js";
import { ClientError } from "./transport.js";

export interface FileEditorLimits {
  readonly maxFiles: number;
  readonly maxFileBytes: number;
  readonly maxTotalBytes: number;
}
/** Trusted caller configuration. Accept is a chooser hint; transport retains server authorization. */
export interface FileEditorOptions<T> {
  readonly label: string;
  readonly accept?: string;
  readonly multiple?: boolean;
  readonly readonly?: boolean;
  readonly limits: FileEditorLimits;
  readonly beforeOpen?: () => boolean | Promise<boolean>;
  readonly prepare?: (files: readonly File[], signal: AbortSignal) => Promise<readonly File[]>;
  readonly transport: UploadTransport<T>;
  /** Awaited by select; rejection propagates without retrying transport. May start a new selection. */
  readonly onComplete: (result: T, files: readonly File[]) => void | Promise<void>;
  readonly messages: MessageServices;
  readonly report: (error: unknown) => void;
  readonly signal?: AbortSignal;
  readonly displayErrors?: boolean;
  readonly blockPreparation?: boolean;
}
export interface FileEditorHandle {
  readonly node: HTMLElement;
  /** Applies the same admission and lifecycle to native choices and caller-delegated file drops. */
  select(files: readonly File[]): Promise<void>;
  dispose(): void;
}
interface Current<T> {
  readonly presentation: object;
  readonly abort: AbortController;
  upload?: FileUpload<T>;
  operation?: UploadOperation<T>;
  confirmation?: ConfirmationHandle;
  release?: () => void;
}
const abortError = (): DOMException => new DOMException("The file operation was cancelled.", "AbortError");
function abortable<T>(promise: Promise<T>, signal: AbortSignal): Promise<T> {
  return new Promise((resolve, reject) => {
    const cancel = (): void => { reject(abortError()); };
    signal.addEventListener("abort", cancel, { once: true });
    void promise.then(resolve, reject).finally(() => signal.removeEventListener("abort", cancel));
    if (signal.aborted) cancel();
  });
}
/** Disposable client editor; no endpoint, file storage, retry or replay policy is selected here. */
export function createFileEditor<T>(options: FileEditorOptions<T>): FileEditorHandle {
  messageOptions(options, ["label", "accept", "multiple", "readonly", "limits", "beforeOpen", "prepare", "transport", "onComplete", "messages", "report", "signal", "displayErrors", "blockPreparation"], "File editor");
  const bounded = (value: unknown): value is string => typeof value === "string" && value.length <= 8192 && new TextEncoder().encode(value).length <= 8192;
  if (!bounded(options.label) || !options.label.trim()) throw new ClientError("validation", "File editor label must be nonempty text of at most 8192 UTF-8 bytes.");
  if (options.accept !== undefined && !bounded(options.accept)) throw new ClientError("validation", "File editor accept must be text of at most 8192 UTF-8 bytes.");
  for (const key of ["multiple", "readonly", "displayErrors", "blockPreparation"] as const) {
    if (options[key] !== undefined && typeof options[key] !== "boolean") throw new ClientError("validation", `File editor ${key} must be a boolean.`);
  }
  messageOptions(options.limits, ["maxFiles", "maxFileBytes", "maxTotalBytes"], "File limits");
  if (![options.limits.maxFiles, options.limits.maxFileBytes, options.limits.maxTotalBytes].every(value => Number.isSafeInteger(value) && value > 0)) throw new ClientError("validation", "File limits must be positive safe integers.");
  for (const key of ["beforeOpen", "prepare"] as const) if (options[key] !== undefined && typeof options[key] !== "function") throw new ClientError("validation", `File editor ${key} must be a function.`);
  for (const key of ["transport", "onComplete", "report"] as const) if (typeof options[key] !== "function") throw new ClientError("validation", `File editor ${key} must be a function.`);
  if (!(options.messages instanceof MessageServices)) throw new ClientError("validation", "File editor messages must be MessageServices.");
  if (options.signal !== undefined && !(options.signal instanceof AbortSignal)) throw new ClientError("validation", "File editor signal must be an AbortSignal.");

  // Caller mutation must not widen admission while an asynchronous preparation is running.
  const limits = { ...options.limits }, multiple = options.multiple ?? false;
  const node = el("section", undefined, "file-editor"); node.setAttribute("aria-label", options.label);
  const caption = el("span", options.label, "file-editor-label");
  const input = el("input", undefined, "file-editor-input"); input.type = "file"; input.hidden = true;
  input.multiple = multiple; input.accept = options.accept ?? ""; input.setAttribute("aria-label", options.label);
  const status = el("p", "No file selected", "file-editor-status"); status.setAttribute("role", "status"); status.setAttribute("aria-live", "polite");
  const names = el("p", undefined, "file-editor-names"); names.hidden = true;
  const progress = el("progress", undefined, "file-editor-progress"); progress.hidden = true; progress.setAttribute("aria-label", `${options.label} progress`);
  let current: Current<T> | undefined, dead = false;
  // The latest token retains no files/results, including while an older completion callback awaits.
  let latestPresentation: object | undefined;
  const lifetime = new AbortController();
  let opening = false;
  const report = (error: unknown): void => {
    if (!dead && !(error instanceof DOMException && error.name === "AbortError")) options.report(error);
  };
  const trigger = button("Choose file", () => { void open().catch(report); }, "file-editor-choose"); trigger.setAttribute("data-file-choose", "");
  const cancel = button("Cancel upload", () => { requestCancellation(); }, "file-editor-cancel"); cancel.setAttribute("data-file-cancel", ""); cancel.hidden = true;
  const controls = el("div", undefined, "file-editor-controls"); controls.append(trigger, cancel);
  node.append(caption, input, controls, names, progress, status);
  const renderBusy = (): void => {
    const busy = current !== undefined || opening;
    input.disabled = dead || options.readonly === true || busy; trigger.disabled = input.disabled;
    node.setAttribute("aria-busy", String(busy)); node.dataset["filePending"] = String(busy);
    cancel.hidden = current?.operation === undefined;
  };
  const release = (scope: Current<T>): void => { const stop = scope.release; scope.release = undefined; stop?.(); };
  const closeConfirmation = (scope: Current<T>): void => { scope.confirmation?.dispose(); scope.confirmation = undefined; };
  const ownsPresentation = (scope: Current<T>): boolean => !dead && latestPresentation === scope.presentation;
  const ownsFocus = (scope: Current<T>): boolean => {
    const doc = node.ownerDocument, active = doc.activeElement;
    const confirmation = scope.confirmation?.dialog;
    // Chromium can blur a focused modal button when confirmation disables its controls.
    // Body focus is ours only while this owner's connected confirmation is still the active modal.
    const blurredConfirmation = active === doc.body && confirmation?.open === true && confirmation.isConnected && options.messages.dialogs.active() === confirmation;
    return active === cancel || active !== null && confirmation?.contains(active) === true || blurredConfirmation;
  };
  const restoreTerminalFocus = (scope: Current<T>, restore: boolean): void => {
    if (restore && ownsPresentation(scope) && node.isConnected && !trigger.disabled) trigger.focus();
  };
  const validFiles = (files: readonly File[]): readonly File[] => {
    if (!Array.isArray(files)) throw new ClientError("validation", "File selection must be an array of files.");
    // Indexed validation includes holes, which Array.some would skip.
    for (let index = 0; index < files.length; index++) {
      if (!(files[index] instanceof File)) throw new ClientError("validation", "File selection must be an array of files.");
    }
    const maximum = multiple ? limits.maxFiles : 1;
    if (files.length > maximum) throw new ClientError("validation", `Choose at most ${maximum} ${maximum === 1 ? "file" : "files"}.`);
    let total = 0;
    for (const file of files) {
      if (file.size > limits.maxFileBytes) throw new ClientError("validation", `Each file must be at most ${limits.maxFileBytes} bytes.`);
      if (!Number.isSafeInteger(file.size) || file.size < 0 || total > limits.maxTotalBytes - file.size) throw new ClientError("validation", `Selected files must total at most ${limits.maxTotalBytes} bytes.`);
      total += file.size;
    }
    return Object.freeze([...files]);
  };
  const showError = (error: unknown): void => {
    if (dead) return;
    status.textContent = errorMessage(error); node.dataset["state"] = "error";
    if (options.displayErrors !== false) {
      try { options.messages.notifications.add(errorMessage(error), { level: "danger", sticky: true }); }
      catch (presentationError) { report(presentationError); }
    }
  };
  function requestCancellation(): void {
    const scope = current;
    if (dead || !scope?.operation || scope.confirmation) return;
    try {
      scope.confirmation = options.messages.dialogs.confirm({ title: "Cancel file operation", message: "Cancel this file operation? A remote change may still need to be checked.", confirmLabel: "Cancel upload", cancelLabel: "Keep uploading",
        confirm: () => { if (!dead && current === scope) scope.abort.abort(); },
      });
      void scope.confirmation.result.then(() => { scope.confirmation = undefined; }, report);
    } catch (error) { report(error); }
  }
  async function open(): Promise<void> {
    if (dead || current || opening || options.readonly) return;
    opening = true; renderBusy();
    try {
      // Preserve synchronous native activation when no asynchronous guard is configured.
      const permitted = options.beforeOpen === undefined ? true : await options.beforeOpen();
      if (typeof permitted !== "boolean") throw new ClientError("validation", "File editor beforeOpen must return a boolean.");
      if (!dead && permitted) { opening = false; renderBusy(); input.click(); }
    } catch (error) { showError(error); throw error; }
    finally { opening = false; renderBusy(); }
  }
  async function select(files: readonly File[]): Promise<void> {
    if (dead) throw new ClientError("validation", "File editor is disposed.");
    if (options.readonly) throw new ClientError("validation", "File editor is readonly.");
    if (current || opening) throw new ClientError("validation", "A file operation is already active.");
    const scope: Current<T> = { abort: new AbortController(), presentation: {} };
    latestPresentation = scope.presentation; current = scope; renderBusy();
    try {
      let selected = validFiles(files);
      if (!selected.length) return;
      // A constructed File can carry a far larger name than an ordinary filesystem permits.
      let shownNames = "";
      for (const file of selected) {
        const remaining = 8192 - shownNames.length;
        if (remaining <= 0) break;
        const separator = shownNames ? ", " : "";
        shownNames += separator.slice(0, remaining) + file.name.slice(0, Math.max(0, remaining - separator.length));
      }
      names.textContent = shownNames; names.hidden = false;
      progress.hidden = true; progress.removeAttribute("value"); node.dataset["state"] = "preparing"; status.textContent = "Preparing files…";
      if (options.prepare) {
        if (options.blockPreparation) scope.release = options.messages.block.begin({ message: "Preparing files…" });
        try { selected = validFiles(await abortable(Promise.resolve().then(() => {
          if (dead || scope.abort.signal.aborted) throw abortError();
          return options.prepare!(selected, scope.abort.signal);
        }), scope.abort.signal)); }
        finally { release(scope); }
      }
      if (dead || scope.abort.signal.aborted) throw abortError();
      if (!selected.length) throw new ClientError("validation", "Preparation must return at least one file.");
      scope.upload = new FileUpload<T>({ transport: options.transport, signal: scope.abort.signal, report,
        onEvent: event => {
          if (dead || current !== scope) return;
          if (event.type === "added") { progress.hidden = false; progress.removeAttribute("value"); status.textContent = "Waiting for progress…"; node.dataset["state"] = "pending"; }
          if (event.type === "progress") {
            const { loaded, total } = event.record;
            if (total === undefined || total === 0) { progress.removeAttribute("value"); status.textContent = `${loaded} bytes`; }
            else { progress.max = total; progress.value = loaded; status.textContent = `${loaded} of ${total} bytes`; }
            node.dataset["state"] = "loading";
          }
        },
      });
      scope.operation = scope.upload.start(selected); renderBusy();
      const result = await scope.operation.done;
      if (dead || scope.abort.signal.aborted) throw abortError();
      const returnFocus = ownsFocus(scope);
      closeConfirmation(scope); current = undefined; input.value = ""; progress.hidden = true;
      status.textContent = "Completed"; node.dataset["state"] = "completed"; renderBusy();
      restoreTerminalFocus(scope, returnFocus);
      const completion = options.onComplete(result, selected);
      // Synchronous callbacks may dispose their owner after receiving successful completion.
      // Only a returned promise needs a lifetime race; its late rejection remains consumed.
      if (completion !== undefined) await abortable(Promise.resolve(completion), lifetime.signal);
    } catch (error) {
      if (ownsPresentation(scope)) {
        if (error instanceof DOMException && error.name === "AbortError") { status.textContent = "Cancelled"; node.dataset["state"] = "aborted"; }
        else showError(error);
      }
      throw error;
    } finally {
      const returnFocus = ownsPresentation(scope) && ownsFocus(scope);
      release(scope); closeConfirmation(scope); scope.upload?.dispose();
      // Completion callbacks may synchronously start the next operation in this editor.
      if (current === scope) current = undefined;
      if (ownsPresentation(scope)) { input.value = ""; progress.hidden = true; renderBusy(); restoreTerminalFocus(scope, returnFocus); }
    }
  }
  const change = (): void => {
    const selected = Array.from(input.files ?? []);
    // Native inputs suppress change when the same path is chosen again. Keep our stable snapshot,
    // then clear every selection before preparation, failure or cancellation can leave it stuck.
    input.value = "";
    void select(selected).catch(report);
  };
  input.addEventListener("change", change);
  const dispose = (): void => {
    if (dead) return; dead = true; opening = false; latestPresentation = undefined; lifetime.abort();
    const scope = current;
    if (scope) { scope.abort.abort(); release(scope); closeConfirmation(scope); scope.upload?.dispose(); }
    current = undefined; input.value = ""; names.textContent = ""; progress.hidden = true;
    input.removeEventListener("change", change); options.signal?.removeEventListener("abort", dispose); renderBusy();
  };
  options.signal?.addEventListener("abort", dispose, { once: true });
  if (options.signal?.aborted) dispose(); else renderBusy();
  return { node, select, dispose };
}
