/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
import { afterEach, beforeEach, expect, test, vi } from "vitest";
import { createFileEditor, type FileEditorOptions } from "../src/client/file-editor.js";
import { MessageServices } from "../src/client/messages.js";
import { ClientError } from "../src/client/transport.js";
import { FileUpload } from "../src/client/file-upload.js";

const originalShow = Object.getOwnPropertyDescriptor(HTMLDialogElement.prototype, "showModal");
const originalClose = Object.getOwnPropertyDescriptor(HTMLDialogElement.prototype, "close");
const cleanups: (() => void)[] = [];
beforeEach(() => {
  Object.defineProperty(HTMLDialogElement.prototype, "showModal", { configurable: true, value(this: HTMLDialogElement) { this.open = true; } });
  Object.defineProperty(HTMLDialogElement.prototype, "close", { configurable: true, value(this: HTMLDialogElement) { this.open = false; } });
});
afterEach(() => {
  cleanups.splice(0).reverse().forEach(dispose => dispose());
  vi.restoreAllMocks(); document.body.replaceChildren();
  if (originalShow) Object.defineProperty(HTMLDialogElement.prototype, "showModal", originalShow); else Reflect.deleteProperty(HTMLDialogElement.prototype, "showModal");
  if (originalClose) Object.defineProperty(HTMLDialogElement.prototype, "close", originalClose); else Reflect.deleteProperty(HTMLDialogElement.prototype, "close");
});
const file = (name = "report.txt", content = "abc"): File => new File([content], name, { type: "text/plain" });
function deferred<T>() {
  let resolve!: (value: T) => void, reject!: (error: unknown) => void;
  const promise = new Promise<T>((yes, no) => { resolve = yes; reject = no; });
  return { promise, resolve, reject };
}
function setup(patch: Partial<FileEditorOptions<string>> = {}) {
  const root = document.createElement("div"); document.body.append(root);
  const report = vi.fn(), onComplete = vi.fn(), transport = vi.fn(async () => "prepared");
  const messages = new MessageServices(root, report); cleanups.push(() => messages.dispose());
  const editor = createFileEditor<string>({ label: "Supporting file", limits: { maxFiles: 2, maxFileBytes: 4, maxTotalBytes: 6 }, messages, report, transport, onComplete, ...patch });
  root.append(editor.node); cleanups.push(editor.dispose);
  const input = editor.node.querySelector<HTMLInputElement>("input[type='file']")!;
  const trigger = editor.node.querySelector<HTMLButtonElement>("[data-file-choose]")!;
  const cancel = (): HTMLButtonElement => editor.node.querySelector<HTMLButtonElement>("[data-file-cancel]")!;
  return { editor, root, input, trigger, cancel, report, messages, onComplete, transport };
}
const dialogClick = (label: string): void => {
  const control = [...document.querySelectorAll<HTMLButtonElement>("dialog button")].find(button => button.textContent === label);
  expect(control).toBeDefined(); control!.click();
};

test("file chooser acceptance combines guarded opening, preflight, preparation, repeat choice and owner lifetime", async () => {
  const uploaded = deferred<string>(), lateUpload = deferred<string>();
  const beforeOpen = vi.fn().mockResolvedValueOnce(false).mockResolvedValue(true);
  const prepare = vi.fn().mockRejectedValueOnce(new ClientError("validation", "Preparation refused.")).mockImplementation(async (files: readonly File[]) => files);
  const transport = vi.fn().mockImplementationOnce(() => uploaded.promise).mockResolvedValueOnce("again").mockImplementationOnce(() => lateUpload.promise);
  const { editor, input, trigger, onComplete } = setup({ multiple: true, accept: ".txt,text/plain", beforeOpen, prepare, transport });
  const nativeOpen = vi.spyOn(input, "click").mockImplementation(() => {});
  expect(input.multiple).toBe(true); expect(input.accept).toBe(".txt,text/plain");
  trigger.click(); await vi.waitFor(() => expect(beforeOpen).toHaveBeenCalledTimes(1));
  await vi.waitFor(() => expect(trigger.disabled).toBe(false)); expect(nativeOpen).not.toHaveBeenCalled();
  trigger.click(); await vi.waitFor(() => expect(nativeOpen).toHaveBeenCalledTimes(1));
  const selected = file();
  await expect(editor.select([selected])).rejects.toMatchObject({ kind: "validation", message: "Preparation refused." });
  expect(trigger.disabled).toBe(false); expect(input.disabled).toBe(false); expect(transport).not.toHaveBeenCalled();
  await expect(editor.select([file("oversize", "12345")])).rejects.toMatchObject({ kind: "validation", message: "Each file must be at most 4 bytes." });
  expect(prepare).toHaveBeenCalledTimes(1); expect(transport).not.toHaveBeenCalled();
  const first = editor.select([selected]); await vi.waitFor(() => expect(transport).toHaveBeenCalledTimes(1));
  expect(input.disabled).toBe(true); expect(trigger.disabled).toBe(true);
  uploaded.resolve("done"); await first;
  expect(onComplete).toHaveBeenCalledWith("done", [selected]); expect(input.value).toBe(""); expect(trigger.disabled).toBe(false);
  await editor.select([selected]); expect(onComplete).toHaveBeenCalledWith("again", [selected]); expect(onComplete).toHaveBeenCalledTimes(2);
  const last = editor.select([selected]); const aborted = expect(last).rejects.toMatchObject({ name: "AbortError", message: "File upload was cancelled." });
  await vi.waitFor(() => expect(transport).toHaveBeenCalledTimes(3)); editor.dispose(); await aborted;
  lateUpload.resolve("late"); await Promise.resolve(); await Promise.resolve();
  expect(onComplete).toHaveBeenCalledTimes(2);
});

