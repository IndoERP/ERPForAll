/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
import { messageOptions } from "./dialog-service.js";
import { ClientError } from "./transport.js";

export interface UploadContext {
  readonly signal: AbortSignal;
  /** Actual transport counters; omit total when it is unknown. */
  readonly progress: (loaded: number, total?: number) => void;
}
/** Trusted caller code owns endpoint selection, authorization and response validation. */
export type UploadTransport<T> = (files: readonly File[], context: UploadContext) => Promise<T>;
export type UploadState = "pending" | "loading" | "loaded" | "error" | "aborted";
export interface UploadRecord<T> {
  readonly id: number;
  readonly state: UploadState;
  readonly loaded: number;
  readonly total?: number;
  readonly result?: T;
  readonly error?: unknown;
}
export interface UploadEvent<T> {
  readonly type: "added" | "progress" | "completed" | "failed";
  readonly record: UploadRecord<T>;
}
export interface UploadOperation<T> {
  readonly record: UploadRecord<T>;
  /** Transport result, or the original transport/abort failure. Reporting failures also reject. */
  readonly done: Promise<T>;
  /** Local cancellation cannot establish whether remote changes were rolled back. */
  abort(): void;
}
export interface FileUploadOptions<T> {
  readonly transport: UploadTransport<T>;
  /** Internal synchronous observation only. Returned thenables reject the operation as invalid. */
  readonly onEvent?: (event: UploadEvent<T>) => void;
  readonly signal?: AbortSignal;
  /** Internal synchronous error sink. Returned thenables reject the operation as invalid. */
  readonly report: (error: unknown) => void;
}
type MutableRecord<T> = { -readonly [Key in keyof UploadRecord<T>]: UploadRecord<T>[Key] };
const PROGRESS_ERROR = "File upload progress must contain nonnegative finite counters with loaded not exceeding total.";

function refuseAsyncHook(value: unknown, hook: "event" | "report"): ClientError | undefined {
  if (value === null || typeof value !== "object" && typeof value !== "function") return undefined;
  // A throwing then getter remains a normal synchronous callback/reporting failure.
  if (typeof (value as { then?: unknown }).then !== "function") return undefined;
  // The invalid hook return is surfaced through operation.done as a validation failure. Observe
  // its eventual rejection deliberately: it must not escape or create a second terminal outcome.
  void Promise.resolve(value).catch(() => undefined);
  return new ClientError("validation", `File upload ${hook} callbacks must be synchronous.`);
}

/** Internal lifecycle for the file editor's already-admitted files. The editor owns file limits.
 * One operation is active per owner; terminal records remain available only through returned handles.
 * This class never posts a request, parses a response, retries an operation or retains file history.
 */
export class FileUpload<T> {
  private current: UploadOperation<T> | undefined;
  private nextId = 1;
  private dead = false;
  private readonly ownerAbort = (): void => this.dispose();
  constructor(private readonly options: FileUploadOptions<T>) {
    messageOptions(options, ["transport", "onEvent", "signal", "report"], "File upload");
    if (typeof options.transport !== "function") throw new ClientError("validation", "A file upload transport callback is required.");
    if (typeof options.report !== "function") throw new ClientError("validation", "A file upload report callback is required.");
    if (options.onEvent !== undefined && typeof options.onEvent !== "function") throw new ClientError("validation", "File upload onEvent must be a function.");
    if (options.signal !== undefined && !(options.signal instanceof AbortSignal)) throw new ClientError("validation", "File upload signal must be an AbortSignal.");
    this.options = Object.freeze({ ...options });
    this.options.signal?.addEventListener("abort", this.ownerAbort, { once: true });
    if (this.options.signal?.aborted) this.dispose();
  }

  /** A fresh collection snapshot, not a mutable view of the controller's active collection. */
  get active(): readonly UploadRecord<T>[] { return this.current ? [this.current.record] : []; }

  start(files: readonly File[]): UploadOperation<T> {
    if (this.dead) throw new ClientError("validation", "File upload controller is disposed.");
    if (this.current) throw new ClientError("validation", "A file upload is already active.");
    if (!Array.isArray(files) || files.length === 0) throw new ClientError("validation", "File upload requires a nonempty array of files.");
    // Indexed validation includes sparse entries; Array.every alone would silently skip holes.
    for (let index = 0; index < files.length; index++) {
      if (!(files[index] instanceof File)) throw new ClientError("validation", "File upload requires a nonempty array of files.");
    }
    if (!Number.isSafeInteger(this.nextId)) throw new ClientError("validation", "File upload identifier limit exceeded.");
    let selected: readonly File[] | undefined = Object.freeze([...files]);
    const record: MutableRecord<T> = { id: this.nextId++, state: "pending", loaded: 0 };
    const controller = new AbortController();
    let settled = false;
    let resolve!: (result: T) => void;
    let reject!: (error: unknown) => void;
    const done = new Promise<T>((yes, no) => { resolve = yes; reject = no; });

    const fail = (error: unknown, aborted: boolean, notify = true): void => {
      if (settled) return;
      settled = true; selected = undefined;
      record.state = aborted ? "aborted" : "error"; record.error = error;
      if (this.current === operation) this.current = undefined;
      // The terminal guard is set first: a synchronous abort listener cannot resettle this record.
      controller.abort();
      const reportingFailure = notify ? this.emit("failed", record) : undefined;
      reject(reportingFailure ?? error);
    };
    const operation: UploadOperation<T> = {
      record, done,
      abort: () => fail(new DOMException("File upload was cancelled.", "AbortError"), true),
    };
    this.current = operation;
    const addedFailure = this.emit("added", record);
    if (addedFailure !== undefined) fail(addedFailure, false, false);

    const progress = (loaded: number, total?: number): void => {
      if (settled || this.dead) return;
      if (typeof loaded !== "number" || !Number.isFinite(loaded) || loaded < 0
        || total !== undefined && (typeof total !== "number" || !Number.isFinite(total) || total < 0 || loaded > total)) {
        fail(new ClientError("validation", PROGRESS_ERROR), false); return;
      }
      record.state = "loading"; record.loaded = loaded;
      if (total === undefined) delete record.total; else record.total = total;
      const reportingFailure = this.emit("progress", record);
      if (reportingFailure !== undefined) fail(reportingFailure, false, false);
    };

    // Deferral lets the caller attach its rejection handler and handles synchronous transport throws.
    void Promise.resolve().then(async () => {
      if (settled || this.dead || selected === undefined) return;
      let result: T;
      try { result = await this.options.transport(selected, { signal: controller.signal, progress }); }
      catch (error) { fail(error, false); return; }
      if (settled || this.dead) return;
      settled = true; selected = undefined; record.state = "loaded"; record.result = result;
      if (this.current === operation) this.current = undefined;
      const reportingFailure = this.emit("completed", record);
      if (reportingFailure === undefined) resolve(result); else reject(reportingFailure);
    });
    return operation;
  }

  private emit(type: UploadEvent<T>["type"], record: UploadRecord<T>): unknown | undefined {
    if (this.dead) return undefined;
    try { return refuseAsyncHook(this.options.onEvent?.({ type, record }), "event"); }
    catch (error) {
      try { return refuseAsyncHook(this.options.report(error), "report"); }
      catch (reportingFailure) { return new AggregateError([error, reportingFailure], "File upload event reporting failed."); }
    }
  }

  /** Abort locally and suppress all later owner events; safe to call repeatedly. */
  dispose(): void {
    if (this.dead) return;
    this.dead = true;
    this.options.signal?.removeEventListener("abort", this.ownerAbort);
    this.current?.abort();
  }
}
