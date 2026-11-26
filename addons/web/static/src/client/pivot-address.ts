/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
// Legacy paths contain scalar group values. Wrap text that cannot travel literally in a shell URL.
export function addressKey(key: unknown): unknown {
  if (typeof key !== "string") return key;
  const json = JSON.stringify(key);
  return json.includes("\\") || json.includes("//") || /[\u007f-\u009f]/u.test(key) ? { text: encodeURIComponent(key) } : key;
}

export function readAddressKey(key: unknown): unknown {
  if (key === null || typeof key !== "object" || Array.isArray(key)) return key;
  const fields = Object.entries(key);
  if (fields.length !== 1 || fields[0]?.[0] !== "text" || typeof fields[0][1] !== "string") return undefined;
  try {
    const decoded = decodeURIComponent(fields[0][1]);
    return encodeURIComponent(decoded) === fields[0][1] ? decoded : undefined;
  } catch { return undefined; }
}
