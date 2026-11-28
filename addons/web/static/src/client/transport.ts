/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
import { AstDecodeError, readResponseJson } from "../../../../../web/packages/expr/src/index.js";

export type FailureKind = "validation" | "permission" | "session" | "network" | "ambiguous" | "server" | "unsupported";
export interface ErrorDiagnostics {
  readonly endpoint: string;
  readonly status: number;
  readonly name?: string;
  readonly timestamp?: number;
  readonly debug?: string;
  readonly reason?: string;
  /** The id the server logs this call: its `X-Request-Id`. */
  readonly requestId?: string;
}
const REQUEST_ID = /^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/i;
/** A fresh v4 UUID; falls back to `crypto.getRandomValues` in non-secure HTTP contexts where `randomUUID` is absent. */
export function newRequestId(): string {
  const c = globalThis.crypto;
  if (typeof c?.randomUUID === "function") return c.randomUUID();
  if (typeof c?.getRandomValues === "function") {
    const bytes = new Uint8Array(16);
    c.getRandomValues(bytes);
    bytes[6] = (bytes[6]! & 0x0f) | 0x40;
    bytes[8] = (bytes[8]! & 0x3f) | 0x80;
    const hex = Array.from(bytes, (b) => b.toString(16).padStart(2, "0")).join("");
    return `${hex.slice(0, 8)}-${hex.slice(8, 12)}-${hex.slice(12, 16)}-${hex.slice(16, 20)}-${hex.slice(20)}`;
  }
  throw new ClientError("unsupported", "A cryptographic random source is unavailable in this browser.");
}
export class ClientError extends Error {
  constructor(readonly kind: FailureKind, message: string, readonly status = 0, readonly diagnostics?: ErrorDiagnostics,
    /** The fields of the record a server refusal is about (`context.fields`, or `context.field`); empty when it names none. */
    readonly fields: readonly string[] = []) {
    super(message);
    this.name = "ClientError";
  }
}
const FIELD_NAME = /^[a-z_][a-z0-9_]*$/;
const MAX_FIELDS = 64;
/**
 * The fields a refusal names in its error context: `fields` (a list of field names, as the server's `Error::with_fields`
 * writes it) and `field` (one name, a convention addons also use). Anything else in the context is left alone; a name that
 * is not a field name is not used, so the refusal is then shown as an error that names no field.
 */
