/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
import { expect, test } from "vitest";
import { registerAddons, type AddonRegistration } from "../../../src/index.js";
import { RegistryHub } from "../../../src/runtime/registry.js";
import { createTestScope, withTestScope } from "../src/index.js";
import { extensionTestAddon } from "./fixtures/extension-test-addon.js";

class TestHeading {
  label(): string { return "Base"; }
}

/// (partial: test-addon template registers ordered extensions and scope restores their patches and registry)
test("extension test-addon template composes in registration order and leaves the next scope clean", async () => {
  const heading = new TestHeading();
  let hub: RegistryHub | undefined;

  await withTestScope(async scope => {
    hub = scope.registries;
    scope.manage(() => {
      scope.registries.define("extensionTemplate", (value): value is string => typeof value === "string");
    });
    const addons: AddonRegistration[] = [
      extensionTestAddon({ id: "first", label: "First", target: TestHeading.prototype }),
      extensionTestAddon({ id: "second", label: "Second", target: TestHeading.prototype }),
    ];
    scope.manage(() => { registerAddons(addons, { registry: scope.registries }); });

    expect(heading.label()).toBe("Base / First / Second");
    expect(scope.registries.category("extensionTemplate").getEntries()).toEqual([["first", "First"], ["second", "Second"]]);
  });

  expect(heading.label()).toBe("Base");
  expect(() => hub!.category("extensionTemplate")).toThrow('Registry category "extensionTemplate" is not defined');

  const next = createTestScope();
  try {
    expect(heading.label()).toBe("Base");
    expect(() => next.registries.category("extensionTemplate")).toThrow(
      'Registry category "extensionTemplate" is not defined',
    );
  } finally {
    await next.dispose();
  }
});

