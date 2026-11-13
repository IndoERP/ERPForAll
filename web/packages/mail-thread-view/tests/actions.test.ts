/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
// Message actions: the registry mechanics and the built-in actions with their capability checks.
import { flushSync } from "@erpforall/elang";
import { expect, test } from "vitest";
import { OWNING_TASK, type MessageAction } from "../src/index";
import { Rig, SCHEMA_MIN, article, message, settle } from "./support";

const ann = { "res.partner": [{ id: 1, name: "Ann" }, { id: 2, name: "Bob" }] };
const toolbar = (el: HTMLElement): string[] => [...el.querySelectorAll<HTMLElement>(".erp-mail-action")].map((b) => b.dataset["action"] ?? "");
const alertText = (root: HTMLElement): string => root.querySelector(".erp-mail-sr-only [aria-live='assertive']")?.textContent ?? "";

test("actions are ordered by sequence, filtered by condition, hidden with a reason when unavailable and report failures visibly", async () => {
  const rig = new Rig({ actions: false });
  rig.records(ann);
  rig.load([message(1), message(2, { author_id: 2 })]);
  const ran: { id: number; anchor: HTMLElement }[] = [];
  const add = (id: string, def: Partial<MessageAction>, sequence = 50): void => {
    rig.registries.messageActions.add(id, { label: (ctx) => `${id} ${ctx.message.id}`, run: () => undefined, ...def }, { sequence });
  };
  add("second", { run: (ctx) => { ran.push({ id: ctx.message.id, anchor: ctx.anchor }); } }, 20);
  add("first", {}, 10);
  add("toggle", { pressed: (ctx) => ctx.message.id === 2 });
  add("never", { condition: () => false });
  add("needs", { unavailable: () => "Needs the thing" });
  add("only-second", { condition: (ctx) => ctx.message.id === 2 }, 60);
  add("glyph", { glyph: "!" }, 70);
  const thread = rig.thread({ order: "asc" });
  expect(toolbar(article(thread, 1))).toEqual(["first", "second", "toggle", "glyph"]);
  expect(toolbar(article(thread, 2))).toEqual(["first", "second", "toggle", "only-second", "glyph"]);
  expect(thread.message(1)?.hiddenActions()).toEqual([{ id: "needs", reason: "Needs the thing" }]);
  const buttons = [...article(thread, 2).querySelectorAll<HTMLButtonElement>(".erp-mail-action")];
  expect(buttons.map((b) => b.textContent)).toEqual(["first 2", "second 2", "toggle 2", "only-second 2", "!glyph 2"]);
  expect(buttons.map((b) => b.getAttribute("aria-pressed"))).toEqual([null, null, "true", null, null]);
  expect(buttons[4]?.querySelector(".erp-mail-glyph")?.getAttribute("aria-hidden")).toBe("true");
  expect(article(thread, 2).querySelector('[role="toolbar"]')?.getAttribute("aria-label")).toBe("Message actions");
  buttons[1]!.click();
  await settle();
  expect(ran).toEqual([{ id: 2, anchor: buttons[1] }]);

  // A failure is shown on the message, announced and reported; the next success clears it.
  const failure = new Error("nope");
  rig.registries.messageActions.add("fail", { label: () => "fail", run: () => { throw failure; } });
  flushSync();
  article(thread, 1).querySelector<HTMLButtonElement>('[data-action="fail"]')!.click();
  await settle();
  const line = article(thread, 1).querySelector(".erp-mail-message-error") as HTMLElement;
  expect(line.hidden).toBe(false);
  expect(line.textContent).toBe("nope");
  expect(alertText(thread.element)).toBe("nope");
  expect(rig.errors).toEqual([failure]);
  article(thread, 1).querySelector<HTMLButtonElement>('[data-action="first"]')!.click();
  await settle();
  expect(line.hidden).toBe(true);

  // A rejection is handled the same way, and a second activation while the action runs is ignored.
  let release!: () => void;
  let runs = 0;
  rig.registries.messageActions.add("slow", { label: () => "slow", run: () => new Promise<void>((resolve) => { runs += 1; release = resolve; }) });
  flushSync();
  const slow = article(thread, 1).querySelector<HTMLButtonElement>('[data-action="slow"]')!;
  slow.click();
  slow.click();
  expect(runs).toBe(1);
  expect(slow.getAttribute("aria-disabled")).toBe("true");
  release();
  await settle();
  expect(slow.hasAttribute("aria-disabled")).toBe(false);

  // A condition that throws is reported and skips only that action.
  rig.errors.length = 0;
  const broken = new Error("condition broke");
  rig.registries.messageActions.add("broken", { label: () => "broken", condition: () => { throw broken; }, run: () => undefined });
  flushSync();
  expect(toolbar(article(thread, 1))).not.toContain("broken");
  expect(toolbar(article(thread, 1))).toContain("first");
  expect(rig.errors).toContain(broken);
});

