/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
import { afterEach, beforeEach, expect, test, vi } from "vitest";
import { MessagingStore } from "@erpforall/mail-store";
import { RpcClient } from "../src/client/transport.js";
import { Chatter } from "../src/client/chatter.js";
import { RecordForm } from "../src/client/record-form.js";
import { RecordState } from "../src/client/record.js";
import "../src/client/fields.js";
import type { Field, Node, View } from "../src/client/metadata.js";

const fieldSpec = (kind: string, extra: Record<string, unknown> = {}): Record<string, unknown> => ({ kind, ...extra });

const TEST_SCHEMA = JSON.stringify({
  wire: 1,
  schema: "chatter-test-s1",
  models: {
    "res.partner": { fields: { name: fieldSpec("string") } },
    "ir.attachment": { fields: { name: fieldSpec("string"), mimetype: fieldSpec("string"), file_size: fieldSpec("int"), checksum: fieldSpec("string"), type: fieldSpec("selection"), url: fieldSpec("string") } },
    "mail.message.reaction": { fields: { content: fieldSpec("string"), partner_id: fieldSpec("many2one", { target: "res.partner" }) } },
    "mail.thread": { fields: { name: fieldSpec("string") } },
    "mail.message.subtype": { fields: { name: fieldSpec("string"), internal: fieldSpec("bool") } },
    "mail.followers": {
      fields: {
        res_model: fieldSpec("string"),
        res_id: fieldSpec("int"),
        partner_id: fieldSpec("many2one", { target: "res.partner" }),
        subtype_ids: fieldSpec("x2many", { target: "mail.message.subtype" }),
      },
    },
    "mail.message": {
      fields: {
        body: fieldSpec("html"),
        subject: fieldSpec("string"),
        message_type: fieldSpec("selection"),
        date: fieldSpec("datetime"),
        author_id: fieldSpec("many2one", { target: "res.partner" }),
        parent_id: fieldSpec("many2one", { target: "mail.message" }),
        is_internal: fieldSpec("bool"),
        tracking: fieldSpec("string"),
        attachment_ids: fieldSpec("x2many", { target: "ir.attachment" }),
        reactions: fieldSpec("x2many", { target: "mail.message.reaction" }),
        starred: fieldSpec("bool"),
        pinned_at: fieldSpec("datetime"),
        edited_at: fieldSpec("datetime"),
      },
    },
  },
});

function answerPayload(xmax: number, records: Record<string, readonly unknown[]>): string {
  return JSON.stringify({
    wire: 1,
    schema: "chatter-test-s1",
    asof: { xmin: "1", xmax: String(xmax), xip: [], own: null },
    records,
  });
}

function flushAsync(): Promise<void> {
  return new Promise((resolve) => setTimeout(resolve, 20));
}

let host: HTMLElement;

beforeEach(() => {
  host = document.createElement("div");
  document.body.append(host);
});

afterEach(() => {
  host.remove();
});

/// (partial: chatter mounts and renders message list with author date body and tracking)
test("chatter mounts and renders message list through thread load RPC newest first with author date body and tracking", async () => {
  const trackingData = JSON.stringify([
    { f: 1, n: "stage_id", t: "many2one", o: 1, v: 2, od: "Draft", vd: "Done" },
  ]);

  const calls: { path: string; body: Record<string, unknown> }[] = [];
  const rpc = new RpcClient(async (path, init) => {
    const body = JSON.parse(String(init?.body)) as Record<string, unknown>;
    calls.push({ path: String(path), body });

    if (String(path).endsWith("/mail.store/store_fetch")) {
      return new Response(
        JSON.stringify({
          store: answerPayload(10, {
            "res.partner": [{ id: 1, name: "Alice Adams" }, { id: 2, name: "Bob Brown" }],
            "mail.message": [
              {
                id: 101,
                body: "<p>Older message</p>",
                message_type: "comment",
                date: "2026-11-10 10:00:00",
                author_id: 1,
              },
              {
                id: 102,
                body: "<p>Newer status update</p>",
                message_type: "comment",
                date: "2026-11-10 10:30:00",
                author_id: 2,
                tracking: trackingData,
              },
            ],
          }),
          topics: [
            { topic: "schema", ok: true, data: JSON.parse(TEST_SCHEMA) },
            {
              topic: "thread",
              ok: true,
              data: {
                thread: { model: "res.partner", id: 42, can_post: true },
                messages: {
                  model: "res.partner",
                  id: 42,
                  message_ids: [102, 101],
                  has_more_before: false,
                  has_more_after: false,
                },
              },
            },
          ],
        }),
        { headers: { "Content-Type": "application/json" } }
      );
    }
    throw new Error(`Unexpected path: ${path}`);
  }, () => {});

  const chatter = new Chatter({
    host,
    model: "res.partner",
    recordId: 42,
    rpc,
  });

  await flushAsync();

  expect(calls.length).toBeGreaterThanOrEqual(1);
  const fetchCall = calls.find((c) => c.path.endsWith("/mail.store/store_fetch"));
  expect(fetchCall).toBeDefined();

  const feed = host.querySelector<HTMLElement>(".erp-mail-feed");
  expect(feed).not.toBeNull();

  const articles = Array.from(feed!.querySelectorAll<HTMLElement>("article.erp-mail-message"));
  expect(articles).toHaveLength(2);

  // Newest first (TH-01): id 102 before id 101
  expect(articles[0]!.textContent).toContain("Newer status update");
  expect(articles[0]!.textContent).toContain("Bob Brown");
  expect(articles[1]!.textContent).toContain("Older message");
  expect(articles[1]!.textContent).toContain("Alice Adams");

  // Tracking line rendering
  const trackingItem = articles[0]!.querySelector(".erp-mail-tracking-change");
  expect(trackingItem).not.toBeNull();
  expect(trackingItem?.getAttribute("data-field")).toBe("stage_id");
  expect(trackingItem?.querySelector(".erp-mail-tracking-old")?.textContent).toBe("Draft");
  expect(trackingItem?.querySelector(".erp-mail-tracking-new")?.textContent).toBe("Done");

  chatter.dispose();
});

/// (partial: chatter renders empty state when thread has no messages)
test("chatter renders empty state when thread has no messages", async () => {
  const rpc = new RpcClient(async (path) => {
    if (String(path).endsWith("/mail.store/store_fetch")) {
      return new Response(
        JSON.stringify({
          store: answerPayload(10, {
            "res.partner": [],
            "mail.message": [],
          }),
          topics: [
            { topic: "schema", ok: true, data: JSON.parse(TEST_SCHEMA) },
            {
              topic: "thread",
              ok: true,
              data: {
                thread: { model: "res.partner", id: 42, can_post: true },
                messages: {
                  model: "res.partner",
                  id: 42,
                  message_ids: [],
                  has_more_before: false,
                  has_more_after: false,
                },
              },
            },
          ],
        }),
        { headers: { "Content-Type": "application/json" } }
      );
    }
    throw new Error(`Unexpected path: ${path}`);
  }, () => {});

  const chatter = new Chatter({
    host,
    model: "res.partner",
    recordId: 42,
    rpc,
  });

  await flushAsync();

  const empty = host.querySelector<HTMLElement>(".erp-mail-empty");
  expect(empty).not.toBeNull();
  expect(empty?.hidden).toBe(false);
  expect(empty?.textContent).toContain("No messages yet");

  chatter.dispose();
});

