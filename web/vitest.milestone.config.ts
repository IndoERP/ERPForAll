/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
// Explicit milestone-end performance run. Ordinary CI never imports this configuration.
import { configDefaults, defineConfig } from "vitest/config";
import { milestoneInclude, sharedConfig } from "./vitest.config.ts";

export default defineConfig({
  ...sharedConfig,
  test: { ...sharedConfig.test, include: milestoneInclude, exclude: configDefaults.exclude },
});