test("upload failure acceptance preserves typed outcomes, optional sticky feedback, confirmation and no replay", async () => {
  // The trusted transport owns HTTP/RPC decoding; these are its normalized known failures.
  const failures = [new ClientError("network", "Connection interrupted."), new ClientError("server", "Upload HTTP response was not successful.", 503), new ClientError("validation", "Upload RPC response was refused.", 422)];
  for (const failure of failures) {
    const events: string[] = [], controllerTransport = vi.fn(async () => { throw failure; });
    const controller = new FileUpload<string>({ transport: controllerTransport, report: () => {}, onEvent: event => events.push(`${event.type}:${event.record.state}`) });
    cleanups.push(() => controller.dispose());
    const operation = controller.start([file()]); expect(controller.active).toHaveLength(1);
    await expect(operation.done).rejects.toMatchObject({ kind: failure.kind, message: failure.message });
    expect(controller.active).toHaveLength(0); expect(operation.record.state).toBe("error");
    expect(events).toEqual(["added:pending", "failed:error"]); expect(controllerTransport).toHaveBeenCalledTimes(1);
    for (const displayErrors of [true, false]) {
      const transport = vi.fn(async () => { throw failure; });
      const { editor, root, onComplete, messages } = setup({ transport, displayErrors });
      const notice = vi.spyOn(messages.notifications, "add");
      await expect(editor.select([file()])).rejects.toMatchObject({ kind: failure.kind, message: failure.message });
      expect(onComplete).not.toHaveBeenCalled(); expect(transport).toHaveBeenCalledTimes(1);
      if (displayErrors) {
        expect(notice).toHaveBeenCalledWith(failure.message, { level: "danger", sticky: true });
        expect(root.querySelector(".client-notification[data-level='danger']")?.textContent).toContain(failure.message);
      } else { expect(notice).not.toHaveBeenCalled(); expect(root.querySelector(".client-notification")).toBeNull(); }
    }
  }
  const pending = deferred<string>(); let signal!: AbortSignal;
  const transport = vi.fn((_files: readonly File[], context: { signal: AbortSignal }) => { signal = context.signal; return pending.promise; });
  const { editor, cancel, onComplete } = setup({ transport });
  const operation = editor.select([file()]); const aborted = expect(operation).rejects.toMatchObject({ name: "AbortError", message: "File upload was cancelled." });
  await vi.waitFor(() => expect(signal).toBeDefined()); cancel().click();
  expect(document.querySelector("dialog")?.textContent).toContain("Cancel file operation");
  dialogClick("Keep uploading"); await vi.waitFor(() => expect(document.querySelector("dialog")).toBeNull());
  expect(signal.aborted).toBe(false); expect(editor.node.getAttribute("aria-busy")).toBe("true");
  cancel().click(); dialogClick("Cancel upload"); await aborted;
  expect(signal.aborted).toBe(true); expect(editor.node.querySelector("[role='status']")?.textContent).toBe("Cancelled");
  expect(editor.node.getAttribute("aria-busy")).toBe("false"); expect(onComplete).not.toHaveBeenCalled();
  pending.resolve("late"); await Promise.resolve(); await Promise.resolve();
  expect(onComplete).not.toHaveBeenCalled(); expect(transport).toHaveBeenCalledTimes(1);
  const abortedEvents: string[] = [];
  const abortedController = new FileUpload<string>({ transport: async () => "unused", report: () => {}, onEvent: event => abortedEvents.push(`${event.type}:${event.record.state}`) });
  cleanups.push(() => abortedController.dispose());
  const explicitAbort = abortedController.start([file()]); const outcome = expect(explicitAbort.done).rejects.toMatchObject({ name: "AbortError", message: "File upload was cancelled." });
  explicitAbort.abort(); await outcome;
  expect(abortedController.active).toHaveLength(0); expect(explicitAbort.record.state).toBe("aborted");
  expect(abortedEvents).toEqual(["added:pending", "failed:aborted"]);
});