/// (partial: chatter error state on store_fetch failure shows alert with retry)
test("chatter error state on store_fetch failure shows alert with retry", async () => {
  let attempt = 0;
  const errors: unknown[] = [];
  const rpc = new RpcClient(async (path) => {
    if (String(path).endsWith("/mail.store/store_fetch")) {
      attempt++;
      if (attempt === 1) {
        throw new Error("Network connection dropped");
      }
      return new Response(
        JSON.stringify({
          store: answerPayload(10, {
            "res.partner": [{ id: 1, name: "Alice" }],
            "mail.message": [
              {
                id: 201,
                body: "<p>Message after recovery</p>",
                message_type: "comment",
                date: "2026-11-10 11:00:00",
                author_id: 1,
              },
            ],
          }),
          topics: [
            { topic: "schema", ok: true, data: JSON.parse(TEST_SCHEMA) },
            {
              topic: "thread",
              ok: true,
              data: {
                thread: { model: "res.partner", id: 42, can_post: true },
                messages: {
                  model: "res.partner",
                  id: 42,
                  message_ids: [201],
                  has_more_before: false,
                  has_more_after: false,
                },
              },
            },
          ],
        }),
        { headers: { "Content-Type": "application/json" } }
      );
    }
    throw new Error(`Unexpected path: ${path}`);
  }, () => {});

  const chatter = new Chatter({
    host,
    model: "res.partner",
    recordId: 42,
    rpc,
    reportError: (err) => errors.push(err),
  });

  await flushAsync();

  // Thread status alert should be displayed
  const errorEl = host.querySelector<HTMLElement>(".erp-mail-thread-error");
  expect(errorEl).not.toBeNull();
  expect(errorEl?.getAttribute("role")).toBe("alert");
  expect(errorEl?.textContent).toContain("Unable to connect");

  // Clicking retry should attempt to reload
  const retryBtn = errorEl?.querySelector<HTMLButtonElement>("button");
  expect(retryBtn).not.toBeNull();
  retryBtn?.click();

  await flushAsync();

  // the retry fetched the thread; the followers read follows it
  expect(attempt).toBeGreaterThanOrEqual(2);
  const feed = host.querySelector<HTMLElement>(".erp-mail-feed");
  expect(feed?.textContent).toContain("Message after recovery");

  chatter.dispose();
});

/// (partial: an author without a name shows the placeholder without any reporter call; the composer hint substitutes its chord)
test("chatter shows a placeholder for an author the store has no name for, calls no reporter, and fills the composer hint", async () => {
  const reporter = vi.fn();
  const warn = vi.spyOn(console, "warn").mockImplementation(() => {});
  const rpc = new RpcClient(async (path) => {
    if (String(path).endsWith("/mail.store/store_fetch")) {
      return new Response(
        JSON.stringify({
          store: answerPayload(10, {
            "res.partner": [{ id: 1, name: "Alice" }],
            "mail.message": [
              { id: 1, body: "<p>a</p>", message_type: "comment", date: "2026-11-10 10:00:00", author_id: 1 },
              { id: 2, body: "<p>b</p>", message_type: "comment", date: "2026-11-10 10:01:00", author_id: 9 },
            ],
          }),
          topics: [
            { topic: "schema", ok: true, data: JSON.parse(TEST_SCHEMA) },
            {
              topic: "thread",
              ok: true,
              data: {
                thread: { model: "res.partner", id: 42, can_post: true },
                messages: { model: "res.partner", id: 42, message_ids: [2, 1], has_more_before: false, has_more_after: false },
              },
            },
          ],
        }),
        { headers: { "Content-Type": "application/json" } },
      );
    }
    throw new Error(`Unexpected path: ${path}`);
  }, () => {});
  const chatter = new Chatter({ host, model: "res.partner", recordId: 42, rpc, reportError: reporter });
  await flushAsync();
  const authors = Array.from(host.querySelectorAll(".erp-mail-author")).map((a) => a.textContent);
  expect(authors).toEqual(["Partner #9", "Alice"]);
  expect(reporter).not.toHaveBeenCalled();
  expect(host.querySelector(".erp-mail-thread-error")).toBeNull();
  const hints = Array.from(host.querySelectorAll(".erp-mail-hint, .erp-mail-sr-only")).map((n) => n.textContent ?? "").filter((t) => t.startsWith("Press"));
  expect(hints).toHaveLength(1);
  expect(hints[0]).toMatch(/^Press .+ to send$/);
  expect(hints[0]).not.toContain("%s");
  warn.mockRestore();
  chatter.dispose();
});

/// (partial: chatter composer switches between send message and log note and posts via thread_post)
test("chatter composer switches between send message and log note and posts via thread_post", async () => {
  let postCall: { path: string; body: Record<string, unknown> } | undefined;
  let fetchCount = 0;

  const rpc = new RpcClient(async (path, init) => {
    const body = init?.body ? (JSON.parse(String(init.body)) as Record<string, unknown>) : {};
    if (String(path).endsWith("/mail.store/store_fetch")) {
      fetchCount++;
      const ids = fetchCount > 1 ? [301] : [];
      return new Response(
        JSON.stringify({
          store: answerPayload(10 + fetchCount * 10, {
            "res.partner": [{ id: 1, name: "Alice" }],
            "mail.message": ids.map((id) => ({
              id,
              body: "<p>First note</p>",
              message_type: "comment",
              date: "2026-11-10 12:00:00",
              author_id: 1,
            })),
          }),
          topics: [
            { topic: "schema", ok: true, data: JSON.parse(TEST_SCHEMA) },
            {
              topic: "thread",
              ok: true,
              data: {
                thread: { model: "res.partner", id: 42, can_post: true },
                messages: {
                  model: "res.partner",
                  id: 42,
                  message_ids: ids,
                  has_more_before: false,
                  has_more_after: false,
                },
              },
            },
          ],
        }),
        { headers: { "Content-Type": "application/json" } }
      );
    }
    if (String(path).endsWith("/res.partner/thread_post")) {
      postCall = { path: String(path), body };
      return new Response(
        JSON.stringify({
          message_id: 301,
          replayed: false,
          notified_partner_ids: [],
          store: answerPayload(30, {
            "res.partner": [{ id: 1, name: "Alice" }],
            "mail.message": [
              {
                id: 301,
                body: "<p>First note</p>",
                message_type: "comment",
                date: "2026-11-10 12:00:00",
                author_id: 1,
              },
            ],
          }),
        }),
        { headers: { "Content-Type": "application/json" } }
      );
    }
    throw new Error(`Unexpected path: ${path}`);
  }, () => {});

  const chatter = new Chatter({
    host,
    model: "res.partner",
    recordId: 42,
    rpc,
  });

  await flushAsync();

  const noteBtn = host.querySelector<HTMLButtonElement>('button.erp-mail-kind[data-kind="note"]');
  const msgBtn = host.querySelector<HTMLButtonElement>('button.erp-mail-kind[data-kind="message"]');
  expect(noteBtn).not.toBeNull();
  expect(msgBtn).not.toBeNull();

  // Switch to Log note
  noteBtn?.click();
  await flushAsync();

  const draft = host.querySelector<HTMLTextAreaElement>("textarea.erp-mail-draft");
  expect(draft).not.toBeNull();
  draft!.value = "First note";
  draft!.dispatchEvent(new Event("input", { bubbles: true }));

  const sendBtn = host.querySelector<HTMLButtonElement>("button.erp-mail-send");
  expect(sendBtn).not.toBeNull();
  sendBtn?.click();

  await flushAsync();

  expect(postCall).toBeDefined();
  expect(postCall?.body["ids"]).toEqual([42]);
  const post = postCall?.body["post"] as Record<string, unknown>;
  expect(post["body"]).toBe("First note");
  expect(post["subtype"]).toBe("note");
  expect(typeof post["client_uid"]).toBe("string");

  // Reload the list after a post
  await flushAsync();
  expect(fetchCount).toBeGreaterThanOrEqual(2);

  chatter.dispose();
});

