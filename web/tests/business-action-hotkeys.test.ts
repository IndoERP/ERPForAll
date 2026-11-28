/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
import { readFileSync } from "node:fs";
import { dirname, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import { expect, test } from "vitest";

test.each([
  ["sale/views/sale_order_views.xml", "view_order_form", { Confirm: "q", "Send by Email": "g", Cancel: "x" }],
  ["sale/views/sale_template_views.xml", "view_sale_quotation_send_form", { Discard: "j" }],
  ["purchase/views/purchase_views.xml", "purchase_order_form", { "Confirm Order": "q", Cancel: "x" }],
  ["account/views/account_invoice_views.xml", "view_invoice_form", { Confirm: "q" }],
  ["account/views/account_move_views.xml", "view_move_form", { Post: "q" }],
  ["stock/views/stock_picking_views.xml", "view_picking_form", { Validate: "v", Cancel: "x" }],
] as const)("%s ships action shortcuts for %s", (file, viewId, keys) => {
  const source = readFileSync(resolve(dirname(fileURLToPath(import.meta.url)), "../../addons", file), "utf8");
  const document = new DOMParser().parseFromString(source, "application/xml");
  const form = document.querySelector(`record[id="${viewId}"] form`);
  expect(form).not.toBeNull();
  for (const [label, key] of Object.entries(keys)) {
    expect(form?.querySelector(`button[string="${label}"]`)?.getAttribute("data-hotkey")).toBe(key);
  }
});