/// (partial: chooser preflight, lifecycle and callback assertions; browser native chooser proof separate)
test("chooser forwards native hints, refuses before-open and never opens after owner disposal", async () => {
  const pending = deferred<boolean>();
  const beforeOpen = vi.fn().mockResolvedValueOnce(false).mockImplementationOnce(() => pending.promise);
  const { input, trigger, editor } = setup({ multiple: true, accept: ".csv,text/plain", beforeOpen });
  const nativeOpen = vi.spyOn(input, "click").mockImplementation(() => {});
  expect(input.multiple).toBe(true); expect(input.accept).toBe(".csv,text/plain");
  expect(input.getAttribute("aria-label")).toBe("Supporting file");
  trigger.click(); await vi.waitFor(() => expect(beforeOpen).toHaveBeenCalledTimes(1));
  expect(nativeOpen).not.toHaveBeenCalled();
  trigger.click(); await vi.waitFor(() => expect(beforeOpen).toHaveBeenCalledTimes(2));
  editor.dispose(); pending.resolve(true); await Promise.resolve(); await Promise.resolve();
  expect(nativeOpen).not.toHaveBeenCalled();
});

test("before-open rejection is reported and the trigger recovers", async () => {
  const error = new ClientError("permission", "Choosing files is not allowed.");
  const { trigger, input, report } = setup({ beforeOpen: async () => { throw error; } });
  const nativeOpen = vi.spyOn(input, "click").mockImplementation(() => {});
  trigger.click(); await vi.waitFor(() => expect(report).toHaveBeenCalledWith(error));
  expect(trigger.disabled).toBe(false); expect(nativeOpen).not.toHaveBeenCalled();
});

test("empty selection does no work and accepted-file hints do not become content authorization", async () => {
  const { editor, transport, onComplete } = setup({ accept: "image/png" });
  await editor.select([]); expect(transport).not.toHaveBeenCalled();
  const selected = file(); await editor.select([selected]);
  expect(onComplete).toHaveBeenCalledWith("prepared", [selected]);
});

test.each<[Partial<FileEditorOptions<string>>, File[], string]>([
  [{}, [file(), file("second")], "Choose at most 1 file."],
  [{ multiple: true }, [file(), file("second"), file("third")], "Choose at most 2 files."],
  [{ multiple: true }, [file(), file("large", "12345")], "Each file must be at most 4 bytes."],
  [{ multiple: true }, [file("first", "1234"), file("second", "123")], "Selected files must total at most 6 bytes."],
])("preflight rejects every-file/count/aggregate violations before preparation: %s", async (patch, selected, message) => {
  const prepare = vi.fn(async (files: readonly File[]) => files);
  const { editor, transport, trigger } = setup({ prepare, ...patch });
  await expect(editor.select(selected)).rejects.toMatchObject({ kind: "validation", message });
  expect(prepare).not.toHaveBeenCalled(); expect(transport).not.toHaveBeenCalled(); expect(trigger.disabled).toBe(false);
});

test("preprocessing output is revalidated and a rejected preparation restores controls", async () => {
  const prepare = vi.fn().mockResolvedValueOnce([file("grown", "12345")]).mockRejectedValueOnce(new ClientError("validation", "Preparation refused."));
  const { editor, trigger, transport } = setup({ prepare });
  await expect(editor.select([file()])).rejects.toMatchObject({ kind: "validation", message: "Each file must be at most 4 bytes." });
  expect(trigger.disabled).toBe(false);
  await expect(editor.select([file()])).rejects.toMatchObject({ kind: "validation", message: "Preparation refused." });
  expect(trigger.disabled).toBe(false); expect(transport).not.toHaveBeenCalled();
});

test("sparse selections fail typed validation before file sizes are inspected", async () => {
  const { editor, transport } = setup();
  await expect(editor.select(new Array<File>(1))).rejects.toMatchObject({ kind: "validation", message: "File selection must be an array of files." });
  expect(transport).not.toHaveBeenCalled();
});

