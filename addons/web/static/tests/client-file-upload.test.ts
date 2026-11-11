/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
import { expect, test, vi } from "vitest";
import { FileUpload, type UploadEvent } from "../src/client/file-upload.js";
import { ClientError } from "../src/client/transport.js";

function deferred<T>() {
  let resolve!: (value: T) => void;
  let reject!: (reason: unknown) => void;
  const promise = new Promise<T>((yes, no) => { resolve = yes; reject = no; });
  return { promise, resolve, reject };
}
const file = (): File => new File(["content"], "record.txt", { type: "text/plain" });
const progressError = "File upload progress must contain nonnegative finite counters with loaded not exceeding total.";

test("an upload exposes pending and real progress, then one loaded record and completion", async () => {
  const pending = deferred<{ token: string }>();
  let progress!: (loaded: number, total?: number) => void;
  const received: string[] = [];
  const events: { type: string; state: string; loaded: number; total: number | undefined; active: number }[] = [];
  const transport = vi.fn((files: readonly File[], context: { signal: AbortSignal; progress: (loaded: number, total?: number) => void }) => {
    received.push(...files.map(item => item.name)); progress = context.progress;
    expect(context.signal.aborted).toBe(false);
    return pending.promise;
  });
  const report = vi.fn();
  const upload: FileUpload<{ token: string }> = new FileUpload({ transport, report, onEvent: event => {
    events.push({ type: event.type, state: event.record.state, loaded: event.record.loaded, total: event.record.total, active: upload.active.length });
  } });
  const operation = upload.start([file()]);
  expect(operation.record.state).toBe("pending");
  expect(upload.active).toEqual([operation.record]);
  await vi.waitFor(() => expect(transport).toHaveBeenCalledTimes(1));
  expect(received).toEqual(["record.txt"]);
  progress(4, 10);
  expect(operation.record).toMatchObject({ state: "loading", loaded: 4, total: 10 });
  expect(upload.active).toEqual([operation.record]);
  const result = { token: "caller-owned-result" };
  pending.resolve(result);
  await expect(operation.done).resolves.toBe(result);
  expect(operation.record).toMatchObject({ state: "loaded", loaded: 4, total: 10, result });
  expect(upload.active).toEqual([]);
  expect(events).toEqual([
    { type: "added", state: "pending", loaded: 0, total: undefined, active: 1 },
    { type: "progress", state: "loading", loaded: 4, total: 10, active: 1 },
    { type: "completed", state: "loaded", loaded: 4, total: 10, active: 0 },
  ]);
  progress(10, 10); operation.abort();
  expect(operation.record).toMatchObject({ state: "loaded", loaded: 4, total: 10, result });
  expect(events).toHaveLength(3); expect(transport).toHaveBeenCalledTimes(1); expect(report).not.toHaveBeenCalled();
  upload.dispose();
});

test("unknown totals remain unknown and an actual empty-file progress event is valid", async () => {
  const pending = deferred<string>();
  let progress!: (loaded: number, total?: number) => void;
  const upload = new FileUpload({ report: vi.fn(), transport: (_files, context) => { progress = context.progress; return pending.promise; } });
  const operation = upload.start([new File([], "empty.txt")]);
  await vi.waitFor(() => expect(progress).toBeTypeOf("function"));
  progress(0, 0);
  expect(operation.record).toMatchObject({ state: "loading", loaded: 0, total: 0 });
  progress(7);
  expect(operation.record.loaded).toBe(7); expect(operation.record.total).toBeUndefined();
  pending.resolve("done"); await expect(operation.done).resolves.toBe("done");
  expect(operation.record.loaded).toBe(7); expect(operation.record.total).toBeUndefined();
  upload.dispose();
});