/// (partial: reload_on_post triggers parent form reload after message post)
test("reload_on_post triggers parent form reload after message post", async () => {
  let reloadedForm = false;
  const reloadForm = vi.fn(async () => {
    reloadedForm = true;
  });

  const rpc = new RpcClient(async (path) => {
    if (String(path).endsWith("/mail.store/store_fetch")) {
      return new Response(
        JSON.stringify({
          store: answerPayload(10, {
            "res.partner": [{ id: 1, name: "Alice" }],
            "mail.message": [],
          }),
          topics: [
            { topic: "schema", ok: true, data: JSON.parse(TEST_SCHEMA) },
            {
              topic: "thread",
              ok: true,
              data: {
                thread: { model: "res.partner", id: 42, can_post: true },
                messages: { model: "res.partner", id: 42, message_ids: [], has_more_before: false, has_more_after: false },
              },
            },
          ],
        }),
        { headers: { "Content-Type": "application/json" } }
      );
    }
    if (String(path).endsWith("/res.partner/thread_post")) {
      return new Response(
        JSON.stringify({
          message_id: 401,
          replayed: false,
          notified_partner_ids: [],
          store: answerPayload(20, {
            "res.partner": [{ id: 1, name: "Alice" }],
            "mail.message": [
              {
                id: 401,
                body: "<p>Form reload message</p>",
                message_type: "comment",
                date: "2026-11-10 13:00:00",
                author_id: 1,
              },
            ],
          }),
        }),
        { headers: { "Content-Type": "application/json" } }
      );
    }
    throw new Error(`Unexpected path: ${path}`);
  }, () => {});

  const chatter = new Chatter({
    host,
    model: "res.partner",
    recordId: 42,
    rpc,
    reloadOnPost: true,
    reloadForm,
  });

  await flushAsync();

  const draft = host.querySelector<HTMLTextAreaElement>("textarea.erp-mail-draft");
  draft!.value = "Form reload message";
  draft!.dispatchEvent(new Event("input", { bubbles: true }));

  const sendBtn = host.querySelector<HTMLButtonElement>("button.erp-mail-send");
  sendBtn?.click();

  await flushAsync();

  expect(reloadedForm).toBe(true);
  expect(reloadForm).toHaveBeenCalled();

  chatter.dispose();
});

/// (partial: a successful thread load does not wipe an error shown by reloadForm)
test("a successful thread load does not clear an error shown by reloadForm", async () => {
  const host = document.createElement("div");
  document.body.append(host);
  const reloadForm = vi.fn(async () => {
    throw new Error("Form reload failed: access denied");
  });
  let storeFetchCount = 0;
  const rpc = new RpcClient(async (path) => {
    if (String(path).endsWith("/mail.store/store_fetch")) {
      storeFetchCount++;
      if (storeFetchCount > 1) {
        await new Promise((r) => setTimeout(r, 10));
      }
      return new Response(
        JSON.stringify({
          store: answerPayload(10, {
            "res.partner": [],
            "mail.message": [],
          }),
          topics: [
            { topic: "schema", ok: true, data: JSON.parse(TEST_SCHEMA) },
            {
              topic: "thread",
              ok: true,
              data: {
                thread: { model: "res.partner", id: 42, can_post: true },
                messages: { model: "res.partner", id: 42, message_ids: [], has_more_before: false, has_more_after: false },
              },
            },
          ],
        }),
        { headers: { "Content-Type": "application/json" } }
      );
    }
    if (String(path).endsWith("/res.partner/thread_post")) {
      return new Response(
        JSON.stringify({
          message_id: 402,
          replayed: false,
          notified_partner_ids: [],
          store: answerPayload(20, {
            "res.partner": [{ id: 1, name: "Alice" }],
            "mail.message": [
              {
                id: 402,
                body: "<p>Message</p>",
                message_type: "comment",
                date: "2026-11-10 13:00:00",
                author_id: 1,
              },
            ],
          }),
        }),
        { headers: { "Content-Type": "application/json" } }
      );
    }
    throw new Error(`Unexpected path: ${path}`);
  }, () => {});

  const chatter = new Chatter({
    host,
    model: "res.partner",
    recordId: 42,
    rpc,
    reloadOnPost: true,
    reloadForm,
  });

  await flushAsync();

  const draft = host.querySelector<HTMLTextAreaElement>("textarea.erp-mail-draft");
  draft!.value = "Message";
  draft!.dispatchEvent(new Event("input", { bubbles: true }));

  const sendBtn = host.querySelector<HTMLButtonElement>("button.erp-mail-send");
  sendBtn?.click();

  await flushAsync();
  await new Promise((r) => setTimeout(r, 50));

  const errorEl = host.querySelector(".erp-mail-chatter-error .erp-mail-error-message");
  expect(errorEl).not.toBeNull();
  expect(errorEl?.textContent).toContain("Form reload failed: access denied");

  chatter.dispose();
});

/// (partial: post error renders failed pending message row with retry and discard)
test("post error renders failed pending message row with retry and discard", async () => {
  const rpc = new RpcClient(async (path) => {
    if (String(path).endsWith("/mail.store/store_fetch")) {
      return new Response(
        JSON.stringify({
          store: answerPayload(10, {
            "res.partner": [],
            "mail.message": [],
          }),
          topics: [
            { topic: "schema", ok: true, data: JSON.parse(TEST_SCHEMA) },
            {
              topic: "thread",
              ok: true,
              data: {
                thread: { model: "res.partner", id: 42, can_post: true },
                messages: { model: "res.partner", id: 42, message_ids: [], has_more_before: false, has_more_after: false },
              },
            },
          ],
        }),
        { headers: { "Content-Type": "application/json" } }
      );
    }
    if (String(path).endsWith("/res.partner/thread_post")) {
      throw new Error("Database deadlock during post");
    }
    throw new Error(`Unexpected path: ${path}`);
  }, () => {});

  const chatter = new Chatter({
    host,
    model: "res.partner",
    recordId: 42,
    rpc,
    reportError: () => {},
  });

  await flushAsync();

  const draft = host.querySelector<HTMLTextAreaElement>("textarea.erp-mail-draft");
  draft!.value = "Will fail";
  draft!.dispatchEvent(new Event("input", { bubbles: true }));

  const sendBtn = host.querySelector<HTMLButtonElement>("button.erp-mail-send");
  sendBtn?.click();

  await flushAsync();

  // Pending failed row should be visible
  const failedRow = host.querySelector<HTMLElement>(".erp-mail-pending.is-failed");
  expect(failedRow).not.toBeNull();
  expect(failedRow?.textContent).toContain("The message was not sent");

  const discardBtn = failedRow?.querySelector<HTMLButtonElement>("button.erp-mail-discard");
  expect(discardBtn).not.toBeNull();
  discardBtn?.click();

  await flushAsync();

  expect(host.querySelector(".erp-mail-pending")).toBeNull();

  chatter.dispose();
});

/// (partial: unsaved record shows placeholder without making RPC calls)
test("unsaved record shows placeholder without making RPC calls", async () => {
  let rpcCalls = 0;
  const rpc = new RpcClient(async () => {
    rpcCalls++;
    throw new Error("RPC should not be called for unsaved record");
  }, () => {});

  const chatter = new Chatter({
    host,
    model: "res.partner",
    recordId: undefined,
    rpc,
  });

  await flushAsync();

  expect(rpcCalls).toBe(0);
  const placeholder = host.querySelector<HTMLElement>(".form-chatter-unsaved");
  expect(placeholder).not.toBeNull();
  expect(placeholder?.textContent).toContain("Save the record to start discussion");

  chatter.dispose();
});

