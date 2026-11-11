/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
/** Transport admission failures are distinct from Python expression exceptions. */
export class AstDecodeError extends Error {
  readonly kind = "InvalidAst";
  constructor(readonly path: string, readonly detail: string) {
    super(`InvalidAst at ${path}: ${detail}`);
    this.name = "AstDecodeError";
  }
}
export function fail(path: string, detail: string): never { throw new AstDecodeError(path, detail); }
/** Explicit application allocation sites, distinct from engine allocation counts. */
export interface AllocationCounter { sites: number; }
const IDENTIFIER = /^[A-Za-z_$\u0080-\uffff][A-Za-z0-9_$\u0080-\uffff]*$/u;
export function fieldPath(path: string, key: string, counter?: AllocationCounter): string {
  if (counter) counter.sites++;
  if (!IDENTIFIER.test(key) || key === "__proto__") {
    if (counter) counter.sites++;
    return `${path}[${JSON.stringify(key)}]`;
  }
  return `${path}.${key}`;
}
/** Host settings may only lower a positive, integral hard ceiling. */
export function lowerLimits<T extends Record<string, number>>(ceilings: T, requested: Partial<T>): T {
  const result = { ...ceilings };
  for (const key of Object.keys(requested)) {
    if (!Object.hasOwn(ceilings, key)) fail(fieldPath("$.limits", key), "unknown limit");
    const value = requested[key];
    const ceiling = ceilings[key]!;
    if (typeof value !== "number" || !Number.isSafeInteger(value) || value < 1 || value > ceiling) {
      fail(fieldPath("$.limits", key), `expected integer in 1..${ceiling}`);
    }
    result[key as keyof T] = value as T[keyof T];
  }
  return Object.freeze(result);
}
