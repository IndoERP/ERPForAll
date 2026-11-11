/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
import { afterEach, expect, test, vi } from "vitest";
import { createAutocomplete, type AutocompleteOption, type AutocompleteHandle } from "../src/client/autocomplete.js";
import { PopoverService } from "../src/client/popover.js";
import { DialogService } from "../src/client/dialog-service.js";
import type { EditorEnvironment } from "../src/client/editor-types.js";
import { auditComponent } from "../../../../web/testing/axe.js";

const stops: (() => void)[] = [];
afterEach(() => { for (const stop of stops.splice(0).reverse()) stop(); vi.useRealTimers(); vi.restoreAllMocks(); document.body.replaceChildren(); });
function fixture() {
  const owner = document.createElement("div"), input = document.createElement("input"); input.setAttribute("aria-label", "Search choices"); owner.append(input); document.body.append(owner);
  const report = vi.fn(), popovers = new PopoverService(document.body), dialogs = new DialogService(report);
  const environment: EditorEnvironment = { messages: { popovers, dialogs }, report };
  stops.push(() => popovers.dispose(), () => dialogs.dispose());
  const build = (options: Parameters<typeof createAutocomplete>[0]): AutocompleteHandle => { const handle = createAutocomplete(options); stops.push(handle.dispose); return handle; };
  return { owner, input, report, environment, build };
}
const key = (input: HTMLElement, name: string) => input.dispatchEvent(new KeyboardEvent("keydown", { key: name, bubbles: true, cancelable: true }));
const option = (label: string, select: () => void | Promise<void> = vi.fn()): AutocompleteOption => ({ kind: "option", label, select });

/// (partial: immediate sources, nonselectable options, pointer and keyboard selection metadata)
test("immediate choices skip headings and status options and expose a stable active descendant", async () => {
  const f = fixture(), select = vi.fn();
  const handle = f.build({ input: f.input, owner: f.owner, environment: f.environment, sources: [{ options: [{ kind: "heading", label: "Group" }, option("First"), { kind: "status", label: "More" }, option("Second", select)] }] });
  await handle.search("");
  expect(f.input.getAttribute("role")).toBe("combobox"); expect(f.input.getAttribute("aria-expanded")).toBe("true");
  expect(document.getElementById(f.input.getAttribute("aria-controls")!)?.getAttribute("role")).toBe("listbox");
  key(f.input, "ArrowDown"); const first = f.input.getAttribute("aria-activedescendant");
  expect(first).not.toBeNull(); expect(document.getElementById(first!)?.textContent).toBe("First");
  key(f.input, "ArrowDown"); expect(document.getElementById(f.input.getAttribute("aria-activedescendant")!)?.textContent).toBe("Second");
  key(f.input, "Enter"); expect(select).toHaveBeenCalledOnce(); expect(handle.isOpen()).toBe(false); expect(f.input.hasAttribute("aria-activedescendant")).toBe(false);
});

test("input debounces, selecting an existing choice discards pending input, and IME waits for composition end", async () => {
  vi.useFakeTimers(); const f = fixture(), selected = vi.fn(), source = vi.fn(() => [option("Existing", selected)]);
  const handle = f.build({ input: f.input, owner: f.owner, environment: f.environment, sources: [{ options: source }], debounceMs: 40 }); await handle.search("");
  f.input.value = "next"; f.input.dispatchEvent(new Event("input")); await vi.advanceTimersByTimeAsync(39); expect(source).toHaveBeenCalledTimes(1);
  handle.dropdown.querySelector<HTMLElement>('[role="option"]')!.click(); await vi.advanceTimersByTimeAsync(100); expect(source).toHaveBeenCalledTimes(1); expect(selected).toHaveBeenCalledOnce();
  f.input.dispatchEvent(new CompositionEvent("compositionstart")); f.input.dispatchEvent(new Event("input")); await vi.advanceTimersByTimeAsync(100); expect(source).toHaveBeenCalledTimes(1);
  f.input.dispatchEvent(new CompositionEvent("compositionend")); await vi.advanceTimersByTimeAsync(40); expect(source).toHaveBeenCalledTimes(2);
});

