/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
import { afterEach, expect, test, vi } from "vitest";
import { withTestScope } from "@erpforall/elang-test";
import { auditComponent } from "../testing/axe.js";
import { createButtonControl, type ButtonControlOptions } from "../../addons/web/static/src/client/button-control.js";
import { ClientError } from "../../addons/web/static/src/client/transport.js";

afterEach(() => document.body.replaceChildren());
function failure(options: ButtonControlOptions, message: string): void {
  try { createButtonControl(options); throw new Error("Expected refusal"); }
  catch (error) { expect(error).toBeInstanceOf(ClientError); expect(error).toMatchObject({ kind: "validation", message }); }
}

/// (partial: client-local seven-rank presentation, size and action identity; server declaration and rendered states remain separate)
test("every rank and size preserves the declared action without inferring confirmation", async () => withTestScope(async () => {
  const action = vi.fn();
  for (const rank of ["primary", "secondary", "link", "success", "info", "warning", "danger"] as const) {
    for (const size of ["small", "normal", "large"] as const) {
      const control = createButtonControl({ label: "Delete", rank, size, action }); document.body.append(control);
      expect(control.tagName).toBe("BUTTON"); expect(control.type).toBe("button");
      expect(control.classList.contains(`erp-button-${rank}`)).toBe(true);
      expect(control.classList.contains(`erp-button-${size}`)).toBe(true);
      control.click();
    }
  }
  expect(action).toHaveBeenCalledTimes(21);
  expect(document.querySelector("dialog")).toBeNull();
}));

/// (partial: icon/caption naming and simple supplied title; help/debug tooltip requires wire attributes)
test("icon-only controls have one accessible target and labels stay text", async () => withTestScope(async () => {
  const action = vi.fn();
  const control = createButtonControl({ label: "", accessibleLabel: "Reload record", title: "Fetch current values", icon: "refresh-cw", action });
  document.body.append(control);
  expect(control.getAttribute("aria-label")).toBe("Reload record"); expect(control.title).toBe("Fetch current values");
  const svg = control.querySelector("svg")!;
  expect(svg.getAttribute("aria-hidden")).toBe("true"); expect(svg.getAttribute("focusable")).toBe("false");
  expect(control.querySelectorAll("button,a,[tabindex]")).toHaveLength(0);
  svg.dispatchEvent(new MouseEvent("click", { bubbles: true })); expect(action).toHaveBeenCalledOnce();
  const data = createButtonControl({ label: "<img src=x>Actions", action }); document.body.append(data);
  expect(data.textContent).toBe("<img src=x>Actions"); expect(data.querySelector("svg,img")).toBeNull();
  await auditComponent(control);
}));

/// (partial: native disabled, pending and missing-action states of a client-local control)
test("disabled, pending and non-action controls cannot invoke callbacks", () => {
  const action = vi.fn();
  for (const options of [{ disabled: true, action }, { pending: true, action }, {}]) {
    const control = createButtonControl({ label: "Run", ...options }); document.body.append(control);
    expect(control.disabled).toBe(true); control.click();
  }
  expect(action).not.toHaveBeenCalled();
  expect(document.querySelector('[aria-busy="true"]')).not.toBeNull();
});

/// (partial: circular component opt-in and native focus target; rendered interaction states tested in browser)
test("circle presentation adds no second focus target or action", () => {
  const action = vi.fn(); const control = createButtonControl({ label: "", title: "Add line", icon: "plus", circle: true, action });
  document.body.append(control); control.focus();
  expect(document.activeElement).toBe(control); expect(control.classList.contains("erp-button-circle")).toBe(true);
  expect(control.getAttribute("aria-label")).toBe("Add line"); control.click(); expect(action).toHaveBeenCalledOnce();
});

test("invalid presentation and inaccessible icon-only buttons fail with kind and message", () => {
  failure({ label: "Run", rank: "neon" as "primary" }, "Unknown button rank: neon.");
  failure({ label: "Run", size: "huge" as "small" }, "Unknown button size: huge.");
  failure({ label: "", icon: "plus", action: vi.fn() }, "A button requires a visible label, accessible label or title.");
  failure({ label: "x".repeat(4097) }, "Button label exceeds the 4096-character limit.");
});
