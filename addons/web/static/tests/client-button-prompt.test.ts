/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
import { afterEach, beforeEach, describe, expect, test, vi } from "vitest";
import { askButtonPrompt, isoDate, promptValue, type PromptAnswer } from "../src/client/button-prompt.js";
import { runObjectButton, type ButtonEnvironment } from "../src/client/buttons.js";
import { DialogService } from "../src/client/dialog-service.js";
import { describeViews, type ButtonNode, type ButtonPrompt } from "../src/client/metadata.js";

const modifiers = { readonly: null, required: null, invisible: null };
const details = (prompt: unknown) => ({ size: "normal", circle: false, classes: [], debounce_ms: 0, block_ui: false, prompt });
const reason: ButtonPrompt = { kind: "text", label: "Reason", required: true, max_length: 10 };
const runDate: ButtonPrompt = { kind: "date", label: "Run for date", required: false };
const outcome: ButtonPrompt = { kind: "selection", label: "Outcome", required: true, choices: [{ value: "lost", label: "Lost" }, { value: "won", label: "Won" }] };

function describeButton(prompt: unknown): ButtonNode {
  const wire = { wire_major: 1, registry_version: "r1", description_version: "d1", model: "test.book", notices: [],
    fields: { "test.book": { name: { type: "char", label: "Name", required: false, readonly: false, selection: null, relation: null, relation_field: null } } },
    views: [{ identity: { kind: "stored", id: 1 }, type: "form", schema_major: 1, renderer: "web.form", root: 0, nodes: [
      { kind: "form", record_mode: "record", children: [1, 3] }, { kind: "header", children: [2] },
      { kind: "button", name: "action_refuse", button_type: "object", label: "Refuse", variant: "secondary", confirm: null, modifiers, details: details(prompt) },
      { kind: "field", force_save: false, field: { model: "test.book", name: "name" }, widget: "char", options: {}, modifiers, label: "Name" },
    ] }] };
  const view = describeViews(wire, "test.book", "r1").views.get("form")!;
  return (view.nodes[0] as { children: { children: ButtonNode[] }[] }).children[0]!.children[0]!;
}

function environment(answer: PromptAnswer | undefined, extra: Partial<ButtonEnvironment> = {}) {
  const call = vi.fn(async () => true), prompt = vi.fn(async () => answer);
  const env: ButtonEnvironment = { model: "test.book", context: { lang: "en_US" }, id: () => 4, dirty: () => false, save: vi.fn(async () => {}), call, reload: vi.fn(async () => {}), confirm: vi.fn(async () => true), prompt, ...extra };
  return { env, call, prompt };
}

describe("button prompt metadata", () => {
  test("text, date and selection prompts are admitted", () => {
    expect(describeButton(reason).details?.prompt).toEqual(reason);
    expect(describeButton(runDate).details?.prompt).toEqual(runDate);
    expect(describeButton(outcome).details?.prompt).toEqual(outcome);
  });

  test("malformed prompts are refused", () => {
    for (const prompt of [
      { ...reason, kind: "number" },
      { ...reason, label: " " },
      { ...runDate, max_length: 5 },
      { ...reason, max_length: 0 },
      { ...reason, choices: outcome.choices },
      { kind: "selection", label: "Outcome", required: true },
      { ...outcome, choices: [{ value: "won", label: "Won" }, { value: "won", label: "Again" }] },
      { ...outcome, choices: [{ value: "Won!", label: "Won" }] },
      { ...reason, extra: true },
    ]) expect(() => describeButton(prompt), JSON.stringify(prompt)).toThrow();
  });
});

describe("button prompt answers", () => {
  test("a required answer cannot be empty; an optional empty one is null", () => {
    expect(() => promptValue(reason, "   ")).toThrow("Reason is required.");
    expect(promptValue(runDate, "")).toBeNull();
    expect(promptValue(reason, "  late  ")).toBe("late");
  });

  test("text over its length, a non-date and a value outside the choices are refused with the label", () => {
    expect(() => promptValue(reason, "x".repeat(11))).toThrow("Reason can be up to 10 characters.");
    expect(() => promptValue(runDate, "2026-02-30")).toThrow("Run for date must be a date.");
    expect(() => promptValue(outcome, "draw")).toThrow("Outcome must be one of the listed choices.");
    expect(promptValue(runDate, "2026-02-28")).toBe("2026-02-28");
    expect(promptValue(outcome, "won")).toBe("won");
    expect(isoDate("2024-02-29")).toBe(true);
    expect(isoDate("2026-2-1")).toBe(false);
  });
});