/// (partial: RecordForm integration mounts chatter into slot and cleans up on dispose)
test("RecordForm integration mounts chatter into slot and cleans up on dispose", async () => {
  const fields: Field[] = [{ name: "name", kind: "char", label: "Name", readonly: false, required: false, selection: null, relation: null, relationField: null }];
  const formNodes: Node[] = [
    {
      kind: "root",
      children: [
        { kind: "sheet", children: [{ kind: "field", field: fields[0]!, label: "Name", widget: "char" }] },
        { kind: "chatter", reloadOnPost: false, reloadOnAttachment: false, reloadOnActivity: false, reloadOnFollower: false, openAttachments: false, hasActivities: false },
      ],
    },
  ];
  const view: View = { type: "form", mode: "record", nodes: formNodes, fields };

  const rpc = new RpcClient(async (path) => {
    if (String(path).endsWith("default_get")) return new Response('{"name": "Partner 1"}', { headers: { "Content-Type": "application/json" } });
    if (String(path).endsWith("web_read")) return new Response(JSON.stringify([{ id: 42, name: "Partner 1" }]), { headers: { "Content-Type": "application/json" } });
    if (String(path).endsWith("/mail.store/store_fetch")) {
      return new Response(
        JSON.stringify({
          store: answerPayload(10, { "res.partner": [], "mail.message": [] }),
          topics: [
            { topic: "schema", ok: true, data: JSON.parse(TEST_SCHEMA) },
            {
              topic: "thread",
              ok: true,
              data: {
                thread: { model: "res.partner", id: 42, can_post: true },
                messages: { model: "res.partner", id: 42, message_ids: [], has_more_before: false, has_more_after: false },
              },
            },
          ],
        }),
        { headers: { "Content-Type": "application/json" } }
      );
    }
    throw new Error(`Unexpected path: ${path}`);
  }, () => {});

  const state = new RecordState(rpc, "res.partner", fields, {});
  await state.load(42);

  const formElement = document.createElement("form");
  const chatterHost = document.createElement("aside");
  chatterHost.className = "record-form-chatter";
  host.append(formElement, chatterHost);

  const recordForm = new RecordForm({
    form: formElement,
    state,
    view,
    rpc,
    context: {},
    chatterHost,
    run: (fn) => fn(),
    report: () => {},
    openRecord: async () => {},
    change: () => {},
    button: () => {},
  });

  recordForm.render();
  await flushAsync();

  const slot = chatterHost.querySelector<HTMLElement>(".form-chatter-slot");
  expect(slot).not.toBeNull();
  expect(slot?.querySelector(".erp-mail-chatter")).not.toBeNull();
  expect(slot?.querySelector(".erp-mail-composer")).not.toBeNull();
  expect(slot?.querySelector(".erp-mail-thread")).not.toBeNull();

  recordForm.dispose();
  expect(chatterHost.querySelector(".erp-mail-chatter")).toBeNull();
});

/// (partial: chatter files panel AT-01 paperclip toggle and count badge)
test("AT-01: chatter top bar paperclip button with count toggles files panel or triggers upload when zero", async () => {
  let fileCount = 0;
  const calls: { path: string; body: Record<string, unknown> }[] = [];
  const rpc = new RpcClient(async (path, init) => {
    const body = JSON.parse(String(init?.body)) as Record<string, unknown>;
    calls.push({ path: String(path), body });

    if (String(path).endsWith("/mail.store/store_fetch")) {
      return new Response(
        JSON.stringify({
          store: answerPayload(10, {
            "res.partner": [{ id: 1, name: "Alice" }],
            "mail.message": [{ id: 101, body: "<p>Hello</p>", message_type: "comment", date: "2026-11-10 10:00:00", author_id: 1 }],
            ...(fileCount > 0 ? {
              "ir.attachment": [
                { id: 501, name: "doc1.pdf", mimetype: "application/pdf", file_size: 1024, checksum: "hash1" },
                { id: 502, name: "doc2.pdf", mimetype: "application/pdf", file_size: 2048, checksum: "hash2" },
              ],
            } : {}),
          }),
          topics: [
            { topic: "schema", ok: true, data: JSON.parse(TEST_SCHEMA) },
            {
              topic: "thread",
              ok: true,
              data: {
                thread: { model: "res.partner", id: 42, can_post: true, attachment_count: fileCount },
                messages: { model: "res.partner", id: 42, message_ids: [101], has_more_before: false, has_more_after: false },
                attachments: {
                  attachment_ids: fileCount > 0 ? [501, 502] : [],
                  has_more: false,
                },
              },
            },
          ],
        }),
        { headers: { "Content-Type": "application/json" } }
      );
    }
    throw new Error(`Unexpected path: ${path}`);
  }, () => {});

  const chatter = new Chatter({
    host,
    model: "res.partner",
    recordId: 42,
    rpc,
  });

  await flushAsync();

  const paperclipBtn = host.querySelector<HTMLButtonElement>(".erp-mail-chatter-attachment-btn");
  expect(paperclipBtn).not.toBeNull();
  const countBadge = paperclipBtn?.querySelector(".erp-mail-chatter-attachment-count");
  expect(countBadge?.textContent).toBe("0");

  const filesPanel = host.querySelector<HTMLElement>(".erp-mail-chatter-files-panel");
  expect(filesPanel?.hidden).toBe(true);

  // When 0 files, clicking paperclip button triggers the hidden file input
  const fileInput = host.querySelector<HTMLInputElement>(".erp-mail-chatter-file-input")!;
  let clicked = false;
  fileInput.click = () => { clicked = true; };
  paperclipBtn?.click();
  expect(clicked).toBe(true);
  expect(filesPanel?.hidden).toBe(true);

  // Now simulate having 2 files
  fileCount = 2;
  await chatter.refresh();
  await flushAsync();

  expect(countBadge?.textContent).toBe("2");
  expect(filesPanel?.hidden).toBe(true);

  // When files exist, clicking paperclip button toggles files panel
  paperclipBtn?.click();
  expect(filesPanel?.hidden).toBe(false);

  paperclipBtn?.click();
  expect(filesPanel?.hidden).toBe(true);

  chatter.dispose();
});

