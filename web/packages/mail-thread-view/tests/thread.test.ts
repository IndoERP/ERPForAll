/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
// The thread: the window as a feed, lease, live updates, paging, failures, squashing, separators and the subject rule.
import { flushSync } from "@erpforall/elang";
import { expect, test } from "vitest";
import { Thread, anchorDelta, readMessage } from "../src/index";
import { Rig, article, failsWith, message, settle } from "./support";

const ann = { "res.partner": [{ id: 1, name: "Ann" }, { id: 2, name: "Bob" }, { id: 3, name: "Cy" }] };

function ids(thread: Thread): number[] {
  return [...thread.element.querySelectorAll<HTMLElement>("article[data-message-id]")].map((a) => Number(a.dataset["messageId"]));
}

test("the window is drawn as a labelled feed of articles, newest first or last", () => {
  const rig = new Rig();
  rig.records(ann);
  rig.load([message(1), message(2, { author_id: 2 }), message(3)]);
  const newestFirst = rig.thread();
  expect(newestFirst.displayedIds()).toEqual([3, 2, 1]);
  expect(ids(newestFirst)).toEqual([3, 2, 1]);
  const feed = newestFirst.element.querySelector('[role="feed"]') as HTMLElement;
  expect(feed.getAttribute("aria-label")).toBe("Messages");
  expect(feed.getAttribute("aria-busy")).toBe("false");
  const second = article(newestFirst, 2);
  expect(second.getAttribute("role")).toBeNull();
  expect(second.tagName).toBe("ARTICLE");
  expect(second.getAttribute("aria-posinset")).toBe("2");
  expect(second.getAttribute("aria-setsize")).toBe("-1");
  // The article is named by its author and time, and described by its body.
  const name = second.getAttribute("aria-labelledby")!.split(" ").map((id) => document.getElementById(id)?.textContent?.replace(/\s/g, " "));
  expect(name).toEqual(["Bob", "09:00 AM"]);
  expect(document.getElementById(second.getAttribute("aria-describedby")!)?.textContent).toBe("Message 2");

  const newestLast = rig.thread({ order: "asc" });
  expect(ids(newestLast)).toEqual([1, 2, 3]);
  expect(newestLast.element.dataset["order"]).toBe("asc");
  // The older-messages control sits at the end the older messages come from.
  expect(newestFirst.element.lastElementChild?.className).toBe("erp-mail-load-older");
  expect(newestLast.element.querySelector(".erp-mail-load-older")?.nextElementSibling).toBe(newestLast.element.querySelector('[role="feed"]'));
});

test("a mounted thread holds its window against eviction and disposal releases it, removes the element and refuses later use", () => {
  const rig = new Rig({ config: { maxThreads: 1 } });
  rig.load([message(1)]);
  for (const id of [8, 9]) rig.store.windows.replace({ model: "mail.thread", id }, [], false);
  const thread = rig.thread();
  expect(rig.store.collect().windows).toBe(1);
  expect(rig.store.windows.get(rig.ref)?.ids).toEqual([1]);

  thread.dispose();
  thread.dispose();
  expect(thread.element.isConnected).toBe(false);
  failsWith(() => thread.focusMessage(1), "disposed", "this Thread was disposed");
  for (const id of [10, 11]) rig.store.windows.replace({ model: "mail.thread", id }, [], false);
  rig.store.collect();
  expect(rig.store.windows.get(rig.ref)).toBeUndefined();

  // A disposed thread no longer follows the store.
  const before = thread.element.innerHTML;
  rig.records({ "mail.message": [message(2)] });
  expect(thread.element.innerHTML).toBe(before);
});

test("a message that changes redraws only its own article and the window follows appended and removed messages", () => {
  const rig = new Rig();
  rig.records(ann);
  rig.load([message(1), message(2)]);
  const thread = rig.thread({ order: "asc" });
  const first = article(thread, 1).querySelector(".erp-mail-message-content");
  const second = article(thread, 2).querySelector(".erp-mail-message-content");
  rig.records({ "mail.message": [message(2, { body: "<p>Edited body</p>" })] });
  expect(article(thread, 2).textContent).toContain("Edited body");
  expect(article(thread, 1).querySelector(".erp-mail-message-content")).toBe(first);
  expect(article(thread, 2).querySelector(".erp-mail-message-content")).not.toBe(second);

  rig.records({ "mail.message": [message(3)] });
  expect(rig.store.windows.appendLive(rig.ref, 3)).toBe("appended");
  flushSync();
  expect(ids(thread)).toEqual([1, 2, 3]);
  const kept = article(thread, 1);
  rig.store.windows.remove(2);
  flushSync();
  expect(ids(thread)).toEqual([1, 3]);
  expect(thread.message(2)).toBeUndefined();
  expect(article(thread, 1)).toBe(kept);
  expect(kept.getAttribute("aria-posinset")).toBe("1");
  expect(article(thread, 3).getAttribute("aria-posinset")).toBe("2");
});

