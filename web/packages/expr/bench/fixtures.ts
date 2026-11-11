/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
// Shared by the isolated-worker measurement and its deterministic allocation-site ratchet.
const scalar = { schema: 1, nodes: [{ kind: "literal", value: { kind: "int", value: "9007199254740993" } }] };
const items = Array.from({ length: 256 }, (_, index) => ({ kind: "literal", value: { kind: "int", value: String(index) } }));
const wide = { schema: 1, nodes: [...items, { kind: "list", items: items.map((_, index) => index) }] };
const unicode = { schema: 1, nodes: [{ kind: "literal", value: { kind: "str", value: "é🙂α".repeat(512) } }] };
export const admissionFixtures = [
  { name: "scalar_i64", text: JSON.stringify(scalar) },
  { name: "wide_list_256", text: JSON.stringify(wide) },
  { name: "unicode_string", text: JSON.stringify(unicode) },
] as const;