test("preparation and upload disable duplicate submission and success permits repeat choice", async () => {
  const prepared = deferred<readonly File[]>(), uploaded = deferred<string>();
  const transport = vi.fn(() => uploaded.promise), prepare = vi.fn(() => prepared.promise);
  const { editor, input, trigger, onComplete } = setup({ prepare, transport });
  const selected = file(), first = editor.select([selected]);
  expect(input.disabled).toBe(true); expect(trigger.disabled).toBe(true);
  await expect(editor.select([file("duplicate")])).rejects.toMatchObject({ kind: "validation", message: "A file operation is already active." });
  prepared.resolve([selected]); await vi.waitFor(() => expect(transport).toHaveBeenCalledTimes(1));
  expect(input.disabled).toBe(true); uploaded.resolve("done"); await first;
  expect(onComplete).toHaveBeenCalledWith("done", [selected]); expect(input.value).toBe(""); expect(trigger.disabled).toBe(false);
  await editor.select([selected]); expect(transport).toHaveBeenCalledTimes(2); expect(onComplete).toHaveBeenCalledTimes(2);
});

test("native change takes a stable selection snapshot and clears successful input for the same file again", async () => {
  const { input, onComplete, transport } = setup();
  const selected = file();
  Object.defineProperty(input, "files", { configurable: true, value: [selected] });
  input.dispatchEvent(new Event("change"));
  await vi.waitFor(() => expect(onComplete).toHaveBeenCalledTimes(1));
  expect(input.value).toBe("");
  input.dispatchEvent(new Event("change"));
  await vi.waitFor(() => expect(onComplete).toHaveBeenCalledTimes(2));
  expect(transport).toHaveBeenCalledTimes(2);
});

test.each(["preparation", "transport"] as const)("native selection resets after %s failure and the same file may be chosen again", async phase => {
  const failure = new ClientError("validation", "The selected file was refused.");
  const prepare = phase === "preparation" ? vi.fn().mockRejectedValueOnce(failure).mockImplementation(async (files: readonly File[]) => files) : undefined;
  const transport = phase === "transport" ? vi.fn().mockRejectedValueOnce(failure).mockResolvedValue("done") : vi.fn(async () => "done");
  const { input, editor, onComplete } = setup({ transport, ...(prepare ? { prepare } : {}) });
  const selected = file(); Object.defineProperty(input, "files", { configurable: true, value: [selected] });
  // jsdom cannot set a native file path; the browser harness checks the real input value.
  Object.defineProperty(input, "value", { configurable: true, writable: true, value: "C:\\fakepath\\report.txt" });
  input.dispatchEvent(new Event("change"));
  await vi.waitFor(() => expect(editor.node.dataset["state"]).toBe("error"));
  expect(input.value).toBe(""); expect(onComplete).not.toHaveBeenCalled();
  input.value = "C:\\fakepath\\report.txt"; input.dispatchEvent(new Event("change"));
  await vi.waitFor(() => expect(onComplete).toHaveBeenCalledWith("done", [selected]));
  expect(input.value).toBe(""); expect(transport).toHaveBeenCalledTimes(phase === "transport" ? 2 : 1);
});

test("native selection resets after cancellation and the same file can start a new operation", async () => {
  const pending = deferred<string>();
  const transport = vi.fn().mockImplementationOnce(() => pending.promise).mockResolvedValue("done");
  const { input, editor, cancel, onComplete } = setup({ transport });
  const selected = file(); Object.defineProperty(input, "files", { configurable: true, value: [selected] });
  Object.defineProperty(input, "value", { configurable: true, writable: true, value: "C:\\fakepath\\report.txt" });
  input.dispatchEvent(new Event("change")); await vi.waitFor(() => expect(transport).toHaveBeenCalledTimes(1));
  cancel().click(); dialogClick("Cancel upload"); await vi.waitFor(() => expect(editor.node.dataset["state"]).toBe("aborted"));
  expect(input.value).toBe(""); expect(onComplete).not.toHaveBeenCalled();
  input.value = "C:\\fakepath\\report.txt"; input.dispatchEvent(new Event("change"));
  await vi.waitFor(() => expect(onComplete).toHaveBeenCalledWith("done", [selected]));
  expect(transport).toHaveBeenCalledTimes(2); pending.resolve("late"); await Promise.resolve(); await Promise.resolve();
  expect(onComplete).toHaveBeenCalledTimes(1);
});

