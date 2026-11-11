/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
import { afterEach, expect, test, vi } from "vitest";
import { createDropzone } from "../src/client/dropzone.js";

const handles: ReturnType<typeof createDropzone>[] = [];
afterEach(() => {
  for (const handle of handles.splice(0)) handle.dispose();
  vi.restoreAllMocks();
  vi.unstubAllGlobals();
  document.body.replaceChildren();
});

function drag(type: string, files = true): DragEvent {
  // jsdom does not supply native drag data. Real target geometry and native file
  // input/drop acceptance belong to the compiled browser evidence, not this fixture.
  const event = new Event(type, { bubbles: true, cancelable: true });
  Object.defineProperty(event, "dataTransfer", {
    value: {
      types: files ? ["Files"] : ["text/plain"],
      files: files ? [new File(["file contents"], "report.txt", { type: "text/plain" })] : []
    }
  });
  return event as DragEvent;
}

function setup(options: { signal?: AbortSignal; disabled?: () => boolean } = {}) {
  const target = document.createElement("div"), child = document.createElement("span");
  target.append(child); document.body.append(target);
  let rectangle = new DOMRect(20, 30, 320, 180);
  const measure = vi.spyOn(target, "getBoundingClientRect").mockImplementation(() => rectangle);
  const onDrop = vi.fn();
  const handle = createDropzone({ target, onDrop, ...options });
  handles.push(handle); document.body.append(handle.node);
  return { target, child, measure, onDrop, handle, rect: (next: DOMRect) => { rectangle = next; } };
}

/// (partial: component rectangle updates; real layout is covered by compiled-browser evidence)
test("dropzone aligns to the current target and refreshes for patch, resize and captured scrolling", () => {
  const { handle, target, rect } = setup();
  const geometry = () => [handle.node.style.position, handle.node.style.left, handle.node.style.top, handle.node.style.width, handle.node.style.height];
  expect(geometry()).toEqual(["fixed", "20px", "30px", "320px", "180px"]);
  rect(new DOMRect(8, 12, 400, 220)); handle.refresh();
  expect(geometry()).toEqual(["fixed", "8px", "12px", "400px", "220px"]);
  rect(new DOMRect(4, 6, 200, 90)); window.dispatchEvent(new Event("resize"));
  expect(geometry()).toEqual(["fixed", "4px", "6px", "200px", "90px"]);
  rect(new DOMRect(-2, -4, 190, 80)); target.dispatchEvent(new Event("scroll"));
  expect(geometry()).toEqual(["fixed", "-2px", "-4px", "190px", "80px"]);
  rect(new DOMRect(0, 0, 0, 0)); handle.refresh();
  expect(geometry()).toEqual(["fixed", "0px", "0px", "0px", "0px"]);
});

/// (partial: drag-state and delegated event contract; native drag behavior is covered separately)
test("nested file drags retain inside state and one delegated drop resets the overlay", () => {
  const { handle, target, child, onDrop } = setup();
  expect(handle.node.classList.contains("file-dropzone")).toBe(true);
  expect(handle.node.dataset["dragging"]).toBe("false");
  expect(handle.node.style.pointerEvents).toBe("none");
  const entered = drag("dragenter"); target.dispatchEvent(entered);
  expect(entered.defaultPrevented).toBe(true);
  expect(handle.node.dataset["dragging"]).toBe("true");
  expect(handle.node.style.pointerEvents).toBe("auto");
  child.dispatchEvent(drag("dragenter")); child.dispatchEvent(drag("dragleave"));
  expect(handle.node.dataset["dragging"]).toBe("true");
  const over = drag("dragover"); handle.node.dispatchEvent(over);
  expect(over.defaultPrevented).toBe(true);
  const dropped = drag("drop"); handle.node.dispatchEvent(dropped);
  expect(dropped.defaultPrevented).toBe(true);
  expect(onDrop).toHaveBeenCalledExactlyOnceWith(dropped);
  expect(handle.node.dataset["dragging"]).toBe("false");
  expect(handle.node.style.pointerEvents).toBe("none");
  target.dispatchEvent(drag("dragenter")); target.dispatchEvent(drag("dragleave"));
  expect(handle.node.dataset["dragging"]).toBe("false");
});

test("target drops are delegated once without selecting a route or storing file data", () => {
  const { target, handle, onDrop } = setup();
  const dropped = drag("drop"); target.dispatchEvent(dropped);
  expect(onDrop).toHaveBeenCalledExactlyOnceWith(dropped);
  expect(handle.node.dataset["dragging"]).toBe("false");
  // The overlay is only a visual cue. The owner supplies the keyboard-accessible
  // file input, accepted-file hints and all validation/transport decisions.
  expect(handle.node.querySelector("input,button,form")).toBeNull();
});

test("non-file drag data is not intercepted or delegated", () => {
  const { target, handle, onDrop } = setup();
  for (const type of ["dragenter", "dragover", "dragleave", "drop"]) {
    const event = drag(type, false); target.dispatchEvent(event);
    expect(event.defaultPrevented).toBe(false);
  }
  expect(handle.node.dataset["dragging"]).toBe("false");
  expect(handle.node.style.pointerEvents).toBe("none");
  expect(onDrop).not.toHaveBeenCalled();
});

