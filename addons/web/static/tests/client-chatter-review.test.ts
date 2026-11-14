/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
import { afterEach, beforeEach, expect, test, vi } from "vitest";
import { MessagingStore } from "@erpforall/mail-store";
import { RpcClient } from "../src/client/transport.js";
import { Chatter } from "../src/client/chatter.js";
import { RecordForm } from "../src/client/record-form.js";
import { RecordState } from "../src/client/record.js";
import { refuseInvalidAttachmentRequest } from "./fixtures/attachment-upload.js";
import "../src/client/fields.js";
import type { Field, Node, View } from "../src/client/metadata.js";

const f = (kind: string, extra: Record<string, unknown> = {}): Record<string, unknown> => ({ kind, ...extra });
const SCHEMA = {
  wire: 1,
  schema: "review-s1",
  models: {
    "res.partner": { fields: { name: f("string") } },
    "ir.attachment": { fields: { name: f("string"), mimetype: f("string"), file_size: f("int"), checksum: f("string"), type: f("selection"), url: f("string") } },
    "mail.message.subtype": { fields: { name: f("string"), internal: f("bool") } },
    "mail.followers": { fields: { res_model: f("string"), res_id: f("int"), partner_id: f("many2one", { target: "res.partner" }), subtype_ids: f("x2many", { target: "mail.message.subtype" }) } },
    "mail.message": {
      fields: {
        body: f("html"), subject: f("string"), message_type: f("selection"), date: f("datetime"),
        author_id: f("many2one", { target: "res.partner" }), is_internal: f("bool"), tracking: f("string"),
        subtype_id: f("many2one", { target: "mail.message.subtype" }), attachment_ids: f("x2many", { target: "ir.attachment" }),
      },
    },
  },
};
const settle = (): Promise<void> => new Promise((resolve) => setTimeout(resolve, 20));
const json = (value: unknown): Response => new Response(JSON.stringify(value), { headers: { "Content-Type": "application/json" } });
const payload = (records: Record<string, readonly unknown[]>): string =>
  JSON.stringify({ wire: 1, schema: SCHEMA.schema, asof: { xmin: "1", xmax: "10", xip: [], own: null }, records });
const message = (id: number, body: string): Record<string, unknown> => ({ id, body: `<p>${body}</p>`, message_type: "comment", date: "2026-11-10 10:00:00", author_id: 1 });

interface Att { id: number; name: string; mimetype?: string; checksum?: string; url?: string }
function threadAnswer(messages: Record<string, unknown>[] = [], attachments: Att[] = []): Response {
  return json({
    store: payload({ "res.partner": [{ id: 1, name: "Alice" }], "mail.message": messages, "ir.attachment": attachments.map((a) => ({ file_size: 10, mimetype: "text/plain", ...a })) }),
    topics: [
      { topic: "schema", ok: true, data: SCHEMA },
      { topic: "thread", ok: true, data: {
        thread: { model: "res.partner", id: 7, can_post: true, attachment_count: attachments.length },
        messages: { model: "res.partner", id: 7, message_ids: messages.map((m) => m["id"]), has_more_before: false, has_more_after: false },
        attachments: { attachment_ids: attachments.map((a) => a.id), has_more: false },
      } },
    ],
  });
}

let host: HTMLElement;
beforeEach(() => { host = document.createElement("div"); document.body.append(host); });
afterEach(() => { host.remove(); vi.restoreAllMocks(); });

let nextId = 700;
const freshStore = (): MessagingStore => new MessagingStore({ reportError: () => {} });
const mount = (rpc: RpcClient, extra: Partial<ConstructorParameters<typeof Chatter>[0]> = {}): Chatter =>
  new Chatter({ host, model: "res.partner", recordId: nextId++, rpc, store: freshStore(), ...extra });
const alertText = (): string => host.querySelector(".erp-mail-chatter-error")?.textContent ?? "";
const path = (p: unknown): string => String(p);