test("owner abort during preparation settles locally and suppresses late transport and completion", async () => {
  const prepared = deferred<readonly File[]>(), owner = new AbortController();
  const { editor, transport, onComplete, messages } = setup({ signal: owner.signal, prepare: () => prepared.promise, blockPreparation: true });
  const operation = editor.select([file()]); const rejection = expect(operation).rejects.toMatchObject({ name: "AbortError", message: "The file operation was cancelled." });
  expect(messages.block.blocked).toBe(true); owner.abort(); await rejection;
  expect(messages.block.blocked).toBe(false); prepared.resolve([file()]); await Promise.resolve(); await Promise.resolve();
  expect(transport).not.toHaveBeenCalled(); expect(onComplete).not.toHaveBeenCalled();
});

test("blocking preparation releases before transport and before the owner callback can dispose the editor", async () => {
  const prepared = deferred<readonly File[]>(); let dispose: (() => void) | undefined;
  let service!: MessageServices;
  const transport = vi.fn(async () => { expect(service.block.blocked).toBe(false); return "done"; });
  const onComplete = vi.fn(() => { expect(service.block.blocked).toBe(false); dispose!(); });
  const { editor, messages } = setup({ prepare: () => prepared.promise, blockPreparation: true, transport, onComplete });
  service = messages; dispose = editor.dispose;
  const operation = editor.select([file()]); expect(messages.block.blocked).toBe(true);
  prepared.resolve([file()]); await operation;
  expect(onComplete).toHaveBeenCalledTimes(1); expect(messages.block.blocked).toBe(false);
});

test("owner destruction aborts an ignored transport signal promptly and leaves no late callback", async () => {
  const uploaded = deferred<string>(); let signal!: AbortSignal;
  const { editor, onComplete } = setup({ transport: (_files, context) => { signal = context.signal; return uploaded.promise; } });
  const operation = editor.select([file()]); const rejection = expect(operation).rejects.toMatchObject({ name: "AbortError", message: "File upload was cancelled." });
  await vi.waitFor(() => expect(signal).toBeDefined()); editor.dispose(); await rejection;
  expect(signal.aborted).toBe(true); uploaded.resolve("late"); await Promise.resolve(); await Promise.resolve();
  expect(onComplete).not.toHaveBeenCalled(); expect(document.querySelector("dialog")).toBeNull();
});

test("immediate owner disposal does not invoke a queued prepare callback after death", async () => {
  const prepare = vi.fn(async (files: readonly File[]) => files);
  const { editor, transport } = setup({ prepare });
  const operation = editor.select([file()]); const rejection = expect(operation).rejects.toMatchObject({ name: "AbortError", message: "The file operation was cancelled." });
  editor.dispose(); await rejection; await Promise.resolve();
  expect(prepare).not.toHaveBeenCalled(); expect(transport).not.toHaveBeenCalled();
});

test("completion may start another selection without the first cleanup hiding its progress", async () => {
  const first = deferred<string>(), second = deferred<string>();
  const transport = vi.fn().mockImplementationOnce(() => first.promise).mockImplementationOnce(() => second.promise);
  let selectAgain!: () => void, secondOperation!: Promise<void>;
  const onComplete = vi.fn().mockImplementationOnce(() => selectAgain());
  const { editor } = setup({ transport, onComplete });
  selectAgain = () => { secondOperation = editor.select([file("second")]); };
  const firstOperation = editor.select([file()]); await vi.waitFor(() => expect(transport).toHaveBeenCalledTimes(1));
  first.resolve("first"); await firstOperation;
  await vi.waitFor(() => expect(transport).toHaveBeenCalledTimes(2));
  try {
    expect(editor.node.querySelector<HTMLProgressElement>("progress")!.hidden).toBe(false);
    expect(editor.node.dataset["state"]).toBe("pending"); expect(editor.node.dataset["filePending"]).toBe("true");
  } finally { second.resolve("second"); await secondOperation; }
});

test("a throwing completion may start another operation without replacing its pending presentation", async () => {
  const pending = deferred<string>(), callbackError = new ClientError("validation", "Completion callback refused.");
  const transport = vi.fn().mockResolvedValueOnce("first").mockImplementationOnce(() => pending.promise);
  let next!: Promise<void>, startNext!: () => void;
  const onComplete = vi.fn().mockImplementationOnce(() => { startNext(); throw callbackError; });
  const { editor, cancel } = setup({ transport, onComplete });
  startNext = () => { next = editor.select([file("second")]); };
  await expect(editor.select([file()])).rejects.toBe(callbackError);
  await vi.waitFor(() => expect(transport).toHaveBeenCalledTimes(2));
  try {
    expect(editor.node.dataset["state"]).toBe("pending"); expect(editor.node.querySelector("[role='status']")?.textContent).toBe("Waiting for progress…");
    expect(editor.node.querySelector<HTMLProgressElement>("progress")!.hidden).toBe(false);
    expect(cancel().hidden).toBe(false); expect(editor.node.dataset["filePending"]).toBe("true");
  } finally { pending.resolve("second"); await next; }
  expect(onComplete).toHaveBeenCalledTimes(2); expect(transport).toHaveBeenCalledTimes(2);
});