test("asynchronous sources show pending state and reject stale responses after query replacement or Escape", async () => {
  const f = fixture(), resolvers: ((value: readonly AutocompleteOption[]) => void)[] = [], signals: AbortSignal[] = [];
  const handle = f.build({ input: f.input, owner: f.owner, environment: f.environment, sources: [{ options: (_query, signal) => { signals.push(signal); return new Promise(resolve => resolvers.push(resolve)); } }] });
  const first = handle.search("old"); expect(handle.dropdown.textContent).toContain("Loading"); expect(f.input.getAttribute("aria-busy")).toBe("true");
  const second = handle.search("new"); expect(signals[0]!.aborted).toBe(true); resolvers[1]!([option("New")]); await second;
  resolvers[0]!([option("Old")]); await first; expect(handle.dropdown.textContent).toBe("New");
  const third = handle.search("cancel"); key(f.input, "Escape"); resolvers[2]!([option("Late")]); await third; expect(handle.isOpen()).toBe(false); expect(f.input.getAttribute("aria-expanded")).toBe("false");
});

test("keyboard selection waiting for asynchronous options is discarded after disposal", async () => {
  const f = fixture(), select = vi.fn(); let resolve!: (value: readonly AutocompleteOption[]) => void;
  const handle = f.build({ input: f.input, owner: f.owner, environment: f.environment, autoSelect: true, sources: [{ options: () => new Promise(yes => { resolve = yes; }) }] });
  const pending = handle.search("query"); key(f.input, "Enter"); handle.dispose(); resolve([option("Late", select)]); await pending; await Promise.resolve();
  expect(select).not.toHaveBeenCalled(); expect(handle.isOpen()).toBe(false); expect(f.input.hasAttribute("aria-controls")).toBe(false);
});

test("Tab selects without trapping focus, cancellation restores represented text only under autoSelect policy", async () => {
  const f = fixture(), select = vi.fn(), cancel = vi.fn();
  const handle = f.build({ input: f.input, owner: f.owner, environment: f.environment, sources: [{ options: [option("Choice", select)] }], autoSelect: true, representedValue: () => "Saved", onCancel: cancel });
  await handle.search(""); const event = new KeyboardEvent("keydown", { key: "Tab", bubbles: true, cancelable: true }); f.input.dispatchEvent(event);
  expect(select).toHaveBeenCalledOnce(); expect(event.defaultPrevented).toBe(false);
  f.input.value = "draft"; await handle.search("draft"); key(f.input, "Escape"); expect(f.input.value).toBe("Saved"); expect(cancel).toHaveBeenCalledOnce();
  handle.dispose(); const second = f.build({ input: f.input, owner: f.owner, environment: f.environment, sources: [{ options: [] }], autoSelect: false, onCancel: cancel });
  f.input.value = "retained"; await second.search("retained"); key(f.input, "Escape"); expect(f.input.value).toBe("retained"); expect(cancel).toHaveBeenCalledTimes(1);
});

test("source errors are reported and rendered separately from an empty result without unsafe markup", async () => {
  const f = fixture(), failure = new Error("Source unavailable");
  const handle = f.build({ input: f.input, owner: f.owner, environment: f.environment, sources: [{ options: () => { throw failure; } }] }); await handle.search("");
  expect(f.report).toHaveBeenCalledWith(failure); expect(handle.dropdown.querySelector('[role="alert"]')?.textContent).toBe("Source unavailable");
  handle.dispose(); const second = f.build({ input: f.input, owner: f.owner, environment: f.environment, sources: [{ options: [option("<img src=x onerror=alert(1)>")] }] }); await second.search("");
  expect(second.dropdown.querySelector("img")).toBeNull(); expect(second.dropdown.textContent).toBe("<img src=x onerror=alert(1)>");
});

test("owner abort and callback rejection close and report without an unhandled rejection", async () => {
  const f = fixture(), controller = new AbortController(), failure = new Error("Selection failed");
  const handle = f.build({ input: f.input, owner: f.owner, environment: { ...f.environment, signal: controller.signal }, sources: [{ options: [option("Fail", () => Promise.reject(failure))] }] });
  await handle.search(""); handle.dropdown.querySelector<HTMLElement>('[role="option"]')!.click(); await Promise.resolve(); await Promise.resolve();
  expect(f.report).toHaveBeenCalledWith(failure); expect(handle.dropdown.querySelector('[role="alert"]')?.textContent).toBe("Selection failed"); expect(handle.isOpen()).toBe(true); controller.abort(); await handle.search(""); expect(handle.isOpen()).toBe(false);
});