/// (partial: chatter files panel AT-02 grouping duplicate count and selection mode)
/// (partial: chatter files panel AT-03 bulk delete and individual delete unlink RPC)
test("AT-02 and AT-03: files panel lists grouped attachments with duplicate count, select mode, bulk unlink and individual unlink RPC", async () => {
  const calls: { path: string; body: Record<string, unknown> }[] = [];
  let currentAttachments = [
    { id: 501, name: "invoice.pdf", mimetype: "application/pdf", file_size: 4096, checksum: "hash_inv" },
    { id: 502, name: "invoice_copy.pdf", mimetype: "application/pdf", file_size: 4096, checksum: "hash_inv" }, // Duplicate by checksum!
    { id: 503, name: "photo.jpg", mimetype: "image/jpeg", file_size: 8192, checksum: "hash_photo" },
  ];

  const rpc = new RpcClient(async (path, init) => {
    const body = JSON.parse(String(init?.body)) as Record<string, unknown>;
    calls.push({ path: String(path), body });

    if (String(path).endsWith("/mail.store/store_fetch")) {
      return new Response(
        JSON.stringify({
          store: answerPayload(10, {
            "res.partner": [{ id: 1, name: "Alice" }],
            "mail.message": [{ id: 101, body: "<p>Hello</p>", message_type: "comment", date: "2026-11-10 10:00:00", author_id: 1 }],
            "ir.attachment": currentAttachments,
          }),
          topics: [
            { topic: "schema", ok: true, data: JSON.parse(TEST_SCHEMA) },
            {
              topic: "thread",
              ok: true,
              data: {
                thread: { model: "res.partner", id: 42, can_post: true, attachment_count: currentAttachments.length },
                messages: { model: "res.partner", id: 42, message_ids: [101], has_more_before: false, has_more_after: false },
                attachments: {
                  attachment_ids: currentAttachments.map((a) => a.id),
                  has_more: false,
                },
              },
            },
          ],
        }),
        { headers: { "Content-Type": "application/json" } }
      );
    }

    if (String(path).endsWith("/ir.attachment/unlink")) {
      const ids = (body["ids"] as number[]) ?? [];
      currentAttachments = currentAttachments.filter((a) => !ids.includes(a.id));
      return new Response(JSON.stringify(true), { headers: { "Content-Type": "application/json" } });
    }

    throw new Error(`Unexpected path: ${path}`);
  }, () => {});

  const chatter = new Chatter({
    host,
    model: "res.partner",
    recordId: 42,
    rpc,
    confirm: async () => true,
  });

  await flushAsync();

  const filesPanel = host.querySelector<HTMLElement>(".erp-mail-chatter-files-panel")!;
  const paperclipBtn = host.querySelector<HTMLButtonElement>(".erp-mail-chatter-attachment-btn");
  paperclipBtn?.click();
  expect(filesPanel.hidden).toBe(false);

  // Groups: 2 groups (invoice group with 2 files, photo group with 1 file)
  const fileGroups = filesPanel.querySelectorAll<HTMLElement>(".erp-mail-chatter-file-group");
  expect(fileGroups).toHaveLength(2);

  // Group 1: duplicate badge showing (2)
  const dupBadge = fileGroups[0]?.querySelector<HTMLElement>(".erp-mail-chatter-duplicate-count");
  expect(dupBadge?.textContent).toBe(" (2)");
  expect(fileGroups[0]?.querySelector(".erp-mail-chatter-file-name")?.textContent).toBe("invoice.pdf");
  expect(fileGroups[0]?.querySelector(".erp-mail-chatter-file-size")?.textContent).toBe(" (4.0 KB)");

  // Group 2: photo has thumbnail
  const thumb = fileGroups[1]?.querySelector<HTMLImageElement>(".erp-mail-chatter-file-thumbnail");
  expect(thumb).not.toBeNull();
  expect(thumb?.getAttribute("src")).toBe("/erp/content/503");

  // "Attach files" button triggers record file input
  const attachFilesBtn = filesPanel.querySelector<HTMLButtonElement>(".erp-mail-chatter-attach-files-btn")!;
  const fileInput = host.querySelector<HTMLInputElement>(".erp-mail-chatter-file-input")!;
  let attachClicked = false;
  fileInput.click = () => { attachClicked = true; };
  attachFilesBtn.click();
  expect(attachClicked).toBe(true);

  // "Select files" button is visible because groups > 1
  const selectFilesBtn = filesPanel.querySelector<HTMLButtonElement>(".erp-mail-chatter-select-files-btn")!;
  expect(selectFilesBtn.hidden).toBe(false);

  // Enter selection mode
  selectFilesBtn.click();
  expect(selectFilesBtn.hidden).toBe(true);
  const selectionActions = filesPanel.querySelector<HTMLElement>(".erp-mail-chatter-selection-actions")!;
  expect(selectionActions.hidden).toBe(false);

  const deleteSelectedBtn = selectionActions.querySelector<HTMLButtonElement>(".erp-mail-chatter-delete-selected")!;
  const discardSelectionBtn = selectionActions.querySelector<HTMLButtonElement>(".erp-mail-chatter-discard-selection")!;
  expect(deleteSelectedBtn.textContent).toBe("Delete (0)");
  expect(deleteSelectedBtn.disabled).toBe(true);

  // In selection mode, checkboxes appear on groups
  const checkboxes = filesPanel.querySelectorAll<HTMLInputElement>(".erp-mail-chatter-file-select");
  expect(checkboxes).toHaveLength(2);

  // Check group 1 (which has 2 attachments: 501 and 502)
  checkboxes[0]!.checked = true;
  checkboxes[0]!.dispatchEvent(new Event("change"));
  expect(deleteSelectedBtn.textContent).toBe("Delete (2)");
  expect(deleteSelectedBtn.disabled).toBe(false);

  // Test discard selection
  discardSelectionBtn.click();
  expect(selectionActions.hidden).toBe(true);
  expect(selectFilesBtn.hidden).toBe(false);

  // Re-enter selection mode and delete selected
  selectFilesBtn.click();
  const checkboxesAfter = filesPanel.querySelectorAll<HTMLInputElement>(".erp-mail-chatter-file-select");
  checkboxesAfter[0]!.checked = true;
  checkboxesAfter[0]!.dispatchEvent(new Event("change"));
  expect(deleteSelectedBtn.textContent).toBe("Delete (2)");

  deleteSelectedBtn.click();
  await flushAsync();

  const unlinkCall = calls.find((c) => c.path.endsWith("/ir.attachment/unlink"));
  expect(unlinkCall).toBeDefined();
  expect((unlinkCall?.body["ids"] as number[]).sort()).toEqual([501, 502]);

  // Now only photo group remains
  const remainingGroups = filesPanel.querySelectorAll<HTMLElement>(".erp-mail-chatter-file-group");
  expect(remainingGroups).toHaveLength(1);

  // With <= 1 group, "Select files" button is hidden
  expect(selectFilesBtn.hidden).toBe(true);

  // Individual delete button on remaining group
  const singleDeleteBtn = remainingGroups[0]?.querySelector<HTMLButtonElement>(".erp-mail-chatter-file-delete");
  expect(singleDeleteBtn).not.toBeNull();
  calls.length = 0;
  singleDeleteBtn?.click();
  await flushAsync();

  const singleUnlink = calls.find((c) => c.path.endsWith("/ir.attachment/unlink"));
  expect(singleUnlink).toBeDefined();
  expect(singleUnlink?.body["ids"]).toEqual([503]);

  chatter.dispose();
});

/**
 * The answer `thread_upload` and `thread_attach` really give, copied from the response the Rust test
 * `the_client_uploads_posts_and_loads_a_file_through_the_rpc_service` receives
 * (`crates/erp-test-models/tests/mail_rpc.rs`).
 */
const UPLOAD_ANSWER = (id: number, name: string, size: number) => ({
  attachment_id: id,
  name,
  mimetype: "text/plain",
  file_size: size,
  checksum: "2aae6c35c94fcfb415dbe95f408b9ce91ee846ed",
});

const jsonResponse = (value: unknown, status = 200): Response =>
  new Response(JSON.stringify(value), { status, headers: { "Content-Type": "application/json" } });

/** The request a chatter must never send: the generic create with another product's field names. */
const expectNoGenericCreate = (calls: { path: string }[]): void => {
  expect(calls.some((c) => c.path.endsWith("/ir.attachment/create"))).toBe(false);
};

