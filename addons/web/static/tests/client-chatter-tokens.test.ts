/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
import { readFileSync } from "node:fs";
import { resolve } from "node:path";
import { expect, test } from "vitest";

const clientCss = readFileSync(resolve(process.cwd(), "..", "addons/web/static/src/client/client.css"), "utf8");
const threadViewCss = readFileSync(resolve(process.cwd(), "packages/mail-thread-view/src/thread-view.css"), "utf8");
const builtinTs = readFileSync(resolve(process.cwd(), "packages/mail-thread-view/src/builtin.ts"), "utf8");

const normal = (text: string): string => text.replace(/\s+/g, " ").trim();

const rulesIn = (stylesheet: string, selector: string): string[] => {
  const wanted = normal(selector);
  const bodies: string[] = [];
  const css = stylesheet.replace(/\/\*[\s\S]*?\*\//g, "");
  for (const chunk of css.split("}")) {
    const open = chunk.lastIndexOf("{");
    if (open < 0) continue;
    const selectors = chunk.slice(0, open).split("{").pop()!.split(",").map(normal);
    if (selectors.includes(wanted)) bodies.push(chunk.slice(open + 1));
  }
  if (bodies.length === 0) throw new Error(`Stylesheet has no top-level rule for ${selector}`);
  return bodies;
};

const lastIn = (stylesheet: string, selector: string): string => rulesIn(stylesheet, selector).at(-1)!;

/// (partial: chatter and thread view styles use design tokens)
test("chatter and mail attachment thumbnails use token radius instead of raw pixels", () => {
  expect(lastIn(clientCss, ".erp-mail-chatter-file-thumbnail")).toMatch(/border-radius:\s*var\(--radius-control\)/);
  expect(lastIn(clientCss, ".erp-mail-attachment-thumbnail")).toMatch(/border-radius:\s*var\(--radius-control\)/);
  expect(lastIn(threadViewCss, ".erp-mail-attachment-thumbnail")).toMatch(/border-radius:\s*var\(--radius-control\)/);
});

/// (partial: chatter badges and count chips use token radius)
test("chatter duplicate counts and badges use token radius", () => {
  expect(lastIn(clientCss, ".erp-mail-chatter-duplicate-count")).toMatch(/border-radius:\s*var\(--radius-control\)/);
  expect(lastIn(clientCss, ".erp-mail-badge")).toMatch(/border-radius:\s*var\(--radius-control\)/);
  expect(lastIn(threadViewCss, ".erp-mail-badge")).toMatch(/border-radius:\s*var\(--radius-control\)/);
});

/// (partial: mail followers popover uses design token shadow)
test("mail followers popover uses menu shadow token instead of raw rgb", () => {
  const popover = lastIn(clientCss, ".erp-mail-followers-popover");
  expect(popover).toMatch(/box-shadow:\s*var\(--shadow-menu\)/);
  expect(popover).not.toMatch(/rgb\(/);
});

/// (partial: mail thread tracking change renders SVG icon without emoji)
test("mail thread tracking change uses SVG icon without literal arrow glyph", () => {
  expect(builtinTs).not.toContain('" → "');
  expect(builtinTs).toMatch(/glyphNode\(.*"arrow-right"/);
});