test("Load older asks the pager for the page below the oldest message, once, and shows the busy state", async () => {
  const rig = new Rig();
  rig.records(ann);
  rig.load([message(31), message(32), message(33)], true);
  const thread = rig.thread();
  const button = thread.element.querySelector(".erp-mail-load-older") as HTMLButtonElement;
  expect(button.hidden).toBe(false);
  expect(button.textContent).toBe("Load older messages");
  let finish!: () => void;
  rig.pager.olderHandler = () => new Promise<void>((resolve) => {
    finish = () => {
      rig.records({ "mail.message": [message(21), message(22)] });
      rig.store.windows.prependOlder(rig.ref, [21, 22], false);
      resolve();
    };
  });
  button.click();
  button.click();
  flushSync();
  expect(rig.pager.older).toEqual([{ ref: rig.ref, before: 31 }]);
  expect(button.getAttribute("aria-disabled")).toBe("true");
  expect(button.textContent).toBe("Loading older messages");
  expect(thread.element.querySelector('[role="feed"]')?.getAttribute("aria-busy")).toBe("true");
  finish();
  await settle();
  expect(thread.displayedIds()).toEqual([33, 32, 31, 22, 21]);
  expect(button.hidden).toBe(true);
  expect(thread.element.querySelector('[role="feed"]')?.getAttribute("aria-busy")).toBe("false");
});

test("the reader's place is kept when older messages are added and a new message follows only a reader at the newest end", () => {
  expect(anchorDelta("asc", 1000, 1400)).toBe(400);
  expect(anchorDelta("desc", 1000, 1400)).toBe(0);
  const rig = new Rig();
  rig.records(ann);
  rig.load([message(31), message(32)], true);
  const thread = rig.thread({ order: "asc" });
  // Each article is 250 px tall in this fake layout, on top of 500 px of chrome.
  const el = thread.element;
  const height = (): number => 500 + el.querySelectorAll("article").length * 250;
  let top = 300;
  Object.defineProperty(el, "scrollHeight", { configurable: true, get: height });
  Object.defineProperty(el, "clientHeight", { configurable: true, get: () => 500 });
  Object.defineProperty(el, "scrollTop", { configurable: true, get: () => top, set: (value: number) => { top = value; } });
  // Older page: the content grows by one article (250 px) above the reader.
  rig.records({ "mail.message": [message(21)] });
  rig.store.windows.prependOlder(rig.ref, [21], false);
  flushSync();
  expect(top).toBe(550);
  // A new message while the reader is not at the newest end leaves the position alone.
  top = 100;
  rig.records({ "mail.message": [message(40)] });
  rig.store.windows.appendLive(rig.ref, 40);
  flushSync();
  expect(top).toBe(100);
  // At the newest end the thread follows the new message.
  top = height() - 500;
  rig.records({ "mail.message": [message(41)] });
  rig.store.windows.appendLive(rig.ref, 41);
  flushSync();
  expect(top).toBe(height());
});

test("a thread with no window loads the latest page once and shows loading, failure with Retry, and the empty state", async () => {
  const rig = new Rig();
  let reject!: (error: Error) => void;
  rig.pager.latestHandler = () => new Promise<void>((_, fail) => { reject = fail; });
  const thread = rig.thread();
  expect(rig.pager.latest).toEqual([rig.ref]);
  const status = thread.element.querySelector(".erp-mail-thread-status") as HTMLElement;
  expect(status.getAttribute("role")).toBe("status");
  expect(status.textContent).toBe("Loading messages");
  const failure = new Error("gateway timeout");
  reject(failure);
  await settle();
  expect(status.getAttribute("role")).toBe("alert");
  expect(status.textContent).toBe("Messages could not be loaded. gateway timeout Retry");
  expect(rig.errors).toEqual([failure]);
  rig.pager.latestHandler = () => {
    rig.store.windows.replace(rig.ref, [], false);
    return Promise.resolve();
  };
  (status.querySelector("button") as HTMLButtonElement).click();
  await settle();
  expect(rig.pager.latest).toHaveLength(2);
  expect(status.textContent).toBe("");
  expect(thread.element.querySelector(".erp-mail-empty")?.textContent).toBe("No messages yet");
});

