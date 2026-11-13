/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
import { afterEach, beforeEach, expect, test } from "vitest";
import { RpcClient } from "../src/client/transport.js";
import { Chatter } from "../src/client/chatter.js";

const SCHEMA = {
  wire: 1,
  schema: "chatter-layout-s1",
  models: {
    "res.partner": { fields: { name: { kind: "string" } } },
    "mail.message": {
      fields: {
        body: { kind: "html" }, subject: { kind: "string" }, message_type: { kind: "selection" }, date: { kind: "datetime" },
        author_id: { kind: "many2one", target: "res.partner" }, is_internal: { kind: "bool" }, tracking: { kind: "string" },
        subtype_id: { kind: "many2one", target: "mail.message.subtype" },
      },
    },
    "mail.message.subtype": { fields: { name: { kind: "string" }, internal: { kind: "bool" } } },
  },
};
const settle = (): Promise<void> => new Promise((resolve) => setTimeout(resolve, 20));
const json = (value: unknown): Response => new Response(JSON.stringify(value), { headers: { "Content-Type": "application/json" } });
const emptyThread = (): Response => json({
  store: JSON.stringify({ wire: 1, schema: SCHEMA.schema, asof: { xmin: "1", xmax: "10", xip: [], own: null }, records: { "res.partner": [], "mail.message": [] } }),
  topics: [
    { topic: "schema", ok: true, data: SCHEMA },
    { topic: "thread", ok: true, data: { thread: { model: "res.partner", id: 7, can_post: true }, messages: { model: "res.partner", id: 7, message_ids: [], has_more_before: false, has_more_after: false } } },
  ],
});

let host: HTMLElement;
beforeEach(() => { host = document.createElement("div"); document.body.append(host); });
afterEach(() => host.remove());

// Each test gets its own record: the default store keeps composer drafts (type, text) per thread across tests.
let nextRecord = 7;
const mount = (answer: () => Response | Promise<Response>): Chatter =>
  new Chatter({ host, model: "res.partner", recordId: nextRecord++, rpc: new RpcClient(async () => answer(), () => {}) });

/// (partial: chatter card layout, header row, empty and loading states)
test("chatter shows skeleton rows while loading, then a centred empty state inside the card", async () => {
  let release!: () => void;
  const gate = new Promise<void>((resolve) => { release = resolve; });
  const chatter = mount(async () => { await gate; return emptyThread(); });
  expect(host.querySelectorAll(".erp-mail-skeleton-row").length).toBe(3);
  expect(host.querySelector(".erp-mail-skeleton")?.getAttribute("aria-hidden")).toBe("true");
  release();
  await settle();
  expect(host.querySelector(".erp-mail-skeleton")).toBeNull();
  expect(host.querySelector(".erp-mail-chatter .erp-mail-empty")?.textContent).toBe("No messages yet");
  // The post-type switch lives in the header row; no attachment control and no activities card exist yet.
  expect(host.querySelector(".erp-mail-chatter-header .erp-mail-kinds")).not.toBeNull();
  expect(host.querySelector(".erp-mail-chatter-header .erp-mail-followers")).not.toBeNull();
  expect(host.querySelector("[data-action=attachments]")).toBeNull();
  expect(host.querySelector<HTMLElement>(".erp-mail-chatter-activities")?.hidden).toBe(true);
  chatter.dispose();
});

/// (partial: a failed load replaces the skeleton with an inline alert in the card)
test("chatter load failure renders an inline alert inside the card and drops the skeleton", async () => {
  const chatter = mount(() => { throw new Error("Unable to connect"); });
  await settle();
  expect(host.querySelector(".erp-mail-skeleton")).toBeNull();
  const alert = host.querySelector(".erp-mail-chatter .erp-mail-chatter-error");
  expect(alert?.getAttribute("role")).toBe("alert");
  expect(alert?.textContent).toContain("Unable to connect");
  chatter.dispose();
});

