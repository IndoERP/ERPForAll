/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
import { describe, expect, test } from "vitest";
import { navOverflowCount } from "../src/client/navbar-overflow.js";

describe("navOverflowCount", () => {
  /// (partial: how many trailing entries move into More; the DOM part has no automated check yet)
  test(": entries that do not fit beside the app name and the More button move, from the end", () => {
    // the Settings app at 1288px as measured on the dev/testing server: app name 73, nine entries, 744 available
    const entries = [73, 61, 96, 95, 71, 110, 94, 134, 103];
    expect(navOverflowCount(73, entries, 2000, 70, 4)).toBe(0);
    expect(navOverflowCount(73, entries, 744, 70, 4)).toBe(3);
    // exact fit keeps everything and needs no More button
    const total = 73 + entries.reduce((sum, width) => sum + 4 + width, 0);
    expect(navOverflowCount(73, entries, total, 70, 4)).toBe(0);
    expect(navOverflowCount(73, entries, total - 1, 70, 4)).toBeGreaterThan(0);
    // nothing fits: every entry moves, the app name and More stay
    expect(navOverflowCount(73, entries, 100, 70, 4)).toBe(entries.length);
    expect(navOverflowCount(73, [], 10, 70, 4)).toBe(0);
  });
});
