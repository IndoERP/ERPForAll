/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
// Mention and other suggestions in the composer: triggers, the listbox, keyboard, insertion, providers.
import { flushSync } from "@erpforall/elang";
import { expect, test } from "vitest";
import { MAX_TERM_LENGTH, findTrigger, type Suggestion } from "../src/index";
import { Rig, key, message, settle, type } from "./support";

const people = { "res.partner": [{ id: 1, name: "Ann Lee" }, { id: 2, name: "Bob" }, { id: 3, name: "Anders" }, { id: 4, name: "Zoë Andrews" }] };
const options = (root: HTMLElement): string[] => [...root.querySelectorAll(".erp-mail-suggestion")].map((o) => o.textContent ?? "");
const list = (root: HTMLElement): HTMLElement => root.querySelector(".erp-mail-suggestions") as HTMLElement;
const polite = (root: HTMLElement): string => root.querySelector(".erp-mail-sr-only [role='status']")?.textContent ?? "";

function setup() {
  const rig = new Rig();
  rig.records(people);
  // Authors, oldest to newest: Bob, Ann Lee, Anders, Zoe.
  rig.load([message(1, { author_id: 2 }), message(2, { author_id: 1 }), message(3, { author_id: 3 }), message(4, { author_id: 4 })]);
  const composer = rig.composer();
  return { rig, composer };
}

test("a trigger is a delimiter at the start of a word before the caret, the longest delimiter wins and an email address is not one", () => {
  const delimiters = ["@", ":", "::", "/"];
  expect(findTrigger("hello @an", 9, delimiters)).toEqual({ delimiter: "@", term: "an", start: 6 });
  expect(findTrigger("@", 1, delimiters)).toEqual({ delimiter: "@", term: "", start: 0 });
  expect(findTrigger("see ::hel", 9, delimiters)).toEqual({ delimiter: "::", term: "hel", start: 4 });
  expect(findTrigger("see :smi", 8, delimiters)).toEqual({ delimiter: ":", term: "smi", start: 4 });
  expect(findTrigger("a@b.com", 7, delimiters)).toBeNull();
  expect(findTrigger("hello @an there", 15, delimiters)).toBeNull();
  expect(findTrigger("hello @an there", 9, delimiters)).toEqual({ delimiter: "@", term: "an", start: 6 });
  expect(findTrigger("line\n@x", 7, delimiters)).toEqual({ delimiter: "@", term: "x", start: 5 });
  expect(findTrigger(`@${"a".repeat(MAX_TERM_LENGTH)}`, MAX_TERM_LENGTH + 1, delimiters)?.term).toHaveLength(MAX_TERM_LENGTH);
  expect(findTrigger(`@${"a".repeat(MAX_TERM_LENGTH + 1)}`, MAX_TERM_LENGTH + 2, delimiters)).toBeNull();
  expect(findTrigger("plain", 5, delimiters)).toBeNull();
  expect(findTrigger("@x", 2, [])).toBeNull();
});

test("typing @ lists the thread's partners with the newest author first, and Enter inserts a structured mention", async () => {
  const { rig, composer } = setup();
  const root = composer.element;
  const f = composer.field;
  type(f, "hello @an");
  await settle();
  // "an" matches Ann Lee, Anders and Zoe Andrews (not Bob); names that start with it come before names that only contain it.
  expect(options(root)).toEqual(["Anders", "Ann Lee", "Zoë Andrews"]);
  expect(list(root).hidden).toBe(false);
  expect(list(root).getAttribute("role")).toBe("listbox");
  expect(list(root).getAttribute("aria-label")).toBe("Mention a partner");
  expect(polite(root)).toBe("3 suggestions");
  const items = [...root.querySelectorAll(".erp-mail-suggestion")];
  expect(items.map((o) => o.getAttribute("role"))).toEqual(["option", "option", "option"]);
  expect(items.map((o) => o.getAttribute("aria-selected"))).toEqual(["true", "false", "false"]);
  expect(f.getAttribute("aria-activedescendant")).toBe(items[0]?.id);
  expect(f.getAttribute("aria-controls")).toBe(list(root).id);
  expect(f.getAttribute("aria-autocomplete")).toBe("list");

  expect(key(f, "ArrowDown").defaultPrevented).toBe(true);
  expect(root.querySelectorAll(".erp-mail-suggestion")[1]?.getAttribute("aria-selected")).toBe("true");
  expect(f.getAttribute("aria-activedescendant")).toBe(root.querySelectorAll(".erp-mail-suggestion")[1]?.id);
  key(f, "ArrowUp");
  key(f, "ArrowUp");
  expect(root.querySelectorAll(".erp-mail-suggestion")[2]?.getAttribute("aria-selected")).toBe("true");
  key(f, "ArrowDown");
  key(f, "ArrowDown");
  expect(root.querySelectorAll(".erp-mail-suggestion")[1]?.getAttribute("aria-selected")).toBe("true");
  expect(key(f, "Enter").defaultPrevented).toBe(true);
  expect(f.value).toBe("hello @Ann Lee ");
  expect(f.selectionStart).toBe(15);
  expect(list(root).hidden).toBe(true);
  expect(f.hasAttribute("aria-activedescendant")).toBe(false);
  expect(rig.store.local("mail_ui.thread").get("mail.thread:7", "mentions")).toEqual([{ start: 6, end: 14, partnerId: 1, text: "@Ann Lee" }]);
  expect(polite(root)).toBe("Ann Lee inserted");
  expect(rig.transport.posts).toEqual([]);

  // Tab inserts too; Escape closes without inserting; a click on an option inserts it.
  type(f, "hello @an");
  await settle();
  key(f, "Tab");
  expect(f.value).toBe("hello @Anders ");
  type(f, "x @bo");
  await settle();
  expect(key(f, "Escape").defaultPrevented).toBe(true);
  expect(list(root).hidden).toBe(true);
  expect(f.value).toBe("x @bo");
  type(f, "y @bo");
  await settle();
  (root.querySelector(".erp-mail-suggestion") as HTMLElement).click();
  expect(f.value).toBe("y @Bob ");
  expect(rig.store.local("mail_ui.thread").get("mail.thread:7", "mentions")).toEqual([{ start: 2, end: 6, partnerId: 2, text: "@Bob" }]);
  // Enter with the list open selects; it never sends.
  expect(rig.transport.posts).toEqual([]);
});