/// (partial: chatter attachment paperclip, chips, and post attachment_ids)
test("composer paperclip uploads through thread_upload and the post carries the answered attachment id", async () => {
  const calls: { path: string; body: Record<string, unknown> }[] = [];
  const rpc = new RpcClient(async (path, init) => {
    const body = JSON.parse(String(init?.body)) as Record<string, unknown>;
    calls.push({ path: String(path), body });

    if (String(path).endsWith("/mail.store/store_fetch")) {
      return jsonResponse({
        store: answerPayload(10, {
          "res.partner": [{ id: 1, name: "Alice" }],
          "mail.message": [{ id: 101, body: "<p>Hello</p>", message_type: "comment", date: "2026-11-10 10:00:00", author_id: 1 }],
        }),
        topics: [
          { topic: "schema", ok: true, data: JSON.parse(TEST_SCHEMA) },
          {
            topic: "thread",
            ok: true,
            data: {
              thread: { model: "res.partner", id: 42, can_post: true, attachment_count: 0 },
              messages: { model: "res.partner", id: 42, message_ids: [101], has_more_before: false, has_more_after: false },
              attachments: { attachment_ids: [], has_more: false },
            },
          },
        ],
      });
    }

    if (String(path).endsWith("/res.partner/thread_upload")) {
      return jsonResponse(UPLOAD_ANSWER(888, "my_upload.txt", 12));
    }

    if (String(path).endsWith("/res.partner/thread_post")) {
      return jsonResponse({
        message_id: 102,
        replayed: false,
        notified_partner_ids: [],
        store: answerPayload(11, {
          "mail.message": [
            {
              id: 102,
              body: (body["post"] as Record<string, unknown>)["body"],
              message_type: "comment",
              date: "2026-11-10 11:00:00",
              author_id: 1,
              attachment_ids: [888],
            },
          ],
          "ir.attachment": [{ id: 888, name: "my_upload.txt", mimetype: "text/plain", file_size: 12 }],
        }),
      });
    }

    throw new Error(`Unexpected path: ${path}`);
  }, () => {});

  const chatter = new Chatter({ host, model: "res.partner", recordId: 42, rpc });
  await flushAsync();

  const composerEl = host.querySelector<HTMLElement>(".erp-mail-composer")!;
  const fileInput = composerEl.querySelector<HTMLInputElement>(".erp-mail-file-input")!;
  const mockFile = new File(["file content"], "my_upload.txt", { type: "text/plain" });
  Object.defineProperty(fileInput, "files", { value: [mockFile], configurable: true });
  fileInput.dispatchEvent(new Event("change"));
  // The file is read by a FileReader on its own timer: wait for the effect, not for a fixed delay.
  await vi.waitFor(() => expect(composerEl.querySelectorAll(".erp-mail-attachment-chip")).toHaveLength(1));

  const uploadCall = calls.find((c) => c.path.endsWith("/res.partner/thread_upload"));
  expect(uploadCall?.body).toEqual({
    ids: [42],
    name: "my_upload.txt",
    mimetype: "text/plain",
    data: "ZmlsZSBjb250ZW50", // base64 of "file content"
  });
  expectNoGenericCreate(calls);

  const chips = composerEl.querySelectorAll<HTMLElement>(".erp-mail-attachment-chip");
  expect(chips).toHaveLength(1);
  expect(chips[0]?.querySelector(".erp-mail-attachment-chip-name")?.textContent).toBe("my_upload.txt");

  composerEl.querySelector<HTMLButtonElement>(".erp-mail-send")!.click();
  await flushAsync();

  const postCall = calls.find((c) => c.path.endsWith("/res.partner/thread_post"));
  expect((postCall?.body["post"] as Record<string, unknown>)["attachment_ids"]).toEqual([888]);

  chatter.dispose();
});

/// (partial: a refused upload shows the server's message inline)
test("a refused composer upload shows the server message inline and adds no chip", async () => {
  const refusal = "Content above 2048 bytes is not passed in memory: stream it with attachments::Uploader::stage and link the upload.";
  const rpc = new RpcClient(async (path) => {
    if (String(path).endsWith("/mail.store/store_fetch")) {
      return jsonResponse({
        store: answerPayload(10, {}),
        topics: [
          { topic: "schema", ok: true, data: JSON.parse(TEST_SCHEMA) },
          {
            topic: "thread",
            ok: true,
            data: {
              thread: { model: "res.partner", id: 42, can_post: true, attachment_count: 0 },
              messages: { model: "res.partner", id: 42, message_ids: [], has_more_before: false, has_more_after: false },
              attachments: { attachment_ids: [], has_more: false },
            },
          },
        ],
      });
    }
    if (String(path).endsWith("/res.partner/thread_upload")) {
      return jsonResponse({ name: "erp.exceptions.ValidationError", message: refusal }, 422);
    }
    throw new Error(`Unexpected path: ${path}`);
  }, () => {});
  const chatter = new Chatter({ host, model: "res.partner", recordId: 42, rpc });
  await flushAsync();
  const composerEl = host.querySelector<HTMLElement>(".erp-mail-composer")!;
  const fileInput = composerEl.querySelector<HTMLInputElement>(".erp-mail-file-input")!;
  Object.defineProperty(fileInput, "files", { value: [new File(["x"], "big.bin")], configurable: true });
  fileInput.dispatchEvent(new Event("change"));
  // the file is read on its own timer: wait for the visible effect, not for a fixed time (a slower host
  // had not finished the read after 20 ms)
  await vi.waitFor(() => expect(composerEl.textContent).toContain(refusal));
  expect(composerEl.querySelectorAll(".erp-mail-attachment-chip")).toHaveLength(0);
  chatter.dispose();
});

/// (partial: the attachments panel attaches a file to the record with thread_attach and reloads the list)
test("the attachments panel uploads through thread_attach and shows the new file and count", async () => {
  const calls: { path: string; body: Record<string, unknown> }[] = [];
  let attached = false;
  const rpc = new RpcClient(async (path, init) => {
    const body = JSON.parse(String(init?.body)) as Record<string, unknown>;
    calls.push({ path: String(path), body });
    if (String(path).endsWith("/mail.store/store_fetch")) {
      return jsonResponse({
        store: answerPayload(10, {
          "res.partner": [{ id: 1, name: "Alice" }],
          ...(attached ? { "ir.attachment": [{ id: 777, name: "hello.txt", mimetype: "text/plain", file_size: 11, checksum: "2aae6c35c94fcfb415dbe95f408b9ce91ee846ed" }] } : {}),
        }),
        topics: [
          { topic: "schema", ok: true, data: JSON.parse(TEST_SCHEMA) },
          {
            topic: "thread",
            ok: true,
            data: {
              thread: { model: "res.partner", id: 42, can_post: true, attachment_count: attached ? 1 : 0 },
              messages: { model: "res.partner", id: 42, message_ids: [], has_more_before: false, has_more_after: false },
              attachments: { attachment_ids: attached ? [777] : [], has_more: false },
            },
          },
        ],
      });
    }
    if (String(path).endsWith("/res.partner/thread_attach")) {
      attached = true;
      return jsonResponse(UPLOAD_ANSWER(777, "hello.txt", 11));
    }
    throw new Error(`Unexpected path: ${path}`);
  }, () => {});
  const chatter = new Chatter({ host, model: "res.partner", recordId: 42, rpc });
  await flushAsync();
  const input = host.querySelector<HTMLInputElement>(".erp-mail-chatter-file-input")!;
  Object.defineProperty(input, "files", { value: [new File(["hello world"], "hello.txt", { type: "text/plain" })], configurable: true });
  input.dispatchEvent(new Event("change"));
  await vi.waitFor(() => expect(host.querySelector(".erp-mail-chatter-attachment-count")?.textContent).toBe("1"));
  const attachCall = calls.find((c) => c.path.endsWith("/res.partner/thread_attach"));
  expect(attachCall?.body).toEqual({ ids: [42], name: "hello.txt", mimetype: "text/plain", data: "aGVsbG8gd29ybGQ=" });
  expectNoGenericCreate(calls);
  expect(host.querySelector(".erp-mail-chatter-attachment-count")?.textContent).toBe("1");
  expect(host.querySelector(".erp-mail-chatter-files-panel")?.textContent).toContain("hello.txt");
  chatter.dispose();
});