export function refusedFields(context: unknown): string[] {
  if (context === null || typeof context !== "object" || Array.isArray(context)) return [];
  const source = context as Record<string, unknown>;
  const names: unknown[] = [...(Array.isArray(source["fields"]) ? source["fields"] : []), ...("field" in source ? [source["field"]] : [])];
  const fields: string[] = [];
  for (const name of names.slice(0, MAX_FIELDS)) if (typeof name === "string" && FIELD_NAME.test(name) && !fields.includes(name)) fields.push(name);
  return fields;
}
/** The name in a `Content-Disposition` header: the exact `filename*` (RFC 8187) when present, else `filename`, else a neutral name. */
export function fileNameOf(disposition: string | null): string {
  const exact = /filename\*=UTF-8''([^;]+)/i.exec(disposition ?? "");
  if (exact) { try { return decodeURIComponent(exact[1]!.trim()); } catch { /* fall back to the ASCII name */ } }
  return /filename="([^"]*)"/i.exec(disposition ?? "")?.[1] ?? "download.xlsx";
}
class ApprovalChallenge extends ClientError {
  constructor(readonly request: number | "key", message: string, status: number, diagnostics: ErrorDiagnostics) {
    super("validation", message, status, diagnostics);
  }
}
export type Fetcher = (input: RequestInfo | URL, init?: RequestInit) => Promise<Response>;
export interface CallOptions {
  readonly write?: boolean;
  readonly signal?: AbortSignal;
  /** Speculative read: no activity indicator. */
  readonly background?: boolean;
}
export type ConnectionEvent = "request" | "online" | "offline" | "error" | "cancelled";
export class RpcClient {
  constructor(
    private readonly fetcher: Fetcher,
    private readonly expired: () => void,
    private readonly activity?: () => () => void,
    /** Runs after every write attempt, whatever its outcome: a failed or ambiguous write may still have changed data. */
    private readonly written?: (model: string) => void,
    private readonly connection?: (event: ConnectionEvent, options: CallOptions) => void,
    private readonly awaitApproval?: (requestId: number) => Promise<boolean>
  ) {}
  call(model: string, method: string, args: Record<string, unknown>, options: CallOptions = {}): Promise<unknown> {
    if (!/^[a-z][a-z0-9_]*(?:\.[a-z][a-z0-9_]*)*$/.test(model) || !/^[a-z][a-z0-9_]*$/.test(method)) {
      throw new ClientError("validation", "Invalid model or method name.");
    }
    return this.post(`/erp/json/2/${model}/${method}`, args, options);
  }
  async post(path: string, args: Record<string, unknown>, options: CallOptions = {}): Promise<unknown> {
    if (
      !/^\/erp\/json\/2\/[a-z0-9_.]+\/[a-z0-9_]+$/.test(path) &&
      path !== "/erp/module/install" &&
      path !== "/erp/module/upgrade" &&
      path !== "/erp/module/uninstall" &&
      path !== "/erp/module/force-demo"
    ) {
      throw new ClientError("validation", "Unsupported request path.");
    }
    return this.runPost(path, options, () => this.sendReviewed(path, JSON.stringify(args), options));
  }
  /** The record import routes receive the original file with separately encoded options. */
  postImport(action: "preview" | "test" | "import", file: File, document: Record<string, unknown> & { model: string }, csrf: string): Promise<unknown> {
    const path = `/erp/impex/${action}`;
    const body = new FormData();
    body.set("file", file);
    body.set("options", JSON.stringify(document));
    const options = action === "import" ? { write: true } : {};
    return this.runPost(path, options, () => this.send(path, body, options, undefined, csrf), document.model);
  }
  /** Stream one attachment file to `/erp/attachments/upload` and return its staged upload descriptor. */
  async postAttachmentUpload(file: File, csrf: string, signal?: AbortSignal): Promise<{ upload_id: string; size: number; mimetype: string; checksum: string }> {
    if (csrf === "") throw new ClientError("validation", "A session upload token is required.");
    const path = "/erp/attachments/upload";
    const body = new FormData();
    body.set("file", file, file.name);
    // Staging creates only an unlinked temporary upload; the business record write happens in the
    // subsequent `thread_upload`/`thread_attach` RPC. Keeping `write` unset lets `signal` abort
    // in-flight transfers and avoids false `ambiguous` recovery state or metadata cache wipes.
    const options: CallOptions = signal ? { signal } : {};
    const raw = await this.runPost(path, options, () => this.send(path, body, options, undefined, csrf));
    if (!raw || typeof raw !== "object" || Array.isArray(raw)) {
      throw new ClientError("server", "The server returned an invalid upload response.", 200, { endpoint: path, status: 200 });
    }
    const data = raw as Record<string, unknown>;
    const upload_id = data["upload_id"], size = data["size"], mimetype = data["mimetype"], checksum = data["checksum"];
    if (Object.keys(data).length !== 4
        || typeof upload_id !== "string" || upload_id === ""
        || typeof size !== "number" || !Number.isSafeInteger(size) || size < 0
        || typeof mimetype !== "string" || mimetype === ""
        || typeof checksum !== "string" || checksum === "") {
      throw new ClientError("server", "The server returned an invalid upload response.", 200, { endpoint: path, status: 200 });
    }
    return { upload_id, size, mimetype, checksum };
  }
  private async runPost(path: string, options: CallOptions, send: () => Promise<unknown>, model?: string): Promise<unknown> {
    const finish = options.background ? undefined : this.activity?.();
    try {
      this.connection?.("request", options);
      const result = await send();
      this.connection?.("online", options);
      return result;
    } catch (error) {
      if (error instanceof ClientError && (error.kind === "network" || error.kind === "ambiguous")) this.connection?.("offline", options);
      else this.connection?.(options.signal?.aborted && !options.write ? "cancelled" : "error", options);
      throw error;
    }
    finally {
      finish?.();
      if (options.write) this.written?.(model ?? (path.startsWith("/erp/module/") ? "ir.module.module" : path.split("/")[4]!));
    }
  }
  /** POST a JSON document to a download route (the pivot spreadsheet, a dynamic report as PDF or XLSX) and take the file it answers (the route answers a file, not JSON). */
  postFile(path: "/erp/pivot/export" | "/erp/reporting/export", document: unknown, type = "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet"): Promise<{ blob: Blob; fileName: string }> {
    return this.file(path, type, { method: "POST", headers: { "Content-Type": "application/json" }, body: JSON.stringify(document) });
  }
  postImpexFile(action: "template" | "export", document: object, type: string, csrf: string): Promise<{ blob: Blob; fileName: string }> {
    return this.file(`/erp/impex/${action}`, type, { method: "POST", headers: { "Content-Type": "application/json", "X-CSRF-Token": csrf }, body: JSON.stringify(document) });
  }
  /** GET a file of this server (a printed report, a stored attachment); a refusal is thrown with the server's sentence. */
  getFile(path: string, type: string): Promise<{ blob: Blob; fileName: string }> {
    return this.file(path, type, { method: "GET" });
  }
  private async file(path: string, type: string, init: { method: string; headers?: Record<string, string>; body?: string }): Promise<{ blob: Blob; fileName: string }> {
    const finish = this.activity?.();
    const requestId = newRequestId();
    const diagnostic = (status: number): ErrorDiagnostics => ({ endpoint: path.split("?")[0] ?? path, status, requestId });
    try {
      let response: Response;
      try {
        response = await this.fetcher(path, {
          ...init, credentials: "same-origin", redirect: "error", cache: "no-store",
          headers: { ...init.headers, "Accept": type, "X-Request-Id": requestId },
        });
      } catch {
        throw new ClientError("network", "Unable to connect. Check your connection and try again.", 0, diagnostic(0));
      }
      if (response.status === 401) { this.expired(); throw new ClientError("session", "Your session has expired. Sign in again.", 401, diagnostic(401)); }
      if (!response.ok) {
        // A refusal below 500 is the server's JSON error (`message`) or a plain-text sentence, shown as sent and bounded; anything else
        // (an HTML error page, a 5xx) gets the fixed sentence, never the raw body.
        const sent = response.headers.get("Content-Type") ?? "";
        let message = "";
        if (response.status < 500 && (sent.startsWith("text/plain") || sent.includes("json"))) {
          const raw = (await response.text().catch(() => "")).slice(0, 2000);
          message = raw;
          if (sent.includes("json")) {
            message = "";
            try { const parsed: unknown = JSON.parse(raw); if (parsed !== null && typeof parsed === "object" && "message" in parsed && typeof parsed.message === "string") message = parsed.message; } catch { /* no message */ }
          }
        }
        throw new ClientError(response.status === 403 ? "permission" : response.status < 500 ? "validation" : "server", message
          || (response.status >= 500 ? "The server is temporarily unavailable. Please try again shortly." : "The download was refused. Please reload and try again."), response.status, diagnostic(response.status));
      }
      const blob = await response.blob();
      if (blob.type.split(";")[0] !== type) throw new ClientError("server", "The server returned an invalid response.", response.status, diagnostic(response.status));
      return { blob, fileName: fileNameOf(response.headers.get("Content-Disposition")) };
    } finally { finish?.(); }
  }
  private async sendReviewed(path: string, body: string, options: CallOptions): Promise<unknown> {
    try {
      return await this.send(path, body, options);
    } catch (error) {
      if (!(error instanceof ApprovalChallenge) || error.request !== "key" || !this.awaitApproval) throw error;
    }
    const key = newRequestId();
    try {
      return await this.send(path, body, options, key);
    } catch (error) {
      if (!(error instanceof ApprovalChallenge) || typeof error.request !== "number") throw error;
      if (!await this.awaitApproval(error.request)) {
        throw new ClientError("validation", `Approval request ${error.request} was not executed.`);
      }
    }
    // The original bytes and key remain local to this call. A lost response or a
    // permission failure never causes another automatic write.
    return this.send(path, body, options, key);
  }
  private async send(path: string, body: string | FormData, options: CallOptions, key?: string, csrf?: string): Promise<unknown> {
    // The server answers with the id it logged; it keeps the one sent unless that one is malformed.
    let requestId = newRequestId();
    const diagnostic = (status: number): ErrorDiagnostics => ({ endpoint: path, status, requestId });
    let response: Response;
    try {
      response = await this.fetcher(path, {
        method: "POST", credentials: "same-origin", redirect: "error", cache: "no-store",
        headers: {
          "Accept": "application/json", "X-Request-Id": requestId,
          ...(typeof body === "string" ? { "Content-Type": "application/json" } : {}),
          ...(key ? { "Idempotency-Key": key } : {}),
          ...(csrf !== undefined ? { "X-CSRF-Token": csrf } : {}),
        },
        body, signal: options.write ? undefined : options.signal,
      });
    } catch (error) {
      if (!options.write && options.signal?.aborted) throw error;
      throw new ClientError(options.write ? "ambiguous" : "network", options.write
        ? "The connection was interrupted. The change may have been saved. Check the record before trying again."
        : "Unable to connect. Check your connection and try again.", 0, diagnostic(0));
    }
    const answered = response.headers.get("X-Request-Id");
    if (answered !== null && REQUEST_ID.test(answered)) requestId = answered.toLowerCase();
    if (response.status === 401) {
      this.expired();
      throw new ClientError("session", "Your session has expired. Sign in again.", 401, diagnostic(401));
    }
    if (response.status === 429) {
      throw new ClientError("server", "The server is busy. Wait a moment, then try again.", 429, diagnostic(429));
    }
    const contentType = (response.headers.get("Content-Type") ?? "").toLowerCase();
    if (csrf !== undefined && !response.ok && response.status < 500 && contentType.startsWith("text/plain")) {
      const message = (await response.text().catch(() => "")).trim().slice(0, 2000);
      const fallback = response.status === 413
        ? "The uploaded file exceeds the server's request size limit."
        : "The request was refused.";
      throw new ClientError(response.status === 403 ? "permission" : "validation", message || fallback, response.status, diagnostic(response.status));
    }
    if (response.status === 413 && !contentType.includes("json")) {
      throw new ClientError("validation", "The uploaded file exceeds the server's request size limit.", 413, diagnostic(413));
    }
    const ambiguousWrite = options.write && response.status >= 500;
    const ambiguousMessage = "The server returned an error after the request was sent. The change may have been saved. Check the record before trying again.";
    let value: unknown;
    const exactRecords = ["web_read", "web_search_read", "web_save", "default_get", "onchange", "read", "search_read"].includes(path.split("/").at(-1)!);
    try { value = await readResponseJson(response, {}, exactRecords ? "exact_decimal" : "finite"); }
    catch (error) {
      if (error instanceof AstDecodeError && error.detail === "number cannot round-trip through the record decimal contract") {
        throw new ClientError(options.write && response.ok ? "ambiguous" : "unsupported", options.write && response.ok
          ? "The change may have been saved, but its numeric response cannot round-trip safely. Check the record before another write. Lossless monetary transport is not available."
          : "This record contains a number that cannot round-trip safely. Lossless monetary transport is not available.", response.status, diagnostic(response.status));
      }
      const details = { ...diagnostic(response.status), reason: "Response could not be decoded as supported JSON." };
      if (ambiguousWrite) throw new ClientError("ambiguous", ambiguousMessage, response.status, details);
      if (response.status >= 500) {
        throw new ClientError("server", "The server is temporarily unavailable. Please try again shortly.", response.status, details);
      }
      throw new ClientError(options.write && response.ok ? "ambiguous" : "server", options.write && response.ok
        ? "The server returned an unreadable answer. Check whether the change was saved before trying again."
        : "The server returned an invalid response.", response.status, details);
    }
    if (!response.ok) {
      if (response.status === 403 && value !== null && typeof value === "object" && "name" in value && value.name === "erp.http.session.SessionExpiredException") {
        this.expired();
        throw new ClientError("session", "Your session has expired. Sign in again.", 403);
      }
      const message = value !== null && typeof value === "object" && "message" in value && typeof value.message === "string"
        ? value.message : "The request was refused. Please reload and try again.";
      // Debug is emitted only after the server's operator-config and capability checks.
      // Never include request arguments, headers, response bodies or arbitrary context.
      const source = value !== null && typeof value === "object" ? value as Record<string, unknown> : {};
      const details: ErrorDiagnostics = {
        ...diagnostic(response.status),
        ...(typeof source["name"] === "string" ? { name: source["name"].slice(0, 256) } : {}),
        ...(typeof source["timestamp"] === "number" ? { timestamp: source["timestamp"] } : {}),
        ...(typeof source["debug"] === "string" && source["debug"] ? { debug: source["debug"].slice(0, 32_768) } : {}),
      };
      const context = source["context"];
      if (path.startsWith("/erp/json/2/") && context && typeof context === "object" && !Array.isArray(context)) {
        const approval = context as Record<string, unknown>;
        if (response.status === 422 && approval["approval_key_required"] === true)
          throw new ApprovalChallenge("key", message, response.status, details);
        const id = approval["approval_request_id"];
        if (response.status === 409 && typeof id === "number" && Number.isSafeInteger(id) && id > 0)
          throw new ApprovalChallenge(id, message, response.status, details);
      }
      if (ambiguousWrite) throw new ClientError("ambiguous", ambiguousMessage, response.status, details);
      const kind: FailureKind = response.status === 403 ? "permission" : response.status < 500 ? "validation" : "server";
      throw new ClientError(kind, message, response.status, details, kind === "validation" ? refusedFields(source["context"]) : []);
    }
    return value;
  }
}