// A malformed store_fetch answer fails loudly instead of showing defaults (a parameterised test carries no SPEC tag).
test.each([
  ["no topics list", () => json({ store: payload({}) }), "no topics list"],
  ["no schema topic", () => json({ store: payload({}), topics: [{ topic: "thread", ok: true, data: {} }] }), "no schema"],
  ["no thread topic", () => json({ store: payload({}), topics: [{ topic: "schema", ok: true, data: SCHEMA }] }), "no thread topic"],
  ["no store payload", () => json({ topics: [{ topic: "schema", ok: true, data: SCHEMA }, { topic: "thread", ok: true, data: { messages: { message_ids: [], has_more_before: false } } }] }), "no store payload"],
  ["no message ids", () => json({ store: payload({}), topics: [{ topic: "schema", ok: true, data: SCHEMA }, { topic: "thread", ok: true, data: { thread: {} } }] }), "no message ids"],
])("a malformed answer (%s) shows an inline error", async (_name, answer, text) => {
  const chatter = mount(new RpcClient(async () => answer(), () => {}));
  await settle();
  expect(alertText()).toContain(text);
  expect(host.querySelector(".erp-mail-skeleton")).toBeNull();
  chatter.dispose();
});

/// (partial: a refused store payload is an error, not an empty thread)
test("a store payload the store refuses surfaces as an inline error", async () => {
  const store = freshStore();
  store.setSchema(JSON.stringify(SCHEMA));
  const answer = JSON.parse(await threadAnswer([message(1, "one")]).text()) as Record<string, unknown>;
  answer["store"] = "garbage";
  const chatter = mount(new RpcClient(async () => json(answer), () => {}), { store });
  await settle();
  expect(alertText()).toContain("could not be updated");
  chatter.dispose();
});

/// (partial: only the latest of overlapping loads replaces the thread)
test("an older load answered last does not replace the newer thread state", async () => {
  const store = freshStore();
  let calls = 0;
  let releaseOld!: () => void;
  const old = new Promise<void>((resolve) => { releaseOld = resolve; });
  const chatter = mount(new RpcClient(async (_p, init) => {
    // the followers control reads the same topic family; only thread loads are counted
    if (String(init?.body).includes('"followers"')) return threadAnswer([message(1, "one")]);
    calls++;
    if (calls === 1) return threadAnswer([message(1, "one")]);
    if (calls === 2) { await old; return threadAnswer([message(1, "one")]); }
    return threadAnswer([message(1, "one"), message(2, "two")]);
  }, () => {}), { store });
  await settle();
  const slow = chatter.refresh();
  await settle();
  await chatter.refresh();
  releaseOld();
  await slow;
  await settle();
  expect(host.textContent).toContain("two");
  chatter.dispose();
});

function attachmentChatter(attachments: Att[], confirm?: (message: string) => Promise<boolean>) {
  const unlinks: number[][] = [];
  let release: (() => void) | undefined;
  const rpc = new RpcClient(async (p, init) => {
    const body = JSON.parse(String(init?.body)) as Record<string, unknown>;
    if (path(p).endsWith("/ir.attachment/unlink")) {
      unlinks.push(body["ids"] as number[]);
      await new Promise<void>((resolve) => { release = resolve; });
      return json(true);
    }
    return threadAnswer([], attachments);
  }, () => {});
  const chatter = mount(rpc, confirm ? { confirm } : {});
  return { chatter, unlinks, release: () => release?.() };
}

/// (partial: attachment URLs never load remotely and never merge by name)
test("a URL attachment shows our own content route as its thumbnail and does not group by name", async () => {
  const { chatter } = attachmentChatter([
    { id: 11, name: "a.png", mimetype: "image/png", url: "https://tracker.example/a.png" },
    { id: 12, name: "a.png", mimetype: "image/png", url: "https://tracker.example/b.png" },
  ]);
  await settle();
  const groups = host.querySelectorAll(".erp-mail-chatter-file-group");
  expect(groups).toHaveLength(2);
  expect(groups[0]!.querySelector("img")!.getAttribute("src")).toBe("/erp/content/11");
  expect(host.innerHTML).not.toContain("tracker.example");
  chatter.dispose();
});