test("disabled hosts do not accept file drags and can accept a later enabled drag", () => {
  let disabled = true;
  const { target, handle, onDrop } = setup({ disabled: () => disabled });
  const entered = drag("dragenter"), dropped = drag("drop");
  target.dispatchEvent(entered); target.dispatchEvent(dropped);
  expect(entered.defaultPrevented).toBe(false); expect(dropped.defaultPrevented).toBe(false);
  expect(handle.node.dataset["dragging"]).toBe("false"); expect(onDrop).not.toHaveBeenCalled();
  disabled = false; target.dispatchEvent(drag("dragenter"));
  expect(handle.node.dataset["dragging"]).toBe("true");
  disabled = true; const disabledOver = drag("dragover"); target.dispatchEvent(disabledOver);
  expect(disabledOver.defaultPrevented).toBe(false);
  expect(handle.node.dataset["dragging"]).toBe("false");
});

test("document drag end resets an unfinished drag without calling the owner", () => {
  const { target, handle, onDrop } = setup();
  target.dispatchEvent(drag("dragenter"));
  expect(handle.node.dataset["dragging"]).toBe("true");
  document.dispatchEvent(new Event("dragend"));
  expect(handle.node.dataset["dragging"]).toBe("false");
  expect(handle.node.style.pointerEvents).toBe("none"); expect(onDrop).not.toHaveBeenCalled();
});

test("owner abort and repeated disposal remove the overlay and prevent late geometry or drop work", () => {
  const abort = new AbortController();
  const { handle, target, onDrop, measure } = setup({ signal: abort.signal });
  target.dispatchEvent(drag("dragenter")); abort.abort();
  expect(handle.node.isConnected).toBe(false);
  const calls = measure.mock.calls.length;
  const late = drag("drop"); target.dispatchEvent(late);
  window.dispatchEvent(new Event("resize")); target.dispatchEvent(new Event("scroll"));
  handle.refresh(); handle.dispose();
  expect(measure.mock.calls.length).toBe(calls);
  expect(late.defaultPrevented).toBe(false); expect(onDrop).not.toHaveBeenCalled();
});

test("an already-aborted owner never attaches drop handlers", () => {
  const abort = new AbortController(); abort.abort();
  const { target, onDrop, handle } = setup({ signal: abort.signal });
  const event = drag("drop"); target.dispatchEvent(event);
  expect(onDrop).not.toHaveBeenCalled(); expect(event.defaultPrevented).toBe(false);
  expect(handle.node.style.pointerEvents).toBe("none");
});

test("target ResizeObserver refreshes geometry and disconnects on disposal", () => {
  let observer: TestResizeObserver | undefined;
  class TestResizeObserver implements ResizeObserver {
    readonly observe = vi.fn(); readonly unobserve = vi.fn(); readonly disconnect = vi.fn();
    constructor(private readonly callback: ResizeObserverCallback) { observer = this; }
    notify(): void { this.callback([], this); }
  }
  vi.stubGlobal("ResizeObserver", TestResizeObserver);
  const { handle, target, rect, measure } = setup();
  expect(observer?.observe).toHaveBeenCalledExactlyOnceWith(target);
  rect(new DOMRect(1, 2, 123, 45)); observer!.notify();
  expect(handle.node.style.width).toBe("123px"); expect(handle.node.style.height).toBe("45px");
  handle.dispose(); expect(observer?.disconnect).toHaveBeenCalledTimes(1);
  const calls = measure.mock.calls.length; observer!.notify();
  expect(measure.mock.calls.length).toBe(calls);
});

test("dropzone rejects malformed trusted options with precise validation failures", () => {
  const target = document.createElement("div"), onDrop = () => {};
  const invalid: readonly [unknown, string][] = [
    [null, "Dropzone options must be a plain object."],
    [{ target, onDrop, route: "/upload" }, "Unknown dropzone option: route."],
    [{ target: "target", onDrop }, "Dropzone target must be an HTMLElement."],
    [{ target, onDrop: true }, "Dropzone onDrop must be a function."],
    [{ target, onDrop, signal: {} }, "Dropzone signal must be an AbortSignal."],
    [{ target, onDrop, disabled: false }, "Dropzone disabled must be a function."]
  ];
  for (const [options, message] of invalid) {
    expect(() => createDropzone(options as Parameters<typeof createDropzone>[0])).toThrowError(expect.objectContaining({ kind: "validation", message }));
  }
});

test("dropzone acceptance aligns after mount/patch and delegates entered/left/dropped files to its owner", () => {
  const { target, handle, rect, onDrop } = setup();
  expect(handle.node.style.cssText).toContain("left: 20px");
  expect(handle.node.style.width).toBe("320px"); expect(handle.node.style.height).toBe("180px");
  rect(new DOMRect(12, 16, 240, 100)); handle.refresh();
  expect([handle.node.style.left, handle.node.style.top, handle.node.style.width, handle.node.style.height]).toEqual(["12px", "16px", "240px", "100px"]);
  target.dispatchEvent(drag("dragenter")); expect(handle.node.dataset["dragging"]).toBe("true");
  target.dispatchEvent(drag("dragleave")); expect(handle.node.dataset["dragging"]).toBe("false");
  target.dispatchEvent(drag("dragenter"));
  const event = drag("drop"); handle.node.dispatchEvent(event);
  expect(onDrop).toHaveBeenCalledExactlyOnceWith(event); expect(handle.node.dataset["dragging"]).toBe("false");
  expect(handle.node.querySelector("input,form")).toBeNull();
  handle.dispose(); target.dispatchEvent(drag("drop")); expect(onDrop).toHaveBeenCalledTimes(1);
});
