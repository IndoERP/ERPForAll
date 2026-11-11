/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
import { afterEach, expect, test, vi } from "vitest";
import * as client from "../src/client/public.js";
import { MessageServices } from "../src/client/messages.js";

interface Control { node: HTMLElement; dispose(): void }
function factory(name: string): (options: Record<string, unknown>) => Control {
  const candidate = (client as Record<string, unknown>)[name];
  expect(typeof candidate, `${name} must be available to trusted addon callers`).toBe("function");
  return candidate as (options: Record<string, unknown>) => Control;
}
const disposals: (() => void)[] = [];
afterEach(() => { for (const dispose of disposals.splice(0).reverse()) dispose(); document.body.replaceChildren(); });

test("palette exposes named selected indexed choices and can omit no-colour", () => {
  const select = vi.fn();
  const create = factory("createColorPalette");
  const palette = create({ value: 2, select }); disposals.push(palette.dispose); document.body.append(palette.node);
  const buttons = [...palette.node.querySelectorAll<HTMLButtonElement>("button")];
  expect(buttons.length).toBe(12); expect(buttons.every(button => Boolean(button.getAttribute("aria-label")))).toBe(true);
  expect(buttons[2]!.getAttribute("aria-pressed")).toBe("true");
  buttons[4]!.click(); expect(select).toHaveBeenCalledWith(4);
  const opaque = create({ value: 2, excludeNone: true, select }); disposals.push(opaque.dispose);
  expect(opaque.node.querySelectorAll("button")).toHaveLength(11);
  expect(opaque.node.querySelector('[data-color-index="0"]')).toBeNull();
});

test("supplied tags reserve overflow position and expand in order without mutating values", () => {
  const tags = Object.freeze(["One", "Two", "Three", "Four"].map((label, id) => Object.freeze({ id: String(id), label })));
  const control = factory("createTagsList")({ tags, limit: 3 }); disposals.push(control.dispose); document.body.append(control.node);
  expect([...control.node.querySelectorAll(".choice-tag")].map(node => node.textContent)).toEqual(["One", "Two"]);
  const more = control.node.querySelector<HTMLButtonElement>("button")!;
  expect(more.textContent).toBe("+2"); more.focus(); more.click();
  expect([...control.node.querySelectorAll(".choice-tag")].map(node => node.textContent)).toEqual(["One", "Two", "Three", "Four"]);
  expect(tags.map(tag => tag.label)).toEqual(["One", "Two", "Three", "Four"]);
  expect(control.node.querySelector("button")).toBeNull();
});

test("emoji picker searches safely, navigates, retains shift/refused selections and removes", () => {
  const host = document.createElement("div"); document.body.append(host);
  const report = vi.fn(), messages = new MessageServices(host, report); disposals.push(() => messages.dispose());
  const select = vi.fn< (value: string | false) => boolean | void >();
  const picker = factory("createEmojiPicker")({ environment: { messages, report }, select, allowRemove: true });
  disposals.push(picker.dispose); host.append(picker.node);
  picker.node.querySelector<HTMLButtonElement>("button")!.click();
  const search = host.querySelector<HTMLInputElement>('input[type="search"]')!;
  const work = [...host.querySelectorAll<HTMLButtonElement>(".emoji-categories button")].find(control => control.textContent === "Work")!;
  search.value = "draft"; work.click(); expect(search.value).toBe(""); expect([...host.querySelectorAll<HTMLButtonElement>(".emoji-categories button")].find(control => control.textContent === "Work")!.getAttribute("aria-pressed")).toBe("true");
  [...host.querySelectorAll<HTMLButtonElement>(".emoji-categories button")].find(control => control.textContent === "All")!.click();
  search.value = "<script>"; search.dispatchEvent(new Event("input"));
  expect(host.textContent).toContain("No emoji matches"); expect(host.querySelector("script")).toBeNull();
  search.value = "smile"; search.dispatchEvent(new Event("input"));
  const emoji = host.querySelector<HTMLButtonElement>("[data-emoji]")!;
  emoji.dispatchEvent(new MouseEvent("click", { bubbles: true, shiftKey: true }));
  expect(select).toHaveBeenCalledWith(emoji.dataset["emoji"]); expect(host.querySelector(".emoji-picker-panel")).not.toBeNull();
  select.mockReturnValueOnce(false); emoji.click(); expect(host.querySelector(".emoji-picker-panel")).not.toBeNull();
  search.dispatchEvent(new KeyboardEvent("keydown", { key: "ArrowDown", bubbles: true }));
  search.dispatchEvent(new KeyboardEvent("keydown", { key: "Enter", bubbles: true }));
  expect(host.querySelector(".emoji-picker-panel")).toBeNull();
  picker.node.querySelector<HTMLButtonElement>("button")!.click();
  host.querySelector<HTMLButtonElement>(".emoji-remove")!.click(); expect(select).toHaveBeenLastCalledWith(false);
  picker.node.querySelector<HTMLButtonElement>("button")!.click();
  host.querySelector<HTMLInputElement>('input[type="search"]')!.dispatchEvent(new KeyboardEvent("keydown", { key: "Escape", bubbles: true }));
  expect(host.querySelector(".emoji-picker-panel")).toBeNull();
  expect(report).not.toHaveBeenCalled();
});

