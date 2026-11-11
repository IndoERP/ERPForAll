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
      },
    },
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