test("public async completion rejection propagates through select without retry", async () => {
  const completion = deferred<void>(), failure = new ClientError("validation", "Async completion refused.");
  // Consume the controlled fixture promise even on the broken implementation, which ignores it.
  void completion.promise.catch(() => {});
  const onComplete = vi.fn(() => completion.promise);
  const { editor, transport } = setup({ onComplete });
  const operation = editor.select([file()]);
  await vi.waitFor(() => expect(onComplete).toHaveBeenCalledTimes(1));
  const rejection = expect(operation).rejects.toBe(failure); completion.reject(failure); await rejection;
  expect(transport).toHaveBeenCalledTimes(1);
});

test("an awaited completion cannot replace a newer operation's completed state with its late failure", async () => {
  const completion = deferred<void>(), secondTransfer = deferred<string>();
  const failure = new ClientError("validation", "Old completion failed."); void completion.promise.catch(() => {});
  const transport = vi.fn().mockResolvedValueOnce("first").mockImplementationOnce(() => secondTransfer.promise);
  let next!: Promise<void>, startNext!: () => void;
  const onComplete = vi.fn().mockImplementationOnce(() => { startNext(); return completion.promise; });
  const { editor } = setup({ transport, onComplete }); startNext = () => { next = editor.select([file("second")]); };
  const first = editor.select([file()]); await vi.waitFor(() => expect(transport).toHaveBeenCalledTimes(2));
  secondTransfer.resolve("second"); await next;
  expect(editor.node.dataset["state"]).toBe("completed");
  const rejection = expect(first).rejects.toBe(failure); completion.reject(failure); await rejection;
  expect(editor.node.dataset["state"]).toBe("completed"); expect(editor.node.querySelector("[role='status']")?.textContent).toBe("Completed");
  expect(onComplete).toHaveBeenCalledTimes(2); expect(transport).toHaveBeenCalledTimes(2);
});

test("disposing an owner while its completion awaits settles locally and consumes late rejection", async () => {
  const completion = deferred<void>(), failure = new ClientError("validation", "Disposed completion failed.");
  void completion.promise.catch(() => {}); const onComplete = vi.fn(() => completion.promise);
  const { editor, report } = setup({ onComplete }); const operation = editor.select([file()]);
  await vi.waitFor(() => expect(onComplete).toHaveBeenCalledTimes(1));
  const rejection = expect(operation).rejects.toMatchObject({ name: "AbortError", message: "The file operation was cancelled." }); editor.dispose(); await rejection;
  const afterDisposal = editor.node.outerHTML;
  completion.reject(failure); await Promise.resolve(); await Promise.resolve();
  expect(editor.node.outerHTML).toBe(afterDisposal); expect(report).not.toHaveBeenCalled();
});

test("notification admission failure cannot replace the upload error and is reported separately", async () => {
  const original = new ClientError("server", "The upload was refused.");
  const presentation = new ClientError("validation", "Notification service is disposed.");
  const { editor, messages, report } = setup({ transport: async () => { throw original; } });
  vi.spyOn(messages.notifications, "add").mockImplementation(() => { throw presentation; });
  await expect(editor.select([file()])).rejects.toBe(original);
  expect(report).toHaveBeenCalledWith(presentation);
  expect(editor.node.querySelector("[role='status']")?.textContent).toBe("The upload was refused.");
});