// The shared default store keeps the first schema it saw, so these tests bring their own store and schema id.
const FOLLOWER_SCHEMA = { ...JSON.parse(TEST_SCHEMA), schema: "chatter-followers-s1" } as { models: Record<string, unknown> };
const followerPayload = (xmax: number, records: Record<string, readonly unknown[]>): string =>
  answerPayload(xmax, records).replace("chatter-test-s1", "chatter-followers-s1");
const newStore = (): MessagingStore => new MessagingStore({ reportError: () => {} });
FOLLOWER_SCHEMA.models["mail.message.subtype"] = { fields: { name: fieldSpec("string"), internal: fieldSpec("bool") } };
FOLLOWER_SCHEMA.models["mail.followers"] = {
  fields: {
    res_model: fieldSpec("string"),
    res_id: fieldSpec("int"),
    partner_id: fieldSpec("many2one", { target: "res.partner" }),
    subtype_ids: fieldSpec("x2many", { target: "mail.message.subtype" }),
  },
};

interface FollowerFixture { calls: { path: string; body: Record<string, unknown> }[]; rpc: RpcClient; followerRows: () => unknown[] }

/** A chatter server with two followers (partner 7 is the current user's partner when `self` is set). */
function followerServer(options: { self: boolean; followersError?: string }): FollowerFixture {
  const calls: { path: string; body: Record<string, unknown> }[] = [];
  let self = options.self;
  const rows = (): unknown[] => [
    { id: 1, res_model: "res.partner", res_id: 42, partner_id: 8, subtype_ids: [1] },
    ...(self ? [{ id: 2, res_model: "res.partner", res_id: 42, partner_id: 7, subtype_ids: [1] }] : []),
  ];
  const json = (value: unknown): Response => new Response(JSON.stringify(value), { headers: { "Content-Type": "application/json" } });
  const rpc = new RpcClient(async (path, init) => {
    const body = init?.body ? (JSON.parse(String(init.body)) as Record<string, unknown>) : {};
    const p = String(path);
    calls.push({ path: p, body });
    if (p.endsWith("/mail.store/store_fetch")) {
      const topics = body["topics"] as { name: string; params: Record<string, unknown> }[];
      const wantsFollowers = topics.some((t) => t.params?.["followers"] === true);
      if (!wantsFollowers) {
        return json({
          store: followerPayload(10, {}),
          topics: [
            { topic: "schema", ok: true, data: FOLLOWER_SCHEMA },
            { topic: "thread", ok: true, data: { messages: { message_ids: [], has_more_before: false } } },
          ],
        });
      }
      if (options.followersError) return json({ store: followerPayload(11, {}), topics: [{ topic: "thread", ok: false, error: { message: options.followersError } }] });
      return json({
        store: followerPayload(12, { "mail.followers": rows() }),
        topics: [{ topic: "thread", ok: true, data: { followers: { count: rows().length, follower_ids: rows().map((r) => (r as { id: number }).id), self_follower_id: self ? 2 : null, next_after: null } } }],
      });
    }
    if (p.endsWith("/res.partner/web_read")) return json([{ id: 8, display_name: "Bob Brown" }, { id: 7, display_name: "Me Myself" }].filter((r) => (body["ids"] as number[]).includes(r.id)));
    if (p.endsWith("/res.users/web_read")) return json([{ id: 5, partner_id: { id: 7, display_name: "Me Myself" } }]);
    if (p.endsWith("/res.partner/thread_subtypes")) {
      return json({ store: followerPayload(13, { "mail.message.subtype": [{ id: 1, name: "Discussions", internal: false }, { id: 2, name: "Activities", internal: true }] }), subtype_ids: [1, 2] });
    }
    if (p.endsWith("/res.partner/thread_follow")) { self = true; return json({ store: followerPayload(14, {}) }); }
    if (p.endsWith("/res.partner/thread_unfollow")) { if ((body["partner_ids"] as number[])[0] === 7) self = false; return json({ store: followerPayload(15, {}) }); }
    if (p.endsWith("/res.partner/thread_set_subtypes")) return json({ store: followerPayload(16, {}) });
    throw new Error(`Unexpected path: ${p}`);
  }, () => {});
  return { calls, rpc, followerRows: rows };
}

/// (partial: followers button shows the count, the popover lists followers and the user can unfollow)
test("followers button names the count, lists followers and unfollows the current user", async () => {
  const fx = followerServer({ self: true });
  const chatter = new Chatter({ host, model: "res.partner", recordId: 42, rpc: fx.rpc, store: newStore(), uid: 5 });
  await flushAsync();
  const button = host.querySelector<HTMLButtonElement>(".erp-mail-followers-button")!;
  expect(button.getAttribute("aria-label")).toBe("Followers: 2");
  expect(button.getAttribute("aria-expanded")).toBe("false");
  button.click();
  await flushAsync();
  expect(button.getAttribute("aria-expanded")).toBe("true");
  const popover = host.querySelector<HTMLElement>(".erp-mail-followers-popover")!;
  expect(popover.hidden).toBe(false);
  expect(popover.textContent).toContain("Bob Brown");
  expect(popover.textContent).toContain("Me Myself");
  const toggle = popover.querySelector<HTMLButtonElement>(".erp-mail-followers-toggle")!;
  expect(toggle.textContent).toBe("Unfollow");
  toggle.click();
  await flushAsync();
  const call = fx.calls.find((c) => c.path.endsWith("/thread_unfollow"))!;
  expect(call.body).toEqual({ ids: [42], partner_ids: [7] });
  expect(host.querySelector(".erp-mail-followers-button")!.getAttribute("aria-label")).toBe("Followers: 1");
  expect(popover.querySelector<HTMLButtonElement>(".erp-mail-followers-toggle")!.textContent).toBe("Follow");
  chatter.dispose();
});

/// (partial: a user who does not follow yet finds their partner and follows)
test("follow resolves the user's partner and calls thread_follow", async () => {
  const fx = followerServer({ self: false });
  const chatter = new Chatter({ host, model: "res.partner", recordId: 42, rpc: fx.rpc, store: newStore(), uid: 5 });
  await flushAsync();
  host.querySelector<HTMLButtonElement>(".erp-mail-followers-button")!.click();
  await flushAsync();
  host.querySelector<HTMLButtonElement>(".erp-mail-followers-toggle")!.click();
  await flushAsync();
  expect(fx.calls.find((c) => c.path.endsWith("/thread_follow"))!.body).toEqual({ ids: [42], partner_ids: [7] });
  expect(host.querySelector(".erp-mail-followers-button")!.classList.contains("is-following")).toBe(true);
  chatter.dispose();
});

/// (partial: subtype checkboxes of the user's own subscription call thread_set_subtypes)
test("subtype checkboxes change the current user's subscription and removing another follower unfollows it", async () => {
  const fx = followerServer({ self: true });
  const chatter = new Chatter({ host, model: "res.partner", recordId: 42, rpc: fx.rpc, store: newStore(), uid: 5 });
  await flushAsync();
  host.querySelector<HTMLButtonElement>(".erp-mail-followers-button")!.click();
  await flushAsync();
  const boxes = host.querySelectorAll<HTMLInputElement>(".erp-mail-follower-subtype input");
  expect(Array.from(boxes).map((b) => b.checked)).toEqual([true, false]);
  boxes[1]!.checked = true;
  boxes[1]!.dispatchEvent(new Event("change", { bubbles: true }));
  await flushAsync();
  expect(fx.calls.find((c) => c.path.endsWith("/thread_set_subtypes"))!.body).toEqual({ ids: [42], partner_id: 7, subtype_ids: [1, 2] });
  host.querySelector<HTMLButtonElement>('[aria-label="Remove follower Bob Brown"]')!.click();
  await flushAsync();
  expect(fx.calls.filter((c) => c.path.endsWith("/thread_unfollow")).at(-1)!.body).toEqual({ ids: [42], partner_ids: [8] });
  chatter.dispose();
});

