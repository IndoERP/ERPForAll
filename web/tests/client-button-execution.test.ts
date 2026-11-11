/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
import { afterEach, expect, test } from "vitest";
import { lockButtonControls } from "../../addons/web/static/src/client/buttons.js";
afterEach(() => document.body.replaceChildren());

/// (partial: selected controls preserve prior disabled state across lock release)
test("pending execution restores only controls that were enabled and release is idempotent", () => {
  const enabled = document.createElement("button"), disabled = document.createElement("button");
  disabled.disabled = true; document.body.append(enabled, disabled);
  const release = lockButtonControls([enabled, disabled]);
  expect(enabled.disabled).toBe(true); expect(disabled.disabled).toBe(true);
  release(); expect(enabled.disabled).toBe(false); expect(disabled.disabled).toBe(true);
  enabled.disabled = true; release(); expect(enabled.disabled).toBe(true);
});