/// (partial: visible progress and completion; controller terminal record assertions in client-file-upload.test.ts)
test("progress presents only transport counters, unknown total is indeterminate and success is visible", async () => {
  const uploaded = deferred<string>(); let progress!: (loaded: number, total?: number) => void;
  const { editor } = setup({ transport: (_files, context) => { progress = context.progress; return uploaded.promise; } });
  const operation = editor.select([file()]); await vi.waitFor(() => expect(progress).toBeTypeOf("function"));
  const meter = editor.node.querySelector<HTMLProgressElement>("progress")!;
  expect(meter.hasAttribute("value")).toBe(false); expect(meter.getAttribute("aria-label")).toBe("Supporting file progress");
  progress(2, 4); expect(meter.value).toBe(2); expect(meter.max).toBe(4);
  expect(editor.node.textContent).toContain("2 of 4 bytes");
  progress(3); expect(meter.hasAttribute("value")).toBe(false); expect(editor.node.textContent).toContain("3 bytes");
  uploaded.resolve("done"); await operation;
  expect(editor.node.textContent).toContain("Completed"); expect(editor.node.getAttribute("aria-busy")).toBe("false");
});

/// (partial: UI failure and confirmed cancellation; controller races in client-file-upload.test.ts)
test("known failures remain failed, optionally show sticky danger and never retry", async () => {
  const failure = new ClientError("server", "The upload was refused.");
  const transport = vi.fn(async () => { throw failure; });
  const { editor, root, trigger } = setup({ transport });
  await expect(editor.select([file()])).rejects.toBe(failure);
  expect(root.querySelector(".client-notification[data-level='danger']")?.textContent).toContain("The upload was refused.");
  expect(trigger.disabled).toBe(false); expect(transport).toHaveBeenCalledTimes(1);
  const suppressed = setup({ transport, displayErrors: false });
  await expect(suppressed.editor.select([file()])).rejects.toBe(failure);
  expect(suppressed.root.querySelector(".client-notification")).toBeNull();
  expect(transport).toHaveBeenCalledTimes(2);
});

test("cancellation asks first, decline retains upload, confirmed abort suppresses late completion", async () => {
  const uploaded = deferred<string>(); let signal!: AbortSignal;
  const onComplete = vi.fn(), transport = vi.fn((_files: readonly File[], context: { signal: AbortSignal }) => { signal = context.signal; return uploaded.promise; });
  const { editor, cancel } = setup({ transport, onComplete });
  const operation = editor.select([file()]); const rejection = expect(operation).rejects.toMatchObject({ name: "AbortError", message: "File upload was cancelled." });
  await vi.waitFor(() => expect(signal).toBeDefined()); cancel().click();
  expect(document.querySelector("dialog")?.textContent).toContain("Cancel file operation");
  dialogClick("Keep uploading"); await vi.waitFor(() => expect(document.querySelector("dialog")).toBeNull());
  expect(signal.aborted).toBe(false); cancel().click(); dialogClick("Cancel upload"); await rejection;
  expect(signal.aborted).toBe(true); uploaded.resolve("late"); await Promise.resolve(); await Promise.resolve();
  expect(onComplete).not.toHaveBeenCalled(); expect(transport).toHaveBeenCalledTimes(1);
});

test("confirmed cancellation of a native choice is a normal outcome rather than an application error", async () => {
  const uploaded = deferred<string>();
  const { input, editor, cancel, report } = setup({ transport: () => uploaded.promise });
  Object.defineProperty(input, "files", { configurable: true, value: [file()] });
  input.dispatchEvent(new Event("change"));
  // The pending meter is visible only after FileUpload has started the callback operation.
  await vi.waitFor(() => expect(editor.node.querySelector<HTMLProgressElement>("progress")!.hidden).toBe(false));
  cancel().click(); dialogClick("Cancel upload");
  await vi.waitFor(() => expect(editor.node.querySelector("[role='status']")?.textContent).toBe("Cancelled"));
  expect(report).not.toHaveBeenCalled();
});

test("confirmed cancellation returns focus from its disappearing controls to the enabled chooser", async () => {
  const pending = deferred<string>(); const { editor, cancel, trigger } = setup({ transport: () => pending.promise });
  const operation = editor.select([file()]); const aborted = expect(operation).rejects.toMatchObject({ name: "AbortError", message: "File upload was cancelled." });
  await vi.waitFor(() => expect(cancel().hidden).toBe(false)); cancel().focus(); cancel().click();
  const confirmation = [...document.querySelectorAll<HTMLButtonElement>("dialog button")].find(control => control.textContent === "Cancel upload")!;
  confirmation.focus(); confirmation.click(); await aborted;
  expect(trigger.disabled).toBe(false); expect(cancel().hidden).toBe(true); expect(document.activeElement).toBe(trigger);
});