test("a failed older page is shown, reported and retried from the same control", async () => {
  const rig = new Rig();
  rig.records(ann);
  rig.load([message(31)], true);
  const thread = rig.thread();
  const offline = new Error("offline");
  rig.pager.olderHandler = () => Promise.reject(offline);
  const button = thread.element.querySelector(".erp-mail-load-older") as HTMLButtonElement;
  button.click();
  await settle();
  const status = thread.element.querySelector(".erp-mail-thread-status") as HTMLElement;
  expect(status.getAttribute("role")).toBe("alert");
  expect(status.textContent).toBe("Older messages could not be loaded. offline");
  expect(button.textContent).toBe("Retry loading older messages");
  expect(button.hidden).toBe(false);
  expect(rig.errors).toEqual([offline]);
  rig.pager.olderHandler = () => {
    rig.store.windows.prependOlder(rig.ref, [11], false);
    return Promise.resolve();
  };
  button.click();
  await settle();
  expect(rig.pager.older.map((c) => c.before)).toEqual([31, 31]);
  expect(status.textContent).toBe("");
  expect(thread.displayedIds()).toEqual([31, 11]);
  expect(button.hidden).toBe(true);
});

test("consecutive messages of one author within five minutes are squashed, except notes, notifications and other types", () => {
  const rig = new Rig();
  rig.records(ann);
  rig.load([
    message(1, { date: "2026-11-08 09:00:00" }),
    message(2, { date: "2026-11-08 09:03:00" }),
    message(3, { date: "2026-11-08 09:09:00" }),
    message(4, { date: "2026-11-08 09:10:00", author_id: 3 }),
    message(5, { date: "2026-11-08 09:11:00", author_id: 3, is_internal: true }),
    message(6, { date: "2026-11-08 09:12:00", author_id: 3, message_type: "notification" }),
    message(7, { date: "2026-11-08 09:13:00", author_id: 3, message_type: "notification" }),
    message(8, { date: "2026-11-08 09:14:00", author_id: 3, message_type: "email" }),
  ]);
  const thread = rig.thread({ order: "asc" });
  const data = (id: number) => readMessage(rig.store, id, null)!;
  expect(thread.isSquashed(data(2), data(1))).toBe(true);
  expect(thread.isSquashed(data(3), data(2))).toBe(false);
  expect(thread.isSquashed(data(4), data(3))).toBe(false);
  expect(thread.isSquashed(data(5), data(4))).toBe(false);
  expect(thread.isSquashed(data(6), data(5))).toBe(false);
  expect(thread.isSquashed(data(7), data(6))).toBe(false);
  expect(thread.isSquashed(data(8), data(7))).toBe(false);
  expect(thread.isSquashed(data(1), undefined)).toBe(false);
  const squashed = (t: Thread) => [...t.element.querySelectorAll<HTMLElement>("article.is-squashed")].map((a) => Number(a.dataset["messageId"]));
  expect(squashed(thread)).toEqual([2]);
  // The head of a squashed message stays in the document for its accessible name, hidden visually.
  expect(article(thread, 2).querySelector("header")?.className).toContain("erp-mail-sr-only");
  // Newest first (the chatter default) does not squash; the option turns it on.
  expect(squashed(rig.thread())).toEqual([]);
  expect(squashed(rig.thread({ squash: true }))).toEqual([2]);
});

test("the first message of each day carries a Today, Yesterday or date label in the viewer's time zone, and the subject shows only when it adds something", () => {
  const rig = new Rig();
  rig.records(ann);
  rig.load([
    message(1, { date: "2026-11-06 23:30:00" }),
    message(2, { date: "2026-11-07 10:00:00", subject: "Re: Order 42" }),
    message(3, { date: "2026-11-08 09:00:00", subject: "Shipping delay" }),
    message(4, { date: "2026-11-08 09:30:00", subject: "order 42" }),
  ]);
  const days = (t: Thread) => [...t.element.querySelectorAll("article")].map((a) => a.querySelector(".erp-mail-day")?.textContent ?? null);
  const asc = rig.thread({ order: "asc", threadName: () => "Order 42" });
  expect(days(asc)).toEqual(["Nov 6, 2026", "Yesterday", "Today", null]);
  // The feed holds articles only (the feed pattern allows nothing else).
  expect([...asc.element.querySelector('[role="feed"]')!.children].every((c) => c.tagName === "ARTICLE")).toBe(true);
  const desc = rig.thread();
  expect(days(desc)).toEqual(["Today", null, "Yesterday", "Nov 6, 2026"]);
  // Only the message whose subject differs from the thread name shows it.
  const subjects = (t: Thread) => [...t.element.querySelectorAll("article")].map((a) => a.querySelector(".erp-mail-subject")?.textContent ?? null);
  expect(subjects(asc)).toEqual([null, null, "Shipping delay", null]);
  expect(subjects(desc)).toEqual(["order 42", "Shipping delay", "Re: Order 42", null]);
  // The day follows the time zone: 20:00 UTC on the 7th is already the 8th in Jakarta.
  rig.load([message(9, { date: "2026-11-07 20:00:00" })]);
  expect(days(rig.thread())).toEqual(["Yesterday"]);
  rig.services = { ...rig.services, timeZone: "Asia/Jakarta" };
  expect(days(rig.thread())).toEqual(["Today"]);
});
