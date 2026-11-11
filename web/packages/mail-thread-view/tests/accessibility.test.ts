/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
// Accessibility of the mounted thread and composer: axe on every state, and the roles and names it cannot judge.
import { expect, test } from "vitest";
import { auditComponent } from "../../../testing/axe";
import { Rig, article, key, message, settle, type } from "./support";

const tracking = JSON.stringify([{ f: 3, n: "stage_id", t: "many2one", o: 1, v: 2, od: "New", vd: "Won" }]);

test("the thread and the composer pass axe in every state and name every control", async () => {
  const rig = new Rig();
  rig.records({
    "res.partner": [{ id: 1, name: "Ann" }, { id: 2, name: "Bob" }],
    "ir.attachment": [{ id: 4, name: "plan.pdf", mimetype: "application/pdf" }],
    "mail.message.reaction": [{ id: 1, content: "\u{1F44D}", partner_id: 1 }],
  });
  rig.load([
    message(1, { attachments: [4], reactions: [1], subject: "Quarterly plan", starred: true }),
    message(2, { author_id: 2, message_type: "notification", body: "", tracking }),
    message(3, { author_id: 2, is_internal: true, body: "<p>Note with <a href=\"https://example.com\">link</a></p>" }),
  ], true);
  rig.services = { ...rig.services, pickEmoji: () => Promise.resolve(null) };
  rig.transport.postHandler = () => Promise.reject(new Error("offline"));
  const main = document.createElement("main");
  document.body.append(main);
  const thread = rig.thread({ order: "asc", allowReply: true });
  const composer = rig.composer();
  main.append(thread.element, composer.element);

  await auditComponent(main);
  // The suggestion list open, then an unsent failed post and the composer error line.
  type(composer.field, "hello @an");
  await settle();
  expect(composer.element.querySelector(".erp-mail-suggestions")?.hasAttribute("hidden")).toBe(false);
  await auditComponent(main);
  key(composer.field, "Escape");
  type(composer.field, "unsent");
  await composer.send();
  await settle();
  expect(thread.element.querySelector(".erp-mail-pending.is-failed")).not.toBeNull();
  await auditComponent(main);
  type(composer.field, "   ");
  await composer.send();
  composer.setKind("note");
  await settle();
  await auditComponent(main);

  // Every button has a text name, every toggle states its state, every list and group is named.
  for (const button of main.querySelectorAll<HTMLButtonElement>("button")) {
    const name = button.getAttribute("aria-label") ?? button.textContent ?? "";
    expect(name.replace(/\s+/g, " ").trim(), button.outerHTML).not.toBe("");
  }
  for (const toggle of main.querySelectorAll(".erp-mail-kind, .erp-mail-reaction")) expect(["true", "false"]).toContain(toggle.getAttribute("aria-pressed"));
  for (const named of main.querySelectorAll('[role="feed"], [role="toolbar"], [role="group"], form, ul.erp-mail-attachments')) {
    expect(named.getAttribute("aria-label") ?? "", named.outerHTML.slice(0, 80)).not.toBe("");
  }
  expect(main.querySelectorAll('[role="alert"]:not([hidden])').length).toBeGreaterThan(0);
  // Decorative glyphs are hidden from assistive technology.
  for (const glyph of main.querySelectorAll(".erp-mail-glyph, .erp-mail-tracking-arrow")) expect(glyph.getAttribute("aria-hidden")).toBe("true");
  // Only the current article is a tab stop, and no element has a positive tabindex.
  expect([...main.querySelectorAll<HTMLElement>("[tabindex]")].filter((e) => e.tabIndex > 0)).toEqual([]);
  expect([...main.querySelectorAll("article")].filter((a) => (a as HTMLElement).tabIndex === 0)).toHaveLength(1);
  expect(article(thread, 1).getAttribute("tabindex")).toBe("0");
});

test("an empty thread, a loading thread and a thread whose load failed pass axe and keep the feed valid", async () => {
  const rig = new Rig();
  const main = document.createElement("main");
  document.body.append(main);
  let fail!: (error: Error) => void;
  rig.pager.latestHandler = () => new Promise<void>((_, reject) => { fail = reject; });
  const thread = rig.thread();
  main.append(thread.element);
  // Loading: no window yet, the feed is hidden because a feed without an article is not valid.
  expect((thread.element.querySelector('[role="feed"]') as HTMLElement).hidden).toBe(true);
  await auditComponent(main);
  fail(new Error("gateway timeout"));
  await settle();
  expect(thread.element.querySelector(".erp-mail-thread-status")?.getAttribute("role")).toBe("alert");
  await auditComponent(main);
  // An empty window: the empty text is outside the feed.
  rig.store.windows.replace(rig.ref, [], false);
  await settle();
  const feed = thread.element.querySelector('[role="feed"]') as HTMLElement;
  expect(feed.hidden).toBe(true);
  expect(feed.querySelector(".erp-mail-empty")).toBeNull();
  expect((thread.element.querySelector(".erp-mail-empty") as HTMLElement).hidden).toBe(false);
  await auditComponent(main);
  // The first message makes the feed visible again.
  rig.records({ "res.partner": [{ id: 1, name: "Ann" }], "mail.message": [message(1)] });
  rig.store.windows.appendLive(rig.ref, 1);
  await settle();
  expect(feed.hidden).toBe(false);
  expect((thread.element.querySelector(".erp-mail-empty") as HTMLElement).hidden).toBe(true);
  await auditComponent(main);
});