test("start snapshots the caller's file array before transport can observe later mutations", async () => {
  const pending = deferred<void>();
  const original = file(); const files = [original]; let received: readonly File[] | undefined;
  const upload = new FileUpload({ report: vi.fn(), transport: selected => { received = selected; return pending.promise; } });
  const operation = upload.start(files); files.splice(0, 1, new File(["other"], "replacement.txt"));
  await vi.waitFor(() => expect(received).toBeDefined());
  expect(received).toEqual([original]); expect(received).not.toBe(files);
  pending.resolve(); await operation.done; upload.dispose();
});

// Typed transport-failure variants supplement the whole-case EDITOR-019 acceptance test.
test.each(["sync", "async"] as const)("%s transport failure preserves kind/message, removes the record and never retries", async mode => {
  const failure = new ClientError("permission", "This file is not permitted.", 403);
  const events: UploadEvent<string>[] = [];
  const transport = vi.fn(() => { if (mode === "sync") throw failure; return Promise.reject(failure); });
  const report = vi.fn(); const upload = new FileUpload<string>({ transport, report, onEvent: event => events.push(event) });
  const operation = upload.start([file()]);
  await expect(operation.done).rejects.toBe(failure);
  expect(operation.record.error).toBe(failure);
  expect(operation.record.error).toMatchObject({ kind: "permission", message: "This file is not permitted.", status: 403 });
  expect(operation.record.state).toBe("error"); expect(upload.active).toEqual([]);
  expect(events.map(event => event.type)).toEqual(["added", "failed"]);
  operation.abort(); await Promise.resolve();
  expect(events.map(event => event.type)).toEqual(["added", "failed"]);
  expect(transport).toHaveBeenCalledTimes(1); expect(report).not.toHaveBeenCalled(); upload.dispose();
});

test.each<[number, number | undefined]>([
  [-1, undefined], [Number.NaN, 5], [Number.POSITIVE_INFINITY, undefined], [1, -1], [1, Number.NaN], [1, Number.POSITIVE_INFINITY], [11, 10],
])("invalid progress loaded=%s total=%s terminates with a typed failure", async (loaded, total) => {
  const pending = deferred<string>(); let progress!: (loaded: number, total?: number) => void; let signal!: AbortSignal;
  const events: string[] = [];
  const upload = new FileUpload({ report: vi.fn(), onEvent: event => events.push(event.type), transport: (_files, context) => { progress = context.progress; signal = context.signal; return pending.promise; } });
  const operation = upload.start([file()]);
  await vi.waitFor(() => expect(progress).toBeTypeOf("function"));
  const rejected = expect(operation.done).rejects.toMatchObject({ kind: "validation", message: progressError });
  progress(loaded, total); await rejected;
  expect(operation.record.state).toBe("error"); expect(upload.active).toEqual([]); expect(signal.aborted).toBe(true);
  expect(events).toEqual(["added", "failed"]);
  pending.resolve("late success"); await Promise.resolve(); await Promise.resolve();
  expect(operation.record.state).toBe("error"); expect(events).toEqual(["added", "failed"]); upload.dispose();
});

/// (partial: explicit abort settlement and late-event suppression, not cancellation confirmation)
test("abort settles promptly even when transport ignores its signal; late failure and progress do not resettle", async () => {
  const pending = deferred<string>(); let signal!: AbortSignal; let progress!: (loaded: number, total?: number) => void;
  const events: string[] = []; const report = vi.fn();
  const transport = vi.fn((_files: readonly File[], context: { signal: AbortSignal; progress: (loaded: number, total?: number) => void }) => { signal = context.signal; progress = context.progress; return pending.promise; });
  const upload = new FileUpload({ transport, report, onEvent: event => events.push(event.type) });
  const operation = upload.start([file()]);
  await vi.waitFor(() => expect(transport).toHaveBeenCalledTimes(1));
  const rejected = expect(operation.done).rejects.toMatchObject({ name: "AbortError", message: "File upload was cancelled." });
  operation.abort(); operation.abort(); await rejected;
  expect(signal.aborted).toBe(true); expect(upload.active).toEqual([]); expect(operation.record.state).toBe("aborted");
  expect(events).toEqual(["added", "failed"]);
  progress(3, 8); pending.reject(new Error("late transport failure")); await Promise.resolve(); await Promise.resolve();
  expect(operation.record.state).toBe("aborted"); expect(events).toEqual(["added", "failed"]);
  expect(transport).toHaveBeenCalledTimes(1); expect(report).not.toHaveBeenCalled(); upload.dispose();
});