describe("prompting object buttons", () => {
  test("the answer reaches the method as prompt_value", async () => {
    const { env, call, prompt } = environment({ value: "late delivery" });
    await runObjectButton(describeButton(reason), env);
    expect(prompt).toHaveBeenCalledExactlyOnceWith(reason, "Refuse");
    expect(call).toHaveBeenCalledExactlyOnceWith("test.book", "action_refuse", { ids: [4], context: { lang: "en_US" }, prompt_value: "late delivery" });
  });

  test("an empty optional answer is sent as null", async () => {
    const { env, call } = environment({ value: null });
    await runObjectButton(describeButton(runDate), env);
    expect(call.mock.calls[0]![2]).toMatchObject({ prompt_value: null });
  });

  test("cancelling the prompt neither saves nor calls", async () => {
    const save = vi.fn(async () => {});
    const { env, call } = environment(undefined, { dirty: () => true, save });
    await runObjectButton(describeButton(reason), env);
    expect(save).not.toHaveBeenCalled();
    expect(call).not.toHaveBeenCalled();
  });

  test("an owner without prompts refuses the button instead of calling without an answer", async () => {
    const { env, call } = environment({ value: "x" });
    delete env.prompt;
    await expect(runObjectButton(describeButton(reason), env)).rejects.toThrow("This button owner does not supply prompts.");
    expect(call).not.toHaveBeenCalled();
  });
});

describe("prompt dialog", () => {
  let dialogs: DialogService;
  beforeEach(() => {
    Object.defineProperty(HTMLDialogElement.prototype, "showModal", { configurable: true, value(this: HTMLDialogElement) { this.open = true; } });
    Object.defineProperty(HTMLDialogElement.prototype, "close", { configurable: true, value(this: HTMLDialogElement) { this.open = false; } });
    dialogs = new DialogService(error => { throw error; });
  });
  afterEach(() => { dialogs.dispose(); document.body.replaceChildren(); });
  const footerButton = (caption: string) => [...document.querySelectorAll<HTMLButtonElement>("dialog .message-dialog-footer button")].find(item => item.textContent === caption)!;
  const control = () => document.querySelector<HTMLTextAreaElement & HTMLInputElement & HTMLSelectElement>("dialog .button-prompt-input")!;

  test("a required empty answer keeps the dialog open with the reason; a valid one answers", async () => {
    const answer = askButtonPrompt({ title: "Refuse", prompt: reason, service: dialogs });
    expect(document.querySelector("dialog label")!.textContent).toBe("Reason *");
    expect(control().maxLength).toBe(10);
    footerButton("Refuse").click();
    expect(document.querySelector("dialog .field-error")!.textContent).toBe("Reason is required.");
    expect(document.querySelector("dialog")).not.toBeNull();
    control().value = "late"; control().dispatchEvent(new Event("input"));
    expect(document.querySelector<HTMLElement>("dialog .field-error")!.hidden).toBe(true);
    footerButton("Refuse").click();
    await expect(answer).resolves.toEqual({ value: "late" });
    expect(document.querySelector("dialog")).toBeNull();
  });

  test("Cancel answers nothing", async () => {
    const answer = askButtonPrompt({ title: "Run", prompt: runDate, service: dialogs });
    expect(control().type).toBe("date");
    footerButton("Cancel").click();
    await expect(answer).resolves.toBeUndefined();
  });

  test("a selection prompt lists its choices and sends the chosen value", async () => {
    const answer = askButtonPrompt({ title: "Close", prompt: outcome, service: dialogs });
    expect([...control().options].map(option => [option.value, option.text])).toEqual([["lost", "Lost"], ["won", "Won"]]);
    control().value = "won";
    footerButton("Close").click();
    await expect(answer).resolves.toEqual({ value: "won" });
  });

  test("an aborted owner closes the dialog without an answer", async () => {
    const owner = new AbortController();
    const answer = askButtonPrompt({ title: "Refuse", prompt: reason, service: dialogs, signal: owner.signal });
    owner.abort();
    await expect(answer).resolves.toBeUndefined();
    expect(document.querySelector("dialog")).toBeNull();
  });
});
