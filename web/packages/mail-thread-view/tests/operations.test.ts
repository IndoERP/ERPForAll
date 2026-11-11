/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
// Badges on a message, and the contract of the answers to message operations.
import { expect, test } from "vitest";
import { Rig, article, failsWith, message, settle } from "./support";
import { checkPostAnswer, performOperation } from "../src/index";

const ann = { "res.partner": [{ id: 1, name: "Ann" }, { id: 2, name: "Bob" }] };

test("a note, a pinned and an edited message say so, and focusing a message that is not in the window is refused", () => {
  const rig = new Rig();
  rig.records(ann);
  rig.load([message(1, { is_internal: true, pinned_at: "2026-11-08 10:00:00", edited_at: "2026-11-08 09:30:00" }), message(2)]);
  const thread = rig.thread({ order: "asc" });
  expect([...article(thread, 1).querySelectorAll(".erp-mail-badge")].map((b) => b.textContent)).toEqual(["Note", "Pinned"]);
  expect(article(thread, 1).querySelector(".erp-mail-edited")?.textContent).toBe("edited");
  // ICU versions differ in the space before AM, so spaces are compared as one kind.
  expect(article(thread, 1).querySelector(".erp-mail-edited")?.getAttribute("title")?.replace(/\s/g, " ")).toBe("Nov 8, 2026, 9:30 AM");
  expect(article(thread, 1).classList.contains("is-note")).toBe(true);
  expect(article(thread, 2).querySelector(".erp-mail-badge")).toBeNull();
  expect(article(thread, 2).querySelector(".erp-mail-edited")).toBeNull();
  expect(article(thread, 1).querySelector("time")?.getAttribute("datetime")).toBe("2026-11-08T09:00:00.000Z");
  failsWith(() => thread.focusMessage(99), "bad_option", "message 99 is not in the window of mail.thread#7");
});

test("an operation or post answer that breaks its contract is a transport error naming the operation", async () => {
  const rig = new Rig();
  rig.load([message(1)]);
  const view = rig.thread().context;
  const run = (answer: unknown) => {
    rig.transport.setBookmark = () => Promise.resolve(answer as never);
    return performOperation(view, "setBookmark", 1, true);
  };
  await expect(run(null)).rejects.toMatchObject({ code: "transport", message: "setBookmark answered with something that is not an object" });
  await expect(run({ store: 5 })).rejects.toMatchObject({ code: "transport", message: "setBookmark answer.store must be the store payload text" });
  await expect(run({ store: rig.payload({ "mail.message": [message(1)] }, "other") })).rejects.toMatchObject({
    code: "transport",
    message: "setBookmark answered with schema other, which this tab does not hold",
  });
  await expect(run({ store: JSON.stringify({ wire: 9 }) })).rejects.toMatchObject({ code: "transport" });
  await expect(run({})).resolves.toBeUndefined();
  await expect(run({ store: rig.payload({ "mail.message": [message(1, { starred: true })] }) })).resolves.toBeUndefined();
  failsWith(() => checkPostAnswer(null), "transport", "thread_post answered with something that is not an object");
  await settle();
});