test("bookmark and pin act through the transport on the viewer's flag and are hidden with the owner task when the schema lacks them", async () => {
  const rig = new Rig();
  rig.records(ann);
  rig.load([message(1, { starred: true }), message(2, { pinned_at: "2026-11-08 10:00:00" })]);
  rig.transport.enable("setBookmark");
  rig.transport.enable("setPinned");
  const thread = rig.thread({ order: "asc" });
  const bookmark = (id: number) => article(thread, id).querySelector<HTMLButtonElement>('[data-action="bookmark"]')!;
  const pin = (id: number) => article(thread, id).querySelector<HTMLButtonElement>('[data-action="pin"]')!;
  expect([bookmark(1).textContent, bookmark(1).getAttribute("aria-pressed"), bookmark(2).textContent, bookmark(2).getAttribute("aria-pressed")]).toEqual(["Remove bookmark", "true", "Bookmark", "false"]);
  expect([pin(1).textContent, pin(2).textContent, pin(2).getAttribute("aria-pressed")]).toEqual(["Pin", "Unpin", "true"]);
  bookmark(1).click();
  bookmark(2).click();
  pin(2).click();
  pin(1).click();
  await settle();
  expect(rig.transport.calls).toEqual([
    { name: "setBookmark", args: [1, false] },
    { name: "setBookmark", args: [2, true] },
    { name: "setPinned", args: [2, false] },
    { name: "setPinned", args: [1, true] },
  ]);

  const bare = new Rig({ schema: SCHEMA_MIN });
  bare.records(ann);
  bare.load([message(1)]);
  const bareThread = bare.thread();
  expect(toolbar(article(bareThread, 1))).not.toContain("bookmark");
  expect(bareThread.message(1)?.hiddenActions().map((a) => a.id)).toEqual(["reaction", "bookmark", "pin"]);
  expect(bareThread.message(1)?.hiddenActions().find((a) => a.id === "bookmark")?.reason).toBe(`Bookmarks need ${OWNING_TASK.bookmark} (the schema has no mail.message.starred)`);
  expect(bareThread.message(1)?.hiddenActions().find((a) => a.id === "pin")?.reason).toBe(`Pinning needs ${OWNING_TASK.pin} (the schema has no mail.message.pinned_at)`);
  expect(bareThread.message(1)?.hiddenActions().find((a) => a.id === "reaction")?.reason).toBe(`Reactions need ${OWNING_TASK.reactions} (the schema has no mail.message.reactions)`);
});

test("a built-in action whose transport operation is missing fails loudly with the owner task instead of doing nothing", async () => {
  const rig = new Rig();
  rig.records(ann);
  rig.load([message(1)]);
  const thread = rig.thread();
  article(thread, 1).querySelector<HTMLButtonElement>('[data-action="bookmark"]')!.click();
  await settle();
  const expected = "Bookmarking a message is not available until a later release (the transport has no setBookmark operation)";
  expect(article(thread, 1).querySelector(".erp-mail-message-error")?.textContent).toBe(expected);
  expect(rig.errors.map((e) => (e as Error).message)).toEqual([expected]);
  expect(alertText(thread.element)).toBe(expected);
  article(thread, 1).querySelector<HTMLButtonElement>('[data-action="pin"]')!.click();
  await settle();
  expect(rig.errors.map((e) => (e as Error).message).at(-1)).toBe("Pinning a message is not available until a later release (the transport has no setPinned operation)");
});

