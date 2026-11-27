/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
import { afterEach, expect, test, vi } from "vitest";
import { MessageServices, type WidgetEnvironment, type WidgetHandle } from "@erpforall/client";
import { methodField } from "../src/method-field.js";

let field: WidgetHandle;
let services: MessageServices;
afterEach(() => { field.dispose(); services.dispose(); document.body.replaceChildren(); });

function mount(model: unknown) {
  const change = vi.fn();
  const report = vi.fn();
  const call = vi.fn(async (resource: string) => resource === "ir.model" ? [{ model: "res.partner" }] : ["web_save"]);
  services = new MessageServices(document.body, report);
  field = methodField({
    values: { model_id: model, effect: "write" }, value: "", field: { name: "method", label: "Method" },
    context: {}, messages: services, change, report, rpc: { call },
  } as unknown as WidgetEnvironment);
  document.body.append(field.node);
  const input = document.querySelector<HTMLInputElement>("input")!;
  return { call, change, input };
}

test("a model and effect determine the searchable methods and the selected value", async () => {
  const { call, change, input } = mount({ id: 23, display_name: "Contact" });
  expect(input.labels?.[0]?.textContent).toBe("Method");
  input.value = "save";
  input.dispatchEvent(new Event("input"));
  await vi.waitFor(() => expect(document.querySelector('[role="option"][aria-selected]')?.textContent).toBe("web_save"));
  (document.querySelector('[role="option"][aria-selected]') as HTMLElement).click();
  expect(call).toHaveBeenCalledWith("ir.approval.policy", "erp_workflow_methods", { model: "res.partner", effect: "write", query: "save", context: {} }, expect.anything());
  expect(change).toHaveBeenCalledWith("web_save");
  input.click();
  await vi.waitFor(() => expect(document.querySelector('[role="option"]')?.textContent).toBe("All methods"));
  (document.querySelector('[role="option"]') as HTMLElement).click();
  expect(change).toHaveBeenCalledWith(false);
});

test("a workflow across all models has no method picker queries", () => {
  const { input, call } = mount(false);
  expect(input.disabled).toBe(true);
  expect(call).not.toHaveBeenCalled();
});