test("invalid declarations and excessive source output fail with typed messages", async () => {
  const f = fixture();
  expect(() => createAutocomplete({ input: f.input, owner: f.owner, environment: f.environment, sources: [], debounceMs: -1 })).toThrowError(expect.objectContaining({ kind: "validation", message: "Autocomplete debounce must be a finite number from 0 to 10000." }));
  const handle = f.build({ input: f.input, owner: f.owner, environment: f.environment, sources: [{ options: Array.from({ length: 1025 }, () => option("x")) }] }); await handle.search("");
  expect(f.report).toHaveBeenCalledWith(expect.objectContaining({ kind: "validation", message: "Autocomplete options exceed 1024 entries or 262144 bytes." }));
});

test("autocomplete combines source states, pending input, selection, cancellation and destroyed-owner guards", async () => {
  vi.useFakeTimers(); const f = fixture(), select = vi.fn(), cancel = vi.fn();
  let resolve!: (value: readonly AutocompleteOption[]) => void;
  const source = vi.fn((query: string): readonly AutocompleteOption[] | Promise<readonly AutocompleteOption[]> => query === "async" ? new Promise(done => { resolve = done; }) : [{ kind: "heading", label: "Group" }, option("First", select), { kind: "status", label: "Unavailable" }, option("Second", select)]);
  const handle = f.build({ input: f.input, owner: f.owner, environment: f.environment, sources: [{ options: source }], debounceMs: 20, autoSelect: true, representedValue: () => "Saved", onCancel: cancel });
  await handle.search(""); expect(f.input.getAttribute("role")).toBe("combobox"); expect(f.input.getAttribute("aria-expanded")).toBe("true");
  expect(document.getElementById(f.input.getAttribute("aria-activedescendant")!)?.textContent).toBe("First"); key(f.input, "ArrowDown");
  expect(document.getElementById(f.input.getAttribute("aria-activedescendant")!)?.textContent).toBe("Second"); key(f.input, "Enter"); expect(select).toHaveBeenCalledOnce(); expect(handle.isOpen()).toBe(false);
  f.input.value = "async"; f.input.dispatchEvent(new Event("input")); await vi.advanceTimersByTimeAsync(19); expect(source).toHaveBeenCalledOnce();
  await vi.advanceTimersByTimeAsync(1); expect(f.input.getAttribute("aria-busy")).toBe("true"); key(f.input, "Tab"); resolve([option("Loaded", select)]); await vi.advanceTimersByTimeAsync(0);
  expect(select).toHaveBeenCalledTimes(2); expect(handle.isOpen()).toBe(false);
  await handle.search(""); handle.dropdown.querySelector<HTMLElement>('[role="option"]')!.click(); expect(select).toHaveBeenCalledTimes(3);
  f.input.value = "draft"; await handle.search("draft"); key(f.input, "Escape"); expect(f.input.value).toBe("Saved"); expect(cancel).toHaveBeenCalledOnce();
  const pending = handle.search("async"); key(f.input, "Enter"); handle.dispose(); resolve([option("After destruction", select)]); await pending; expect(select).toHaveBeenCalledTimes(3); expect(f.input.hasAttribute("aria-controls")).toBe(false);
});

test("pending Enter during debounce waits for its query and option admission rejects null payloads", async () => {
  vi.useFakeTimers(); const f = fixture(), select = vi.fn();
  const handle = f.build({ input: f.input, owner: f.owner, environment: f.environment, sources: [{ options: [option("Picked", select)] }], autoSelect: true, debounceMs: 20 });
  await handle.search(""); f.input.value = "next"; f.input.dispatchEvent(new Event("input")); key(f.input, "Enter"); await vi.advanceTimersByTimeAsync(20); expect(select).toHaveBeenCalledOnce();
  handle.dispose(); const invalid = f.build({ input: f.input, owner: f.owner, environment: f.environment, sources: [{ options: [null as unknown as AutocompleteOption] }] }); await invalid.search("");
  expect(f.report).toHaveBeenCalledWith(expect.objectContaining({ kind: "validation", message: "Autocomplete option options must be a plain object." }));
});

test("Enter during the first debounced input waits for its options instead of submitting unresolved text", async () => {
  vi.useFakeTimers(); const f = fixture(), select = vi.fn();
  const handle = f.build({ input: f.input, owner: f.owner, environment: f.environment, sources: [{ options: [option("First result", select)] }], debounceMs: 20 });
  f.input.value = "first"; f.input.dispatchEvent(new Event("input"));
  const event = new KeyboardEvent("keydown", { key: "Enter", bubbles: true, cancelable: true }); f.input.dispatchEvent(event);
  expect(event.defaultPrevented).toBe(true); await vi.advanceTimersByTimeAsync(20); expect(select).toHaveBeenCalledOnce(); expect(handle.isOpen()).toBe(false);
});