/// (partial: composer collapses to a prompt and expands on focus or on choosing a type)
test("composer is collapsed by default, expands on focus or type choice and collapses again when left empty", async () => {
  const chatter = mount(emptyThread);
  await settle();
  const wrap = host.querySelector<HTMLElement>(".erp-chatter-composer")!;
  const field = wrap.querySelector<HTMLTextAreaElement>("textarea")!;
  expect(wrap.classList.contains("is-collapsed")).toBe(true);
  field.dispatchEvent(new FocusEvent("focusin", { bubbles: true }));
  expect(wrap.classList.contains("is-collapsed")).toBe(false);
  wrap.dispatchEvent(new FocusEvent("focusout", { bubbles: true, relatedTarget: document.body }));
  expect(wrap.classList.contains("is-collapsed")).toBe(true);
  host.querySelector<HTMLButtonElement>('.erp-mail-kind[data-kind="note"]')!.click();
  expect(wrap.classList.contains("is-collapsed")).toBe(false);
  // A draft with text keeps the composer open when focus leaves.
  field.value = "half written";
  field.dispatchEvent(new Event("input", { bubbles: true }));
  wrap.dispatchEvent(new FocusEvent("focusout", { bubbles: true, relatedTarget: document.body }));
  expect(wrap.classList.contains("is-collapsed")).toBe(false);
  chatter.dispose();
});

/// (partial: note mode is flagged for styling; the send hint is real text)
test("log note sets the note flag on the chatter and the send hint has no raw placeholder", async () => {
  const chatter = mount(emptyThread);
  await settle();
  const root = host.querySelector<HTMLElement>(".erp-mail-chatter")!;
  expect(root.dataset["kind"]).toBe("message");
  host.querySelector<HTMLButtonElement>('.erp-mail-kind[data-kind="note"]')!.click();
  await settle();
  expect(root.dataset["kind"]).toBe("note");
  host.querySelector<HTMLButtonElement>('.erp-mail-kind[data-kind="message"]')!.click();
  await settle();
  expect(root.dataset["kind"]).toBe("message");
  const hint = host.querySelector(".erp-mail-hint")?.textContent ?? "";
  expect(hint).toMatch(/^Press .+ to send$/);
  expect(hint).not.toContain("%s");
  chatter.dispose();
});

/// (partial: a note as the server sends it, an internal subtype with is_internal false, is styled as a note)
test("a comment on an internal subtype is a note although the server leaves is_internal false", async () => {
  const chatter = mount(() => json({
    store: JSON.stringify({ wire: 1, schema: SCHEMA.schema, asof: { xmin: "1", xmax: "10", xip: [], own: null }, records: {
      "res.partner": [{ id: 1, name: "Alice Adams" }],
      "mail.message.subtype": [{ id: 1, name: "Discussions", internal: false }, { id: 2, name: "Note", internal: true }],
      "mail.message": [
        { id: 11, body: "<p>Plain message</p>", message_type: "comment", subtype_id: 1, date: "2026-11-10 10:00:00", author_id: 1, is_internal: false },
        { id: 12, body: "<p>Private remark</p>", message_type: "comment", subtype_id: 2, date: "2026-11-10 10:05:00", author_id: 1, is_internal: false },
        { id: 13, body: "", message_type: "notification", subtype_id: 2, date: "2026-11-10 10:06:00", author_id: 1, is_internal: false },
      ],
    } }),
    topics: [
      { topic: "schema", ok: true, data: SCHEMA },
      { topic: "thread", ok: true, data: { thread: { model: "res.partner", id: 7, can_post: true }, messages: { model: "res.partner", id: 7, message_ids: [13, 12, 11], has_more_before: false, has_more_after: false } } },
    ],
  }));
  await settle();
  const note = host.querySelector('.erp-mail-message[data-message-id="12"]')!;
  expect(note.classList.contains("is-note")).toBe(true);
  expect(note.querySelector(".erp-mail-badge")?.textContent).toBe("Note");
  expect(host.querySelector('.erp-mail-message[data-message-id="11"]')!.classList.contains("is-note")).toBe(false);
  // System rows on the note subtype keep their own look.
  expect(host.querySelector('.erp-mail-message[data-message-id="13"]')!.classList.contains("is-note")).toBe(false);
  chatter.dispose();
});