test("a file without a preview shows a file icon and a failed image preview becomes one, never a broken image", async () => {
  const { chatter } = attachmentChatter([
    { id: 41, name: "notes.txt", mimetype: "text/plain", checksum: "c1" },
    { id: 42, name: "photo.png", mimetype: "image/png", checksum: "c2" },
  ]);
  await settle();
  const group = (name: string): Element => [...host.querySelectorAll(".erp-mail-chatter-file-group")].find(item => item.textContent?.includes(name))!;
  expect(group("notes.txt").querySelector("img")).toBeNull();
  expect(group("notes.txt").querySelector(".erp-mail-chatter-file-icon svg")).not.toBeNull();
  const image = group("photo.png").querySelector("img")!;
  expect(image.getAttribute("alt")).toBe("");
  image.dispatchEvent(new Event("error"));
  expect(group("photo.png").querySelector("img")).toBeNull();
  expect(group("photo.png").querySelector(".erp-mail-chatter-file-icon svg")).not.toBeNull();
  chatter.dispose();
});

/// (partial: deleting attachments asks first and ignores a second click while it runs)
test("deleting an attachment needs confirmation and runs once", async () => {
  const confirm = vi.fn(async () => true);
  const { chatter, unlinks, release } = attachmentChatter([{ id: 21, name: "x.txt", checksum: "c1" }], confirm);
  await settle();
  const button = host.querySelector<HTMLButtonElement>(".erp-mail-chatter-file-delete")!;
  button.click();
  button.click();
  await settle();
  expect(confirm).toHaveBeenCalledTimes(1);
  expect(unlinks).toEqual([[21]]);
  expect(button.disabled).toBe(true);
  release();
  await settle();
  chatter.dispose();
});

test("declining the confirmation deletes nothing, and a host without a dialog fails loudly", async () => {
  const declined = attachmentChatter([{ id: 31, name: "x.txt", checksum: "c1" }], async () => false);
  await settle();
  host.querySelector<HTMLButtonElement>(".erp-mail-chatter-file-delete")!.click();
  await settle();
  expect(declined.unlinks).toEqual([]);
  declined.chatter.dispose();
  host.replaceChildren();
  const noDialog = attachmentChatter([{ id: 32, name: "y.txt", checksum: "c2" }]);
  await settle();
  host.querySelector<HTMLButtonElement>(".erp-mail-chatter-file-delete")!.click();
  await settle();
  expect(noDialog.unlinks).toEqual([]);
  expect(alertText()).toContain("confirmation dialog");
  noDialog.chatter.dispose();
});

/// (partial: a file over the cap is refused before it is read or sent)
test("a file over the upload cap is refused before it is read", async () => {
  const calls: string[] = [];
  const { chatter } = (() => {
    const rpc = new RpcClient(async (p) => { calls.push(path(p)); if (path(p).endsWith("/ir.attachment/erp_upload_limits")) return json({ max_file_bytes: 4, inline_max_bytes: 4 }); return threadAnswer(); }, () => {});
    return { chatter: mount(rpc) };
  })();
  await settle();
  const input = host.querySelector<HTMLInputElement>(".erp-mail-chatter-file-input")!;
  const big = new File(["x"], "big.bin");
  Object.defineProperty(big, "size", { value: 5 });
  const read = vi.spyOn(big, "arrayBuffer");
  Object.defineProperty(input, "files", { value: [big], configurable: true });
  input.dispatchEvent(new Event("change"));
  await settle();
  expect(read).not.toHaveBeenCalled();
  expect(calls.some((c) => c.endsWith("/thread_attach"))).toBe(false);
  expect(alertText()).toContain("upload limit");
  chatter.dispose();
});

function formFixture(opts: { dirty: boolean; notify?: (message: string) => void }) {
  const fields: Field[] = [{ name: "name", kind: "char", label: "Name", readonly: false, required: false, selection: null, relation: null, relationField: null }];
  const nodes: Node[] = [{ kind: "root", children: [
    { kind: "sheet", children: [{ kind: "field", field: fields[0]!, label: "Name", widget: "char" }] },
    { kind: "chatter", reloadOnPost: true, reloadOnAttachment: false, reloadOnActivity: false, reloadOnFollower: false, openAttachments: false, hasActivities: false },
  ] }];
  const view: View = { type: "form", mode: "record", nodes, fields };
  const counts = { reads: 0, fetches: 0 };
  const rpc = new RpcClient(async (p) => {
    if (path(p).endsWith("web_read")) { counts.reads++; return json([{ id: 42, name: "Partner" }]); }
    if (path(p).endsWith("/mail.store/store_fetch")) { counts.fetches++; return threadAnswer(); }
    if (path(p).endsWith("/thread_post")) return json({ message_id: 9, replayed: false, notified_partner_ids: [], store: payload({ "res.partner": [{ id: 1, name: "Alice" }], "mail.message": [message(9, "posted")] }) });
    throw new Error(`Unexpected path: ${p}`);
  }, () => {});
  return { fields, view, rpc, counts, opts };
}