test("confirmed cancellation restores chooser focus when the native disabled confirmation button blurs to body", async () => {
  const pending = deferred<string>(); const { editor, cancel, trigger } = setup({ transport: () => pending.promise });
  const operation = editor.select([file()]); const aborted = expect(operation).rejects.toMatchObject({ name: "AbortError", message: "File upload was cancelled." });
  await vi.waitFor(() => expect(cancel().hidden).toBe(false)); cancel().focus(); cancel().click();
  const confirmation = [...document.querySelectorAll<HTMLButtonElement>("dialog button")].find(control => control.textContent === "Cancel upload")!;
  // Native Chromium may blur a disabled focused button; jsdom needs that behavior explicitly.
  let observedBody = false;
  // Blur in capture while the button is enabled; jsdom cannot blur it once the native
  // confirmation handler disables it. The real Chromium failure is retained separately.
  confirmation.addEventListener("click", () => { confirmation.blur(); observedBody = document.activeElement === document.body; }, { capture: true });
  confirmation.focus(); confirmation.click(); await aborted;
  expect(observedBody).toBe(true);
  expect(trigger.disabled).toBe(false); expect(document.activeElement).toBe(trigger);
});

test("completion with a cancellation dialog open returns focus to a surviving chooser", async () => {
  const pending = deferred<string>(); const { editor, cancel, trigger } = setup({ transport: () => pending.promise });
  const operation = editor.select([file()]); await vi.waitFor(() => expect(cancel().hidden).toBe(false));
  cancel().focus(); cancel().click(); expect(document.querySelector("dialog")).not.toBeNull();
  pending.resolve("done"); await operation;
  expect(document.querySelector("dialog")).toBeNull(); expect(trigger.disabled).toBe(false); expect(document.activeElement).toBe(trigger);
});

test("terminal upload cleanup does not steal focus from an unrelated control", async () => {
  const pending = deferred<string>(); const { editor } = setup({ transport: () => pending.promise });
  const other = document.createElement("button"); other.textContent = "Unrelated destination"; document.body.append(other);
  const operation = editor.select([file()]); await vi.waitFor(() => expect(editor.node.dataset["state"]).toBe("pending")); other.focus();
  pending.resolve("done"); await operation; expect(document.activeElement).toBe(other);
});

test("completion closes stale cancellation confirmation before the next operation", async () => {
  const first = deferred<string>(), second = deferred<string>();
  const transport = vi.fn().mockImplementationOnce(() => first.promise).mockImplementationOnce(() => second.promise);
  const { editor, cancel, onComplete } = setup({ transport });
  const one = editor.select([file()]); await vi.waitFor(() => expect(transport).toHaveBeenCalledTimes(1)); cancel().click();
  first.resolve("first"); await one; expect(document.querySelector("dialog")).toBeNull();
  const two = editor.select([file("second")]); await vi.waitFor(() => expect(transport).toHaveBeenCalledTimes(2));
  second.resolve("second"); await two; expect(onComplete).toHaveBeenCalledTimes(2);
});

test("readonly and disposed editors refuse programmatic mutation", async () => {
  const { editor, input, trigger, transport } = setup({ readonly: true });
  expect(input.disabled).toBe(true); expect(trigger.disabled).toBe(true);
  await expect(editor.select([file()])).rejects.toMatchObject({ kind: "validation", message: "File editor is readonly." });
  expect(transport).not.toHaveBeenCalled(); editor.dispose();
  await expect(editor.select([file()])).rejects.toMatchObject({ kind: "validation", message: "File editor is disposed." });
});

test.each<[Record<string, unknown>, string]>([
  [{ unknown: true }, "Unknown file editor option: unknown."],
  [{ limits: { maxFiles: 0, maxFileBytes: 4, maxTotalBytes: 6 } }, "File limits must be positive safe integers."],
  [{ limits: { maxFiles: 2, maxFileBytes: Infinity, maxTotalBytes: 6 } }, "File limits must be positive safe integers."],
  [{ limits: { maxFiles: 2, maxFileBytes: 4, maxTotalBytes: 6, unknown: true } }, "Unknown file limits option: unknown."],
  [{ label: "" }, "File editor label must be nonempty text of at most 8192 UTF-8 bytes."],
  [{ displayErrors: "yes" }, "File editor displayErrors must be a boolean."],
])("invalid declarations fail with precise typed messages: %j", (patch, message) => {
  const root = document.createElement("div"); document.body.append(root);
  const messages = new MessageServices(root, () => {}); cleanups.push(() => messages.dispose());
  expect(() => createFileEditor({ label: "File", limits: { maxFiles: 1, maxFileBytes: 4, maxTotalBytes: 4 }, messages, report: () => {}, transport: async () => "done", onComplete: () => {}, ...patch } as never)).toThrowError(expect.objectContaining({ kind: "validation", message }));
});