test("an abort that wins before a resolved transport microtask suppresses completion", async () => {
  const events: string[] = [];
  const upload = new FileUpload({ report: vi.fn(), transport: async () => "result", onEvent: event => events.push(event.type) });
  const operation = upload.start([file()]);
  const rejected = expect(operation.done).rejects.toMatchObject({ name: "AbortError", message: "File upload was cancelled." });
  operation.abort(); await rejected; await Promise.resolve();
  expect(operation.record.state).toBe("aborted"); expect(events).toEqual(["added", "failed"]); upload.dispose();
});

test.each(["dispose", "signal"] as const)("%s ends local lifetime without delivering terminal callbacks to a dead owner", async mode => {
  const pending = deferred<string>(); const owner = new AbortController(); let signal!: AbortSignal; let progress!: (loaded: number, total?: number) => void;
  const events: string[] = []; const report = vi.fn();
  const upload = new FileUpload({ report, signal: owner.signal, onEvent: event => events.push(event.type), transport: (_files, context) => { signal = context.signal; progress = context.progress; return pending.promise; } });
  const operation = upload.start([file()]);
  await vi.waitFor(() => expect(progress).toBeTypeOf("function"));
  const rejected = expect(operation.done).rejects.toMatchObject({ name: "AbortError", message: "File upload was cancelled." });
  if (mode === "dispose") upload.dispose(); else owner.abort();
  await rejected;
  expect(signal.aborted).toBe(true); expect(operation.record.state).toBe("aborted"); expect(upload.active).toEqual([]);
  expect(events).toEqual(["added"]);
  pending.resolve("late result"); progress(8, 8); await Promise.resolve(); await Promise.resolve();
  expect(events).toEqual(["added"]); expect(report).not.toHaveBeenCalled();
  expect(() => upload.start([file()])).toThrowError(expect.objectContaining({ kind: "validation", message: "File upload controller is disposed." }));
  upload.dispose();
});

test("an already-aborted owner refuses work before transport or an added callback", () => {
  const owner = new AbortController(); owner.abort(); const transport = vi.fn(async () => "result"), onEvent = vi.fn();
  const upload = new FileUpload({ transport, onEvent, signal: owner.signal, report: vi.fn() });
  expect(() => upload.start([file()])).toThrowError(expect.objectContaining({ kind: "validation", message: "File upload controller is disposed." }));
  expect(transport).not.toHaveBeenCalled(); expect(onEvent).not.toHaveBeenCalled(); upload.dispose();
});

test("admission rejects concurrent starts, including reentry from the added event", async () => {
  const pending = deferred<string>(); let reentrantFailure: unknown;
  const transport = vi.fn(() => pending.promise);
  const upload: FileUpload<string> = new FileUpload({ transport, report: vi.fn(), onEvent: event => {
    if (event.type === "added") { try { upload.start([file()]); } catch (error) { reentrantFailure = error; } }
  } });
  const operation = upload.start([file()]);
  expect(reentrantFailure).toMatchObject({ kind: "validation", message: "A file upload is already active." });
  expect(() => upload.start([file()])).toThrowError(expect.objectContaining({ kind: "validation", message: "A file upload is already active." }));
  pending.resolve("result"); await operation.done; expect(transport).toHaveBeenCalledTimes(1); upload.dispose();
});

