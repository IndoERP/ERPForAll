/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
/** Spreadsheet exports must retain text as text even when it starts with an operator. */
export function csvCell(value: unknown): string {
  const text = String(value ?? "");
  const literal = typeof value === "string" && (/^[\t\r\n]/u.test(text) || /^[\s\uFEFF]*[=+@\-]/u.test(text))
    ? `'${text}` : text;
  return `"${literal.replace(/"/g, '""')}"`;
}