/// (partial: the popover closes with Escape and returns focus to the button)
test("followers popover closes with Escape and returns focus to the button", async () => {
  const fx = followerServer({ self: true });
  const chatter = new Chatter({ host, model: "res.partner", recordId: 42, rpc: fx.rpc, store: newStore(), uid: 5 });
  await flushAsync();
  const button = host.querySelector<HTMLButtonElement>(".erp-mail-followers-button")!;
  button.click();
  await flushAsync();
  const popover = host.querySelector<HTMLElement>(".erp-mail-followers-popover")!;
  expect(popover.contains(document.activeElement)).toBe(true);
  popover.dispatchEvent(new KeyboardEvent("keydown", { key: "Escape", bubbles: true }));
  expect(popover.hidden).toBe(true);
  expect(button.getAttribute("aria-expanded")).toBe("false");
  expect(document.activeElement).toBe(button);
  chatter.dispose();
});

/// (partial: a followers failure renders inline and never reaches the global error reporter)
test("followers and thread failures render inline and never call the global error reporter", async () => {
  const reported: unknown[] = [];
  const fx = followerServer({ self: true, followersError: "Only internal users can list followers." });
  const chatter = new Chatter({ host, model: "res.partner", recordId: 42, rpc: fx.rpc, store: newStore(), uid: 5, reportError: (e) => reported.push(e) });
  await flushAsync();
  host.querySelector<HTMLButtonElement>(".erp-mail-followers-button")!.click();
  await flushAsync();
  const alert = host.querySelector<HTMLElement>(".erp-mail-followers-error")!;
  expect(alert.getAttribute("role")).toBe("alert");
  expect(alert.textContent).toContain("Only internal users can list followers.");
  chatter.dispose();

  const broken = new RpcClient(async () => { throw new Error("Store exploded"); }, () => {});
  const second = new Chatter({ host, model: "res.partner", recordId: 42, rpc: broken, store: newStore(), reportError: (e) => reported.push(e) });
  await flushAsync();
  expect(host.querySelector(".erp-mail-chatter-error")?.getAttribute("role")).toBe("alert");
  expect(reported).toEqual([]);
  second.dispose();
});

/// (partial: follower count must be a safe integer; an invalid count fails loudly without displaying NaN)
test("follower count must be a safe integer and fails loudly without showing NaN", async () => {
  const fx = followerServer({ self: false });
  const originalCall = fx.rpc.call.bind(fx.rpc);
  fx.rpc.call = async (model, method, args, opts) => {
    if (model === "mail.store" && method === "store_fetch") {
      const res = (await originalCall(model, method, args, opts)) as Record<string, unknown>;
      const topics = (res["topics"] as { topic: string; data?: { followers?: Record<string, unknown> } }[]).map((t) => {
        if (t.topic === "thread" && t.data?.followers) {
          const { count, ...rest } = t.data.followers as { count?: unknown };
          return { ...t, data: { ...t.data, followers: rest } };
        }
        return t;
      });
      return { ...res, topics };
    }
    return originalCall(model, method, args, opts);
  };
  const chatter = new Chatter({ host, model: "res.partner", recordId: 42, rpc: fx.rpc, store: newStore(), uid: 5 });
  await flushAsync();
  const button = host.querySelector<HTMLButtonElement>(".erp-mail-followers-button")!;
  expect(button.textContent).not.toContain("NaN");
  expect(button.getAttribute("aria-label")).not.toContain("NaN");
  button.click();
  await flushAsync();
  const alert = host.querySelector<HTMLElement>(".erp-mail-followers-error")!;
  expect(alert.getAttribute("role")).toBe("alert");
  expect(alert.textContent).toContain("invalid follower count");
  chatter.dispose();
});

/// (partial: follower change preserves focus in the popover and falls back when removed)
test("follower change preserves focus in popover and falls back to toggle when follower removed", async () => {
  const fx = followerServer({ self: true });
  const chatter = new Chatter({ host, model: "res.partner", recordId: 42, rpc: fx.rpc, store: newStore(), uid: 5 });
  await flushAsync();
  const button = host.querySelector<HTMLButtonElement>(".erp-mail-followers-button")!;
  button.click();
  await flushAsync();
  const popover = host.querySelector<HTMLElement>(".erp-mail-followers-popover")!;
  const toggle = popover.querySelector<HTMLButtonElement>(".erp-mail-followers-toggle")!;
  toggle.focus();
  expect(document.activeElement).toBe(toggle);
  toggle.click();
  await flushAsync();
  expect(document.activeElement).toBe(popover.querySelector(".erp-mail-followers-toggle"));

  const remove = popover.querySelector<HTMLButtonElement>('[aria-label="Remove follower Bob Brown"]')!;
  remove.focus();
  expect(document.activeElement).toBe(remove);
  remove.click();
  await flushAsync();
  expect(document.activeElement).not.toBe(document.body);
  expect(popover.contains(document.activeElement)).toBe(true);

  popover.dispatchEvent(new KeyboardEvent("keydown", { key: "Escape", bubbles: true }));
  expect(popover.hidden).toBe(true);
  chatter.dispose();
});

/// (partial: chatters without explicit store scope default store per host element)
test("chatters without explicit store scope default store per host element", async () => {
  const host1 = document.createElement("div");
  const host2 = document.createElement("div");
  document.body.append(host1, host2);
  const errs1: unknown[] = [];
  const errs2: unknown[] = [];
  const fx1 = followerServer({ self: false });
  const fx2 = followerServer({ self: false });

  const chatter1 = new Chatter({ host: host1, model: "res.partner", recordId: 42, rpc: fx1.rpc, reportError: (e) => errs1.push(e) });
  const chatter2 = new Chatter({ host: host2, model: "res.partner", recordId: 43, rpc: fx2.rpc, reportError: (e) => errs2.push(e) });
  await flushAsync();

  expect(chatter1.store).not.toBe(chatter2.store);
  chatter2.store?.applyAnswer(JSON.stringify({ xmax: 99, records: { "unknown.model": [{ id: 1 }] } }));
  expect(errs2.length).toBe(1);
  expect(errs1.length).toBe(0);

  chatter1.dispose();
  chatter2.dispose();
  host1.remove();
  host2.remove();
});

/// (partial: inline chatter error is cleared after a subsequent successful action)
test("inline chatter error is cleared after a subsequent successful action", async () => {
  let failFirst = true;
  const calls: Record<string, unknown>[] = [];
  const rpc = new RpcClient(async (path, init) => {
    const body = init?.body ? (JSON.parse(String(init.body)) as Record<string, unknown>) : {};
    const p = String(path);
    calls.push(body);
    if (p.endsWith("/mail.store/store_fetch")) {
      if (failFirst) {
        throw new Error("Temporary network glitch");
      }
      return new Response(JSON.stringify({
        store: followerPayload(10, {}),
        topics: [
          { topic: "schema", ok: true, data: FOLLOWER_SCHEMA },
          { topic: "thread", ok: true, data: { messages: { message_ids: [], has_more_before: false } } },
        ],
      }), { headers: { "Content-Type": "application/json" } });
    }
    return new Response(JSON.stringify({}), { headers: { "Content-Type": "application/json" } });
  }, () => {});

  const chatter = new Chatter({ host, model: "res.partner", recordId: 42, rpc, store: newStore() });
  await flushAsync();
  expect(host.querySelector(".erp-mail-chatter-error")).not.toBeNull();

  failFirst = false;
  await chatter.refresh();
  await flushAsync();
  expect(host.querySelector(".erp-mail-chatter-error")).toBeNull();
  chatter.dispose();
});

