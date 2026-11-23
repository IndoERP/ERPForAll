/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
import { afterEach, expect, test } from "vitest";
import { DEFAULT_PRODUCT_NAME, productName } from "../src/client/product-name.js";

afterEach(() => { document.head.querySelectorAll('meta[name="application-name"]').forEach(meta => meta.remove()); });
const named = (content: string): void => {
  const meta = document.createElement("meta"); meta.name = "application-name"; meta.content = content; document.head.append(meta);
};

test("the page's application name is the product name", () => {
  named("Acme & <Suite>");
  expect(productName()).toBe("Acme & <Suite>");
});

test("a page without a name, or with an empty one, keeps the built-in name", () => {
  expect(productName()).toBe(DEFAULT_PRODUCT_NAME);
  named("   ");
  expect(productName()).toBe("ERPForAll");
});