test("pending, empty and failed sources retain accessible listbox children", async () => {
  const f = fixture(); let resolve!: (value: readonly AutocompleteOption[]) => void, fail = false;
  const handle = f.build({ input: f.input, owner: f.owner, environment: f.environment, sources: [{ options: () => {
    if (fail) throw new Error("Choices unavailable"); return new Promise(done => { resolve = done; });
  } }] });
  const pending = handle.search("pending"); await auditComponent(handle.dropdown.parentElement!);
  resolve([]); await pending; await auditComponent(handle.dropdown.parentElement!);
  fail = true; await handle.search("failure"); await auditComponent(handle.dropdown.parentElement!);
  const placeholder = handle.dropdown.querySelector<HTMLElement>('[role="option"][aria-disabled="true"]');
  expect(placeholder?.textContent).toBe("Choices unavailable"); key(f.input, "ArrowDown"); expect(f.input.hasAttribute("aria-activedescendant")).toBe(false);
});

test("raw input invalidates the previous request before debounce and binds pending Enter to its own query", async () => {
  vi.useFakeTimers(); const f = fixture(), select = vi.fn();
  const resolvers = new Map<string, (value: readonly AutocompleteOption[]) => void>(), signals = new Map<string, AbortSignal>();
  const source = vi.fn((query: string, signal: AbortSignal): Promise<readonly AutocompleteOption[]> => { signals.set(query, signal); return new Promise(resolve => { resolvers.set(query, resolve); }); });
  const handle = f.build({ input: f.input, owner: f.owner, environment: f.environment, sources: [{ options: source }], debounceMs: 40, autoSelect: true });
  const first = handle.search("A"); f.input.value = "B"; f.input.dispatchEvent(new Event("input")); key(f.input, "Enter");
  resolvers.get("A")!([option("A result", () => select("A"))]); await first; await vi.advanceTimersByTimeAsync(0);
  expect(select).not.toHaveBeenCalled(); expect(handle.dropdown.textContent).not.toContain("A result"); expect(signals.get("A")!.aborted).toBe(true);
  await vi.advanceTimersByTimeAsync(39); expect(source).toHaveBeenCalledTimes(1);
  await vi.advanceTimersByTimeAsync(1); expect(source).toHaveBeenCalledTimes(2); expect(source.mock.calls[1]![0]).toBe("B");
  resolvers.get("B")!([option("B result", () => select("B"))]); await vi.advanceTimersByTimeAsync(0);
  expect(select).toHaveBeenCalledTimes(1); expect(select).toHaveBeenCalledWith("B"); expect(handle.isOpen()).toBe(false);
});

test("stationary hover preserves keyboard choice while moving mouse or pen changes it", async () => {
  const f = fixture(), first = vi.fn(), second = vi.fn();
  const handle = f.build({ input: f.input, owner: f.owner, environment: f.environment, autoSelect: true, sources: [{ options: [option("First", first), option("Second", second)] }] });
  await handle.search(""); key(f.input, "ArrowDown");
  const rows = handle.dropdown.querySelectorAll<HTMLElement>('[role="option"]');
  const active = () => document.getElementById(f.input.getAttribute("aria-activedescendant")!)?.textContent;
  rows[0]!.dispatchEvent(new MouseEvent("mouseenter")); expect(active()).toBe("Second");
  const move = (target: HTMLElement, pointerType: string, movementX: number): void => {
    const event = new Event("pointermove", { bubbles: true }); Object.defineProperties(event, { pointerType: { value: pointerType }, movementX: { value: movementX }, movementY: { value: 0 } }); target.dispatchEvent(event);
  };
  move(rows[0]!, "mouse", 0); expect(active()).toBe("Second"); move(rows[0]!, "touch", 3); expect(active()).toBe("Second");
  move(rows[0]!, "pen", 2); expect(active()).toBe("First"); move(rows[1]!, "mouse", 2); expect(active()).toBe("Second");
  key(f.input, "Enter"); expect(first).not.toHaveBeenCalled(); expect(second).toHaveBeenCalledOnce();
});
