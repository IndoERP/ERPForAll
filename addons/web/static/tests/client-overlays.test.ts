/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
// One overlay scale and one dialog anatomy (UI package A): the stylesheet contract and the dialog shells it styles.
import { readFileSync } from "node:fs";
import { resolve } from "node:path";
import { afterEach, beforeEach, expect, test, vi } from "vitest";
import { DialogService } from "../src/client/dialog-service.js";
import { ErrorDialog } from "../src/client/error-dialog.js";
import { openCommandPalette } from "../src/client/command-palette.js";
import { renderDebugMenu } from "../src/client/debug-menu.js";
import { button, choose } from "../src/client/dom.js";

const css = readFileSync(resolve(process.cwd(), "..", "addons/web/static/src/client/client.css"), "utf8").replace(/\/\*[\s\S]*?\*\//g, "");
const norm = (text: string): string => text.replace(/\s+/g, " ").trim();
/** Declarations of every top-level rule whose selector list names `selector`, joined in source order. */
const rule = (selector: string): string => {
  const found: string[] = [];
  for (const chunk of css.split("}")) {
    const open = chunk.lastIndexOf("{");
    if (open >= 0 && chunk.slice(0, open).split("{").pop()!.split(",").map(norm).includes(selector)) found.push(norm(chunk.slice(open + 1)));
  }
  if (found.length === 0) throw new Error(`no rule for ${selector}`);
  return found.join(" ");
};

let dialogs: DialogService;
beforeEach(() => {
  Object.defineProperty(HTMLDialogElement.prototype, "showModal", { configurable: true, value(this: HTMLDialogElement) { this.open = true; } });
  Object.defineProperty(HTMLDialogElement.prototype, "close", { configurable: true, value(this: HTMLDialogElement) { this.open = false; this.dispatchEvent(new Event("close")); } });
  dialogs = new DialogService(() => {});
});
afterEach(() => { dialogs.dispose(); vi.restoreAllMocks(); document.body.replaceChildren(); });

test("overlay tokens exist in light and both dark blocks and every overlay rule uses them", () => {
  for (const token of ["--shadow-menu", "--shadow-dialog"]) expect(css.split(`${token}:`).length - 1).toBe(3);
  expect(css).toMatch(/--radius-overlay:\s*10px/);
  const anchored = rule(".nav-dropdown-menu");
  expect(anchored).toContain("box-shadow: var(--shadow-menu)");
  expect(anchored).toContain("border-radius: var(--radius-overlay)");
  for (const selector of [".nav-dropdown-menu", ".erp-mail-followers-popover", ".debug-menu-dropdown", ".command-palette-dialog"]) expect(rule(selector)).toContain("padding: var(--overlay-padding)");
  expect(rule(".command-palette-dialog")).toContain("var(--shadow-dialog)");
  expect(rule(".addon-info-dialog")).toContain("var(--radius-overlay)");
  expect(rule(".erp-mail-followers-popover")).not.toMatch(/rgb|rgba/);
  expect(rule(".debug-menu-dropdown")).not.toContain("var(--shadow-sm)");
});

test("dialog widths come from one set and never exceed the viewport minus 32px", () => {
  expect(rule(".message-dialog")).toContain("min(560px, calc(100vw - 32px))");
  expect(rule('.message-dialog[data-size="sm"]')).toContain("min(420px, calc(100vw - 32px))");
  expect(rule('.message-dialog[data-size="lg"]')).toContain("min(760px, calc(100vw - 32px))");
  expect(rule(".choice-dialog")).toContain("calc(100vw - 32px)");
  expect(rule(".choice-dialog > h2")).toContain("font-size: 18px");
  expect(rule(".login-error")).not.toContain("border-left-width");
});

test("a confirmation puts Cancel first and the primary last; Ctrl+Enter and the first focus use the primary", async () => {
  const confirm = vi.fn();
  const opener = button("Open", () => {}); document.body.append(opener); opener.focus();
  const handle = dialogs.confirm({ message: "Go on?", confirm });
  const controls = [...handle.dialog.querySelectorAll<HTMLButtonElement>(".message-dialog-footer button")];
  expect(controls.map(control => control.textContent)).toEqual(["Cancel", "OK"]);
  expect(controls[1]!.classList.contains("primary")).toBe(true);
  expect(handle.dialog.querySelector(".message-dialog-close")?.getAttribute("aria-label")).toBe("Close");
  expect(document.activeElement).toBe(controls[1]);
  handle.dialog.dispatchEvent(new KeyboardEvent("keydown", { key: "Enter", ctrlKey: true, bubbles: true }));
  await expect(handle.result).resolves.toBe("confirm");
  expect(confirm).toHaveBeenCalledTimes(1);
  expect(document.activeElement).toBe(opener);
});

test("a destructive confirmation uses the danger style in the primary position, not the accent fill", () => {
  const handle = dialogs.confirm({ message: "Delete it?", confirmLabel: "Delete", destructive: true });
  const last = handle.dialog.querySelector<HTMLButtonElement>(".message-dialog-footer button:last-child")!;
  expect(last.textContent).toBe("Delete");
  expect(last.classList.contains("danger")).toBe(true);
  expect(last.classList.contains("primary")).toBe(false);
  expect(rule(".message-dialog-footer button.danger")).toContain("var(--danger-line)");
  expect(() => dialogs.confirm({ message: "x", destructive: "yes" as unknown as boolean })).toThrowError("Dialog destructive must be a boolean.");
});

test("the error dialog shares the dialog anatomy: header with a close button, body, footer", () => {
  const controller = new ErrorDialog(dialogs);
  controller.show(new Error("Boom"));
  const dialog = document.querySelector<HTMLDialogElement>("dialog.client-error-dialog")!;
  expect(dialog.dataset["size"]).toBe("md");
  expect(dialog.querySelector(".message-dialog-header h2")?.textContent).toBe("Unable to complete the operation");
  const close = dialog.querySelector<HTMLButtonElement>(".message-dialog-header .message-dialog-close")!;
  expect(close.getAttribute("aria-label")).toBe("Close");
  expect(dialog.querySelector(".message-dialog-body")?.textContent).toContain("Boom");
  expect(dialog.querySelector(".message-dialog-footer .primary")?.textContent).toBe("Close");
  close.click();
  expect(document.querySelector("dialog")).toBeNull();
});

test("the command palette hints before typing and uses the ellipsis character", async () => {
  const palette = openCommandPalette({ providers: [{ provide: () => [] }] });
  const input = palette.node.querySelector<HTMLInputElement>(".command-palette-input")!;
  expect(input.placeholder).toBe("Search commands…");
  await vi.waitFor(() => expect(palette.node.querySelector(".command-palette-empty")?.textContent).toBe("Type to search menus and commands."));
  input.value = "zz"; input.dispatchEvent(new Event("input"));
  await vi.waitFor(() => expect(palette.node.querySelector(".command-palette-empty")?.textContent).toBe("No commands found"));
  palette.close();
});

test("a debug menu without entries says so instead of opening an empty sliver", async () => {
  const container = document.createElement("div");
  await renderDebugMenu(container, [], { get: () => ({ label: "", sequence: 0 }), has: () => false }, {});
  expect(container.querySelector(".debug-menu-empty")?.textContent).toBe("No developer tools for this screen.");
});

test("a choice lists its first (primary) option last, in the danger style when destructive", async () => {
  const pending = choose("Delete record?", "This cannot be undone.", ["Delete", "Cancel"], undefined, dialogs);
  const labels = (): HTMLButtonElement[] => [...document.querySelectorAll<HTMLButtonElement>(".message-dialog-footer button")];
  expect(labels().map(control => control.textContent)).toEqual(["Cancel", "Delete"]);
  expect(labels()[1]!.classList.contains("danger")).toBe(true);
  expect(labels()[1]!.classList.contains("primary")).toBe(false);
  expect(document.activeElement).toBe(labels()[1]);
  labels()[1]!.click();
  await expect(pending).resolves.toBe("Delete");
  const save = choose("Unsaved changes", "Save first?", ["Save", "Discard", "Cancel"], undefined, dialogs);
  expect(labels().map(control => control.textContent)).toEqual(["Cancel", "Discard", "Save"]);
  expect(labels()[2]!.classList.contains("primary")).toBe(true);
  labels()[2]!.click();
  await expect(save).resolves.toBe("Save");
});
