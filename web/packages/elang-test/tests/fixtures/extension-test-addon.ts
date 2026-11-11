/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
import type { AddonRegistration } from "../../../../src/runtime/addons.js";

declare module "../../../../src/runtime/registry.js" {
  interface Registries {
    extensionTemplate: string;
  }
}

interface PatchableHeading {
  label(): string;
}

export interface ExtensionTestAddonOptions {
  readonly id: string;
  readonly label: string;
  readonly target: PatchableHeading;
}

/** Test-only addon template: add one registry value and one reversible component patch. */
export function extensionTestAddon(options: ExtensionTestAddonOptions): AddonRegistration {
  return {
    id: options.id,
    register(context) {
      context.registry.category("extensionTemplate").add(options.id, options.label);
      context.patch(options.target, sup => ({
        label() { return `${sup.label.call(this)} / ${options.label}`; },
      }));
    },
  };
}
