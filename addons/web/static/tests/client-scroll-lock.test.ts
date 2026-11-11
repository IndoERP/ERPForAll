/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
import { readFileSync } from "node:fs";
import { resolve } from "node:path";
import { afterEach, expect, test } from "vitest";
import { lockDocumentScroll, scrollLockClass } from "../src/client/scroll-lock.js";

afterEach(() => document.documentElement.classList.remove(scrollLockClass));

const locked = (): boolean => document.documentElement.classList.contains(scrollLockClass);

test("scroll lock is held until the last holder releases, and each release counts once", () => {
  expect(locked()).toBe(false);
  const homeMenu = lockDocumentScroll();
  const drawer = lockDocumentScroll();
  expect(locked()).toBe(true);
  homeMenu();
  homeMenu();
  expect(locked()).toBe(true);
  drawer();
  expect(locked()).toBe(false);
  const again = lockDocumentScroll();
  expect(locked()).toBe(true);
  again();
  expect(locked()).toBe(false);
});

const clientCss = readFileSync(resolve(process.cwd(), "..", "addons/web/static/src/client/client.css"), "utf8");
const ruleBody = (selector: string): string => {
  const escaped = selector.replace(/[.*+?^${}()|[\]\\]/g, "\\$&");
  const match = new RegExp(`(?:^|\\})\\s*${escaped}\\s*\\{([^}]*)\\}`, "m").exec(clientCss);
  if (!match) throw new Error(`client.css has no rule for ${selector}`);
  return match[1]!;
};

test("covering layers stop the page under them from scrolling or showing through", () => {
  expect(ruleBody("html.erp-scroll-locked, html.erp-scroll-locked body")).toMatch(/overflow:\s*hidden/);
  expect(ruleBody("[data-client-content]")).toMatch(/isolation:\s*isolate/);
  expect(ruleBody("[data-client-content].home-menu-covered")).toMatch(/visibility:\s*hidden/);
  expect(ruleBody(".home-menu-overlay")).toMatch(/overscroll-behavior:\s*contain/);
  expect(ruleBody(".mobile-drawer-backdrop")).toMatch(/touch-action:\s*none/);
});