test("choice declarations reject malformed limits and indices with kind and message", () => {
  for (const [name, options, message] of [
    ["createColorPalette", { value: -1, select: vi.fn() }, "Colour index must be an integer from 0 to 11."],
    ["createTagsList", { tags: [], limit: 0 }, "Tag limit must be an integer from 1 to 1000."]
  ] as const) {
    expect(() => factory(name)(options)).toThrowError(message);
    try { factory(name)(options); } catch (error) { expect(error).toMatchObject({ kind: "validation", message }); }
  }
});

/// (partial: async data failure, owner disposal and malformed catalogue)
test("emoji loading errors stay visible and late loads cannot revive a disposed picker", async () => {
  const host = document.createElement("div"); document.body.append(host);
  const report = vi.fn(), messages = new MessageServices(host, report); disposals.push(() => messages.dispose());
  const create = factory("createEmojiPicker");
  const failure = new Error("Emoji source unavailable");
  const failed = create({ environment: { messages, report }, load: () => Promise.reject(failure), select: vi.fn() });
  disposals.push(failed.dispose); host.append(failed.node); failed.node.querySelector<HTMLButtonElement>("button")!.click();
  await vi.waitFor(() => expect(host.textContent).toContain("Emoji could not be loaded: Emoji source unavailable"));
  expect(report).toHaveBeenCalledWith(failure); failed.dispose(); failed.node.remove(); report.mockClear();
  let resolve!: (value: unknown) => void;
  const selected = vi.fn();
  const late = create({ environment: { messages, report }, load: () => new Promise(yes => { resolve = yes; }), select: selected });
  host.append(late.node); late.node.querySelector<HTMLButtonElement>("button")!.click(); await Promise.resolve();
  late.dispose(); resolve([{ value: "😀", label: "Smile", category: "Faces", keywords: [] }]); await Promise.resolve(); await Promise.resolve();
  expect(host.querySelector(".emoji-picker-panel")).toBeNull(); expect(selected).not.toHaveBeenCalled(); expect(report).not.toHaveBeenCalled();
  expect(() => create({ environment: { messages, report }, data: null, select: selected })).toThrowError("Emoji data must be an array of at most 1024 entries.");
});

test("oversized emoji search cannot select an invisible prior result and recovers after correction", () => {
  const host = document.createElement("div"); document.body.append(host);
  const report = vi.fn(), select = vi.fn(), messages = new MessageServices(host, report); disposals.push(() => messages.dispose());
  const picker = factory("createEmojiPicker")({ environment: { messages, report }, select }); disposals.push(picker.dispose); host.append(picker.node);
  picker.node.querySelector<HTMLButtonElement>("button")!.click();
  const search = host.querySelector<HTMLInputElement>('input[type="search"]')!;
  search.value = "x".repeat(4097); search.dispatchEvent(new Event("input"));
  search.dispatchEvent(new KeyboardEvent("keydown", { key: "Enter", bubbles: true }));
  expect(select).not.toHaveBeenCalled(); expect(host.querySelector("[data-emoji]")).toBeNull();
  expect(report).toHaveBeenCalledWith(expect.objectContaining({ kind: "validation", message: "Emoji search must be text of at most 4096 bytes." }));
  search.value = "smile"; search.dispatchEvent(new Event("input"));
  search.dispatchEvent(new KeyboardEvent("keydown", { key: "ArrowDown", bubbles: true }));
  search.dispatchEvent(new KeyboardEvent("keydown", { key: "Enter", bubbles: true }));
  expect(select).toHaveBeenCalledTimes(1);
});
