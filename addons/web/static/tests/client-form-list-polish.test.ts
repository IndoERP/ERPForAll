/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
// These assert the
// stylesheet contract that the live check measures on the dev/testing deployment.
import { readFileSync } from "node:fs";
import { resolve } from "node:path";
import { expect, test } from "vitest";

const clientCss = readFileSync(resolve(process.cwd(), "..", "addons/web/static/src/client/client.css"), "utf8");
const normal = (text: string): string => text.replace(/\s+/g, " ").trim();
/** Bodies of the top-level rules whose selector list names `selector` (later rules last). */
const rulesFor = (selector: string): string[] => {
  const wanted = normal(selector);
  const bodies: string[] = [];
  const css = clientCss.replace(/\/\*[\s\S]*?\*\//g, "");
  // Each "}" closes one rule; the text after the last "{" before it is the body, the text before that "{" (after any
  // enclosing at-rule's "{") is the selector list.
  for (const chunk of css.split("}")) {
    const open = chunk.lastIndexOf("{");
    if (open < 0) continue;
    const selectors = chunk.slice(0, open).split("{").pop()!.split(",").map(normal);
    if (selectors.includes(wanted)) bodies.push(chunk.slice(open + 1));
  }
  if (bodies.length === 0) throw new Error(`client.css has no top-level rule for ${selector}`);
  return bodies;
};
const last = (selector: string): string => rulesFor(selector).at(-1)!;

test("a read-only boolean toggle keeps a visible track, off and on", () => {
  expect(last(".field-readonly input[type=checkbox].boolean-toggle")).toMatch(/background:\s*var\(--input-line\)\s*!important/);
  expect(last(".field-readonly input[type=checkbox].boolean-toggle:checked")).toMatch(/background:\s*var\(--accent\)\s*!important/);
});

const quiet = ".record-form:not(.settings-form) .field-shell:not(.field-readonly)";
// Relation editors sit in a plain field label (no field-shell class).
const relation = ".record-form:not(.settings-form) .field-label:not(.field-readonly)";
const text = `${quiet} > input:not([type=checkbox]):not([type=radio]):not([type=color]):not([type=range]):not([type=file])`;

test("editable form fields use one bottom rule, with the accent on hover and focus", () => {
  for (const selector of [text, `${quiet} > select`, `${relation} .many2one-control`, `${relation} .many2many-tags .tag-list`, `${quiet} .field-currency-value > input`, `${quiet} .field-copy-clipboard > input`]) {
    const body = last(selector);
    expect(body).toMatch(/border:\s*1px solid transparent/);
    expect(body).toMatch(/border-bottom-color:\s*var\(--field-rule\)/);
    expect(body).toMatch(/background:\s*transparent/);
  }
  expect(last(`${text}:hover`)).toMatch(/border-bottom-color:\s*var\(--accent\)/);
  const focus = last(`${text}:focus`);
  expect(focus).toMatch(/box-shadow:\s*inset 0 -2px 0 var\(--accent\)/);
  // Opaque: the browser readability check reads the field's own background, and a translucent wash fails it.
  expect(focus).toMatch(/background:\s*var\(--surface\)/);
  expect(last(`${relation} .many2one-control:focus-within`)).toBe(focus);
  // The quiet rule has its own token in both themes; an invalid field keeps a danger rule.
  expect(last(":root")).toMatch(/--field-rule:\s*#[0-9a-f]{6}/i);
  expect(last(':root[data-theme="dark"]')).toMatch(/--field-rule:\s*#[0-9a-f]{6}/i);
  expect(last(".record-form:not(.settings-form) .field-invalid.field-shell > input")).toMatch(/border-bottom-color:\s*var\(--danger\)/);
  // Read-only fields and settings forms are outside the quiet rule's selector by construction.
  expect(quiet).toContain(":not(.field-readonly)");
  expect(quiet).toContain(":not(.settings-form)");
});

test("every form title uses one borderless treatment and labels are not bold", () => {
  const [title, phoneTitle] = rulesFor(".record-form .form-title-heading input:not([type=checkbox])");
  expect(phoneTitle).toMatch(/font-size:\s*23px\s*!important/);
  expect(title).toMatch(/font-size:\s*26px/);
  expect(title).toMatch(/border:\s*1px solid transparent\s*!important/);
  expect(title).toMatch(/border-bottom-color:\s*var\(--line\)\s*!important/);
  expect(last(".record-form .form-title-heading .field-label > span")).toBe(last(".record-form .form-title-field .field-label > span"));
  // Slice D: the caption is visually hidden (still read by assistive technology); the empty title shows it as placeholder.
  expect(last(".record-form .form-title-heading .field-label > span")).toMatch(/clip-path:\s*inset\(50%\)/);
  expect(last(".record-form .form-title-heading .field-label > span")).toMatch(/position:\s*absolute/);
  expect(last(".record-form:not(.settings-form) .field-label > span")).toMatch(/font-weight:\s*500/);
});

test("list rows are compact and breadcrumb links have no button frame", () => {
  expect(last(".list-table td button.link")).toMatch(/min-height:\s*24px/);
  expect(last(".list-table td")).toMatch(/padding-block:\s*6px/);
  expect(last(".list-table .many2many-tags .tag-list")).toMatch(/min-height:\s*0/);
  expect(last(".form-breadcrumb-path button")).toMatch(/border:\s*0/);
  expect(last(".many2one-buttons .relation-search-more")).toMatch(/border:\s*0/);
});

test("the default theme uses one top bar: navigation inside the header, the header button style kept off its buttons", () => {
  const [bar, phoneBar] = rulesFor(".app-bar:has(> .app-navigation)");
  expect(bar).toMatch(/min-height:\s*48px/);
  expect(phoneBar).toMatch(/padding-inline:\s*8px 12px/);
  const [navigation] = rulesFor(".app-bar > .app-navigation");
  expect(navigation).toMatch(/border:\s*0/);
  expect(navigation).toMatch(/background:\s*transparent/);
  expect(rulesFor(".app-bar button:not(.app-navigation *)").length).toBe(1);
  expect(() => rulesFor(".app-bar button")).toThrow();
});

test("form dialogs use one frame, dialog-sized titles and the primary action first", () => {
  const [sheet, phoneSheet] = rulesFor(".wizard-dialog > .content-sheet");
  expect(sheet).toMatch(/border:\s*0/);
  expect(sheet).toMatch(/border-radius:\s*0/);
  expect(phoneSheet).toMatch(/padding:\s*16px/);
  expect(last(".wizard-dialog .record-form .form-title-field input")).toMatch(/font-size:\s*20px\s*!important/);
  expect(last(".wizard-dialog .record-form .form-title-heading input:not([type=checkbox])")).toMatch(/font-size:\s*20px\s*!important/);
  expect(last(".wizard-footer .erp-button-primary")).toMatch(/order:\s*-1/);
  expect(last(".wizard-heading h2")).toMatch(/font-size:\s*18px/);
});

/** Nesting depth at which each top-level selector list starts, scanning the comment-free stylesheet. */
const depthOf = (selector: string): number[] => {
  const css = clientCss.replace(/\/\*[\s\S]*?\*\//g, ""), wanted = normal(selector), depths: number[] = [];
  let depth = 0, start = 0;
  for (let i = 0; i < css.length; i++) {
    if (css[i] === "{") {
      if (css.slice(start, i).split(",").map(normal).includes(wanted)) depths.push(depth);
      depth++; start = i + 1;
    } else if (css[i] === "}") { depth--; start = i + 1; } else if (css[i] === ";" && depth > 0) start = i + 1;
  }
  return depths;
};

// 2026-11-07: a merge left a phone @media block unclosed, so the dialog and list-width rules after it applied only
// below 768 px on the dev/testing deployment. The braces must balance and the desktop rules must be top level.
test("the stylesheet's braces balance and the desktop polish rules sit at the top level", () => {
  const css = clientCss.replace(/\/\*[\s\S]*?\*\//g, "");
  expect(css.split("{").length).toBe(css.split("}").length);
  for (const selector of [".wizard-footer .erp-button-primary", ".list-table.list-auto-layout", ".wizard-heading h2", ".settings-app-icon", ".app-bar > .app-navigation"]) {
    expect(depthOf(selector)[0], selector).toBe(0);
  }
});

test("desktop forms with a mouse hide the field rule at rest; touch and phone keep it", () => {
  const css = clientCss.replace(/\/\*[\s\S]*?\*\//g, "");
  const block = /@media \(hover: hover\) and \(pointer: fine\) and \(min-width: 769px\) \{([^{}]*\{[^{}]*\})*\s*\}/.exec(css)?.[0] ?? "";
  expect(block).toMatch(/\.record-form:not\(\.settings-form\) \{ --field-rule: transparent; \}/);
  expect(block).toMatch(/\.record-form:not\(\.settings-form\) \.form-group-grid \{ row-gap: 4px; \}/);
  // Hover and focus still draw the rule and the accent; an invalid field keeps its own danger rule.
  expect(last(`${text}:hover`)).toMatch(/border-bottom-color:\s*var\(--accent\)/);
  expect(last(".record-form:not(.settings-form) .field-invalid.field-shell > input")).toMatch(/border-bottom-color:\s*var\(--danger\)/);
});

test("the record picker lists plain rows, not one framed button per record", () => {
  const row = last(".select-create-list button");
  expect(row).toMatch(/border:\s*0/);
  expect(row).toMatch(/background:\s*transparent/);
  expect(row).toMatch(/text-align:\s*start/);
  expect(last(".select-create-list button:hover")).toMatch(/background:\s*var\(--hover\)/);
  expect(last(".select-create-list button:focus-visible")).toMatch(/outline:\s*2px solid var\(--accent\)/);
  expect(last(".select-create-card")).toMatch(/border:\s*0/);
  expect(last(".select-create-results")).toMatch(/max-height:/);
});

test("one2many cells keep one line like the main list: no letter breaks, tags on one row", () => {
  expect(last(".x2many-table .field-cell > .field-read-value")).toMatch(/white-space:\s*inherit/);
  expect(last(".x2many-table .field-cell > .field-read-value")).toMatch(/overflow-wrap:\s*inherit/);
  expect(last(".x2many-table .many2many-tags .tag-list")).toMatch(/flex-wrap:\s*nowrap/);
  expect(last(".x2many-table td")).toMatch(/text-overflow:\s*ellipsis/);
  expect(rulesFor(".x2many-table th").some(body => /white-space:\s*nowrap/.test(body))).toBe(true);
  expect(last(".x2many-scroll")).toMatch(/overflow-x:\s*auto/);
});

test("the content-sizing layout lets long values wrap, so a first draw never paints a table wider than its text", () => {
  expect(last(".list-table.list-auto-layout td button.link")).toMatch(/white-space:\s*normal/);
  expect(last(".list-table.list-auto-layout td")).toMatch(/overflow-wrap:\s*anywhere/);
});

test("the open home menu takes the app's menus and phone burger out of the top bar", () => {
  for (const selector of ["[data-client-navigation].home-menu-open .app-sections", "[data-client-navigation].home-menu-open .burger-btn"]) {
    expect(last(selector)).toMatch(/visibility:\s*hidden/);
  }
});
