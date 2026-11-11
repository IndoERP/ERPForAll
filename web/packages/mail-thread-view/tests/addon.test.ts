/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
// Two test addons extend the shared components through the registries and typed patch(), without changing the package.
import { registerAddons, type AddonRegistration } from "@erpforall/elang";
import { withTestScope } from "@erpforall/elang-test";
import { expect, test } from "vitest";
import { Composer, Message, Thread, readMessage, type MessageData, type Suggestion } from "../src/index";
import { Rig, article, message, settle, type } from "./support";

const people = { "res.partner": [{ id: 1, name: "Ann" }, { id: 2, name: "Bob" }] };

/** The first addon: a message action, a renderer, a suggestion provider, a composer action and three patches. */
const acmeCalls: AddonRegistration = {
  id: "acme_calls",
  register(ctx) {
    ctx.registry.category("mail.messageActions").add("acme_calls.flag", {
      label: (a) => `Flag ${a.message.id}`,
      pressed: () => false,
      run: (a) => { flagged.push(a.message.id); },
    }, { sequence: 15 });
    ctx.registry.category("mail.messageRenderers").add("acme_calls.call", {
      match: (m: MessageData) => m.messageType === "acme_call",
      layout: "row",
      render: ({ view, message: m }) => {
        const card = view.document.createElement("div");
        card.className = "acme-call-card";
        card.textContent = `Call log for #${m.id}`;
        return card;
      },
    }, { sequence: 5 });
    ctx.registry.category("mail.suggestionProviders").add("#", {
      delimiter: "#",
      label: () => "Channels",
      search: (term): readonly Suggestion[] => ["general", "sales"].filter((c) => c.startsWith(term)).map((c) => ({ id: c, label: `#${c}` })),
    });
    ctx.registry.category("mail.composerActions").add("acme_calls.log", {
      label: () => "Log call",
      run: (a) => { a.insertText("[call] "); },
    });
    ctx.patch(Message, (sup) => ({ authorName(m) { return `${sup.authorName.call(this, m)} (A)`; } }));
    ctx.patch(Thread, (sup) => ({ dayLabel(date, now) { return `${sup.dayLabel.call(this, date, now)}!`; } }));
    ctx.patch(Composer, (sup) => ({ buildPost() { return { ...sup.buildPost.call(this), subject: "Re: order" }; } }));
  },
};

/** The second addon patches the same members, and adds an action to the same registry. */
const acmeTags: AddonRegistration = {
  id: "acme_tags",
  register(ctx) {
    ctx.registry.category("mail.messageActions").add("acme_tags.tag", { label: () => "Tag", run: () => undefined }, { sequence: 12 });
    ctx.patch(Message, (sup) => ({ authorName(m) { return `${sup.authorName.call(this, m)} [B]`; } }));
    ctx.patch(Composer, (sup) => ({ buildPost() { return { ...sup.buildPost.call(this), partnerIds: [2] }; } }));
  },
};

const flagged: number[] = [];

test("addons add actions, renderers, providers and composer actions through the registries", async () => withTestScope(async (scope) => {
  flagged.length = 0;
  const rig = new Rig({ hub: scope.registries });
  scope.manage(() => registerAddons([acmeCalls, acmeTags], { registry: scope.registries }));
  rig.records(people);
  rig.load([message(1), message(2, { message_type: "acme_call", body: "" })]);
  rig.transport.postHandler = () => Promise.resolve({ message_id: 9, replayed: false, store: rig.payload({ "mail.message": [message(9)] }) });
  const thread = rig.thread({ order: "asc" });
  // Both addons' actions sit between the built-ins by sequence.
  const names = [...article(thread, 1).querySelectorAll<HTMLElement>(".erp-mail-action")].map((b) => b.dataset["action"]);
  expect(names).toEqual(["acme_tags.tag", "acme_calls.flag", "bookmark", "copy-text", "pin"]);
  article(thread, 1).querySelector<HTMLButtonElement>('[data-action="acme_calls.flag"]')!.click();
  await settle();
  expect(flagged).toEqual([1]);
  // The addon's renderer draws its message type.
  expect(article(thread, 2).querySelector(".acme-call-card")?.textContent).toBe("Call log for #2");
  expect(article(thread, 2).dataset["layout"]).toBe("row");

  const composer = rig.composer();
  const actions = [...composer.element.querySelectorAll<HTMLElement>(".erp-mail-composer-action")].map((b) => b.dataset["action"]);
  expect(actions).toEqual(["acme_calls.log"]);
  (composer.element.querySelector('[data-action="acme_calls.log"]') as HTMLButtonElement).click();
  await settle();
  expect(composer.field.value).toBe("[call] ");
  type(composer.field, "[call] see #ge");
  await settle();
  expect([...composer.element.querySelectorAll(".erp-mail-suggestion")].map((o) => o.textContent)).toEqual(["#general"]);
  composer.field.dispatchEvent(new KeyboardEvent("keydown", { key: "Enter", bubbles: true, cancelable: true }));
  expect(composer.field.value).toBe("[call] see #general ");
  expect(rig.store.local("mail_ui.thread").get("mail.thread:7", "mentions")).toEqual([]);
}));

test("two addons patch the same members of Message, Thread and Composer and the chain composes and unwinds", async () => withTestScope(async (scope) => {
  const rig = new Rig({ hub: scope.registries });
  const sets = scope.manage(() => registerAddons([acmeCalls, acmeTags], { registry: scope.registries }));
  rig.records(people);
  rig.load([message(1), message(2, { date: "2026-11-07 10:00:00" })]);
  rig.transport.postHandler = () => Promise.resolve({ message_id: 9, replayed: false, store: rig.payload({ "mail.message": [message(9)] }) });
  const thread = rig.thread({ order: "asc" });
  const data = readMessage(rig.store, 1, null)!;
  // Registration order: acme_calls wraps the original, acme_tags wraps that.
  expect(thread.message(1)!.authorName(data)).toBe("Ann (A) [B]");
  expect(article(thread, 1).querySelector(".erp-mail-author")?.textContent).toBe("Ann (A) [B]");
  expect([...thread.element.querySelectorAll(".erp-mail-day")].map((d) => d.textContent)).toEqual(["Today!", "Yesterday!"]);
  const composer = rig.composer();
  type(composer.field, "hello");
  await composer.send();
  await settle();
  // Both addons' keys reached the DTO; the retry keeps them because they are stored with the unsent post.
  expect(rig.transport.posts[0]?.dto).toEqual({
    body: "hello", message_type: "comment", subtype: "comment", subject: "Re: order", partner_ids: [2], client_uid: "uid-1",
  });

  // Removing one addon unwinds only its layer of each chain.
  sets.get("acme_calls")!.unpatchAll();
  expect(thread.message(1)!.authorName(data)).toBe("Ann [B]");
  expect(thread.dayLabel(new Date("2026-11-07T10:00:00Z"), new Date("2026-11-08T12:00:00Z"))).toBe("Yesterday");
  type(composer.field, "again");
  await composer.send();
  await settle();
  expect(rig.transport.posts[1]?.dto).toEqual({ body: "again", message_type: "comment", subtype: "comment", partner_ids: [2], client_uid: "uid-2" });
  sets.get("acme_tags")!.unpatchAll();
  expect(thread.message(1)!.authorName(data)).toBe("Ann");
}));
