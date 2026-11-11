/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
import { afterEach, beforeEach, describe, expect, test, vi } from "vitest";
import { createFieldControl } from "../src/client/fields.js";
import type { Field } from "../src/client/metadata.js";
import { RpcClient } from "../src/client/transport.js";

const rpc = new RpcClient(
  async () => {
    throw new Error("Unexpected RPC");
  },
  () => {}
);

function makeField(
  name: string,
  kind: Field["kind"],
  widget: string,
  readonly = false,
  options: Record<string, unknown> = {}
): Field {
  return {
    name,
    label: name.replace(/_/g, " ").toUpperCase(),
    kind,
    readonly,
    required: false,
    relation: null,
    relationField: null,
    selection: null,
    widget,
    options,
  };
}

function mountControl(field: Field, value: unknown = "", change = vi.fn()) {
  const handle = createFieldControl({
    field,
    value,
    rpc,
    context: {},
    change,
    run: job => {
      void job();
    },
  });
  document.body.append(handle.node);
  return { ...handle, change };
}

describe("CopyClipboard widgets", () => {
  let originalClipboard: Clipboard;

  beforeEach(() => {
    originalClipboard = navigator.clipboard;
  });

  afterEach(() => {
    document.body.replaceChildren();
    Object.defineProperty(navigator, "clipboard", {
      value: originalClipboard,
      writable: true,
      configurable: true,
    });
  });

  /// (partial: CopyClipboardChar in read mode displays value and visible copy button)
  test("CopyClipboardChar renders read value and visible copy button", () => {
    const f = makeField("reference_code", "char", "CopyClipboardChar", true);
    const handle = mountControl(f, "REF-12345");

    const readVal = handle.node.querySelector(".field-read-value");
    expect(readVal).not.toBeNull();
    expect(readVal?.textContent).toBe("REF-12345");

    const copyBtn = handle.node.querySelector<HTMLButtonElement>(".o_clipboard_button");
    expect(copyBtn).not.toBeNull();
    expect(copyBtn?.hidden).toBe(false);
    expect(copyBtn?.classList.contains("o_btn_char_copy")).toBe(true);
    expect(copyBtn?.getAttribute("aria-label")).toBe("Copy REFERENCE CODE");
  });

  /// (partial: copy button is hidden when value is empty)
  test("Copy button is hidden on empty char field and shows when populated", () => {
    const f = makeField("reference_code", "char", "CopyClipboardChar", false);
    const handle = mountControl(f, "");

    const copyBtn = handle.node.querySelector<HTMLButtonElement>(".o_clipboard_button");
    expect(copyBtn).not.toBeNull();
    expect(copyBtn?.hidden).toBe(true);

    const input = handle.node.querySelector<HTMLInputElement>("input")!;
    expect(input).not.toBeNull();

    // Type text into input: button becomes visible
    input.value = "NEW-CODE";
    input.dispatchEvent(new Event("input"));
    expect(copyBtn?.hidden).toBe(false);

    // Clear input: button is hidden again
    input.value = "";
    input.dispatchEvent(new Event("input"));
    expect(copyBtn?.hidden).toBe(true);
  });

  /// (partial: successful copy writes to clipboard and shows short confirmation)
  test("successful clipboard write shows short confirmation", async () => {
    const writeText = vi.fn().mockResolvedValue(undefined);
    Object.defineProperty(navigator, "clipboard", {
      value: { writeText },
      writable: true,
      configurable: true,
    });

    const f = makeField("token", "char", "CopyClipboardChar", true);
    const handle = mountControl(f, "secret-token-xyz");

    const copyBtn = handle.node.querySelector<HTMLButtonElement>(".o_clipboard_button")!;
    copyBtn.click();

    await vi.waitFor(() => {
      expect(writeText).toHaveBeenCalledWith("secret-token-xyz");
    });

    const feedback = handle.node.querySelector<HTMLElement>(".field-copy-feedback")!;
    expect(feedback.textContent).toBe("Copied");
    expect(feedback.classList.contains("field-copy-success")).toBe(true);
  });

  /// (partial: refused clipboard permission shows a visible error, not silence)
  test("refused clipboard permission displays a visible error instead of silence", async () => {
    const writeText = vi.fn().mockRejectedValue(new DOMException("Permission denied", "NotAllowedError"));
    Object.defineProperty(navigator, "clipboard", {
      value: { writeText },
      writable: true,
      configurable: true,
    });

    const f = makeField("token", "char", "CopyClipboardChar", true);
    const handle = mountControl(f, "token-to-copy");

    const copyBtn = handle.node.querySelector<HTMLButtonElement>(".o_clipboard_button")!;
    copyBtn.click();

    await vi.waitFor(() => {
      expect(writeText).toHaveBeenCalledWith("token-to-copy");
    });

    // One visible status message; the field value itself is not marked invalid.
    const feedback = handle.node.querySelector<HTMLElement>(".field-copy-feedback")!;
    await vi.waitFor(() => expect(feedback.textContent).toBe("Clipboard copy failed: Permission denied"));
    expect(feedback.hidden).toBe(false);
    expect(feedback.classList.contains("field-copy-error")).toBe(true);
    expect(handle.node.classList.contains("field-invalid")).toBe(false);
    expect(copyBtn.getAttribute("aria-label")).toBe("Copy TOKEN");
  });

  /// (partial: CopyClipboardText copies multi-line text and validates text field kind)
  test("CopyClipboardText renders textarea in edit mode and copies multi-line text", async () => {
    const writeText = vi.fn().mockResolvedValue(undefined);
    Object.defineProperty(navigator, "clipboard", {
      value: { writeText },
      writable: true,
      configurable: true,
    });

    const f = makeField("notes", "text", "CopyClipboardText", false);
    const multiLine = "Line 1\nLine 2\nLine 3";
    const handle = mountControl(f, multiLine);

    const textarea = handle.node.querySelector<HTMLTextAreaElement>("textarea")!;
    expect(textarea).not.toBeNull();
    expect(textarea.value).toBe(multiLine);

    const copyBtn = handle.node.querySelector<HTMLButtonElement>(".o_clipboard_button")!;
    expect(copyBtn.hidden).toBe(false);
    expect(copyBtn.classList.contains("o_btn_text_copy")).toBe(true);

    copyBtn.click();
    await vi.waitFor(() => {
      expect(writeText).toHaveBeenCalledWith(multiLine);
    });
  });

  /// (partial: CopyClipboardURL renders safe link in read mode and copies URL value)
  test("CopyClipboardURL renders safe link in read mode and copies URL", async () => {
    const writeText = vi.fn().mockResolvedValue(undefined);
    Object.defineProperty(navigator, "clipboard", {
      value: { writeText },
      writable: true,
      configurable: true,
    });

    const f = makeField("website", "char", "CopyClipboardURL", true);
    const url = "http://example.test";
    const handle = mountControl(f, url);

    const link = handle.node.querySelector<HTMLAnchorElement>("a.field-link")!;
    expect(link).not.toBeNull();
    expect(link.href).toBe("http://example.test/");

    const copyBtn = handle.node.querySelector<HTMLButtonElement>(".o_clipboard_button")!;
    expect(copyBtn.hidden).toBe(false);
    expect(copyBtn.classList.contains("o_btn_url_copy")).toBe(true);

    copyBtn.click();
    await vi.waitFor(() => {
      expect(writeText).toHaveBeenCalledWith(url);
    });
  });

  /// (partial: widget declares strict field type compatibility)
  test("widgets refuse incompatible field kinds loudly", () => {
    // CopyClipboardChar on integer
    const intField = makeField("count", "integer", "CopyClipboardChar", false);
    expect(() => mountControl(intField, 42)).toThrowError(
      "Field count with CopyClipboardChar widget must be a char field."
    );

    // CopyClipboardText on char
    const charField = makeField("code", "char", "CopyClipboardText", false);
    expect(() => mountControl(charField, "ABC")).toThrowError(
      "Field code with CopyClipboardText widget must be a text field."
    );
  });

  /// (partial: a missing clipboard API is reported, not ignored)
  test("missing clipboard API shows an error and the next edit clears it", async () => {
    Object.defineProperty(navigator, "clipboard", { value: undefined, writable: true, configurable: true });
    const handle = mountControl(makeField("code", "char", "CopyClipboardChar", false), "ABC");
    handle.node.querySelector<HTMLButtonElement>(".o_clipboard_button")!.click();
    const feedback = handle.node.querySelector<HTMLElement>(".field-copy-feedback")!;
    await vi.waitFor(() => expect(feedback.textContent).toBe("Clipboard copy failed: the clipboard is not available in this browser context"));
    const input = handle.node.querySelector<HTMLInputElement>("input")!;
    input.value = "ABCD";
    input.dispatchEvent(new Event("input"));
    expect(feedback.hidden).toBe(true);
    expect(feedback.textContent).toBe("");
  });

  /// (partial: the confirmation disappears after its delay and dispose cancels the timer)
  test("confirmation clears after its delay and dispose cancels a pending timer", async () => {
    vi.useFakeTimers();
    try {
      Object.defineProperty(navigator, "clipboard", { value: { writeText: vi.fn().mockResolvedValue(undefined) }, writable: true, configurable: true });
      const handle = mountControl(makeField("code", "char", "CopyClipboardChar", true), "ABC");
      handle.node.querySelector<HTMLButtonElement>(".o_clipboard_button")!.click();
      await vi.advanceTimersByTimeAsync(0);
      const feedback = handle.node.querySelector<HTMLElement>(".field-copy-feedback")!;
      expect(feedback.textContent).toBe("Copied");
      await vi.advanceTimersByTimeAsync(800);
      expect(feedback.hidden).toBe(true);
      handle.node.querySelector<HTMLButtonElement>(".o_clipboard_button")!.click();
      await vi.advanceTimersByTimeAsync(0);
      expect(vi.getTimerCount()).toBe(1);
      handle.dispose();
      expect(vi.getTimerCount()).toBe(0);
    } finally {
      vi.useRealTimers();
    }
  });

  /// (partial: an unsafe URL value is shown as text, never as a link)
  test("CopyClipboardURL shows an unsafe value as plain text", () => {
    const handle = mountControl(makeField("website", "char", "CopyClipboardURL", true), "javascript:alert(1)\u0007");
    expect(handle.node.querySelector("a")).toBeNull();
    expect(handle.node.querySelector(".field-read-value")?.textContent).toBe("javascript:alert(1)\u0007");
    const safe = mountControl(makeField("website", "char", "CopyClipboardURL", true), "example.com");
    const link = safe.node.querySelector<HTMLAnchorElement>("a.field-link")!;
    expect(link.getAttribute("href")).toBe("http://example.com");
    expect(link.rel).toBe("noopener noreferrer");
    expect(link.target).toBe("_blank");
  });

  /// (partial: the field label names the editable input in form and list)
  test("the label targets the input in form edit mode and the input is named in a list", () => {
    const form = mountControl(makeField("reference_code", "char", "CopyClipboardChar", false), "R1");
    const input = form.node.querySelector<HTMLInputElement>("input")!;
    const label = form.node.querySelector<HTMLLabelElement>("label")!;
    expect(label.htmlFor).toBe(input.id);
    expect(input.closest(".field-copy-clipboard")).not.toBeNull();
    const list = createFieldControl({ viewContext: "list", field: makeField("reference_code", "char", "CopyClipboardChar", false), value: "R1", rpc, context: {}, change: vi.fn(), run: job => { void job(); } });
    expect(list.node.querySelector("input")!.getAttribute("aria-label")).toBe("REFERENCE CODE");
  });
});
