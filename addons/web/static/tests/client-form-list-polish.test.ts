/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
// These assert the
// stylesheet contract that the live check measures on the dev/testing deployment.
import { readFileSync } from "node:fs";
import { resolve } from "node:path";
import { expect, test } from "vitest";
import { lastRule, rulesOf } from "./support/stylesheet.js";

const clientCss = readFileSync(resolve(process.cwd(), "..", "addons/web/static/src/client/client.css"), "utf8");
const normal = (text: string): string => text.replace(/\s+/g, " ").trim();
/** Bodies of the top-level rules whose selector list names `selector` (later rules last). */
const rulesFor = (selector: string, required = true): string[] => {
  const bodies = rulesOf(clientCss, selector).map((r) => r.body);
  if (required && bodies.length === 0) throw new Error(`client.css has no top-level rule for ${selector}`);
  return bodies;
};
const last = (selector: string): string => lastRule(clientCss, selector, { media: "any" }).body;

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
    // No side or top border, so read-only and editable text start on one x; the bottom rule carries the field.
    expect(body).toMatch(/border:\s*0 solid transparent/);
    expect(body).toMatch(/border-bottom:\s*1px solid var\(--field-rule\)/);
    expect(body).toMatch(/background-color:\s*transparent/);
  }
  expect(last(`${text}:hover`)).toMatch(/border-bottom-color:\s*var\(--accent\)/);
  const focus = last(`${text}:focus`);
  expect(focus).toMatch(/box-shadow:\s*var\(--shadow-field-focus\)/);
  // Opaque: the browser readability check reads the field's own background, and a translucent wash fails it.
  expect(focus).toMatch(/background-color:\s*var\(--surface\)/);
  expect(last(`${relation} .many2one-control:focus-within`)).toBe(focus);
  // The quiet rule has its own token in both themes; an invalid field keeps a danger rule.
  expect(rulesFor(":root").join(" ")).toMatch(/--field-rule:\s*#[0-9a-f]{6}/i);
  expect(rulesFor(':root[data-theme="dark"]').join(" ")).toMatch(/--field-rule:\s*#[0-9a-f]{6}/i);
  expect(last(".record-form:not(.settings-form) .field-invalid.field-shell > input")).toMatch(/border-bottom-color:\s*var\(--danger\)/);
  // Read-only fields and settings forms are outside the quiet rule's selector by construction.
  expect(quiet).toContain(":not(.field-readonly)");
  expect(quiet).toContain(":not(.settings-form)");
});

test("every form title uses one borderless treatment and labels are not bold", () => {
  const [title, phoneTitle] = rulesFor(".record-form .form-title-heading input:not([type=checkbox])");
  expect(phoneTitle).toMatch(/font-size:\s*23px/);
  expect(title).toMatch(/font-size:\s*26px/);
  expect(title).toMatch(/border:\s*0 solid transparent/);
  expect(title).toMatch(/border-bottom:\s*1px solid var\(--line\)/);
  expect(last(".record-form .form-title-heading .field-label > span:first-child")).toBe(last(".record-form .form-title-field .field-label > span:first-child"));
  // Slice D: the caption is visually hidden (still read by assistive technology); the empty title shows it as placeholder.
  expect(last(".record-form .form-title-heading .field-label > span:first-child")).toMatch(/clip-path:\s*inset\(50%\)/);
  expect(last(".record-form .form-title-heading .field-label > span:first-child")).toMatch(/position:\s*absolute/);
  expect(last(".record-form:not(.settings-form) .field-label > span")).toMatch(/font-weight:\s*500/);
  // fieldShell puts the caption first; a readonly value and validation/help spans are siblings.
  // Hiding/styling every span made the tax's readonly name disappear in the real browser.
  for (const selector of [".record-form .form-title-heading .field-label > span", ".record-form .form-title-field .field-label > span"]) {
    expect(rulesFor(selector, false).join(" ")).not.toMatch(/clip-path|font-size:\s*11px|text-transform:\s*uppercase/);
  }

});