async function mountForm(fx: ReturnType<typeof formFixture>) {
  const state = new RecordState(fx.rpc, "res.partner", fx.fields, {});
  await state.load(42);
  const form = document.createElement("form");
  const chatterHost = document.createElement("aside");
  host.append(form, chatterHost);
  const recordForm = new RecordForm({
    form, state, view: fx.view, rpc: fx.rpc, context: {}, chatterHost, messagingStore: freshStore(),
    ...(fx.opts.notify ? { notify: fx.opts.notify } : {}),
    run: (job) => { void job(); }, report: () => {}, openRecord: async () => {}, change: () => {}, button: () => {},
  });
  recordForm.render();
  await settle();
  return { state, recordForm, chatterHost };
}

/// (partial: the form keeps one chatter per record across re-renders)
test("the chatter is mounted once per record id and survives form re-renders", async () => {
  const fx = formFixture({ dirty: false });
  const { recordForm, chatterHost } = await mountForm(fx);
  const element = chatterHost.querySelector(".erp-mail-chatter");
  const fetches = fx.counts.fetches;
  expect(element).not.toBeNull();
  recordForm.render();
  recordForm.render();
  await settle();
  expect(chatterHost.querySelector(".erp-mail-chatter")).toBe(element);
  expect(chatterHost.querySelectorAll(".form-chatter-slot")).toHaveLength(1);
  expect(fx.counts.fetches).toBe(fetches);
  recordForm.dispose();
  expect(chatterHost.querySelector(".erp-mail-chatter")).toBeNull();
});

async function post(chatterHost: HTMLElement): Promise<void> {
  const draft = chatterHost.querySelector<HTMLTextAreaElement>("textarea.erp-mail-draft")!;
  draft.value = "hello";
  draft.dispatchEvent(new Event("input", { bubbles: true }));
  chatterHost.querySelector<HTMLButtonElement>("button.erp-mail-send")!.click();
  await settle();
}

/// (partial: a post never throws away unsaved form edits)
test("posting with unsaved edits does not reload the form and tells the user why", async () => {
  const notify = vi.fn();
  const fx = formFixture({ dirty: true, notify });
  const { state, chatterHost } = await mountForm(fx);
  state.change("name", "Edited");
  const reads = fx.counts.reads;
  await post(chatterHost);
  expect(fx.counts.reads).toBe(reads);
  expect(state.values["name"]).toBe("Edited");
  expect(notify).toHaveBeenCalledWith(expect.stringContaining("unsaved changes"));
});

test("posting on a clean form still reloads it", async () => {
  const fx = formFixture({ dirty: false, notify: vi.fn() });
  const { chatterHost } = await mountForm(fx);
  const reads = fx.counts.reads;
  await post(chatterHost);
  expect(fx.counts.reads).toBeGreaterThan(reads);
});

/// (partial: a server reload or save refreshes the kept chatter, a field edit does not)
test("a record reload from the server refreshes the kept chatter and a field edit does not", async () => {
  const fx = formFixture({ dirty: false });
  const { state, recordForm, chatterHost } = await mountForm(fx);
  const element = chatterHost.querySelector(".erp-mail-chatter");
  const fetches = fx.counts.fetches;
  state.change("name", "Edited");
  recordForm.render();
  await settle();
  expect(fx.counts.fetches).toBe(fetches);
  state.discard();
  await settle();
  expect(fx.counts.fetches).toBe(fetches + 1);
  await state.load(42);
  await settle();
  expect(fx.counts.fetches).toBe(fetches + 2);
  expect(chatterHost.querySelector(".erp-mail-chatter")).toBe(element);
});