/// (partial: the composer shows the user's avatar beside the input only when the user's name is known)
test("the composer carries the user's initials as its avatar, and none without a name", async () => {
  const named = new Chatter({ host, model: "res.partner", recordId: nextRecord++, userName: "Alice Adams", rpc: new RpcClient(async () => emptyThread(), () => {}) });
  await settle();
  const wrap = host.querySelector<HTMLElement>(".erp-chatter-composer")!;
  expect(wrap.classList.contains("has-avatar")).toBe(true);
  expect([...wrap.children].map((c) => c.className)).toEqual([expect.stringContaining("erp-mail-avatar"), "erp-mail-composer"]);
  expect(wrap.querySelector(".erp-mail-avatar")?.textContent).toBe("AA");
  expect(wrap.querySelector(".erp-mail-avatar")?.getAttribute("aria-hidden")).toBe("true");
  named.dispose();
  const anonymous = mount(emptyThread);
  await settle();
  const bare = host.querySelector<HTMLElement>(".erp-chatter-composer")!;
  expect(bare.classList.contains("has-avatar")).toBe(false);
  expect(bare.querySelector(".erp-mail-avatar")).toBeNull();
  anonymous.dispose();
});

/// (partial: the load error sits directly after the composer, before the activities card, the files and the thread)
test("a failed load puts the error line right after the composer slot content, before the activities", async () => {
  const chatter = new Chatter({ host, model: "res.partner", recordId: nextRecord++, hasActivities: true, rpc: new RpcClient(async () => { throw new Error("Unable to connect"); }, () => {}) });
  await settle();
  const status = host.querySelector<HTMLElement>(".erp-mail-chatter-status")!;
  const slot = status.parentElement!;
  expect(slot.className).toBe("erp-mail-chatter-composer");
  expect(slot.previousElementSibling?.className).toContain("erp-mail-chatter-header");
  expect(slot.nextElementSibling?.className).toContain("erp-mail-chatter-activities");
  expect(status.querySelector(".erp-mail-chatter-error")?.textContent).toContain("Unable to connect");
  chatter.dispose();
});

/// (partial: with the thread loaded the status line follows the composer in its slot)
test("with a mounted composer the status line is the composer's next sibling", async () => {
  const chatter = mount(emptyThread);
  await settle();
  const slot = host.querySelector<HTMLElement>(".erp-mail-chatter-composer")!;
  expect([...slot.children].map((c) => c.className.split(" ")[0])).toEqual(["erp-chatter-composer", "erp-mail-chatter-status"]);
  chatter.dispose();
});

/// (partial: a name of only whitespace draws no avatar)
test("a whitespace-only user name draws no composer avatar", async () => {
  const chatter = new Chatter({ host, model: "res.partner", recordId: nextRecord++, userName: "   ", rpc: new RpcClient(async () => emptyThread(), () => {}) });
  await settle();
  const wrap = host.querySelector<HTMLElement>(".erp-chatter-composer")!;
  expect(wrap.classList.contains("has-avatar")).toBe(false);
  expect(wrap.querySelector(".erp-mail-avatar")).toBeNull();
  chatter.dispose();
});

/// (partial: the composer's document order is its visual and Tab order: Send, hint, tool icons)
test("the composer bar lists Send, the hint, then the tool icons", async () => {
  const chatter = mount(emptyThread);
  await settle();
  const bar = host.querySelector(".erp-mail-composer-bar")!;
  expect([...bar.children].map((c) => c.className.split(" ")[0])).toEqual(["erp-mail-send", "erp-mail-hint", "erp-mail-composer-actions"]);
  chatter.dispose();
});

/// (partial: the header button that schedules an activity reads Activity)
test("the header row offers Send message, Log note and Activity as separate buttons", async () => {
  const chatter = new Chatter({ host, model: "res.partner", recordId: nextRecord++, hasActivities: true, rpc: new RpcClient(async () => emptyThread(), () => {}) });
  await settle();
  const header = host.querySelector(".erp-mail-chatter-header")!;
  // The composer follows the button row directly, before the activities card and the thread.
  expect(header.nextElementSibling?.className).toBe("erp-mail-chatter-composer");
  expect(header.nextElementSibling?.querySelector(".erp-chatter-composer")).not.toBeNull();
  expect([...header.querySelectorAll(".erp-mail-kind, .erp-mail-activities-button")].map((b) => b.textContent?.trim().replace(/\d+$/, ""))).toEqual(["Send message", "Log note", "Activity"]);
  chatter.dispose();
});