test("completed-event callbacks may dispose the controller without losing the completed result", async () => {
  const report = vi.fn(); const ownerFailure = new Error("Owner completion callback failed.");
  const events: string[] = [];
  const upload: FileUpload<string> = new FileUpload({ transport: async () => "result", report, onEvent: event => {
    events.push(event.type);
    if (event.type === "completed") { expect(upload.active).toEqual([]); upload.dispose(); throw ownerFailure; }
  } });
  const operation = upload.start([file()]); await expect(operation.done).resolves.toBe("result");
  expect(operation.record.state).toBe("loaded"); expect(events).toEqual(["added", "completed"]);
  expect(report).toHaveBeenCalledExactlyOnceWith(ownerFailure);
});

test("added-event disposal prevents starting transport after the owner is dead", async () => {
  const transport = vi.fn(async () => "result"); const events: string[] = [];
  const upload: FileUpload<string> = new FileUpload({ transport, report: vi.fn(), onEvent: event => { events.push(event.type); if (event.type === "added") upload.dispose(); } });
  const operation = upload.start([file()]);
  await expect(operation.done).rejects.toMatchObject({ name: "AbortError", message: "File upload was cancelled." });
  expect(transport).not.toHaveBeenCalled(); expect(operation.record.state).toBe("aborted"); expect(upload.active).toEqual([]);
  expect(events).toEqual(["added"]);
});

test.each<[Record<string, unknown> | null, string]>([
  [null, "File upload options must be a plain object."],
  [{ unexpected: true }, "Unknown file upload option: unexpected."],
  [{ transport: null }, "A file upload transport callback is required."],
  [{ report: null }, "A file upload report callback is required."],
  [{ onEvent: true }, "File upload onEvent must be a function."],
  [{ signal: {} }, "File upload signal must be an AbortSignal."],
])("invalid controller options have precise admission failures: %j", (patch, message) => {
  const options = patch === null ? null : { transport: async () => "result", report: vi.fn(), ...patch };
  expect(() => new FileUpload(options as never)).toThrowError(expect.objectContaining({ kind: "validation", message }));
});

test.each([[], ["file"], null, {}])("invalid file selection is refused before transport: %j", files => {
  const transport = vi.fn(async () => "result"); const upload = new FileUpload({ transport, report: vi.fn() });
  expect(() => upload.start(files as never)).toThrowError(expect.objectContaining({ kind: "validation", message: "File upload requires a nonempty array of files." }));
  expect(transport).not.toHaveBeenCalled(); expect(upload.active).toEqual([]); upload.dispose();
});

test.each(["added", "progress", "completed", "failed"] as const)("an async %s event hook is refused without floating its rejected promise", async target => {
  const pending = deferred<string>(); let progress: ((loaded: number, total?: number) => void) | undefined;
  const events: string[] = [];
  const transport = vi.fn((_files: readonly File[], context: { signal: AbortSignal; progress: (loaded: number, total?: number) => void }) => { progress = context.progress; return pending.promise; });
  const report = vi.fn();
  const upload = new FileUpload({ transport, report, onEvent: event => {
    events.push(event.type);
    if (event.type === target) return Promise.reject(new Error("Invalid async event hook rejected."));
    return undefined;
  } });
  try {
    const operation = upload.start([file()]);
    const rejected = expect(operation.done).rejects.toMatchObject({ kind: "validation", message: "File upload event callbacks must be synchronous." });
    if (target !== "added") {
      await vi.waitFor(() => expect(progress).toBeTypeOf("function"));
      if (target === "progress") { progress!(2, 8); pending.resolve("caller result"); }
      else if (target === "completed") pending.resolve("caller result");
      else pending.reject(new ClientError("server", "Transport refused."));
    } else pending.resolve("caller result");
    await rejected;
    expect(upload.active).toEqual([]);
    expect(["loaded", "error", "aborted"]).toContain(operation.record.state);
    const terminalState = operation.record.state, terminalEvents = [...events];
    operation.abort(); pending.resolve("late caller result");
    if (progress) progress(8, 8);
    // Vitest's default unhandled-rejection gate fails the run if the refused promise escapes.
    await new Promise(resolve => setTimeout(resolve, 0));
    expect(operation.record.state).toBe(terminalState); expect(events).toEqual(terminalEvents);
    expect(transport).toHaveBeenCalledTimes(target === "added" ? 0 : 1);
  } finally { upload.dispose(); }
});

