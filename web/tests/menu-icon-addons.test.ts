/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
import { readdirSync, readFileSync, statSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import { expect, test } from "vitest";
import { knownMenuIcon, menuIconImageUrl } from "../../addons/web/static/src/client/icons.js";

test("every lucide web_icon an addon ships names an icon this client ships", () => {
  // jsdom replaces the global URL, which node:url refuses; resolve from the file path instead.
  const root = join(dirname(fileURLToPath(import.meta.url)), "../..");
  const addons = join(root, "addons");
  const used: string[] = [];
  const walk = (dir: string): void => {
    for (const entry of readdirSync(dir)) {
      const path = join(dir, entry);
      if (statSync(path).isDirectory()) { if (entry !== "node_modules" && entry !== "static") walk(path); }
      else if (entry.endsWith(".xml")) {
        const xml = new DOMParser().parseFromString(readFileSync(path, "utf8"), "application/xml");
        expect(xml.querySelector("parsererror"), path).toBeNull();
        for (const menu of xml.querySelectorAll("menuitem[web_icon]")) used.push(menu.getAttribute("web_icon")!);
        for (const field of xml.querySelectorAll('record[model="ir.ui.menu"] > field[name="web_icon"]')) used.push(field.textContent!.trim());
      }
    }
  };
  walk(addons);
  expect(used).toContain("lucide:library");
  expect(used.filter(value => knownMenuIcon(value) === null && menuIconImageUrl(value) === null)).toEqual([]);
});
