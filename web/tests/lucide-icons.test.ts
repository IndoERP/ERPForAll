/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
import { readFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import { expect, test } from "vitest";
import { icon, type IconName } from "../../addons/web/static/src/client/icons.js";
import { iconNodes } from "../vendor/lucide/nodes.js";

test("every local icon preserves pinned SVG geometry with no active content or focus target", () => {
  const safe = new Set(["path", "rect", "circle", "ellipse", "line", "polygon", "polyline"]);
  for (const name of Object.keys(iconNodes) as IconName[]) {
    const raw = readFileSync(join(dirname(fileURLToPath(import.meta.url)), `../vendor/lucide/icons/${name}.svg`), "utf8");
    const original = new DOMParser().parseFromString(raw, "image/svg+xml").documentElement;
    const rendered = icon(name);
    const geometry = (svg: Element) => [...svg.children].map(node => [node.localName, Object.fromEntries([...node.attributes].map(a => [a.name, a.value]))]);
    expect(geometry(rendered)).toEqual(geometry(original));
    expect(rendered.getAttribute("aria-hidden")).toBe("true");
    expect(rendered.getAttribute("focusable")).toBe("false");
    expect(rendered.hasAttribute("tabindex")).toBe(false);
    expect(rendered.getAttribute("stroke")).toBe("currentColor");
    for (const node of rendered.children) {
      expect(safe.has(node.localName)).toBe(true);
      for (const attr of node.attributes) expect(/^(?:on|href|style)/i.test(attr.name)).toBe(false);
      // The only paint a shape may carry is the inherited colour (filled dots in tag and palette).
      if (node.hasAttribute("fill")) expect(node.getAttribute("fill")).toBe("currentColor");
    }
  }
});
