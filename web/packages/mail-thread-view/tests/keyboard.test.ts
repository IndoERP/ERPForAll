/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
// Keyboard use of the feed and the message toolbar: one tab stop per level, arrows, Home/End, Escape, focus after removal.
import { flushSync } from "@erpforall/elang";
import { expect, test } from "vitest";
import { Rig, article, key, message } from "./support";

const ann = { "res.partner": [{ id: 1, name: "Ann" }, { id: 2, name: "Bob" }] };
const buttons = (el: HTMLElement): HTMLButtonElement[] => [...el.querySelectorAll<HTMLButtonElement>(".erp-mail-action")];
const tabbable = (root: HTMLElement): string[] =>
  [...root.querySelectorAll<HTMLElement>("article, .erp-mail-action, .erp-mail-reaction")].filter((e) => e.tabIndex === 0).map((e) => (e.tagName === "ARTICLE" ? `article ${e.dataset["messageId"]}` : `${e.closest("article")?.dataset["messageId"]}:${e.dataset["action"]}`));

function setup(order: "asc" | "desc" = "asc") {
  const rig = new Rig();
  rig.records(ann);
  rig.load([message(1), message(2), message(3), message(4)]);
  const thread = rig.thread({ order });
  return { rig, thread };
}

test("the feed has one tab stop, the current article's toolbar is the next, and the arrows, Page keys, Home and End move between articles", () => {
  const { thread } = setup();
  expect(tabbable(thread.element)).toEqual(["article 1", "1:bookmark"]);
  thread.focusMessage(1);
  expect(document.activeElement).toBe(article(thread, 1));
  expect(key(article(thread, 1), "ArrowDown").defaultPrevented).toBe(true);
  expect(document.activeElement).toBe(article(thread, 2));
  expect(tabbable(thread.element)).toEqual(["article 2", "2:bookmark"]);
  key(article(thread, 2), "PageDown");
  expect(document.activeElement).toBe(article(thread, 3));
  key(article(thread, 3), "End");
  expect(document.activeElement).toBe(article(thread, 4));
  key(article(thread, 4), "ArrowDown");
  expect(document.activeElement).toBe(article(thread, 4));
  key(article(thread, 4), "PageUp");
  expect(document.activeElement).toBe(article(thread, 3));
  key(article(thread, 3), "ArrowUp");
  key(article(thread, 2), "Home");
  expect(document.activeElement).toBe(article(thread, 1));
  expect(tabbable(thread.element)).toEqual(["article 1", "1:bookmark"]);
  // Keys with a modifier belong to the browser or the page.
  expect(key(article(thread, 1), "ArrowDown", { ctrlKey: true }).defaultPrevented).toBe(false);
  expect(document.activeElement).toBe(article(thread, 1));
  // Clicking into a message makes it the current one.
  article(thread, 3).querySelector<HTMLElement>(".erp-mail-body")?.dispatchEvent(new Event("click", { bubbles: true }));
  buttons(article(thread, 3))[1]!.focus();
  expect(tabbable(thread.element)).toEqual(["article 3", "3:copy-text"]);
});

test("newest first, Down moves toward older messages; the toolbar is a roving group with Escape back to its article", () => {
  const { thread } = setup("desc");
  expect(thread.displayedIds()).toEqual([4, 3, 2, 1]);
  thread.focusMessage(4);
  key(article(thread, 4), "ArrowDown");
  expect(document.activeElement).toBe(article(thread, 3));
  const bar = buttons(article(thread, 3));
  expect(bar.map((b) => b.dataset["action"])).toEqual(["bookmark", "copy-text", "pin"]);
  bar[0]!.focus();
  expect(tabbable(thread.element)).toEqual(["article 3", "3:bookmark"]);
  key(bar[0]!, "ArrowRight");
  expect(document.activeElement).toBe(bar[1]);
  expect(tabbable(thread.element)).toEqual(["article 3", "3:copy-text"]);
  key(bar[1]!, "End");
  expect(document.activeElement).toBe(bar[2]);
  key(bar[2]!, "ArrowRight");
  expect(document.activeElement).toBe(bar[0]);
  key(bar[0]!, "ArrowLeft");
  expect(document.activeElement).toBe(bar[2]);
  key(bar[2]!, "Home");
  expect(document.activeElement).toBe(bar[0]);
  // The toolbar's own arrows do not move between articles.
  expect(document.activeElement?.closest("article")).toBe(article(thread, 3));
  key(bar[0]!, "Escape");
  expect(document.activeElement).toBe(article(thread, 3));
});

test("removing the focused message moves focus to its neighbour, and removing the last one moves it to the thread", () => {
  const { rig, thread } = setup();
  thread.focusMessage(2);
  rig.store.windows.remove(2);
  flushSync();
  expect(document.activeElement).toBe(article(thread, 3));
  expect(tabbable(thread.element)).toEqual(["article 3", "3:bookmark"]);
  rig.store.windows.remove(4);
  flushSync();
  thread.focusMessage(3);
  rig.store.windows.remove(3);
  flushSync();
  expect(document.activeElement).toBe(article(thread, 1));
  rig.store.windows.remove(1);
  flushSync();
  expect(document.activeElement).toBe(thread.element);
  expect(thread.element.tabIndex).toBe(-1);
  // Focus that was elsewhere stays where it was.
  rig.records({ "mail.message": [message(8)] });
  rig.store.windows.appendLive(rig.ref, 8);
  const outside = document.createElement("button");
  document.body.append(outside);
  outside.focus();
  flushSync();
  rig.store.windows.remove(8);
  flushSync();
  expect(document.activeElement).toBe(outside);
});
