/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
// The addon client package is a member of the web/ pnpm workspace (
// §3.1): it resolves @erpforall/elang and its ABI through the workspace link, and its suites run in the
// same vitest run as the runtime's.

import { expect, test } from "vitest";
import * as abi from "@erpforall/elang/abi";
import { Component, mount, signal } from "@erpforall/elang";

test("the web addon resolves @erpforall/elang and @erpforall/elang/abi from the workspace", () => {
  expect(typeof Component).toBe("function");
  expect(typeof mount).toBe("function");
  expect(signal(1)()).toBe(1);
  expect(Object.keys(abi)).toHaveLength(17);
});
