/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
// Message windows: contiguity, caps and live messages (spec messaging-client-store.md).
import { expect, test } from "vitest";
import { Harness, failsWith } from "./support";

const thread = { model: "mail.thread", id: 7 };
const prefix = "ThreadWindows(mail.thread#7)";

test("a window is one contiguous run and refuses calls that would break it", () => {
  const w = new Harness().store.windows;
  expect(w.get(thread)).toBeUndefined();
  failsWith(
    () => w.prependOlder(thread, [1], false),
    "window",
    `${prefix}.prependOlder: the thread has no window (never loaded or evicted): load the latest page first`,
  );

  w.replace(thread, [31, 32, 33], true);
  expect(w.get(thread)).toEqual({ ids: [31, 32, 33], hasOlder: true, hasNewer: false });
  failsWith(() => w.appendNewer(thread, [34], false), "window", `${prefix}.appendNewer: the window has no newer messages to load`);
  failsWith(
    () => w.prependOlder(thread, [29, 30, 31], false),
    "window",
    `${prefix}.prependOlder: the page must be non-empty and end below the window's oldest id 31`,
  );
  failsWith(
    () => w.prependOlder(thread, [], false),
    "window",
    `${prefix}.prependOlder: the page must be non-empty and end below the window's oldest id 31`,
  );
  failsWith(() => w.replace(thread, [3, 2], false), "window", `${prefix}.replace: ids must be strictly ascending, ids[1] = 2 follows 3`);
  failsWith(() => w.replace(thread, [0], false), "window", `${prefix}.replace: ids[0] must be a positive safe integer, got 0`);
  expect(w.get(thread)).toEqual({ ids: [31, 32, 33], hasOlder: true, hasNewer: false });

  w.prependOlder(thread, [21, 22], true);
  w.prependOlder(thread, [11, 12], false);
  expect(w.get(thread)).toEqual({ ids: [11, 12, 21, 22, 31, 32, 33], hasOlder: false, hasNewer: false });
  failsWith(() => w.prependOlder(thread, [1], false), "window", `${prefix}.prependOlder: the window has no older messages to load`);

  // Installing the latest page again discards the earlier run; an empty page is a thread without messages.
  w.replace(thread, [], false);
  expect(w.get(thread)).toEqual({ ids: [], hasOlder: false, hasNewer: false });
});

test("the window cap trims the far side and records it", () => {
  const w = new Harness({ pageSize: 2, maxWindowPages: 2 }).store.windows;
  expect(w.maxMessages).toBe(4);

  // The latest page is longer than the cap: the newest messages stay, older ones exist.
  w.replace(thread, [1, 2, 3, 4, 5, 6], false);
  expect(w.get(thread)).toEqual({ ids: [3, 4, 5, 6], hasOlder: true, hasNewer: false });

  // Paging older past the cap trims the newest side.
  w.prependOlder(thread, [1, 2], false);
  expect(w.get(thread)).toEqual({ ids: [1, 2, 3, 4], hasOlder: false, hasNewer: true });

  // Paging newer past the cap trims the oldest side.
  w.appendNewer(thread, [5, 6], true);
  expect(w.get(thread)).toEqual({ ids: [3, 4, 5, 6], hasOlder: true, hasNewer: true });
  failsWith(
    () => w.appendNewer(thread, [6, 7], true),
    "window",
    `${prefix}.appendNewer: the page must be non-empty and start above the window's newest id 6`,
  );
});

test("live messages join a window only where it stays contiguous", () => {
  const h = new Harness();
  const w = h.store.windows;
  w.replace(thread, [10, 12, 13], true);
  expect(w.appendLive(thread, 14)).toBe("appended");
  expect(w.appendLive(thread, 14)).toBe("duplicate");
  expect(w.appendLive(thread, 11)).toBe("inserted");
  expect(w.appendLive(thread, 8)).toBe("older");
  expect(w.get(thread)?.ids).toEqual([10, 11, 12, 13, 14]);

  w.replace(thread, [10, 11], false);
  expect(w.appendLive(thread, 5)).toBe("prepended");
  expect(w.get(thread)?.ids).toEqual([5, 10, 11]);

  const small = new Harness({ pageSize: 2, maxWindowPages: 2 }).store.windows;
  small.replace(thread, [3, 4, 5, 6], true);
  small.prependOlder(thread, [1, 2], false);
  expect(small.get(thread)).toEqual({ ids: [1, 2, 3, 4], hasOlder: false, hasNewer: true });
  expect(small.appendLive(thread, 9)).toBe("gap");
  expect(small.get(thread)).toEqual({ ids: [1, 2, 3, 4], hasOlder: false, hasNewer: true });
  small.replace(thread, [1, 2, 3, 4], false);
  expect(small.appendLive(thread, 5)).toBe("appended");
  expect(small.get(thread)).toEqual({ ids: [2, 3, 4, 5], hasOlder: true, hasNewer: false });

  const other = { model: "mail.thread", id: 8 };
  expect(w.appendLive(other, 1)).toBe("no_window");
  expect(w.get(other)).toBeUndefined();
  failsWith(() => w.appendLive(thread, 0), "window", `${prefix}.appendLive: message id must be a positive safe integer, got 0`);
});
