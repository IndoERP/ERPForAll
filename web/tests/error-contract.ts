/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
import { expect } from "vitest";
import { UIError } from "../src/index.js";
export function uiError(fn: () => unknown, code: "value"|"usage", message: string): void {
  let error: unknown;
  try { fn(); } catch (caught) { error = caught; }
  expect(error).toBeInstanceOf(UIError);
  expect(error).toMatchObject({code,message});
}