/// (partial: a post reloads the form without fetching the chatter a second time)
test("a post that reloads the form fetches the thread once", async () => {
  const fx = formFixture({ dirty: false, notify: vi.fn() });
  const { chatterHost } = await mountForm(fx);
  const fetches = fx.counts.fetches;
  await post(chatterHost);
  expect(fx.counts.fetches).toBe(fetches + 1);
});

/// (partial: staged-but-uncommitted editor drafts count as unsaved edits before a reload)
test("a reload checks for uncommitted editor drafts before it reads the record", async () => {
  const notify = vi.fn();
  const fx = formFixture({ dirty: false, notify });
  const { recordForm, chatterHost } = await mountForm(fx);
  const needs = vi.spyOn(recordForm, "needsCommit").mockReturnValue(true);
  const reads = fx.counts.reads;
  await post(chatterHost);
  expect(needs).toHaveBeenCalled();
  expect(fx.counts.reads).toBe(reads);
  expect(notify).toHaveBeenCalledWith(expect.stringContaining("unsaved changes"));
});

/// (partial: files attached before a failing one are shown at once)
test("when a later file fails, the files already attached still appear in the panel", async () => {
  let attached = false;
  const rpc = new RpcClient(async (p, init) => {
    if (path(p).endsWith("/ir.attachment/erp_upload_limits")) return json({ max_file_bytes: 4, inline_max_bytes: 4 });
    if (path(p).endsWith("/thread_attach")) {
      const refusal = refuseInvalidAttachmentRequest(JSON.parse(String(init?.body)), 4);
      if (refusal) return refusal;
      attached = true;
      return json({ attachment_id: 91, name: "ok.txt", mimetype: "text/plain", file_size: 1 });
    }
    return threadAnswer([], attached ? [{ id: 91, name: "ok.txt", checksum: "c91" }] : []);
  }, () => {});
  const chatter = mount(rpc);
  await settle();
  const input = host.querySelector<HTMLInputElement>(".erp-mail-chatter-file-input")!;
  const big = new File(["x"], "big.bin");
  Object.defineProperty(big, "size", { value: 5 });
  Object.defineProperty(input, "files", { value: [new File(["a"], "ok.txt", { type: "text/plain" }), big], configurable: true });
  input.dispatchEvent(new Event("change"));
  await vi.waitFor(() => expect(alertText()).toContain("upload limit"));
  await vi.waitFor(() => expect(host.querySelector(".erp-mail-chatter-file-name")?.textContent).toBe("ok.txt"));
  chatter.dispose();
});

/// (partial: delete buttons keep the keyboard focus until the user confirmed, then focus moves on)
test("the focused delete button is not disabled before the confirmation and focus moves to the next file after it", async () => {
  let files: Att[] = [{ id: 51, name: "a.txt", checksum: "c1" }, { id: 52, name: "b.txt", checksum: "c2" }];
  const rpc = new RpcClient(async (p, init) => {
    if (path(p).endsWith("/ir.attachment/unlink")) { files = files.filter((a) => !(JSON.parse(String(init?.body)) as { ids: number[] }).ids.includes(a.id)); return json(true); }
    return threadAnswer([], files);
  }, () => {});
  let first!: HTMLButtonElement;
  let disabledAtConfirm: boolean | undefined;
  const chatter = mount(rpc, { confirm: async () => { disabledAtConfirm = first.disabled; return true; } });
  await settle();
  // The panel opens with the paperclip; a hidden control cannot hold focus.
  host.querySelector<HTMLButtonElement>(".erp-mail-chatter-attachment-btn")!.click();
  first = host.querySelectorAll<HTMLButtonElement>(".erp-mail-chatter-file-delete")[0]!;
  first.focus();
  expect(document.activeElement).toBe(first);
  first.click();
  await vi.waitFor(() => expect(host.querySelectorAll(".erp-mail-chatter-file-delete")).toHaveLength(1));
  expect(disabledAtConfirm).toBe(false);
  // The list is rebuilt first and focus moves right after: wait for the effect.
  await vi.waitFor(() => expect(document.activeElement).toBe(host.querySelector(".erp-mail-chatter-file-delete")));
  expect(host.querySelector<HTMLButtonElement>(".erp-mail-chatter-file-delete")!.disabled).toBe(false);
  chatter.dispose();
});
