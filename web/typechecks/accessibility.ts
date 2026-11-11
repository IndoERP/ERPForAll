/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
// Checked addon-guide example: host-supplied bindings are declared, never executed.
import { createFocusTrap, createRovingTabindex, LiveRegion, createTranslator, createEnv } from "@erpforall/elang";
import type { TranslationCatalogue } from "@erpforall/elang";
import type { Page } from "playwright";
import { auditPage } from "../testing/axe.js";
declare const dialogElement: HTMLElement;
declare const tablistElement: HTMLElement;
declare const appElement: HTMLElement;
declare const closeDialog: () => void;
declare const userLanguage: string;
declare const catalogue: TranslationCatalogue;
declare const displayName: string;
const trap = createFocusTrap(dialogElement, { onEscape: closeDialog });
const tabs = createRovingTabindex(tablistElement, {
  items: () => [...tablistElement.querySelectorAll<HTMLButtonElement>('[role="tab"]')],
  orientation: "horizontal",
});
tabs.refresh();
const announcements = new LiveRegion(appElement);
announcements.announce("Saved");
const { _t } = createTranslator({ language: userLanguage, catalogue });
const label: string | String = _t("Hello %s", displayName);
void label;
trap.dispose(); tabs.dispose(); announcements.destroy();
const env = createEnv(); env.services.add(LiveRegion);
// Validate structural helper compatibility with a real Playwright Page's types.
declare const page: Page;
void auditPage(page);

import { createHotkeyController, createHotkeyBoundary } from "@erpforall/elang";
declare const executeButton: HTMLButtonElement;
declare const operationPending: boolean;
declare const reportError: (error: unknown) => void;
const shortcuts = createHotkeyController(appElement, {
  bindings: () => [{kind: "dom", chord: "alt+x", target: executeButton}],
  blocked: () => operationPending, report: reportError,
});
const overlayKeys = createHotkeyBoundary(dialogElement, {onEscape: closeDialog, report: reportError});
shortcuts.refresh(); overlayKeys.dispose(); shortcuts.dispose();
