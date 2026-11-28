/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
import { expect, test, vi } from "vitest";
import { RpcClient, ClientError, newRequestId, refusedFields } from "../src/client/transport.js";
import { RecordState } from "../src/client/record.js";
import type { Field } from "../src/client/metadata.js";
import { parseRoute, routeUrl } from "../src/client/route.js";

const recordFields: Field[] = [{ name: "name", kind: "char", label: "Name", readonly: false, required: true, selection: null, relation: null, relationField: null }];

test("newRequestId generates a valid v4 UUID via getRandomValues when randomUUID is absent", () => {
  const realCrypto = globalThis.crypto;
  const fallbackCrypto = { getRandomValues: realCrypto.getRandomValues.bind(realCrypto) };
  vi.stubGlobal("crypto", fallbackCrypto);
  try {
    const id = newRequestId();
    expect(id).toMatch(/^[0-9a-f]{8}-[0-9a-f]{4}-4[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$/);
  } finally {
    vi.unstubAllGlobals();
  }
});

test("JSON2 sends same-origin cookies and returns the unwrapped method value", async () => {
  const fetcher = vi.fn(async () => new Response('{"records":[],"length":0}', { headers: { "Content-Type": "application/json" } }));
  const client = new RpcClient(fetcher, () => {});
  expect(await client.call("acceptance.note", "web_search_read", { domain: [] })).toEqual({ records: [], length: 0 });
  expect(fetcher.mock.calls[0]).toEqual(["/erp/json/2/acceptance.note/web_search_read", expect.objectContaining({
    method: "POST", credentials: "same-origin", redirect: "error", body: '{"domain":[]}',
  })]);
});

test("a JSON-2 request carries exactly the expected headers and never X-reference-Database", async () => {
  // TO-SHELL.2: the database comes from the host and the session cookie, never from a request header.
  const fetcher = vi.fn(async (_input: RequestInfo | URL, _init?: RequestInit) => new Response("null", { headers: { "Content-Type": "application/json" } }));
  const client = new RpcClient(fetcher, () => {});
  await client.call("acceptance.note", "web_search_read", {});
  await client.call("acceptance.note", "web_save", {}, { write: true });
  expect(fetcher).toHaveBeenCalledTimes(2);
  for (const [, init] of fetcher.mock.calls) {
    expect(init).toBeDefined();
    const headers = new Headers(init!.headers);
    expect([...headers.keys()].sort()).toEqual(["accept", "content-type", "x-request-id"]);
    expect(headers.has("X-reference-Database")).toBe(false);
    expect(headers.get("Content-Type")).toBe("application/json");
    expect(headers.get("Accept")).toBe("application/json");
    expect(headers.get("X-Request-Id")).toMatch(/^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/i);
    expect(Object.keys(init!).sort()).toEqual(["body", "cache", "credentials", "headers", "method", "redirect", "signal"]);
  }
});

test("auth expiry clears protected state before surfacing the typed failure", async () => {
  const expired = vi.fn();
  const client = new RpcClient(async () => new Response('{"name":"AccessDenied","message":"Sign in again"}', { status: 401 }), expired);
  await expect(client.call("res.users", "web_read", {})).rejects.toMatchObject({ kind: "session", message: "Your session has expired. Sign in again." });
  expect(expired).toHaveBeenCalledOnce();
});

test("ambiguous writes are never replayed, and response bodies cannot inject markup", async () => {
  const fetcher = vi.fn(async () => { throw new TypeError("network down"); });
  const client = new RpcClient(fetcher, () => {});
  await expect(client.call("acceptance.note", "web_save", {}, { write: true })).rejects.toMatchObject({ kind: "ambiguous" });
  expect(fetcher).toHaveBeenCalledOnce();
  const denied = new RpcClient(async () => new Response('{"name":"erp.exceptions.AccessError","message":"You cannot edit this record."}', { status: 403 }), () => {});
  await expect(denied.call("res.users", "web_save", {}, { write: true })).rejects.toEqual(new ClientError("permission", "You cannot edit this record.", 403, { endpoint: "/erp/json/2/res.users/web_save", status: 403, name: "erp.exceptions.AccessError", requestId: expect.any(String) }));
});

test("strict route grammar preserves decimal ids and rejects unknown or duplicate state", () => {
  const route = parseRoute(new URL("https://erp.example/erp/action-20?menu=10&id=9007199254740991&view_type=form&cids=2,1"));
  expect(routeUrl(route)).toBe("/erp/action-20?menu=10&id=9007199254740991&view_type=form&cids=2%2C1");
  for (const path of ["/erp?action=01", "/erp/action-20?domain=[]", "/erp/action-20?id=1&id=2&view_type=form", "/erp/action-20?id=1", "/erp/action-20#x", "/erp/action-20?cids=1,1"]) {
    expect(() => parseRoute(new URL(path, "https://erp.example"))).toThrow(ClientError);
  }
});

test("real JSON2 session-expired 403 clears state while ordinary access 403 does not", async () => {
  const expired = vi.fn();
  const response = (name: string) => new Response(JSON.stringify({ name, message: "Access denied", arguments: ["Access denied"], timestamp: 0, context: {}, debug: "" }), { status: 403, headers: { "Content-Type": "application/json" } });
  const session = new RpcClient(async () => response("erp.http.session.SessionExpiredException"), expired);
  await expect(session.call("test.note", "web_read", {})).rejects.toMatchObject({ kind: "session", status: 403 });
  expect(expired).toHaveBeenCalledOnce();
  const denied = new RpcClient(async () => response("erp.exceptions.AccessError"), expired);
  await expect(denied.call("test.note", "web_read", {})).rejects.toMatchObject({ kind: "permission", status: 403 });
  expect(expired).toHaveBeenCalledOnce();
});

test.each([
  ["HTML 503", 503, new Response("<html>upstream unavailable</html>", { status: 503, headers: { "Content-Type": "text/html" } })],
  ["JSON 502", 502, new Response(JSON.stringify({ name: "GatewayError", message: "upstream unavailable" }), { status: 502, headers: { "Content-Type": "application/json" } })],
])("write %s stays ambiguous and blocks a blind record retry", async (_label, status, failure) => {
  let writes = 0;
  const client = new RpcClient(async path => {
    if (String(path).endsWith("web_read")) return new Response('[{"id":5,"name":"Before"}]', { headers: { "Content-Type": "application/json" } });
    writes++;
    return failure;
  }, () => {});
  const record = new RecordState(client, "test.note", recordFields, {});
  await record.load(5);
  record.change("name", "After");

  await expect(record.save()).rejects.toEqual(new ClientError(
    "ambiguous",
    "The server returned an error after the request was sent. The change may have been saved. Check the record before trying again.",
    status,
    status === 503
      ? { endpoint: "/erp/json/2/test.note/web_save", status, reason: "Response could not be decoded as supported JSON.", requestId: expect.any(String) }
      : { endpoint: "/erp/json/2/test.note/web_save", status, name: "GatewayError", requestId: expect.any(String) },
  ));
  expect(record.values["name"]).toBe("After");
  expect(record.dirty).toBe(true);
  await expect(record.save()).rejects.toMatchObject({ kind: "ambiguous" });
  expect(writes).toBe(1);
});

test("non-JSON read 5xx reports temporary unavailability and preserves HTTP status", async () => {
  const client = new RpcClient(async () => new Response("<html>maintenance</html>", { status: 503, headers: { "Content-Type": "text/html" } }), () => {});
  await expect(client.call("res.lang", "web_search_read", {})).rejects.toEqual(
    new ClientError("server", "The server is temporarily unavailable. Please try again shortly.", 503, { endpoint: "/erp/json/2/res.lang/web_search_read", status: 503, reason: "Response could not be decoded as supported JSON.", requestId: expect.any(String) }),
  );
});

test("429 shows a busy message for reads and writes without retrying", async () => {
  const fetcher = vi.fn(async () => new Response("rate limited", { status: 429, headers: { "Content-Type": "text/plain" } }));
  const client = new RpcClient(fetcher, () => {});

  await expect(client.call("res.lang", "web_search_read", {})).rejects.toEqual(
    new ClientError("server", "The server is busy. Wait a moment, then try again.", 429, { endpoint: "/erp/json/2/res.lang/web_search_read", status: 429, requestId: expect.any(String) }),
  );
  await expect(client.call("test.note", "web_save", {}, { write: true })).rejects.toEqual(
    new ClientError("server", "The server is busy. Wait a moment, then try again.", 429, { endpoint: "/erp/json/2/test.note/web_save", status: 429, requestId: expect.any(String) }),
  );
  expect(fetcher).toHaveBeenCalledTimes(2);
});


test("legacy shell aliases parse but emit only canonical ERP destinations", () => {
  for (const path of ["/erp", "/odoo", "/web"]) {
    expect(routeUrl(parseRoute(new URL(path, "https://erp.example")))).toBe("/erp");
    expect(routeUrl(parseRoute(new URL(path + "?action=20&menu=10", "https://erp.example")))).toBe("/erp/action-20?menu=10");
  }
  for (const path of ["/erp/action-20", "/odoo/action-20"]) {
    expect(routeUrl(parseRoute(new URL(path + "?id=5&view_type=form", "https://erp.example")))).toBe("/erp/action-20?id=5&view_type=form");
  }
});

test("ERP transport accepts the installer and refuses legacy mutation paths before fetch", async () => {
  const fetcher = vi.fn(async () => new Response("{}", {headers:{"Content-Type":"application/json"}}));
  const client = new RpcClient(fetcher, () => {});
  await client.post("/erp/module/install", {modules:["acceptance_notes"]}, {write:true});
  expect(fetcher.mock.calls[0]).toEqual(["/erp/module/install", expect.objectContaining({method:"POST"})]);
  for (const path of ["/web/module/install", "/json/2/res.users/web_read", "/erp/json/2/res.users/web_read/extra", "//evil.example/erp/module/install"]) {
    await expect(client.post(path, {})).rejects.toEqual(new ClientError("validation", "Unsupported request path."));
  }
  expect(fetcher).toHaveBeenCalledOnce();
});

// Shared transport regressions apply to every model, not only res.users.
test.each(["res.users", "res.company", "res.lang", "res.groups", "ir.rule", "acceptance.note"])("%s preserves server-authorized diagnostics without request values", async model => {
  const client = new RpcClient(async () => new Response(JSON.stringify({ name: "erp.exceptions.ValidationError", message: "Cannot open this record.", timestamp: 123, context: {}, debug: "Authorized server trace" }), { status: 400 }), () => {});
  await expect(client.call(model, "web_read", { secret: "not-for-diagnostics" })).rejects.toMatchObject({
    diagnostics: { endpoint: `/erp/json/2/${model}/web_read`, status: 400, name: "erp.exceptions.ValidationError", debug: "Authorized server trace" }
  });
});

test("invalid JSON retains a bounded reason and endpoint without reflecting its body", async () => {
  const client = new RpcClient(async () => new Response("<html>private-body</html>"), () => {});
  try { await client.call("res.users", "web_read", {}); throw new Error("Expected rejection"); }
  catch (error) {
    expect(error).toBeInstanceOf(ClientError);
    const details = (error as ClientError & { diagnostics: object }).diagnostics;
    expect(details).toMatchObject({ endpoint: "/erp/json/2/res.users/web_read", status: 200, reason: "Response could not be decoded as supported JSON." });
    expect(JSON.stringify(details)).not.toContain("private-body");
  }
});


test("failed writes retain authorized backend trace while staying ambiguous and never replayed", async () => {
  const fetcher = vi.fn(async () => new Response(JSON.stringify({ name: "InternalError", message: "Internal failure", debug: "Authorized trace" }), { status: 500 }));
  const client = new RpcClient(fetcher, () => {});
  await expect(client.call("test.note", "web_save", {}, { write: true })).rejects.toMatchObject({ kind: "ambiguous", diagnostics: { status: 500, debug: "Authorized trace" } });
  expect(fetcher).toHaveBeenCalledOnce();
});

test("every write attempt reports its model, and background reads show no activity", async () => {
  const written = vi.fn(), finish = vi.fn(), activity = vi.fn(() => finish);
  const ok = new RpcClient(async () => new Response("[]", { headers: { "Content-Type": "application/json" } }), () => {}, activity, written);
  await ok.call("ir.ui.view", "web_save", {}, { write: true });
  expect(written).toHaveBeenLastCalledWith("ir.ui.view");
  await ok.post("/erp/module/install", { modules: ["x"] }, { write: true });
  expect(written).toHaveBeenLastCalledWith("ir.module.module");
  await ok.call("ir.ui.view", "web_read", {});
  expect(written).toHaveBeenCalledTimes(2);
  expect(activity).toHaveBeenCalledTimes(3);
  await ok.call("ir.actions.act_window", "erp_load_web_action", {}, { background: true });
  expect(activity).toHaveBeenCalledTimes(3);
  const failing = new RpcClient(async () => { throw new TypeError("network down"); }, () => {}, undefined, written);
  await expect(failing.call("res.groups", "web_save", {}, { write: true })).rejects.toMatchObject({ kind: "ambiguous" });
  expect(written).toHaveBeenLastCalledWith("res.groups");
});

/// (partial: the call sends an id and the failure carries the one the server answered with)
test("a call sends a request id and a refusal carries the id the server answered with", async () => {
  const logged = "0b7c2a4e-5d1f-4c3a-9e8b-1a2b3c4d5e6f";
  const fetcher = vi.fn(async (_path: RequestInfo | URL, _init?: RequestInit) => new Response(JSON.stringify({ name: "erp.exceptions.ValidationError", message: "Invalid", request_id: logged }), { status: 422, headers: { "Content-Type": "application/json", "X-Request-Id": logged } }));
  const client = new RpcClient(fetcher, () => {});
  const failure = await client.call("test.note", "web_read", {}).catch((error: unknown) => error);
  expect(failure).toBeInstanceOf(ClientError);
  expect((failure as ClientError).diagnostics?.requestId).toBe(logged);
  const sent = fetcher.mock.calls[0]![1]!.headers as Record<string, string>;
  expect(sent["X-Request-Id"]).toMatch(/^[0-9a-f]{8}-[0-9a-f]{4}-4[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$/);
});

/// (partial: no answer keeps the sent id; a malformed answered id is ignored)
test("a call that gets no answer keeps the id it sent, and a malformed answered id is not shown", async () => {
  let sent = "";
  const lost = new RpcClient(async (_path, init) => { sent = (init!.headers as Record<string, string>)["X-Request-Id"]!; throw new TypeError("offline"); }, () => {});
  const offline = await lost.call("test.note", "web_read", {}).catch((error: unknown) => error) as ClientError;
  expect(offline.kind).toBe("network");
  expect(offline.diagnostics?.requestId).toBe(sent);
  const odd = new RpcClient(async (_path, init) => {
    sent = (init!.headers as Record<string, string>)["X-Request-Id"]!;
    return new Response('{"message":"no"}', { status: 400, headers: { "Content-Type": "application/json", "X-Request-Id": "<script>" } });
  }, () => {});
  const refused = await odd.call("test.note", "web_read", {}).catch((error: unknown) => error) as ClientError;
  expect(refused.diagnostics?.requestId).toBe(sent);
});

test("a refusal carries the fields its error context names, as a list or as one field", async () => {
  const refusal = (context: unknown, status = 422) => vi.fn(async () => new Response(JSON.stringify({ name: "erp.exceptions.ValidationError", message: "Refused", context }), { status, headers: { "Content-Type": "application/json" } }));
  const failure = async (fetcher: ReturnType<typeof refusal>): Promise<ClientError> => await new RpcClient(fetcher, () => {}).call("test.note", "web_save", {}, { write: true }).catch((error: unknown) => error) as ClientError;
  expect((await failure(refusal({ fields: ["date_start", "date_end"] }))).fields).toEqual(["date_start", "date_end"]);
  expect((await failure(refusal({ field: "nik" }))).fields).toEqual(["nik"]);
  expect((await failure(refusal({}))).fields).toEqual([]);
  // an access refusal is not a field error
  expect((await failure(refusal({ fields: ["name"] }, 403))).fields).toEqual([]);
});

test("only field names are taken from an error context, each once and at most 64", () => {
  expect(refusedFields({ fields: ["name", "Name", "a.b", 3, "name"], field: "ref", blocking_model: "res.partner" })).toEqual(["name", "ref"]);
  expect(refusedFields(null)).toEqual([]);
  expect(refusedFields(["name"])).toEqual([]);
  expect(refusedFields({ fields: "name" })).toEqual([]);
  expect(refusedFields({ fields: Array.from({ length: 80 }, (_, i) => `f${i}`) })).toHaveLength(64);
});

test("File import posts the original file with isolated browser options and only commits invalidate model data", async () => {
  const fetcher = vi.fn(async (_path: RequestInfo | URL, _init?: RequestInit) => new Response("{}", { headers: { "Content-Type": "application/json" } }));
  const written = vi.fn();
  const client = new RpcClient(fetcher, () => {}, undefined, written);
  const file = new File(["Name\r\nExample"], "notes.csv", { type: "text/csv" });
  const options = { model: "test.note", format: "csv", context: {}, fields: ["name"] };
  for (const action of ["preview", "test", "import"] as const) await client.postImport(action, file, options, "test-csrf");
  expect(fetcher.mock.calls.map(([path]) => path)).toEqual(["/erp/impex/preview", "/erp/impex/test", "/erp/impex/import"]);
  for (const [, init] of fetcher.mock.calls) {
    expect(init?.credentials).toBe("same-origin");
    const headers = new Headers(init?.headers);
    expect(headers.has("content-type")).toBe(false); // fetch adds the boundary for FormData.
    expect(headers.get("x-csrf-token")).toBe("test-csrf");
    const body = init?.body as FormData;
    expect(body.get("file")).toBe(file);
    expect(JSON.parse(body.get("options") as string)).toEqual(options);
  }
  expect(written).toHaveBeenCalledExactlyOnceWith("test.note");
});

test("The export route posts JSON and preserves the filename of the returned file", async () => {
  const fetcher = vi.fn(async (_path: RequestInfo | URL, _init?: RequestInit) => new Response("Name\r\nExample", {
    headers: { "Content-Type": "text/csv;charset=utf-8", "Content-Disposition": 'attachment; filename="notes.csv"' },
  }));
  const client = new RpcClient(fetcher, () => {});
  const options = { model: "test.note", format: "csv", ids: [5] };
  const file = await client.postImpexFile("export", options, "text/csv", "test-csrf");
  const [path, init] = fetcher.mock.calls[0]!;
  expect(path).toBe("/erp/impex/export");
  expect(new Headers(init?.headers).get("x-csrf-token")).toBe("test-csrf");
  expect(JSON.parse(init!.body as string)).toEqual(options);
  expect(file.fileName).toBe("notes.csv");
  expect(await file.blob.text()).toBe("Name\r\nExample");
});

test("An upload refusal shows the server's plain-text request limit", async () => {
  const message = "The import request exceeds its size limit.";
  const client = new RpcClient(async () => new Response(message, { status: 413 }), () => {});
  await expect(client.postImport("preview", new File([], "notes.csv"), { model: "test.note" }, "test-csrf"))
    .rejects.toMatchObject({ kind: "validation", status: 413, message });
});

test("postAttachmentUpload streams FormData with CSRF, validates shape, handles 413, and supports AbortSignal", async () => {
  const written = vi.fn();
  const fetcher = vi.fn(async (_path: RequestInfo | URL, _init?: RequestInit) => new Response(JSON.stringify({
    upload_id: "01950000-0000-7000-8000-000000000001",
    size: 4096,
    mimetype: "image/jpeg",
    checksum: "2aae6c35c94fcfb415dbe95f408b9ce91ee846ed",
  }), { headers: { "Content-Type": "application/json" } }));
  const client = new RpcClient(fetcher, () => {}, undefined, written);
  const file = new File([new Uint8Array(4096)], "photo.jpg", { type: "image/jpeg" });
  const result = await client.postAttachmentUpload(file, "csrf-1");
  expect(result).toEqual({
    upload_id: "01950000-0000-7000-8000-000000000001",
    size: 4096,
    mimetype: "image/jpeg",
    checksum: "2aae6c35c94fcfb415dbe95f408b9ce91ee846ed",
  });
  const [path, init] = fetcher.mock.calls[0]!;
  expect(path).toBe("/erp/attachments/upload");
  expect(init?.credentials).toBe("same-origin");
  expect(new Headers(init?.headers).get("x-csrf-token")).toBe("csrf-1");
  const uploaded = (init?.body as FormData).get("file") as File;
  expect(uploaded.name).toBe("photo.jpg");
  expect(uploaded.size).toBe(4096);
  expect(written).not.toHaveBeenCalled();

  // Rejects malformed or extra-key upload payloads and empty CSRF.
  await expect(client.postAttachmentUpload(file, "")).rejects.toMatchObject({ kind: "validation" });
  for (const badPayload of [
    null,
    [],
    { upload_id: "", size: 1, mimetype: "image/jpeg", checksum: "abc" },
    { upload_id: "u1", size: -1, mimetype: "image/jpeg", checksum: "abc" },
    { upload_id: "u1", size: 1, mimetype: "", checksum: "abc" },
    { upload_id: "u1", size: 1, mimetype: "image/jpeg", checksum: "" },
    { upload_id: "u1", size: 1, mimetype: "image/jpeg", checksum: "abc", extra: true },
  ]) {
    const badClient = new RpcClient(async () => new Response(JSON.stringify(badPayload), { headers: { "Content-Type": "application/json" } }), () => {});
    await expect(badClient.postAttachmentUpload(file, "csrf-1")).rejects.toMatchObject({ kind: "server" });
  }

  // Handles reverse-proxy HTML 413 and empty text/plain 413 cleanly.
  for (const res of [
    new Response("<html>413 Request Entity Too Large</html>", { status: 413, headers: { "Content-Type": "text/html" } }),
    new Response("", { status: 413, headers: { "Content-Type": "TEXT/PLAIN; charset=utf-8" } }),
  ]) {
    const capped = new RpcClient(async () => res, () => {});
    await expect(capped.postAttachmentUpload(file, "csrf-1")).rejects.toMatchObject({
      kind: "validation",
      status: 413,
      message: "The uploaded file exceeds the server's request size limit.",
    });
  }

  // Passes AbortSignal through to fetch and propagates cancellation.
  const controller = new AbortController();
  let seenSignal: AbortSignal | null | undefined;
  const abortable = new RpcClient(async (_p, init) => {
    seenSignal = init?.signal;
    throw new DOMException("Aborted", "AbortError");
  }, () => {});
  controller.abort();
  await expect(abortable.postAttachmentUpload(file, "csrf-1", controller.signal)).rejects.toMatchObject({ name: "AbortError" });
  expect(seenSignal).toBe(controller.signal);
});