test("the mention list is not offered inside a word, closes when the caret leaves it and says when nothing matches", async () => {
  const { composer } = setup();
  const root = composer.element;
  const f = composer.field;
  type(f, "mail a@an");
  await settle();
  expect(list(root).hidden).toBe(true);
  type(f, "hi @an");
  await settle();
  expect(list(root).hidden).toBe(false);
  type(f, "hi @an ");
  await settle();
  expect(list(root).hidden).toBe(true);
  type(f, "hi @qqq");
  await settle();
  expect(list(root).hidden).toBe(true);
  expect(polite(root)).toBe("No matching suggestions");
  // Moving the caret back into the trigger reopens the list.
  type(f, "hi @an there", 6);
  await settle();
  expect(list(root).hidden).toBe(false);
  f.setSelectionRange(11, 11);
  f.dispatchEvent(new Event("click", { bubbles: true }));
  await settle();
  expect(list(root).hidden).toBe(true);
  // Leaving the field closes the list.
  type(f, "hi @an");
  await settle();
  f.dispatchEvent(new Event("blur"));
  expect(list(root).hidden).toBe(true);
});

test("the host's partner search is merged without duplicates, a stale search is ignored and a failing search is shown and reported", async () => {
  const { rig } = setup();
  const pending: { term: string; resolve: (v: { id: number; name: string; detail?: string }[]) => void; signal: AbortSignal }[] = [];
  rig.services = {
    ...rig.services,
    searchPartners: (term, signal) => new Promise((resolve) => { pending.push({ term, resolve, signal }); }),
  };
  const fresh = rig.composer();
  const g = fresh.field;
  type(g, "@an");
  type(g, "@and");
  expect(pending.map((p) => p.term)).toEqual(["an", "and"]);
  expect(pending[0]?.signal.aborted).toBe(true);
  pending[1]!.resolve([{ id: 3, name: "Anders" }, { id: 9, name: "Andy Remote", detail: "andy@example.com" }]);
  await settle();
  pending[0]!.resolve([{ id: 10, name: "Stale Anna" }]);
  await settle();
  expect(options(fresh.element)).toEqual(["Anders", "Andy Remote andy@example.com", "Zoë Andrews"]);

  const failure = new Error("search is down");
  rig.services = { ...rig.services, searchPartners: () => Promise.reject(failure) };
  const broken = rig.composer();
  type(broken.field, "@an");
  await settle();
  expect(list(broken.element).hidden).toBe(true);
  expect(broken.element.querySelector(".erp-mail-composer-error")?.textContent).toBe("search is down");
  expect(rig.errors).toEqual([failure]);
});

test("an addon provider for another delimiter inserts plain text and records no mention", async () => {
  const { rig, composer } = setup();
  const f = composer.field;
  const emoji: Suggestion[] = [{ id: "smile", label: ":smile:", insert: "\u{1F604}" }, { id: "smirk", label: ":smirk:", detail: "smirk", insert: "\u{1F60F}" }];
  rig.registries.suggestionProviders.add(":", {
    delimiter: ":",
    label: () => "Emoji",
    search: (term) => emoji.filter((e) => e.label.includes(term)),
  });
  type(f, "ok :sm");
  await settle();
  expect(list(composer.element).getAttribute("aria-label")).toBe("Emoji");
  expect(options(composer.element)).toEqual([":smile:", ":smirk: smirk"]);
  key(f, "Enter");
  expect(f.value).toBe("ok \u{1F604} ");
  expect(rig.store.local("mail_ui.thread").get("mail.thread:7", "mentions")).toEqual([]);
  // A provider added after the composer exists is used at once; removing it stops the list.
  rig.registries.suggestionProviders.remove(":");
  type(f, "again :sm");
  await settle();
  expect(list(composer.element).hidden).toBe(true);
  flushSync();
});