test("reactions: chips toggle the viewer's own reaction, the picker adds one, and the answer's store payload updates the counts", async () => {
  const rig = new Rig();
  const rows = [{ id: 1, content: "\u{1F44D}", partner_id: 1 }, { id: 2, content: "\u{1F44D}", partner_id: 2 }, { id: 3, content: "\u{1F389}", partner_id: 1 }];
  rig.records({ ...ann, "mail.message.reaction": rows });
  rig.load([message(1, { reactions: [1, 2, 3] })]);
  const picks: (string | null)[] = ["\u{1F602}", null, "\u{1F389}"];
  rig.services = { ...rig.services, pickEmoji: () => Promise.resolve(picks.shift() ?? null) };
  let next = 10;
  rig.transport.setReaction = (id, content, present) => {
    rig.transport.calls.push({ name: "setReaction", args: [id, content, present] });
    const records: unknown[] = [];
    if (present) {
      rows.push({ id: next, content, partner_id: 2 });
      records.push({ id: next, content, partner_id: 2 });
      next += 1;
    } else {
      const at = rows.findIndex((r) => r.content === content && r.partner_id === 2);
      records.push({ id: rows[at]!.id, delete: true });
      rows.splice(at, 1);
    }
    return Promise.resolve({ store: rig.payload({ "mail.message.reaction": records, "mail.message": [{ id, reactions: rows.map((r) => r.id) }] }) });
  };
  const thread = rig.thread();
  const chips = [...article(thread, 1).querySelectorAll<HTMLButtonElement>(".erp-mail-reaction")];
  expect(chips.map((c) => c.getAttribute("aria-label"))).toEqual(["\u{1F44D}, 2 reactions, you reacted", "\u{1F389}, 1 reaction"]);
  expect(chips.map((c) => c.getAttribute("aria-pressed"))).toEqual(["true", "false"]);
  chips[0]!.click();
  await settle();
  chips[1]!.click();
  await settle();
  expect(rig.transport.calls).toEqual([
    { name: "setReaction", args: [1, "\u{1F44D}", false] },
    { name: "setReaction", args: [1, "\u{1F389}", true] },
  ]);
  const labels = () => [...article(thread, 1).querySelectorAll(".erp-mail-reaction")].map((c) => c.getAttribute("aria-label"));
  expect(labels()).toEqual(["\u{1F44D}, 1 reaction", "\u{1F389}, 2 reactions, you reacted"]);
  // The picker: a new emoji is added, a dismissed picker does nothing, an emoji the viewer already used does nothing.
  rig.transport.calls.length = 0;
  const react = () => article(thread, 1).querySelector<HTMLButtonElement>('[data-action="reaction"]')!;
  react().click();
  await settle();
  react().click();
  await settle();
  react().click();
  await settle();
  expect(rig.transport.calls).toEqual([{ name: "setReaction", args: [1, "\u{1F602}", true] }]);
  expect(labels()).toEqual(["\u{1F44D}, 1 reaction", "\u{1F389}, 2 reactions, you reacted", "\u{1F602}, 1 reaction, you reacted"]);
  expect(rig.errors).toEqual([]);
});

