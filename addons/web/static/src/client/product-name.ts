/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
// The product name the server puts in <meta name="application-name">: the installed brand addon's name
// (static/brand/brand.json, validated by the server) or the built-in one.

/** The product name when the page names none. */
export const DEFAULT_PRODUCT_NAME = "ERPForAll";

/** The product name of `root`, for logo text and labels. Plain text: set it as text or an attribute value. */
export function productName(root: Document = document): string {
  const value = root.querySelector('meta[name="application-name"]')?.getAttribute("content")?.trim();
  return value ? value : DEFAULT_PRODUCT_NAME;
}