test("a fulfilled event thenable is also refused rather than treated as synchronous success", async () => {
  const transport = vi.fn(async () => "result");
  const upload = new FileUpload({ transport, report: vi.fn(), onEvent: () => Promise.resolve() });
  const operation = upload.start([file()]);
  await expect(operation.done).rejects.toMatchObject({ kind: "validation", message: "File upload event callbacks must be synchronous." });
  expect(operation.record.state).toBe("error"); expect(upload.active).toEqual([]); expect(transport).not.toHaveBeenCalled(); upload.dispose();
});

test("an async reporter is refused after a synchronous event failure and its later rejection is consumed", async () => {
  const callbackFailure = new Error("Synchronous event hook failed.");
  const report = vi.fn(async () => { throw new Error("Invalid async reporter rejected."); });
  const transport = vi.fn(async () => "result");
  const upload = new FileUpload({ transport, report, onEvent: () => { throw callbackFailure; } });
  try {
    const operation = upload.start([file()]);
    await expect(operation.done).rejects.toMatchObject({ kind: "validation", message: "File upload report callbacks must be synchronous." });
    expect(operation.record.state).toBe("error"); expect(upload.active).toEqual([]);
    expect(report).toHaveBeenCalledExactlyOnceWith(callbackFailure); expect(transport).not.toHaveBeenCalled();
    // Yield for Vitest's own unhandled-rejection gate; no separate fake escaped-error counter.
    operation.abort(); await new Promise(resolve => setTimeout(resolve, 0));
    expect(report).toHaveBeenCalledTimes(1);
  } finally { upload.dispose(); }
});

test("a fulfilled reporter thenable is refused as an invalid synchronous hook", async () => {
  const report = vi.fn(() => Promise.resolve()); const transport = vi.fn(async () => "result");
  const upload = new FileUpload({ transport, report, onEvent: () => { throw new Error("Event hook failed."); } });
  const operation = upload.start([file()]);
  await expect(operation.done).rejects.toMatchObject({ kind: "validation", message: "File upload report callbacks must be synchronous." });
  expect(upload.active).toEqual([]); expect(operation.record.state).toBe("error"); expect(transport).not.toHaveBeenCalled(); upload.dispose();
});

test("a throwing event then getter preserves the original error through the synchronous reporter", async () => {
  const getterFailure = new Error("Event then getter failed."); const report = vi.fn();
  const upload = new FileUpload({ transport: async () => "result", report, onEvent: event => {
    if (event.type === "completed") return { get then() { throw getterFailure; } };
    return undefined;
  } });
  const operation = upload.start([file()]); await expect(operation.done).resolves.toBe("result");
  expect(report).toHaveBeenCalledExactlyOnceWith(getterFailure); expect(operation.record.state).toBe("loaded"); upload.dispose();
});

test("a throwing report then getter preserves both callback and reporting failures", async () => {
  const callbackFailure = new Error("Event hook failed."), getterFailure = new Error("Report then getter failed.");
  const upload = new FileUpload({ transport: async () => "result", report: () => ({ get then() { throw getterFailure; } }), onEvent: event => {
    if (event.type === "completed") throw callbackFailure;
  } });
  const operation = upload.start([file()]);
  await expect(operation.done).rejects.toMatchObject({ name: "AggregateError", message: "File upload event reporting failed.", errors: [callbackFailure, getterFailure] });
  expect(upload.active).toEqual([]); expect(operation.record.state).toBe("loaded"); upload.dispose();
});