test("copy text uses the host service or the clipboard and refuses loudly when neither exists", async () => {
  const rig = new Rig();
  rig.records(ann);
  rig.load([message(1, { body: "<p>One</p><p>Two</p>" })]);
  const copied: string[] = [];
  rig.services = { ...rig.services, copyText: (text) => { copied.push(text); return Promise.resolve(); } };
  const thread = rig.thread();
  article(thread, 1).querySelector<HTMLButtonElement>('[data-action="copy-text"]')!.click();
  await settle();
  expect(copied).toEqual(["One\nTwo"]);
  expect(thread.element.querySelector(".erp-mail-sr-only [role='status']")?.textContent).toBe("Copied");

  const bare = new Rig();
  bare.records(ann);
  bare.load([message(1)]);
  const bareThread = bare.thread();
  article(bareThread, 1).querySelector<HTMLButtonElement>('[data-action="copy-text"]')!.click();
  await settle();
  const expected = "Copying text is not available: this browser context has no clipboard (it needs a secure context) and the host gave no services.copyText";
  expect(article(bareThread, 1).querySelector(".erp-mail-message-error")?.textContent).toBe(expected);
  expect(bare.errors.map((e) => (e as Error).message)).toEqual([expected]);
});

test("delete is offered on the viewer's own comments, asks for confirmation and applies the answer that removes the message", async () => {
  const rig = new Rig();
  rig.records(ann);
  rig.load([message(1, { author_id: 1 }), message(2, { author_id: 2 }), message(3, { author_id: 2, message_type: "notification" })]);
  const asked: unknown[] = [];
  let accept = false;
  rig.services = { ...rig.services, confirm: (options) => { asked.push(options); return Promise.resolve(accept); } };
  rig.transport.deleteMessage = (id) => {
    rig.transport.calls.push({ name: "deleteMessage", args: [id] });
    return Promise.resolve({ store: rig.payload({ "mail.message": [{ id, delete: true }] }) });
  };
  const thread = rig.thread({ order: "asc" });
  expect(toolbar(article(thread, 1))).not.toContain("delete");
  expect(toolbar(article(thread, 3))).not.toContain("delete");
  const del = () => article(thread, 2).querySelector<HTMLButtonElement>('[data-action="delete"]')!;
  del().click();
  await settle();
  expect(asked).toEqual([{ title: "Delete message", message: "Do you really want to delete this message?", accept: "Delete", cancel: "Cancel" }]);
  expect(rig.transport.calls).toEqual([]);
  accept = true;
  del().click();
  await settle();
  expect(rig.transport.calls).toEqual([{ name: "deleteMessage", args: [2] }]);
  expect(thread.displayedIds()).toEqual([1, 3]);

  // Without the operation or the dialog the action is hidden with the reason.
  const bare = new Rig();
  bare.records(ann);
  bare.load([message(2, { author_id: 2 })]);
  const reason = (r: Rig) => r.thread().message(2)?.hiddenActions().find((a) => a.id === "delete")?.reason;
  expect(reason(bare)).toBe(`Deleting a message needs ${OWNING_TASK.delete} (the transport has no deleteMessage operation)`);
  bare.transport.deleteMessage = () => Promise.resolve({});
  expect(reason(bare)).toBe("Deleting a message needs the host's confirmation dialog (services.confirm)");
});

test("reply is offered only where the host allows it, sets the composer's reply target and toggles it off", async () => {
  const rig = new Rig();
  rig.records(ann);
  rig.load([message(1), message(2, { message_type: "notification" })]);
  const flat = rig.thread();
  expect(toolbar(article(flat, 1))).not.toContain("reply");
  const thread = rig.thread({ allowReply: true });
  expect(toolbar(article(thread, 2))).not.toContain("reply");
  const reply = () => article(thread, 1).querySelector<HTMLButtonElement>('[data-action="reply"]')!;
  expect(reply().getAttribute("aria-pressed")).toBe("false");
  reply().click();
  await settle();
  expect(rig.store.local("mail_ui.thread").get("mail.thread:7", "reply_to")).toBe(1);
  expect(reply().getAttribute("aria-pressed")).toBe("true");
  reply().click();
  await settle();
  expect(rig.store.local("mail_ui.thread").get("mail.thread:7", "reply_to")).toBeNull();
  expect(reply().getAttribute("aria-pressed")).toBe("false");
});