test("list rows are compact and breadcrumb links have no button frame", () => {
  expect(last(".list-table td button.link")).toMatch(/min-height:\s*24px/);
  expect(last(".list-table td")).toMatch(/height:\s*var\(--list-row-height\);\s*padding-block:\s*0/); // the row is one fixed height (36px) instead of 6px padding around a 24px link
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

// 2026-11-15: measuring wrapped values sized every column to its longest word, so names,
// relations and tags were cut on the dev/testing deployment. The measure now reads one-line widths; columns.ts caps them.
test("the content-sizing layout measures one-line values in a table as wide as its columns", () => {
  expect(rulesFor(".list-table.list-auto-layout td", false).length).toBe(0);
  expect(last(".list-table.list-auto-layout")).toMatch(/width:\s*max-content/);
  expect(last(".list-table .many2many-tags .tag-list")).toMatch(/flex-wrap:\s*nowrap/);
  expect(last(".list-table .list-number .sort-button")).toMatch(/justify-content:\s*safe flex-end/);
});

test("compact list headers and related names ellipsize on the element painting the text; Wrap includes column titles", () => {
  for (const selector of [".list-table .sort-label", ".list-table .many2one-value"]) {
    const compact = last(selector);
    expect(compact).toMatch(/min-width:\s*0/);
    expect(compact).toMatch(/text-overflow:\s*ellipsis/);
    expect(compact).toMatch(/overflow:\s*hidden/);
    expect(compact).toMatch(/white-space:\s*nowrap/);
  }
  expect(last(".list-wrap .list-table:not(.list-auto-layout) .sort-label")).toMatch(/white-space:\s*normal/);
});

test("the open home menu takes the app's menus and phone burger out of the top bar", () => {
  for (const selector of ["[data-client-navigation].home-menu-open .app-sections", "[data-client-navigation].home-menu-open .burger-btn"]) {
    expect(last(selector)).toMatch(/visibility:\s*hidden/);
  }
});

test("a read-only value in a form is text like an editable one, and the record's name is the title", () => {
  const read = last(".record-form:not(.settings-form) .field-label > .field-read-value");
  expect(rulesFor(".record-form:not(.settings-form) .field-label > .field-read-value")[0]).toMatch(/font-size:\s*14px;\s*font-weight:\s*400;\s*color:\s*var\(--text\)/);
  expect(read).toMatch(/align-self:\s*center/);
  expect(last(".record-form:not(.settings-form) .many2one-readonly")).toMatch(/min-height:\s*var\(--form-row-height\);\s*padding:\s*0/);
  expect(last(".record-form:not(.settings-form) .many2one-readonly > .many2one-open-btn")).toMatch(/color:\s*var\(--text\)/);
  // The helper splits selector lists on commas, so a selector with `:is(a, b)` is matched in the stylesheet text itself.
  expect(clientCss).toMatch(/\.record-form :is\(\.form-title-heading, \.form-title-field\) \.field-label > \.field-read-value \{[^}]*font-size:\s*26px;\s*font-weight:\s*600/);
  // Only the label of a required field is emphasised: the old rule made the read value of a required line cell bold.
  expect(rulesFor(".field-required > span", false)).toEqual([]);
  expect(last(".record-form:not(.settings-form) .field-label.field-readonly > .field-currency-value")).toMatch(/font-size:\s*14px/);
  // A dialog's title stays the dialog's size.
  expect(clientCss).toMatch(/\.wizard-dialog \.record-form :is\(\.form-title-heading, \.form-title-field\) \.field-label > \.field-read-value \{[^}]*font-size:\s*20px/);
  // Three amounts make a totals block; a debit and a credit side by side do not.
  expect(clientCss).toContain(".form-group-grid:has(> .field-monetary ~ .field-monetary ~ .field-monetary)");
  expect(clientCss).not.toMatch(/\.form-group-grid:has\(> \.field-monetary ~ \.field-monetary\):not/);
});

test("the open notebook tab carries its accent edge inside one hairline outline", () => {
  expect(last(".record-form:not(.settings-form) .form-tabs")).toMatch(/background:\s*transparent/);
  // Not `last`: the reduced-motion rule of the same selector comes after it.
  expect(rulesFor(".record-form:not(.settings-form) .form-tab").some(body => /border:\s*1px solid transparent;\s*border-bottom:\s*0/.test(body))).toBe(true);
  expect(last(".record-form:not(.settings-form) .form-tab[aria-selected=true]::before")).toMatch(/height:\s*2px;\s*background:\s*var\(--accent\)/);
});

test("a tags field and its input sit on the form's row height", () => {
  expect(last(".record-form:not(.settings-form) .field-label .many2many-tags .tag-list")).toMatch(/min-height:\s*var\(--form-row-height\)/);
  expect(last(".record-form:not(.settings-form) .field-label .many2many-tags .tag-list > input")).toMatch(/min-height:\s*0/);
});

// Form pass follow-ups (review of 2026-11-18): relation link cue, relation in a title, tag chips, hover in a line cell.
test("a read-only relation link, title relation, and tag chips in a line cell", () => {
  // 5a: focus-visible ring on the relation open button.
  expect(last(".record-form:not(.settings-form) .many2one-readonly > .many2one-open-btn:focus-visible")).toMatch(/outline:\s*2px solid var\(--focus\)/);
  // 5a: touch devices underline the relation link (inside @media (hover: none)).
  expect(rulesFor(".record-form:not(.settings-form) .many2one-readonly > button.many2one-open-btn").some(body => /text-decoration:\s*underline/.test(body))).toBe(true);
  // 5b: hover colour in a line cell.
  expect(last(".record-form:not(.settings-form) .x2many-table .many2one-readonly > .many2one-open-btn:hover")).toMatch(/color:\s*var\(--accent\)/);
  // 6: a read-only relation in the title takes the title's size (selector contains :is(), so match stylesheet text).
  expect(clientCss).toMatch(/\.record-form :is\(\.form-title-heading, \.form-title-field\) \.many2one-readonly \{[^}]*font-size:\s*26px/);
  expect(clientCss).toMatch(/max-width:\s*640px[^}]*\.record-form :is\(\.form-title-heading, \.form-title-field\) \.many2one-readonly \{[^}]*font-size:\s*23px/);
  // 7: tag chips in a line cell do not all shrink.
  expect(last(".x2many-table .field-readonly .many2many-tags .relation-chip")).toMatch(/flex:\s*0 0 auto/);
  expect(last(".x2many-table .field-readonly .many2many-tags .relation-chip:last-child")).toMatch(/flex:\s*0 1 auto/);
});

test("the Documents tags in a wizard dialog stay on one line", () => {
  expect(last(".wizard-dialog .many2many-tags .tag-list")).toMatch(/flex-wrap:\s*nowrap/);
});
