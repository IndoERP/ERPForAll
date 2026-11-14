/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
// Isolated compiled-asset browser rendering test of compiled assets against explicit HTTP protocol fixtures.
// This is NOT backend, authentication, persistence, deployment or whole-case HUMAN acceptance evidence.
import assert from "node:assert/strict";
import { waitForReadiness } from "./browser-readiness.mjs";
import { officeMeasurementExpression } from "./browser-office-measurements.mjs";
import { access, readFile, writeFile, mkdir, mkdtemp, rm } from "node:fs/promises";
import { constants } from "node:fs";
import { createServer } from "node:http";
import { createHash } from "node:crypto";
import { spawn } from "node:child_process";
import { once } from "node:events";
import { tmpdir, hostname } from "node:os";
import path from "node:path";
import { fileURLToPath } from "node:url";
const root = fileURLToPath(new URL("../../", import.meta.url));
const output = process.env.CLIENT_BROWSER_OUTPUT;
/// (partial: compiled fixture palettes; live served first-paint path is separate)
const theme = process.env.CLIENT_BROWSER_THEME ?? "light";
const officeBlue = process.env.CLIENT_BROWSER_OFFICE_BLUE === "1";
const shellMeasurements = [];
// Settings gets a "Users" menu and a "Technical" dropdown with two groups and enough rows to need a scroll (shell checks).
const shellFixtureMenus = () => {
  const rows = [];
  rows.push({ module: null, id: 15, parent_id: 11, children: [], label: "Users", icon: null, action_id: 21 });
  rows.push({ module: null, id: 16, parent_id: 11, children: [17, 30], label: "Technical", icon: null, action_id: null });
  rows.push({ module: null, id: 17, parent_id: 16, children: [18, 19, 20, 21, 22, 23, 24, 25, 26, 27], label: "Database structure", icon: null, action_id: null });
  rows.push({ module: null, id: 30, parent_id: 16, children: [31, 32, 33, 34, 35, 36, 37, 38, 39, 40, 41, 42, 43, 44, 45], label: "Automation and jobs", icon: null, action_id: null });
  for (let id = 18; id <= 27; id++) rows.push({ module: null, id, parent_id: 17, children: [], label: ["Models", "Fields", "Model relations", "Decimal accuracy", "Sequences", "Sequence date ranges", "External identifiers", "Views", "Menu items", "Actions"][id - 18], icon: null, action_id: 21 });
  for (let id = 31; id <= 45; id++) rows.push({ module: null, id, parent_id: 30, children: [], label: `Background job ${id - 30}`, icon: null, action_id: 21 });
  return rows;
};
const officeBaseline = process.env.CLIENT_BROWSER_OFFICE_BASELINE === "1";
if (officeBaseline && !officeBlue) throw new Error("Office baseline requires Office Blue.");
const visualFindings = [];
// Both appearances must satisfy the same component contracts.
const themeCheck = (condition, message) => assert.ok(condition, message);
// One dialog anatomy and one overlay scale, checked on the open `.choice-dialog` in either theme.
const assertDialogAnatomy = async evaluate => {
  const anatomy = await evaluate(`(()=>{const d=document.querySelector('.choice-dialog'),cs=getComputedStyle(d),body=getComputedStyle(d.querySelector('.message-dialog-body')),last=[...d.querySelectorAll('.message-dialog-footer button')].at(-1),probe=document.createElement('div');probe.style.boxShadow='var(--shadow-dialog)';document.body.append(probe);const shadow=getComputedStyle(probe).boxShadow;probe.remove();return {width:d.getBoundingClientRect().width,radius:cs.borderTopLeftRadius,shadow:cs.boxShadow,expectedShadow:shadow,padding:body.padding,close:d.querySelector('.message-dialog-header .message-dialog-close')?.getAttribute('aria-label'),lastClass:last.className,lastText:last.textContent.trim(),lastBackground:getComputedStyle(last).backgroundColor,accent:(()=>{const p=document.createElement('button');p.className='primary';document.body.append(p);const c=getComputedStyle(p).backgroundColor;p.remove();return c})(),footerAlign:getComputedStyle(d.querySelector('.message-dialog-footer')).justifyContent,buttonHeight:last.getBoundingClientRect().height}})()`);
  themeCheck(anatomy.width === 420, `confirm dialog is 420px wide: ${JSON.stringify(anatomy)}`);
  themeCheck(anatomy.radius === "10px", "dialog uses --radius-overlay");
  assert.equal(anatomy.shadow, anatomy.expectedShadow, "dialog uses --shadow-dialog");
  themeCheck(anatomy.padding === "20px", "dialog body uses --overlay-dialog-padding");
  assert.equal(anatomy.close, "Close", "dialog has a close button");
  themeCheck(anatomy.footerAlign === "flex-end", "footer actions are right-aligned");
  themeCheck(anatomy.buttonHeight === 32, `dialog buttons are 32px (${anatomy.buttonHeight})`);
  assert.ok(/danger/.test(anatomy.lastClass) && !/primary/.test(anatomy.lastClass) && anatomy.lastBackground !== anatomy.accent, `destructive confirm is the last action in the danger style: ${JSON.stringify(anatomy)}`);
};
const visualCheck = (condition, message) => {
  if (officeBaseline && !condition) visualFindings.push(message);
  else assert.ok(condition, message);
};
// Moli supports real narrow viewport layout, but explicitly rejects mobile device emulation.
const viewportOnly = process.env.CLIENT_BROWSER_VIEWPORT_ONLY === "1";
const listOnly = process.env.CLIENT_BROWSER_LIST_ONLY === "1";
const dialogProbe = process.env.CLIENT_BROWSER_DIALOG_PROBE === "1";
if (listOnly && !officeBlue) throw new Error("CLIENT_BROWSER_LIST_ONLY requires Office Blue.");
let officeChoice = "office-blue", officeInstalled = true, missingOfficeCss = false;
let readPause;
assert.ok(["light", "dark", "auto-light", "auto-dark"].includes(theme), "CLIENT_BROWSER_THEME must be light, dark, auto-light, or auto-dark");
if (!output) throw new Error("Set CLIENT_BROWSER_OUTPUT to the isolated evidence directory.");
await mkdir(path.dirname(path.resolve(output)), { recursive: true });
try { await mkdir(output); }
catch (error) {
  if (error.code === "EEXIST") throw new Error(`Evidence directory already exists: ${output}`, { cause: error });
  throw error;
}
const chromeBin = process.env.CHROME_BIN ?? (process.platform === "darwin" ? "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome" : "/usr/bin/google-chrome");
const remoteCdp = process.env.CLIENT_BROWSER_CDP;
if (!remoteCdp) {
  try { await access(chromeBin, constants.X_OK); }
  catch (error) { throw new Error(`Chrome executable is unavailable: ${chromeBin}`, { cause: error }); }
}
const manifest = JSON.parse(await readFile(path.join(root, "web/dist/human-ui/manifest.json"), "utf8"));
const assets = new Map();
for (const asset of manifest.assets) {
  assert.match(asset.path, /^client\.[0-9a-f]{64}\.(js|css)$/);
  const data = await readFile(path.join(root, "web/dist/human-ui", asset.path));
  assert.equal(createHash("sha256").update(data).digest("hex"), asset.sha256);
  assets.set("/erp/assets/" + asset.path, { data, type: asset.content_type });
}
const js = manifest.assets.find(asset => asset.name === "client.js"), css = manifest.assets.find(asset => asset.name === "client.css");
for (const name of ["logo.svg", "logo-dark.svg"]) {
  assets.set("/erp/brand/" + name, { data: await readFile(path.join(root, "crates/erp-server/brand/erpforall-" + name)), type: "image/svg+xml" });
}
if (officeBlue) assets.set("/theme_office_blue/static/src/theme.css", { data: await readFile(path.join(root, "addons/theme_office_blue/static/src/theme.css")), type: "text/css; charset=utf-8" });
const bootstrap = { wire_major: 1, registry_version: "fixture-r1", asset_version: "fixture-a1", identity: { uid: 2, name: "Rendering fixture user", internal: true }, companies: [{ id: 1, name: "Rendering fixture company" }, { id: 2, name: "Second fixture company" }], current_company_id: 1, allowed_company_ids: [1, 2], active_company_ids: [1], locale: { lang: "en_US", tz: "UTC", decimal_point: ".", thousands_sep: ",", grouping: [3, 0], date_format: "%m/%d/%Y", time_format: "%H:%M:%S" }, csrf: { logout: "fixture-not-a-real-token" }, renderers: [{ key: "web.list", schema_major: 1 }, { key: "web.form", schema_major: 1 }] };
const scalar = (type, label, required = false, extra = {}) => ({type, label, required, readonly:false, selection:null, relation:null, relation_field:null, ...extra});
const fields = {
 name:scalar("char","Title",true), body:scalar("text","Description"),
 line_count:scalar("integer","Lines",false,{readonly:true}),
 attachment_total:scalar("integer","Attachments",false,{readonly:true}),
 task_total:scalar("integer","Open follow-up tasks",false,{readonly:true}),
 priority:scalar("selection","Priority",false,{selection:[["draft","Draft"],["review","Review"],["normal","Normal"],["high","High"],["done","Done"],["cancelled","Cancelled"]]}),
 owner_id:scalar("many2one","Owner",true,{relation:"res.users"}),
 company_id:scalar("many2one","Company",true,{relation:"res.company"}),
 active:scalar("boolean","Active"), internal_memo:scalar("text","Internal Memo"),
 scheduled_date:scalar("date","Scheduled date",true), reference:scalar("char","Reference",false,{readonly:true}),
 create_uid:scalar("many2one","Created by",false,{readonly:true,relation:"res.users"}),
 create_date:scalar("datetime","Created on",false,{readonly:true}),
 write_uid:scalar("many2one","Last edited by",false,{readonly:true,relation:"res.users"}),
 write_date:scalar("datetime","Last edited on",false,{readonly:true}),
 format:scalar("selection","Format",false,{selection:[["hardcover","Hardcover"],["paperback","Paperback"],["ebook","E-book"]]}),
 isbn:scalar("char","ISBN"), pub_year:scalar("integer","Publication year"), print_run:scalar("integer","Print run"),
 genre_ids:scalar("many2many","Genres",false,{relation:"res.genre"}),
 favorite:scalar("boolean","Favorite")
};
const formField = name => ({kind:"field", force_save: false,field:{model:"acceptance_notes.note",name},widget:name==="name"?"acceptance_notes.note_title":fields[name].type,options:{},modifiers:{readonly:null,required:null,invisible:null},label:fields[name].label});
const formBalanceGeometry = `(() => {
  const q = s => document.querySelector(s), R = e => { if (!e) return null; const b = e.getBoundingClientRect(); return { l: +b.left.toFixed(1), r: +b.right.toFixed(1), t: +b.top.toFixed(1), b: +b.bottom.toFixed(1), h: +b.height.toFixed(1) }; };
  const nw = R(q('.control-panel-form [data-control-actions] > button')), act = R(q('.form-header-actions .form-header-btn')), sheet = R(q('.record-form-viewport > .content-sheet'));
  const bar = R(q('.form-statusbar')), kinds = R(q('.erp-mail-chatter-header .erp-mail-kinds')), pager = R(q('.form-pager .pager-btn')), strip = R(q('.form-button-box'));
  const rowHeights = { control: [nw, pager, strip].filter(Boolean).map(b => b.h), action: [act, ...[...document.querySelectorAll('.form-statusbar .statusbar-step')].map(R), ...[...document.querySelectorAll('.erp-mail-chatter-header :is(.erp-mail-kinds, .erp-mail-activities-button, .erp-mail-chatter-attachment-btn, .erp-mail-followers-button)')].map(R)].filter(Boolean).map(b => b.h) };
  const controlBottom = Math.max(...[nw, pager, strip].filter(Boolean).map(b => b.b)), actionBottom = Math.max(...[act, bar].filter(Boolean).map(b => b.b));
  const actionTop = Math.min(...[act, bar].filter(Boolean).map(b => b.t));
  return { width: innerWidth, newLeft: nw && nw.l, actionLeft: act && act.l, sheetLeft: sheet && sheet.l, barRight: bar && bar.r, sheetRight: sheet && sheet.r, sheetTop: sheet && sheet.t, actionTop, chatterButtonTop: kinds && kinds.t, rowHeights, gapControlToAction: +(actionTop - controlBottom).toFixed(1), gapActionToSheet: sheet ? +(sheet.t - actionBottom).toFixed(1) : null, strip, stripClipped: [...document.querySelectorAll('.form-record-smart .form-stat-content > *, .form-record-smart .form-stat-field > *')].some(e => e.scrollWidth > e.clientWidth + 1), overflow: document.documentElement.scrollWidth > innerWidth };
})()`;
// where the chatter column's content sits against the column, the sheet and the action row (every number is a getBoundingClientRect).
const chatterColumnGeometry = `(() => {
  const q = s => document.querySelector(s), all = s => [...document.querySelectorAll(s)];
  const R = e => { if (!e) return null; const b = e.getBoundingClientRect(); return { l: +b.left.toFixed(1), r: +b.right.toFixed(1), t: +b.top.toFixed(1), b: +b.bottom.toFixed(1), w: +b.width.toFixed(1), h: +b.height.toFixed(1) }; };
  const col = R(q('.record-form-chatter')), sheet = R(q('.record-form-viewport > .content-sheet')), act = R(q('.form-header-actions')), bar = R(q('.form-statusbar'));
  const header = R(q('.erp-mail-chatter-header')), btns = all('.erp-mail-chatter-header :is(.erp-mail-kind, .erp-mail-activities-button, .erp-mail-chatter-attachment-btn, .erp-mail-followers-button)').map(R);
  const composer = R(q('.erp-chatter-composer')), avatar = R(q('.erp-chatter-composer > .erp-mail-avatar')), input = R(q('.erp-chatter-composer .erp-mail-draft'));
  const thread = R(q('.erp-mail-chatter .erp-mail-thread')), day = q('.erp-mail-chatter .erp-mail-day'), msgs = all('.erp-mail-chatter article.erp-mail-message');
  const rows = msgs.map(a => R(a.querySelector('.erp-mail-message-row')));
  const rowAvatars = msgs.map(a => R(a.querySelector('.erp-mail-message-row > .erp-mail-avatar'))).filter(Boolean);
  const first = msgs[0], author = first && first.querySelector('.erp-mail-author'), time = first && first.querySelector('.erp-mail-time');
  const ruleOf = (e, w) => { const c = getComputedStyle(e, w); return c.content !== 'none' && c.height === '1px'; };
  const sorted = [...btns].sort((a, b) => a.l - b.l);
  const kinds = all('.erp-mail-chatter-header .erp-mail-kind').map(R);
  return {
    width: innerWidth, overflow: document.documentElement.scrollWidth > innerWidth,
    column: col, sheet, headerTop: header && header.t, headerBottom: header && header.b, actionTop: act ? Math.min(act.t, bar ? bar.t : act.t) : null,
    buttons: btns, buttonHeights: [...new Set(btns.map(b => b.h))], buttonOverlap: sorted.some((b, i) => i && b.l < sorted[i - 1].r - 0.5),
    firstButtonLeft: sorted[0] && sorted[0].l, lastButtonRight: sorted.length ? Math.max(...sorted.map(b => b.r)) : null,
    kindGap: kinds.length > 1 ? +(kinds[1].l - kinds[0].r).toFixed(1) : null,
    composer, avatar, input, avatarToInput: avatar && input ? +(input.l - avatar.r).toFixed(1) : null,
    headerToComposer: header && composer ? +(composer.t - header.b).toFixed(1) : null,
    thread, composerToThread: composer && thread ? +(thread.t - composer.b).toFixed(1) : null,
    day: R(day), dayRules: !!day && ruleOf(day, '::before') && ruleOf(day, '::after'), dayText: day && day.textContent,
    rowAvatars, avatarSizes: [...new Set(rowAvatars.map(a => a.w + 'x' + a.h).concat(avatar ? [avatar.w + 'x' + avatar.h] : []))],
    firstRowLeft: rows[0] && rows[0].l, firstRowRight: rows[0] && rows[0].r,
    messageGaps: rows.slice(1).map((r, i) => +(r.t - rows[i].b).toFixed(1)),
    nameTimeSameLine: !!author && !!time && Math.abs((R(author).t + R(author).b) / 2 - (R(time).t + R(time).b) / 2) <= 5,
    authorWeight: author && getComputedStyle(author).fontWeight,
    horizontalScroll: [q('.record-form-chatter'), q('.erp-mail-chatter'), q('.erp-mail-thread')].filter(Boolean).some(e => e.scrollWidth > e.clientWidth + 1),
  };
})()`;
// states the unit tests cannot see because they are CSS. Run with the composer focused and holding text.
const chatterStateProbe = `(() => {
  const q = s => document.querySelector(s), R = e => { const b = e.getBoundingClientRect(); return { l: +b.left.toFixed(1), r: +b.right.toFixed(1), t: +b.top.toFixed(1), b: +b.bottom.toFixed(1) }; };
  const ta = q('.erp-chatter-composer textarea');
  ta.focus(); ta.value = 'draft'; ta.dispatchEvent(new Event('input', { bubbles: true }));
  const wrap = q('.erp-chatter-composer'), visible = e => e.getClientRects().length > 0;
  const send = q('.erp-chatter-composer .erp-mail-send'), hint = q('.erp-chatter-composer .erp-mail-hint'), clip = q('.erp-chatter-composer .erp-mail-composer-action');
  const out = { collapsed: wrap.classList.contains('is-collapsed'), input: R(ta), send: send && visible(send) ? R(send) : null, hint: hint && visible(hint) ? R(hint) : null, clip: clip && visible(clip) ? R(clip) : null };
  // Tab order = document order of the focusable controls (no tabindex is set); list them with their boxes.
  out.tabOrder = [...wrap.querySelectorAll('textarea, button, input:not([hidden])')].filter(visible).map(e => ({ cls: e.className.split(' ')[0], ...R(e) }));
  ta.value = ''; ta.dispatchEvent(new Event('input', { bubbles: true })); ta.blur();
  const tr = q('.erp-mail-tracking-change');
  if (tr) { const part = c => { const e = tr.querySelector(c); return e ? R(e) : null; }; out.tracking = { old: part('.erp-mail-tracking-old'), arrow: part('.erp-mail-tracking-arrow'), next: part('.erp-mail-tracking-new'), field: part('.erp-mail-tracking-field'), fieldBefore: getComputedStyle(tr.querySelector('.erp-mail-tracking-field'), '::before').content, newWeight: getComputedStyle(tr.querySelector('.erp-mail-tracking-new')).fontWeight }; }
  const bubbleOf = a => { const b = a && a.querySelector('.erp-mail-message-content > .erp-mail-body'); if (!b) return null; const c = getComputedStyle(b); return { border: c.borderTopWidth, radius: parseFloat(c.borderTopLeftRadius), bg: c.backgroundColor, w: R(b).r - R(b).l, mainW: R(a.querySelector('.erp-mail-message-main')).r - R(a.querySelector('.erp-mail-message-main')).l }; };
  out.bubble = bubbleOf(q('article.is-bubble:not(.is-note)'));
  out.noteBubble = bubbleOf(q('article.is-note'));
  out.rowBubble = bubbleOf(q('article.is-row'));
  return out;
})()`;
const chatterStateChecks = g => {
  const near = (a, b, d = 1) => Math.abs(a - b) <= d, ok = {};
  ok.composerExpandedWhenTyped = g.collapsed === false;
  ok.sendOnLeftUnderInput = !!g.send && near(g.send.l, g.input.l) && g.send.t >= g.input.b - 0.5;
  ok.hintBetweenSendAndClip = !!g.hint && !!g.clip && g.hint.l >= g.send.r - 0.5 && g.hint.r <= g.clip.l + 0.5;
  ok.clipAtRightEdge = !!g.clip && near(g.clip.r, g.input.r, 2);
  ok.rowOnOneLine = !!g.send && !!g.clip && near((g.send.t + g.send.b) / 2, (g.clip.t + g.clip.b) / 2, 4);
  // Tab order equals visual order: the textarea, then Send, then the paperclip, each to the right of or below the one before.
  ok.tabOrderIsVisualOrder = g.tabOrder.length >= 3 && g.tabOrder.every((e, i) => !i || e.t > g.tabOrder[i - 1].b - 1 || e.l >= g.tabOrder[i - 1].r - 1);
  ok.tabOrderNames = g.tabOrder.slice(0, 3).map(e => e.cls).join(',') === 'erp-mail-draft,erp-mail-send,erp-mail-composer-action';
  ok.trackingReadsOldArrowNewField = !!g.tracking && g.tracking.old.l < g.tracking.arrow.l && g.tracking.arrow.l < g.tracking.next.l && g.tracking.next.l < g.tracking.field.l && near(g.tracking.old.t, g.tracking.field.t, 6);
  ok.trackingNewIsBold = !!g.tracking && Number(g.tracking.newWeight) >= 600;
  ok.commentIsABubble = !!g.bubble && g.bubble.border === '1px' && g.bubble.radius > 0 && g.bubble.w <= g.bubble.mainW + 0.5;
  ok.noteAndSystemRowHaveNoBubble = (!g.noteBubble || g.noteBubble.border === '0px') && (!g.rowBubble || g.rowBubble.border === '0px');
  return ok;
};
// The checks of the chatter column, as booleans (the Office Blue run records them, the default theme asserts them).
const chatterColumnChecks = (g, width, theme = "default") => {
  const c = g.column, near = (a, b, d = 1) => a !== null && a !== undefined && b !== null && b !== undefined && Math.abs(a - b) <= d;
  return {
    noOverflow: g.overflow === false && g.horizontalScroll === false,
    buttonRowOneHeight: g.buttonHeights.length === 1 && g.buttonHeights[0] === 32,
    buttonsDoNotOverlap: g.buttonOverlap === false,
    buttonRowSpansColumn: near(g.firstButtonLeft, c.l) && near(g.lastButtonRight, c.r),
    // The record form has one layout in every theme (the officeForm branch is gone): the button row shares the action row's top.
    topEqualsSheetOrActionRow: width <= 1120 || near(g.headerTop, g.actionTop ?? g.sheet.t),
    composerAvatarOnLeftEdge: near(g.avatar && g.avatar.l, c.l) && g.avatarSizes.length === 1 && g.avatarSizes[0] === '36x36',
    composerInputFillsColumn: near(g.input && g.input.r, c.r) && near(g.avatarToInput, 8),
    threadSpansColumn: near(g.thread && g.thread.l, c.l) && near(g.thread && g.thread.r, c.r),
    dayIsARuleWithDate: g.dayRules === true && near(g.day && g.day.l, c.l) && near(g.day && g.day.r, c.r),
    rowsSpanColumn: near(g.firstRowLeft, c.l) && near(g.firstRowRight, c.r),
    nameAndTimeOnOneLine: g.nameTimeSameLine === true,
    oneGapPerKind: g.kindGap === null || near(g.kindGap, 8, 0.5),
  };
};
// The form action area (one control height, one centre line per row, edges on the sheet and the chatter column): measured and asserted in both themes.
const formActionProbe = `(() => {
  const r = s => { const e = document.querySelector(s); return e ? e.getBoundingClientRect() : null; };
  const all = s => [...document.querySelectorAll(s)].map(e => e.getBoundingClientRect());
  const mid = b => b.top + b.height / 2;
  const buttons = all('.control-panel-form [data-control-actions] > button, .control-panel-form [data-control-actions] .form-actions-dropdown > button');
  const steps = all('.form-statusbar .statusbar-step'), lend = r('.form-header-actions .form-header-btn');
  const prev = r('.form-pager .pager-prev'), next = r('.form-pager .pager-next'), sw = r('.view-switcher'), path = r('.form-breadcrumb-path, .breadcrumbs');
  const sheet = r('.record-form-viewport > .content-sheet'), chatter = r('.record-form-viewport > .record-form-chatter'), bar = r('.form-statusbar');
  return { heights: [...buttons, prev, next, sw, lend, ...steps].map(b => Math.round(b.height * 10) / 10),
    row: [...buttons, prev, next, sw, path].map(b => Math.round(mid(b) * 10) / 10), band: [lend, ...steps].map(b => Math.round(mid(b) * 10) / 10),
    leftGap: buttons.length > 1 ? Math.round((buttons[1].left - buttons[0].right) * 10) / 10 : null, stepGaps: steps.slice(1).map((b, i) => Math.round((b.left - steps[i].right) * 10) / 10), stepCount: steps.length,
    trackRight: r('.statusbar-track').right, headerRight: r('.form-header').right,
    currentBg: getComputedStyle(document.querySelector('.statusbar-step.is-current')).backgroundColor, accentBg: (() => { const p = document.createElement('i'); p.style.background = 'var(--accent)'; document.body.append(p); const c = getComputedStyle(p).backgroundColor; p.remove(); return c; })(),
    states: [...document.querySelectorAll('.form-statusbar .statusbar-step')].map(e => e.className.replace('statusbar-step', '').trim() || 'upcoming'),
    left: { new: buttons[0].left, lend: lend.left, sheet: sheet.left }, right: { switcher: sw.right, statusbar: bar.right, chatter: chatter.right, sheet: sheet.right },
    switcher: { top: sw.top, right: sw.right }, panel: r('.control-panel-form').height,
    archiveInHeader: [...document.querySelectorAll('.form-header-actions button')].some(b => /archive/i.test(b.textContent)),
    menuInBreadcrumb: !!document.querySelector('.breadcrumbs .form-actions-dropdown .record-menu-btn') && !document.querySelector('[data-control-actions] .form-actions-dropdown'), breadcrumbLines: (() => { const p = document.querySelector('.breadcrumb-previous'), c = document.querySelector('.form-breadcrumb-record'), m = document.querySelector('.record-menu-btn'); if (!p || !c || !m) return null; const a = p.getBoundingClientRect(), b = c.getBoundingClientRect(), d = m.getBoundingClientRect(); return { previousBottom: a.bottom, currentTop: b.top, currentRight: b.right, menuLeft: d.left, menuMid: d.top + d.height / 2, currentMid: b.top + b.height / 2 }; })() };
})()`;
const assertFormActionGeometry = (fa, label = "default theme") => {
  assert.ok(fa.heights.every(h => Math.abs(h - 32) <= 0.5), `${label}: form action controls share one 32px height: ${JSON.stringify(fa)}`);
  assert.ok(Math.max(...fa.row) - Math.min(...fa.row) <= 1, `${label}: control panel row shares one centre line: ${JSON.stringify(fa)}`);
  assert.ok(Math.max(...fa.band) - Math.min(...fa.band) <= 1, `${label}: header band shares one centre line: ${JSON.stringify(fa)}`);
  assert.equal(fa.menuInBreadcrumb, true, `${label}: the record menu sits beside the record name, not as a button left of the breadcrumb: ${JSON.stringify(fa)}`);
  assert.ok(fa.stepCount >= 4, `${label}: the fixture statusbar shows at least four states: ${JSON.stringify(fa)}`);
  assert.ok(fa.stepGaps.every(gap => gap <= 0), `${label}: status steps touch with no gap (chevron joints overlap): ${JSON.stringify(fa)}`);
  assert.deepEqual(fa.states, ["is-passed", "is-passed", "is-current", "upcoming", "upcoming"], `${label}: the current step sits in the middle, earlier steps are passed and the folded state is not on the track: ${JSON.stringify(fa)}`);
  assert.equal(fa.currentBg, fa.accentBg, `${label}: the current step is filled with the accent token: ${JSON.stringify(fa)}`);
  assert.ok(Math.abs(fa.trackRight - fa.headerRight) <= 1 || Math.abs(fa.trackRight - fa.right.chatter) <= 1, `${label}: the track ends on the header's right edge: ${JSON.stringify(fa)}`);
  assert.ok(Math.abs(fa.left.new - fa.left.sheet) <= 1 && Math.abs(fa.left.lend - fa.left.sheet) <= 1, `${label}: left edges of both rows sit on the sheet edge: ${JSON.stringify(fa)}`);
  assert.ok(Math.abs(fa.right.switcher - fa.right.chatter) <= 1 && Math.abs(fa.right.statusbar - fa.right.sheet) <= 1, `${label}: the pager ends on the right column edge and the statusbar on the sheet's right edge: ${JSON.stringify(fa)}`);
  assert.equal(fa.archiveInHeader, false, `${label}: Archive is not a header button`);
  assert.ok(fa.breadcrumbLines && fa.breadcrumbLines.currentTop >= fa.breadcrumbLines.previousBottom - 1 && fa.breadcrumbLines.menuLeft >= fa.breadcrumbLines.currentRight - 1 && Math.abs(fa.breadcrumbLines.menuMid - fa.breadcrumbLines.currentMid) <= 2, `${label}: the breadcrumb is two lines (parent above, record below) with the menu beside the name: ${JSON.stringify(fa.breadcrumbLines)}`);
};
// Office Blue on a phone: the drawer controls and the bar share one row (equal top, the bar-height token), and the page fills the window.
const assertPhoneBarRow = async (evaluate, label, width) => {
  await evaluate('scrollTo(0, 0); new Promise(resolve => requestAnimationFrame(() => requestAnimationFrame(resolve)))');
  const g = await evaluate(`(() => { const r = s => document.querySelector(s).getBoundingClientRect(), p = document.createElement('div'); p.style.height = 'var(--app-bar-height)'; document.body.append(p); const token = p.getBoundingClientRect().height; p.remove(); const nav = r('.app-navigation'), bar = r('.app-bar'), main = r('#erp-content'); return { innerWidth, innerHeight, token, nav: { top: nav.top, height: nav.height, left: nav.left, right: nav.right }, bar: { top: bar.top, height: bar.height, left: bar.left }, mainBottom: main.bottom, mainTop: main.top }; })()`);
  const note = `${label} at ${width}: ${JSON.stringify(g)}`;
  assert.ok(Math.abs(g.nav.top - g.bar.top) <= 0.5, `the drawer controls and the bar share one top, ${note}`);
  assert.ok(Math.abs(g.nav.height - g.token) <= 0.5 && Math.abs(g.bar.height - g.token) <= 0.5, `both are one --app-bar-height tall, ${note}`);
  assert.ok(g.nav.right <= g.bar.left + 0.5, `the bar starts after the drawer controls, ${note}`);
  assert.ok(g.mainTop >= g.bar.top + g.token - 0.5, `the page starts under the bar, ${note}`);
  return g;
};
// The same balance checks as booleans, so a theme that does not meet them yet can be recorded without failing the run.
// Missing geometry fails explicitly, before arithmetic can coerce null into a valid coordinate.
const formBalanceChecks = (g, width) => ({
  completeGeometry: [g.newLeft, g.actionLeft, g.sheetLeft, g.barRight, g.sheetRight, g.sheetTop, g.actionTop, g.chatterButtonTop, g.gapControlToAction, g.gapActionToSheet].every(Number.isFinite) && g.rowHeights.control.length === 3 && g.rowHeights.action.length >= 3 && [...g.rowHeights.control, ...g.rowHeights.action].every(h => Number.isFinite(h) && h > 0),
  leftEdgesShared: Math.abs(g.newLeft - g.actionLeft) <= 1 && Math.abs(g.actionLeft - g.sheetLeft) <= 1,
  statusbarEndsOnSheetEdge: Math.abs(g.barRight - g.sheetRight) <= 1,
  controlRowOneHeight: Math.max(...g.rowHeights.control) - Math.min(...g.rowHeights.control) <= 0.5,
  actionRowOneHeight: Math.max(...g.rowHeights.action) - Math.min(...g.rowHeights.action) <= 0.5,
  chatterArrangement: width > 1120 ? Math.abs(g.actionTop - g.chatterButtonTop) <= 1 : g.chatterButtonTop > g.sheetTop,
  unclippedStrip: !g.stripClipped,
  noOverflow: !g.overflow,
  rowGapsAlike: Math.abs(g.gapControlToAction - g.gapActionToSheet) <= 4,
  smartStripAboveSheet: !!g.strip && g.sheetTop !== undefined && g.strip.b <= g.sheetTop,
});
const copyCalls = [];
const catalogueField = (name, widget, options = {}) => ({...formField(name), widget, options});
const formNodes=[];
function arena(node) {
 const index=formNodes.length;formNodes.push({});
 const {children,...props}=node;
 formNodes[index]={...props,...(children?{children:children.map(arena)}:{})};return index;
}
const lendButton = {kind:"button",name:"lend",button_type:"object",label:"Lend",variant:"primary",confirm:"Lend this book?",modifiers:{readonly:null,required:null,invisible:{schema:1,nodes:[
 {kind:"name",name:"priority"},{kind:"literal",value:{kind:"str",value:"normal"}},{kind:"compare",left:0,ops:["not_eq"],comparators:[1]}]}}};
arena({kind:"form",record_mode:"record",children:[{kind:"header",children:[lendButton,{...formField("priority"),widget:"statusbar",options:{visible:null,folded:["cancelled"]}}]},{kind:"sheet",children:[
 {kind:"button_box",children:[{kind:"stat_button",name:"inspect_lines",button_type:"object",label:"Lines",variant:"secondary",confirm:null,modifiers:{readonly:null,required:null,invisible:null},children:[{...formField("line_count"),widget:"statinfo"}]},
  {kind:"stat_button",name:"inspect_attachments",button_type:"object",label:"Attachments",variant:"secondary",confirm:null,modifiers:{readonly:null,required:null,invisible:null},children:[{...formField("attachment_total"),widget:"statinfo"}]},
  {kind:"stat_button",name:"inspect_tasks",button_type:"object",label:"Open follow-up tasks",variant:"secondary",confirm:null,modifiers:{readonly:null,required:null,invisible:null},children:[{...formField("task_total"),widget:"statinfo"}]}]},
 {kind:"title",children:[catalogueField("favorite","boolean_favorite"),{kind:"heading",level:1,children:[formField("name")]}]}, {kind:"group",label:"",columns:4,colspan:2,children:[
  {kind:"group",label:"Details",columns:2,colspan:2,children:[formField("priority"),formField("owner_id"),formField("scheduled_date"),formField("reference")]},
  {kind:"group",label:"Organization",columns:2,colspan:2,children:[formField("company_id"),formField("active")]}
 ]}, {kind:"notebook",children:[
  {kind:"page",name:"description",label:"Description",children:[formField("body")]},
  {kind:"page",name:"internal",label:"Internal Memo",children:[formField("internal_memo")]},
  {kind:"page",name:"catalogue",label:"Catalogue",children:[{kind:"group",label:"",columns:2,colspan:2,children:[catalogueField("format","radio"),catalogueField("isbn","CopyClipboardChar"),catalogueField("pub_year","integer",{type:"number"}),catalogueField("print_run","integer"),catalogueField("genre_ids","many2many")]}]}
 ]}
]},{kind:"aside",children:[{kind:"group",label:"Record information",columns:2,colspan:2,children:[
 formField("create_uid"),formField("create_date"),formField("write_uid"),formField("write_date")
]}]},{kind:"chatter",reload_on_post:true,reload_on_attachment:false,reload_on_activity:false,reload_on_follower:false,open_attachments:false,has_activities:true}]});
const description = {wire_major:1,registry_version:"fixture-r1",description_version:"fixture-d1",model:"acceptance_notes.note",fields:{"acceptance_notes.note":fields},notices:[],views:[
 {identity:{kind:"stored",id:1},type:"list",schema_major:1,renderer:"web.list",root:0,nodes:[{kind:"list",children:[1,2,3],page_limit:80,...(officeBlue ? {search_panel:{kind:"selection",field:{model:"acceptance_notes.note",name:"priority"},label:"Priority",enable_counters:true,expand:true,default_value:null,notice:null}} : {})},{...formField("name"),widget:"char"},formField("priority"),formField("print_run")]},
 {identity:{kind:"stored",id:2},type:"form",schema_major:1,renderer:"web.form",root:0,nodes:formNodes},
 // The pivot of the analysis action: notes by priority (rows) and format (columns), measured in print run.
 {identity:{kind:"stored",id:6},type:"pivot",schema_major:1,renderer:"web.pivot",root:0,nodes:[{kind:"pivot",children:[1,2,3],disable_linking:false,display_quantity:false},
  {kind:"pivot_field",field:{model:"acceptance_notes.note",name:"priority"},label:"Priority",role:"row"},
  {kind:"pivot_field",field:{model:"acceptance_notes.note",name:"format"},label:"Format",role:"col"},
  {kind:"pivot_field",field:{model:"acceptance_notes.note",name:"print_run"},label:"Print run",role:"measure"}]}
]};
const pivotRecords = [
 {priority:"normal",format:"hardcover",print_run:12000,pub_year:1974}, {priority:"normal",format:"paperback",print_run:30500,pub_year:1980}, {priority:"normal",format:"paperback",print_run:2200,pub_year:1980},
 {priority:"high",format:"hardcover",print_run:800,pub_year:1951}, {priority:"high",format:"ebook",print_run:150000,pub_year:2005}, {priority:"draft",format:"paperback",print_run:450,pub_year:2006}, {priority:false,format:"ebook",print_run:60,pub_year:false}
];
// What formatted_read_grouping_sets answers: one list of group rows per set, each with its value, domain, count and sum.
// The domain keeps the records every `[field, "=", value]` term holds for (the pivot sends header domains; this fixture has no other term).
const pivotAnswer = (sets, aggregates, domain = []) => sets.map(set => {
  const groups = new Map();
  for (const record of pivotRecords.filter(record => domain.every(term => record[term[0]] === term[2]))) { const key = JSON.stringify(set.map(name => record[name])); groups.set(key, [...(groups.get(key) ?? []), record]); }
  return [...groups.values()].map(group => Object.fromEntries([["__extra_domain", set.map(name => [name, "=", group[0][name]])], ...set.map(name => [name, group[0][name]]),
    ...aggregates.map(spec => [spec, spec === "__count" ? group.length : group.reduce((total, record) => total + record[spec.split(":")[0]], 0)])]));
});
// The search view of the grouped-list scene: two declared group-by items (the second one a date interval the client leaves out).
const groupByContext = spec => ({schema:1,nodes:[{kind:"literal",value:{kind:"str",value:"group_by"}},{kind:"literal",value:{kind:"str",value:spec}},{kind:"dict",items:[[0,1]]}]});
const searchDescription = {...description,description_version:"fixture-search-d1",views:[{identity:{kind:"stored",id:3},type:"search",schema_major:1,renderer:"web.search",root:0,nodes:[{kind:"search",children:[1,2]},
 {kind:"search_filter",name:"group_priority",label:"Priority",context:groupByContext("priority")},{kind:"search_filter",name:"group_month",label:"Created by month",context:groupByContext("create_date:month")}]}]};
const groupableFields = {name:{string:"Name",type:"char",groupable:true},priority:{string:"Priority",type:"selection",groupable:true,selection:fields.priority.selection},create_date:{string:"Created on",type:"datetime",groupable:true}};
const pivotFieldList = {...groupableFields, format:{string:"Format",type:"selection",groupable:true,selection:fields.format.selection}, pub_year:{string:"Publication year",type:"integer",groupable:true}, print_run:{string:"Print run",type:"integer",groupable:true,aggregator:"sum"}};
const recordValues=()=>({id:5,line_count:lineCount,attachment_total:3,task_total:1204,name:note,body:"Draft description",priority:priorityValue,owner_id:{id:2,display_name:"Rendering fixture user"},company_id:{id:1,display_name:"Rendering fixture company"},active:true,internal_memo:"",scheduled_date:"2026-11-01",reference:"Readonly fixture reference",create_uid:{id:2,display_name:"Rendering fixture user"},create_date:"2026-11-01 09:00:00",write_uid:{id:2,display_name:"Rendering fixture user"},write_date:"2026-11-01 09:30:00",format:"paperback",isbn:"9780061054884",pub_year:1974,print_run:12000,genre_ids:[{id:1,display_name:"Science Fiction"},{id:2,display_name:"Fantasy"}],favorite:false});
const userFields = { name: scalar("char", "Name", true), email: scalar("char", "Email"), phone: scalar("char", "Phone"), lang: scalar("char", "Language", true), color_scheme: scalar("selection", "Theme", false, { selection: [["system", "System"], ["light", "Light"], ["dark", "Dark"]] }), notification_type: scalar("selection", "Notification", true, { selection: [["email", "Handle by Emails"], ["inbox", "Handle in App"]] }), tz: scalar("char", "Timezone") };
const userField = (name, widget = "char", options = {}) => ({ kind: "field", force_save: false, field: { model: "res.users", name }, widget, options, modifiers: { readonly: null, required: null, invisible: null }, label: userFields[name].label });
const prefNodes = [];
const prefArena = node => { const index = prefNodes.length; prefNodes.push({}); const { children, ...props } = node; prefNodes[index] = { ...props, ...(children ? { children: children.map(prefArena) } : {}) }; return index; };
const noMod = { readonly: null, required: null, invisible: null };
prefArena({ kind: "form", record_mode: "record", children: [
  { kind: "sheet", children: [
    { kind: "title", children: [{ kind: "heading", level: 1, children: [userField("name")] }, userField("email", "email"), userField("phone", "phone")] },
    { kind: "notebook", children: [
      { kind: "page", name: "preferences_page", label: "Preferences", children: [{ kind: "group", label: "", columns: 4, colspan: 2, children: [
        { kind: "group", label: "", columns: 2, colspan: 2, children: [userField("lang", "web.preference_choice")] },
        { kind: "group", label: "", columns: 2, colspan: 2, children: [userField("color_scheme", "radio", { horizontal: true }), userField("notification_type", "radio", { horizontal: true })] }
      ] }] },
      { kind: "page", name: "calendar", label: "Calendar", children: [{ kind: "group", label: "", columns: 2, colspan: 2, children: [userField("tz", "web.preference_choice")] }] }
    ] }
  ] },
  { kind: "footer", children: [
    { kind: "button", name: "preference_change_password", button_type: "object", label: "Change password", variant: "secondary", confirm: null, modifiers: noMod },
    { kind: "button", name: "preference_save", button_type: "object", label: "Update Preferences", variant: "primary", confirm: null, modifiers: noMod },
    { kind: "cancel_button", label: "Cancel", variant: "secondary", modifiers: noMod }
  ] }
] });
// Same arch as addons/base/data/user_preferences_views.xml (users_preferences_form).
const preferencesDescription = { wire_major: 1, registry_version: "fixture-r1", description_version: "prefs-d1", model: "res.users", fields: { "res.users": userFields }, notices: [], views: [{ identity: { kind: "stored", id: 9 }, type: "form", schema_major: 1, renderer: "web.form", root: 0, nodes: prefNodes }] };
const settingsDescription = { wire_major: 1, registry_version: "fixture-r1", description_version: "settings-d1", model: "ir.config.setting", fields: { "ir.config.setting": {} }, notices: [], views: [{ identity: { kind: "stored", id: 3 }, type: "form", schema_major: 1, renderer: "web.form", root: 0, nodes: [{ kind: "form", record_mode: "service", children: [1] }, { kind: "widget", widget: "web.settings", options: { scope_kinds: ["global", "company"], app: null }, modifiers: { readonly: null, required: null, invisible: null } }] }] };
const addonsDescription = { ...settingsDescription, model: "ir.module.module", fields: { "ir.module.module": {} }, views: [{ ...settingsDescription.views[0], identity: { kind: "stored", id: 4 }, nodes: [{ kind: "form", record_mode: "service", children: [1] }, { kind: "widget", widget: "web.addons", options: {}, modifiers: { readonly: null, required: null, invisible: null } }] }] };
// Synthetic protocol data, never a claim about installed records or caller rights.
const settingsTargetModels = { 130: "res.users", 131: "res.lang", 132: "res.company" };
const settingsTargetDescriptions = Object.fromEntries(Object.values(settingsTargetModels).map(model => {
  const name = { kind: "field", force_save: false, field: { model, name: "name" }, label: "Name", widget: "char", options: {}, modifiers: { readonly: null, required: null, invisible: null } };
  return [model, { wire_major: 1, registry_version: "fixture-r1", description_version: "settings-target-d1", model, fields: { [model]: { name: scalar("char", "Name") } }, notices: [], views: [
    { identity: { kind: "stored", id: 130 }, type: "list", schema_major: 1, renderer: "web.list", root: 0, nodes: [{ kind: "list", children: [1] }, name] },
    { identity: { kind: "stored", id: 131 }, type: "form", schema_major: 1, renderer: "web.form", root: 0, nodes: [{ kind: "form", record_mode: "record", children: [1] }, name] }
  ] }];
}));
const settingsPanelItems = [
  { kind: "info", key: "base.active_users", label: "Active Users", app: "base", block: "Users", sequence: 10, value: 12, detail: null },
  { kind: "link", key: "base.manage_users", label: "Manage Users", app: "base", block: "Users", sequence: 20, target: { kind: "action", id: 130, res_id: null } },
  { kind: "info", key: "base.languages", label: "Languages", app: "base", block: "Languages", sequence: 30, value: 2, detail: null },
  { kind: "link", key: "base.manage_languages", label: "Manage Languages", app: "base", block: "Languages", sequence: 40, target: { kind: "action", id: 131, res_id: null } },
  { kind: "info", key: "base.company_info", label: "Company", app: "base", block: "Companies", sequence: 50, value: "Rendering fixture company", detail: "First street\nJakarta\nVAT: fixture" },
  { kind: "link", key: "base.company_update", label: "Update Info", app: "base", block: "Companies", sequence: 60, target: { kind: "action", id: 132, res_id: 1 } },
  { kind: "info", key: "base.companies", label: "Company", app: "base", block: "Companies", sequence: 70, value: 1, detail: null },
  { kind: "link", key: "base.manage_companies", label: "Manage Companies", app: "base", block: "Companies", sequence: 80, target: { kind: "action", id: 132, res_id: null } }
];
let rejectSettingsLink = false;
let rejectNextAction = false;
const permissionMessage = "You do not have access to this view. Return to an available menu or ask your administrator.";
const deniedActions = [];
let delayNextRead = false;
let selectionFixture = false;
let note = "Browser fixture note", formActionGeometry;
let priorityValue = "normal";
let lineCount = 17;
const inspectCalls = [];
const lendCalls = [];
const writes = [], calls = [], searchDomains = [], pageReads = [], groupDomains = [], groupReads = [], pivotReads = [];
let rejectNextCount = false;
function html(login) {
  const data = login ? { csrf: "fixture-not-a-real-token", database: "fixture", redirect: "/erp", error: "<img src=x onerror=alert(1)>" } : bootstrap;
  return `<!doctype html><html lang="en"${theme.startsWith("auto-") ? "" : ` data-theme="${theme}"`}><head><meta charset="utf-8"><meta name="viewport" content="width=device-width,initial-scale=1"><link rel="stylesheet" href="/erp/assets/${css.path}"><title>Compiled client rendering fixture</title></head><body><div id="erp-root"></div><script type="application/json" id="${login ? "erp-login" : "erp-bootstrap"}">${JSON.stringify(data).replaceAll("<", "\\u003c")}</script><script type="module" src="/erp/assets/${js.path}"></script></body></html>`;
}
const server = createServer(async (request, response) => {
  try {
    const pathname = new URL(request.url, "http://fixture").pathname;
    response.setHeader("Cache-Control", "no-store");
    response.setHeader("Content-Security-Policy", "default-src 'self'; script-src 'self'; style-src 'self'; object-src 'none'; base-uri 'none'; frame-ancestors 'none'");
    if (officeBlue && pathname === "/theme_office_blue/static/src/theme.css" && (missingOfficeCss || !officeInstalled)) { response.statusCode = 404; response.end(); return; }
    const asset = assets.get(pathname);
    if (asset) { response.setHeader("Content-Type", asset.type); response.end(asset.data); return; }
    if (request.method === "GET" && ["/erp/login", "/erp", "/erp/action-20", "/erp/action-21", "/erp/action-22", "/erp/action-25"].includes(pathname)) {
      selectionFixture = pathname === "/erp/action-20" && new URL(request.url, "http://127.0.0.1").searchParams.get("view_type") === "list";
      response.setHeader("Content-Type", "text/html; charset=utf-8"); response.end(html(pathname === "/erp/login")); return;
    }
    if (request.method !== "POST" || !pathname.startsWith("/erp/json/2/") && pathname !== "/fixture/deny-settings-link") { response.statusCode = 404; response.end(); return; }
    let input = ""; for await (const chunk of request) { input += chunk; if (input.length > 8192) throw new Error("Fixture request exceeds bound"); }
    const body = JSON.parse(input); calls.push(pathname);
    let value;
    if (pathname.endsWith("/erp_load_web_menus")) { value = { wire_major: 1, registry_version: "fixture-r1", roots: [10, 11, 12, 13], nodes: [{ module: "acceptance_notes", id: 10, parent_id: null, children: [], label: "Notes", icon: "lucide:notebook-pen", action_id: 20 }, { module: "base", id: 11, parent_id: null, children: officeBlue ? [] : [15, 16], label: "Settings", icon: "lucide:settings", action_id: 21 }, { module: null, id: 12, parent_id: null, children: [], label: "Addons", icon: "lucide:layers", action_id: 22 }, { module: null, id: 13, parent_id: null, children: [], label: "Restricted", icon: "lucide:shield", action_id: 23 }] };
      if (!officeBlue) value.nodes.push(...shellFixtureMenus());
      if (officeBlue) {
        value.roots = value.roots.map(id => id === 10 ? 14 : id);
        value.nodes[0].parent_id = 14;
        value.nodes[0].module = null;
        value.nodes[0].label = "All Notes";
        value.nodes.push({ module: "acceptance_notes", id: 14, parent_id: null, children: [10], label: "Notes", icon: "lucide:notebook-pen", action_id: null });
      }
    }
    else if (pathname === "/fixture/deny-settings-link") { rejectSettingsLink = true; value = {}; }
    else if (pathname.endsWith("/erp_load_web_action_views") && body.ids[0] === 132 && rejectSettingsLink) { rejectSettingsLink = false; response.statusCode = 403; value = { name: "erp.exceptions.AccessError", message: "Settings company action refused by fixture." }; }
    else if ((pathname.endsWith("/erp_load_web_action") || pathname.endsWith("/erp_load_web_action_views")) && settingsTargetModels[body.ids[0]]) value = { wire_major: 1, id: body.ids[0], name: "Settings linked records", model: settingsTargetModels[body.ids[0]], target: "current", views: [{ id: 130, type: "list" }, { id: 131, type: "form" }], domain: null, context: null };
    else if (["res.users", "res.lang", "res.company"].some(model => pathname === `/erp/json/2/${model}/web_search_read`)) value = { length: 1, records: [{ id: 1, name: "Settings fixture record" }] };
    else if (pathname === "/erp/json/2/res.company/web_read") value = [{ id: 1, name: "Rendering fixture company" }];
    else if ((pathname.endsWith("/erp_load_web_action") || pathname.endsWith("/erp_load_web_action_views")) && rejectNextAction && body.ids[0] === 23) { deniedActions.push({ pathname, ids: body.ids }); response.statusCode = 403; value = { name: "erp.exceptions.AccessError", message: permissionMessage }; }
    else if ((pathname.endsWith("/erp_load_web_action") || pathname.endsWith("/erp_load_web_action_views")) && body.ids[0] === 25) value = { wire_major: 1, id: 25, name: "Note analysis", model: "acceptance_notes.note", target: "current", views: [{ id: 1, type: "list" }, { id: 6, type: "pivot" }], domain: null, context: null };
    else if ((pathname.endsWith("/erp_load_web_action") || pathname.endsWith("/erp_load_web_action_views")) && body.ids[0] === 22) value = { wire_major: 1, id: 22, name: "Addons", model: "ir.module.module", target: "current", views: [{ id: 4, type: "form" }], domain: null, context: null };
    else if ((pathname.endsWith("/erp_load_web_action") || pathname.endsWith("/erp_load_web_action_views")) && body.ids[0] === 21) value = { wire_major: 1, id: 21, name: "Settings", model: "ir.config.setting", target: "current", views: [{ id: 3, type: "form" }], domain: null, context: null };
    else if ((pathname.endsWith("/erp_load_web_action") || pathname.endsWith("/erp_load_web_action_views"))) value = { wire_major: 1, id: 20, name: "Notes", model: "acceptance_notes.note", target: "current", views: [{ id: 1, type: "list" }, { id: 2, type: "form" }], domain: null, context: null };
    else if (pathname === "/erp/json/2/res.users/action_get") value = { type: "ir.actions.act_window", id: 7, name: "My Preferences", res_model: "res.users", target: "new", view_mode: "form", views: [[9, "form"]] };
    else if (pathname === "/erp/json/2/res.users/erp_normalize_action_result") value = { wire_major: 1, result: body.result?.type === "ir.actions.act_window" ? { kind: "window", name: "My Preferences", model: "res.users", res_id: 2, views: [{ id: 9, type: "form" }], context: {} } : { kind: "close" } };
    else if (pathname === "/erp/json/2/res.users/get_views") value = preferencesDescription;
    // The saved scheme the client applies at start: the fixture page is rendered in `theme`, so the user's own row says the same.
    else if (pathname === "/erp/json/2/res.users/read") value = [{ id: 2, color_scheme: theme.startsWith("auto-") ? "system" : theme }];
    else if (pathname === "/erp/json/2/res.users/web_read") value = [{ id: 2, name: "Rendering fixture user", email: "operator@example.com", phone: "+62 21 555 0100", lang: "en_US", color_scheme: "system", notification_type: "email", tz: "Asia/Jakarta" }];
    else if (pathname === "/erp/json/2/res.partner/fields_get") value = Object.fromEntries(body.allfields.map(field => [field, { selection: field === "lang" ? [["en_US", "English (US)"], ["id_ID", "Indonesian"]] : field === "tz" ? [["Asia/Jakarta", "Asia/Jakarta"], ["UTC", "UTC"]] : [] }]));
    else if (pathname === "/erp/json/2/ir.module.module/get_views") value = addonsDescription;
    else if (pathname.endsWith("/erp_addon_detail")) value = { wire_major: 1, registry_version: "fixture-r1", name: "acceptance_notes", label: "Sample Library", summary: "Shared notes for a distributed office team.", description: "Keeps meeting notes, decisions and follow-ups in one searchable place.", author: "ERPForAll", available_version: "1.0", installed_version: "1.0", state: "installed", dependencies: { items: ["web", "base"], next_cursor: null }, dependents: { items: [], next_cursor: null }, menus: { items: [{ id: 1, name: "Notes", label: "Notes" }, { id: 2, name: "acceptance_notes_all", label: "All notes" }], next_cursor: null }, views: { items: [{ id: 1, name: "acceptance.notes.form", kind: "form", model: "acceptance.note" }, { id: 2, name: "acceptance.notes.list", kind: "list", model: "acceptance.note" }], next_cursor: null }, upgrade_refusal: null, uninstall_refusal: "Uninstall impact cannot describe package runtime lifecycle callbacks; unsupported until a later release" };
    else if (pathname.endsWith("/erp_available_addons")) value = { wire_major: 1, addons: [{ name: "acceptance_notes", label: "Shared notes for a distributed office team", version: "1.0", state: "installed", installable: true, summary: "Long catalogue content remains readable and constrained on narrow screens.", category: "Productivity", author: "ERPForAll", application: true, website: "", license: "LGPL-3", depends: ["web"] }] };
    else if (pathname === "/erp/json/2/ir.config.setting/get_views") value = settingsDescription;
    else if (pathname === "/erp/json/2/ir.config.setting/erp_describe") value = { wire_major: 1, registry_version: "fixture-r1", scope: body.scope, items: settingsPanelItems, declarations: [...(officeBlue && officeInstalled ? [{ key: "theme_office_blue.appearance", kind: "selection", label: "Database appearance (refresh after saving)", choices: [["default", "Default"], ["office-blue", "Office Blue"]], writable: true, app: "theme_office_blue", block: "General", sequence: 100 }] : []), { key: "acceptance_notes.enabled", default: false, kind: "boolean", label: "Enable shared notes", help: "Everyone in the company can read and edit a note that is marked shared.", choices: null, writable: true, app: "acceptance_notes", block: "General", sequence: 100 }, { key: "acceptance_notes.colour", default: "yellow", kind: "selection", label: "Default note colour", help: "New notes start with this colour.", choices: [["yellow", "Yellow"], ["blue", "Blue"], ["green", "Green"]], writable: true, app: "acceptance_notes", block: "General", sequence: 110 }, { key: "acceptance_notes.archive_days", default: 30, kind: "integer", label: "Archive after (days)", help: "Notes that were not touched for this long are archived.", choices: null, writable: true, app: "acceptance_notes", block: "General", sequence: 120 }, { key: "acceptance_notes.mail_to", default: "", kind: "string", label: "Notification e-mail", help: "Digest of new shared notes goes to this address.", choices: null, writable: true, app: "acceptance_notes", block: "Sharing", sequence: 200 }, { key: "acceptance_notes.public_link", default: false, kind: "boolean", label: "Allow public links", help: "A shared note may be opened without signing in.", choices: null, writable: true, app: "acceptance_notes", block: "Sharing", sequence: 210 }] };
    else if (pathname === "/erp/json/2/ir.config.setting/erp_read_values") value = { wire_major: 1, scope: body.scope, revision: "1", values: body.keys.map(key => key === "theme_office_blue.appearance" ? { key, value: officeChoice, source: "stored" } : { key, value: ({ "acceptance_notes.colour": "yellow", "acceptance_notes.archive_days": 30, "acceptance_notes.mail_to": "notes@example.com", "acceptance_notes.public_link": false, "web.pivot_max_groups": 2000, "web.pivot_max_open_headers": 200, "web.pivot_max_cells": 20000 })[key] ?? true, source: "default" }) };
    else if (officeBlue && pathname === "/erp/json/2/ir.config.setting/erp_write_values") {
      for (const change of body.changes) { assert.equal(change.key, "theme_office_blue.appearance"); officeChoice = change.value; }
      value = { wire_major: 1, scope: body.scope, revision: "2", values: [{ key: "theme_office_blue.appearance", value: officeChoice, source: "stored" }] };
    }
    else if (pathname.endsWith("/get_views") && body.views?.[0]?.[1] === "search") value = searchDescription;
    else if (pathname === "/erp/json/2/acceptance_notes.note/fields_get") {
      // The list's group-by menu asks for four attributes; the pivot also asks which fields sum (`aggregator`).
      if (body.attributes.includes("currency_field")) { assert.deepEqual(body.attributes, ["string", "type", "groupable", "selection", "aggregator", "currency_field"]); value = {...groupableFields, print_run:pivotFieldList.print_run}; }
      else if (body.attributes.includes("aggregator")) { assert.deepEqual(body.attributes, ["string", "type", "groupable", "selection", "aggregator"]); value = pivotFieldList; }
      else { assert.deepEqual(body.attributes, ["string", "type", "groupable", "selection"]); value = groupableFields; }
    }
    else if (pathname === "/erp/json/2/acceptance_notes.note/web_read_group") {
      assert.deepEqual(body.aggregates, ["__count", "print_run:sum"]); groupReads.push(body);
      const spec = body.groupby[0];
      const groups = spec === "priority" ? [
        { priority: "normal", "print_run:sum": 12000, __count: 2, __extra_domain: [["priority", "=", "normal"]] },
        { priority: "high", "print_run:sum": 800, __count: 1, __extra_domain: [["priority", "=", "high"]] },
        { priority: false, "print_run:sum": 0, __count: 1, __extra_domain: [["priority", "=", false]] }
      ] : spec === "name" ? [{ name: note, "print_run:sum": 12000, __count: 1, __extra_domain: [["name", "=", note]] }] : spec === "create_date:month" ? [{ "create_date:month": ["2026-11-01 00:00:00", "November 2026"], "print_run:sum": 12000, __count: 1, __extra_domain: [["create_date", ">=", "2026-11-01 00:00:00"], ["create_date", "<", "2026-12-01 00:00:00"]] }] : undefined;
      assert.ok(groups, `unexpected list grouping ${spec}`);
      value = { groups: groups.slice(body.offset, body.offset + body.limit), length: groups.length };
    }
    else if (pathname === "/erp/json/2/acceptance_notes.note/formatted_read_grouping_sets") { pivotReads.push(body); value = pivotAnswer(body.grouping_sets, body.aggregates, body.domain); }
    else if (pathname.endsWith("/get_views")) value = description;
    else if (pathname.endsWith("/web_read")) value = [recordValues()];
    else if (pathname.endsWith("/copy")) { copyCalls.push(body); value = [5]; }
    else if (pathname.endsWith("/formatted_read_group")) {
      assert.deepEqual(body.groupby, ["priority"]); assert.deepEqual(body.aggregates, ["__count"]);
      groupDomains.push(body.domain);
      if (rejectNextCount) { rejectNextCount = false; throw new Error("Explicit fixture count failure"); }
      const matches = body.domain.every(([field, op, value]) => {
        assert.equal(field, "name"); assert.equal(op, "ilike");
        return note.toLowerCase().includes(value.toLowerCase());
      });
      value = matches ? [{priority:priorityValue,__count:selectionFixture ? 2 : 1}] : [];
    }
    else if (pathname === "/erp/json/2/mail.activity/web_search_read") {
      // Two planned activities on the open record: one overdue, one planned (state is computed by the server).
      assert.deepEqual(body.domain, [["res_model", "=", "acceptance_notes.note"], ["res_id", "=", 5]]);
      const day = offset => { const d = new Date(); d.setDate(d.getDate() + offset); return `${d.getFullYear()}-${String(d.getMonth() + 1).padStart(2, "0")}-${String(d.getDate()).padStart(2, "0")}`; };
      value = { length: 2, records: [
        { id: 201, summary: "Send the lending contract", note: "", date_deadline: day(-3), state: "overdue", can_write: true, user_id: { id: 2, display_name: "Alice Adams" }, activity_type_id: { id: 4, display_name: "Call", icon: "phone" } },
        { id: 202, summary: "Review the return condition", note: "", date_deadline: day(4), state: "planned", can_write: true, user_id: { id: 2, display_name: "Alice Adams" }, activity_type_id: { id: 2, display_name: "To-Do", icon: "check" } },
      ] };
    }
    else if (pathname === "/erp/json/2/mail.activity.type/web_search_read") value = { length: 2, records: [
      { id: 4, name: "Call", summary: "Call", delay_count: 2, delay_unit: "days", default_user_id: null },
      { id: 2, name: "To-Do", summary: "To-Do", delay_count: 5, delay_unit: "days", default_user_id: null },
    ] };
    else if (pathname.endsWith("/web_search_read")) {
      if (readPause) await readPause;
      if (delayNextRead) { delayNextRead = false; await new Promise(resolve => setTimeout(resolve, 1800)); }
      searchDomains.push(body.domain); pageReads.push({limit:body.limit,offset:body.offset,domain:body.domain});
      assert.ok(Array.isArray(body.domain));
      const matches = body.domain.every(condition => {
        assert.ok(Array.isArray(condition)); assert.equal(condition.length, 3);
        if (condition[0] === "priority") { assert.equal(condition[1], "="); return priorityValue === condition[2]; }
        if (condition[0] === "create_date") { assert.ok([">=", "<"].includes(condition[1])); return true; }
        assert.equal(condition[0], "name"); assert.ok(["ilike", "="].includes(condition[1]));
        return condition[1] === "=" ? note === condition[2] : note.toLowerCase().includes(condition[2].toLowerCase());
      });
      const records = matches ? [{ id: 5, name: note, priority: priorityValue, print_run:12000 }, ...(selectionFixture ? [{ id: 6, name: "Second fixture row", priority: priorityValue }] : [])] : [];
      value = { length: records.length, records };
    }
    else if (pathname.endsWith("/web_name_search")) {
      assert.equal(pathname,"/erp/json/2/res.users/web_name_search");
      assert.equal(body.name,"Rendering");
      assert.deepEqual(body.specification,{display_name:{}});
      value = [{id:2,display_name:"Rendering fixture user"},{id:3,display_name:"Second fixture user"}];
    }
    else if (pathname.endsWith("/erp_normalize_action_result")) value = { wire_major: 1, result: { kind: "none" } };
    else if (pathname.endsWith("/onchange")) value = { value: { name: body.values.name } };
    else if (pathname === "/erp/json/2/acceptance_notes.note/inspect_lines") { assert.deepEqual(body.ids,[5]); inspectCalls.push(body); lineCount=18; value=true; }
    else if (pathname === "/erp/json/2/acceptance_notes.note/lend") { assert.deepEqual(body.ids, [5]); lendCalls.push(body); priorityValue = "high"; value = true; }
    else if (pathname.endsWith("/web_save")) { assert.deepEqual(body.ids, [5]); note = body.vals.name; writes.push(body); value = [recordValues()]; }
    else if (pathname === "/erp/json/2/acceptance_notes.note/thread_subtypes") {
      assert.deepEqual(body.ids, [5]);
      value = { subtype_ids: [1, 2], store: JSON.stringify({ wire: 1, schema: "fixture-mail-s1", asof: { xmin: "1", xmax: "10", xip: [], own: null }, records: {} }) };
    }
    else if (pathname.endsWith("/mail.store/store_fetch")) {
      value = {
        store: JSON.stringify({
          wire: 1,
          schema: "fixture-mail-s1",
          asof: { xmin: "1", xmax: "10", xip: [], own: null },
          records: {
            "res.partner": [{ id: 1, name: "Alice Adams" }, { id: 2, name: "Bob Brown" }],
            "mail.message.subtype": [{ id: 1, name: "Discussions", internal: false }, { id: 2, name: "Note", internal: true }],
            // Same shape as the server's: a note is a comment whose subtype is internal; is_internal stays false.
            "mail.message": [
              { id: 101, body: "<p>Kick-off notes shared with the team.</p>", message_type: "comment", subtype_id: 1, date: "2026-11-09 09:15:00", author_id: 1, is_internal: false },
              { id: 102, body: "<p>Internal: check the loan period before approving.</p>", message_type: "comment", subtype_id: 2, date: "2026-11-10 10:00:00", author_id: 2, is_internal: false },
              { id: 103, body: "", message_type: "notification", date: "2026-11-10 10:30:00", author_id: 2, tracking: JSON.stringify([{ f: 1, n: "name", t: "string", o: null, v: null, od: "Draft", vd: "Done" }]) },
            ],
          },
        }),
        topics: [
          {
            topic: "schema",
            ok: true,
            data: {
              wire: 1,
              schema: "fixture-mail-s1",
              models: {
                "res.partner": { fields: { name: { kind: "string" } } },
                // the thread's own model: the store holds the record the chatter belongs to
                "acceptance_notes.note": { fields: { name: { kind: "string" } } },
                "mail.message": {
                  fields: {
                    body: { kind: "html" },
                    subject: { kind: "string" },
                    message_type: { kind: "selection" },
                    date: { kind: "datetime" },
                    author_id: { kind: "many2one", target: "res.partner" },
                    is_internal: { kind: "bool" },
                    subtype_id: { kind: "many2one", target: "mail.message.subtype" },
                    tracking: { kind: "string" },
                  },
                },
                "mail.message.subtype": { fields: { name: { kind: "string" }, internal: { kind: "bool" } } },
                "mail.followers": {
                  fields: {
                    res_model: { kind: "string" },
                    res_id: { kind: "int" },
                    partner_id: { kind: "many2one", target: "res.partner" },
                    subtype_ids: { kind: "x2many", target: "mail.message.subtype" },
                  },
                },
              },
            },
          },
          {
            topic: "thread",
            ok: true,
            data: {
              thread: { model: "acceptance_notes.note", id: 5, can_post: true },
              messages: {
                model: "acceptance_notes.note",
                id: 5,
                message_ids: [103, 102, 101],
                has_more_before: false,
                has_more_after: false,
              },
              followers: { count: 0, follower_ids: [], self_follower_id: null, next_after: null },
            },
          },
        ],
      };
    }
    // The shell reads the saved app order at start; the fixture user has none.
    else if (pathname === "/erp/json/2/res.users.settings/get_res_users_settings") value = { homemenu_config: null };
    // The activity menu in the top bar: the server's answer shape (addons/mail/src/activity_menu.rs).
    else if (pathname === "/erp/json/2/mail.activity/erp_activity_groups") value = {
      activity_counter: 3,
      groups: [
        { model: "res.partner", name: "Contact", addon: "base", domain: [], overdue_count: 1, today_count: 2, planned_count: 0, total_count: 3, activity_ids: [1, 2, 3] },
        { model: "mail.activity", name: "Other Activities", addon: "mail", domain: [], overdue_count: 0, today_count: 0, planned_count: 1, total_count: 0, activity_ids: [4] },
      ],
    };
    else throw new Error("Unexpected fixture endpoint " + pathname);
    response.setHeader("Content-Type", "application/json"); response.end(JSON.stringify(pathname.endsWith("erp_load_web_action_views") && response.statusCode === 200 ? { action: value, views: value.model === "ir.module.module" ? addonsDescription : value.model === "ir.config.setting" ? settingsDescription : settingsTargetDescriptions[value.model] ?? description } : value));
  } catch (error) { response.statusCode = 500; response.setHeader("Content-Type", "application/json"); response.end(JSON.stringify({ name: "FixtureError", message: error.message })); }
});
server.listen(0, "127.0.0.1"); await once(server, "listening");
const base = `http://127.0.0.1:${server.address().port}`, profile = await mkdtemp(path.join(tmpdir(), "erp-client-chrome-"));
const chrome = remoteCdp ? undefined : spawn(chromeBin, ["--headless=new", "--no-first-run", "--no-default-browser-check", "--disable-background-networking", "--blink-settings=primaryHoverType=2,availableHoverTypes=2,primaryPointerType=4,availablePointerTypes=4", "--remote-debugging-address=127.0.0.1", "--remote-debugging-port=0", `--user-data-dir=${profile}`, "about:blank"], { stdio: ["ignore", "ignore", "pipe"] });
let socket, sequence = 0, session;
const pending = new Map(), exceptions = [], screenshots = [], listLayouts = [], errorLayouts = [], resizeLayouts = [];
class CdpProtocolError extends Error {
  constructor(method, detail) { super(`${method}: ${detail}`); this.name = "CdpProtocolError"; this.method = method; this.detail = detail; }
}

try {
  const endpoint = remoteCdp ? (await (await fetch(new URL("/json/version", remoteCdp))).json()).webSocketDebuggerUrl : await new Promise((resolve, reject) => {
    const finish = (error, endpoint) => {
      clearTimeout(timer); chrome.off("error", failed); chrome.off("exit", exited); chrome.stderr.off("data", listening);
      if (error) reject(error); else resolve(endpoint);
    };
    const failed = error => finish(error);
    const exited = code => finish(new Error(`Chrome exited before connection: ${code}`));
    const listening = chunk => { const match = /DevTools listening on (ws:\/\/[^\s]+)/.exec(chunk.toString()); if (match) finish(undefined, match[1]); };
    const timer = setTimeout(() => finish(new Error("Chrome startup deadline exceeded")), 15000);
    chrome.once("error", failed); chrome.once("exit", exited); chrome.stderr.on("data", listening);
  });
  socket = new WebSocket(endpoint); await once(socket, "open");
  socket.addEventListener("message", event => {
    const message = JSON.parse(event.data);
    if (message.id) { const request = pending.get(message.id); if (!request) return; pending.delete(message.id); clearTimeout(request.timer); message.error ? request.reject(new CdpProtocolError(request.method, message.error.message)) : request.resolve(message.result); }
    else if (message.method === "Runtime.exceptionThrown") exceptions.push(message.params.exceptionDetails.text + ": " + (message.params.exceptionDetails.exception?.description ?? ""));
  });
  const call = (method, params = {}, scoped = true) => new Promise((resolve, reject) => {
    const id = ++sequence, timer = setTimeout(() => { pending.delete(id); reject(new Error(`CDP deadline: ${method}`)); }, 15000);
    pending.set(id, { resolve, reject, timer, method }); socket.send(JSON.stringify({ id, method, params, ...(scoped && session ? { sessionId: session } : {}) }));
  });
  const version = await call("Browser.getVersion", {}, false);
  const target = await call("Target.createTarget", { url: "about:blank" }, false);
  session = (await call("Target.attachToTarget", { targetId: target.targetId, flatten: true }, false)).sessionId;
  await call("Page.enable"); await call("Runtime.enable");
  await call("Emulation.setEmulatedMedia", { features: [{ name: "prefers-color-scheme", value: theme === "auto-light" || theme === "dark" ? "light" : "dark" }, { name: "prefers-reduced-motion", value: "reduce" }] });
  const evaluate = async expression => { const result = await call("Runtime.evaluate", { expression, returnByValue: true, awaitPromise: true }); if (result.exceptionDetails) throw new Error(result.exceptionDetails.exception?.description ?? result.exceptionDetails.text); return result.result.value; };
  const wait = expression => waitForReadiness(evaluate, expression, async () =>
    `state=${JSON.stringify(await evaluate('({buttons:[...document.querySelectorAll(".form-header-btn,.form-stat-button")].map(b=>({text:b.textContent,disabled:b.disabled})),errors:[...document.querySelectorAll("[role=alert],dialog,.client-notifications")].map(e=>e.textContent),body:document.body.innerText.slice(-1600)})'))} popup=${JSON.stringify(await evaluate('(()=>{const p=document.querySelector(".erp-search-menu-panel");if(!p)return null;const r=p.getBoundingClientRect();return {left:r.left,right:r.right,viewport:innerWidth,transform:p.style.transform,parent:p.parentElement.getBoundingClientRect().toJSON()};})()'))}`,
    error => error instanceof CdpProtocolError && error.method === "Runtime.evaluate"
      && error.detail === "Inspected target navigated or closed");

  // Actual computed styles on the compiled client, including ancestor backgrounds.
  // These assertions concern readable UI, not product performance ratchets.
  const appearance = async () => {
    const result = await evaluate(`(() => {
      const rgb = value => value.match(/[\\d.]+/g)?.slice(0,3).map(Number) ?? [];
      const luminance = color => rgb(color).map(c=>{const v=c/255;return v<=.04045?v/12.92:((v+.055)/1.055)**2.4}).reduce((n,v,i)=>n+v*[.2126,.7152,.0722][i],0);
      const background = node => { for(let n=node;n;n=n.parentElement){const c=getComputedStyle(n).backgroundColor;if(c!=="rgba(0, 0, 0, 0)"&&c!=="transparent")return c;} return getComputedStyle(document.documentElement).backgroundColor; };
      const selectors = ["html", ".app-navigation", ".control-panel", ".content-sheet", ".table-scroll", ".list-table th", ".erp-search-menu-panel", ".settings-sidebar", ".settings-section-title", ".login-card", ".app-bar", ".user-dropdown", ".home-menu-overlay", ".mobile-drawer", ".addons-panel", ".addon-tile", ".addon-info-dialog", ".choice-dialog", ".client-error-dialog", ".wizard-dialog", "input:not([type=checkbox]):not([type=hidden])", "select", "textarea", "button.primary", ".erp-btn-primary"];
      return selectors.flatMap(selector=>[...document.querySelectorAll(selector)].filter(n=>n.getBoundingClientRect().width&&n.getBoundingClientRect().height&&getComputedStyle(n).display!=="none").map(n=>{const bg=background(n),fg=getComputedStyle(n).color,a=luminance(bg),b=luminance(fg);return {selector,bg,fg,luminance:a,contrast:(Math.max(a,b)+.05)/(Math.min(a,b)+.05)};}));
    })()`);
    const activeOffice = await evaluate('document.documentElement.dataset.officeTheme === "office-blue"');
    for (const surface of result) {
      assert.ok(surface.contrast >= 4.5, `readable ${theme} ${surface.selector}: ${JSON.stringify(surface)}`);
      if (surface.selector.includes("primary")) continue;
      // The navy sidebar is the desktop shell; on a phone the same element is the bar's drawer controls, in the header colours.
      if (officeBlue && surface.selector === ".app-navigation" && await evaluate('document.documentElement.dataset.officeTheme === "office-blue" && innerWidth > 768')) assert.ok(surface.luminance < .18, `navy navigation: ${JSON.stringify(surface)}`);
      else if (!activeOffice && theme.endsWith("dark")) assert.ok(surface.luminance < .18, `dark surface ${surface.selector}: ${JSON.stringify(surface)}`);
      else assert.ok(surface.luminance > .7, `light surface ${surface.selector}: ${JSON.stringify(surface)}`);
    }
    return result;
  };
  const dialogGeometry = selector => evaluate(`(() => {
    const d=document.querySelector(${JSON.stringify(selector)}),r=d.getBoundingClientRect(),s=getComputedStyle(d);
    let modal; try { modal={matches:d.matches(':modal')}; } catch(error) { modal={error:String(error)}; }
    return {open:d.open,rect:{x:r.x,y:r.y,width:r.width,height:r.height},computed:{position:s.position,top:s.top,right:s.right,bottom:s.bottom,left:s.left,margin:s.margin,transform:s.transform,zIndex:s.zIndex,display:s.display,visibility:s.visibility},modal,viewport:{innerWidth,innerHeight,clientWidth:document.documentElement.clientWidth,clientHeight:document.documentElement.clientHeight},scroll:{x:scrollX,y:scrollY,height:document.scrollingElement.scrollHeight},focus:document.activeElement?.outerHTML.slice(0,300)};
  })()`);
  const assertDialogVisible = layout => {
    const r=layout.rect,v=layout.viewport;
    assert.ok(r.width>0 && r.height>0 && r.x>=0 && r.y>=0 && r.x+r.width<=v.innerWidth+1 && r.y+r.height<=v.innerHeight+1, `dialog lies inside actual viewport: ${JSON.stringify(layout)}`);
  };
  const centerProbeDialog = selector => evaluate(`(() => {const d=document.querySelector(${JSON.stringify(selector)});Object.assign(d.style,{position:'fixed',top:'50%',left:'50%',right:'auto',bottom:'auto',margin:'0',transform:'translate(-50%, -50%)'});})()`);
  const appearanceChecks = [];
  // The same assertions run in the default theme and in
  // Office Blue: the control row and the table's surface share their left and right edges, the control row has one control
  // height, the header and the body rows have one height each, the first column's text starts on one x in the header and
  // in the body, nothing scrolls sideways and the description line sits clear of every control.
  const listGeometries = [];
  // The menu bar and the control panel stay in view while a tall page scrolls, and the column titles stay under the panel (default theme and Office Blue).
  async function assertListHeaderPinned(label, ready) {
    await call("Emulation.setDeviceMetricsOverride", {width:1440,height:160,deviceScaleFactor:1,mobile:false});
    await call("Page.reload"); await wait(ready);
    await wait('document.querySelector(".app-bar") !== null && document.querySelector(".control-panel-host") !== null');
    const pinned = await evaluate(`(() => { const q = s => document.querySelector(s) ?? (() => { throw new Error('missing ' + s); })(), top = s => Math.round(q(s).getBoundingClientRect().top), bottom = s => Math.round(q(s).getBoundingClientRect().bottom); const before = { y: Math.round(scrollY), bar: top('.app-bar'), tall: document.documentElement.scrollHeight > innerHeight + 40 }; scrollTo(0, 40); return { before, after: { y: Math.round(scrollY), bar: top('.app-bar'), barBottom: bottom('.app-bar'), panel: top('.control-panel-host') } }; })()`);
    assert.equal(pinned.before.tall, true, `${label}: the list page is taller than the window: ${JSON.stringify(pinned)}`);
    assert.deepEqual([pinned.before.y, pinned.before.bar], [0, 0], `${label}: a tall page opens unscrolled with the menu bar in view: ${JSON.stringify(pinned)}`);
    assert.ok(pinned.after.y > 0 && pinned.after.bar === 0 && Math.abs(pinned.after.panel - pinned.after.barBottom) <= 1, `${label}: the menu bar and the control panel stay in view while the page scrolls: ${JSON.stringify(pinned)}`);
    // a little scroll only: a header sticks inside its table, so a two-row fixture table can only hold it for a few pixels
    await evaluate('scrollTo(0, 10)');
    // the panel's height reaches the header's offset on the next resize notification, one frame after the layout settles
    await wait(`Math.abs(document.querySelector('.list-table thead th:nth-child(2)').getBoundingClientRect().top - document.querySelector('.control-panel-host').getBoundingClientRect().bottom) <= 1`).catch(() => {});
    const listHead = await evaluate(`(() => { const r = s => document.querySelector(s).getBoundingClientRect(); const th = document.querySelector('.list-table thead th:nth-child(2)'); return { panelBottom: Math.round(r('.control-panel-host').bottom), headTop: Math.round(th.getBoundingClientRect().top), sticky: getComputedStyle(th).position, panelVar: getComputedStyle(document.documentElement).getPropertyValue('--control-panel-height'), hosts: document.querySelectorAll('.control-panel-host').length, hostHeight: document.querySelector('.control-panel-host').offsetHeight, scrollY: Math.round(scrollY), naturalGap: Math.round(document.querySelector('.list-results').getBoundingClientRect().top - r('.control-panel-host').bottom) }; })()`);
    assert.equal(listHead.sticky, "sticky", `${label}: the list header is sticky: ${JSON.stringify(listHead)}`);
    assert.ok(Math.abs(listHead.headTop - listHead.panelBottom) <= 1, `${label}: the column titles stay directly under the control panel while the page is scrolled: ${JSON.stringify(listHead)}`);
    await evaluate('scrollTo(0, 0)');
  }

  async function assertListGeometry(label, width) {
    const g = await evaluate(`(() => {
      const rect = el => { const r = el.getBoundingClientRect(); return { left: r.left, right: r.right, top: r.top, bottom: r.bottom, height: r.height }; };
      const one = selector => { const el = document.querySelector(selector); return el ? rect(el) : null; };
      const textLeft = el => { const walker = document.createTreeWalker(el, NodeFilter.SHOW_TEXT); let node; while ((node = walker.nextNode())) if (node.textContent.trim()) { const range = document.createRange(); range.selectNodeContents(node); return range.getBoundingClientRect().left; } return null; };
      const panel = document.querySelector('.control-panel-list');
      const controls = [...panel.querySelectorAll('[data-control-actions] button, .view-switcher, .erp-search-view, .search-form, .pager-btn, [data-control-search] .choice-dropdown-trigger')].filter(el => el.getClientRects().length).map(el => ({ name: el.className.toString().split(' ')[0] || el.tagName, ...rect(el) }));
      const description = panel.querySelector('[data-control-extension]');
      const head = [...document.querySelectorAll('.list-table thead th')].map(th => Math.round(th.getBoundingClientRect().height * 10) / 10);
      const body = [...document.querySelectorAll('.list-table tbody tr')].map(tr => Math.round(tr.getBoundingClientRect().height * 10) / 10);
      const checkbox = document.querySelector('.list-table tbody .list-row-select'), cell = checkbox?.closest('td'), cellBox = cell?.getBoundingClientRect(), row = checkbox?.closest('tr').getBoundingClientRect();
      return { surface: one('.list-results'), newButton: one('[data-control-actions] button'), switcher: { right: Math.max(...[...panel.querySelectorAll('[data-control-tools] .view-switcher, [data-control-tools] .pager')].map(el => el.getBoundingClientRect().right)) }, controls, head, body,
        headText: textLeft(document.querySelector('.list-table thead th:nth-child(2) .sort-button') ?? document.querySelector('.list-table thead th:nth-child(2)')),
        bodyText: textLeft(document.querySelector('.list-table tbody td:nth-child(2)')),
        selectorWidth: document.querySelector('.list-table thead th.list-record-selector')?.getBoundingClientRect().width,
        boxMiddle: checkbox ? Math.abs((checkbox.getBoundingClientRect().top + checkbox.getBoundingClientRect().height / 2) - (row.top + row.height / 2)) : null,
        boxInCell: checkbox ? [checkbox.getBoundingClientRect().left - cellBox.left, cellBox.right - checkbox.getBoundingClientRect().right] : null,
        description: description && description.getClientRects().length && description.textContent.trim() ? rect(description) : null,
        overflow: document.documentElement.scrollWidth - innerWidth };
    })()`);
    listGeometries.push({ label, width, ...g });
    const at = `${label} at ${width}: ${JSON.stringify(g)}`;
    assert.ok(Math.abs(g.newButton.left - g.surface.left) <= 1, `New starts on the table surface's left edge (${at})`);
    assert.ok(Math.abs(g.switcher.right - g.surface.right) <= 1, `the last tool (pager, after the view switcher) ends on the table surface's right edge (${at})`);
    // The control row: one height for every control (44 px for New and the filter trigger on a phone, a touch target; the search box around that trigger is its 44 px plus its 1 px borders).
    for (const control of g.controls) assert.ok(Math.abs(control.height - (control.name === 'choice-dropdown-trigger' ? (width <= 768 ? 44 : 26) : control.name === 'primary' && width <= 768 ? 44 : /^(search-form|erp-search-view)$/.test(control.name) && width <= 768 && g.controls.some(other => other.name === 'choice-dropdown-trigger') ? 46 : 32)) <= 0.5, `control ${control.name} has its one control height (${at})`);
    assert.equal(new Set(g.head).size, 1, `one header row height (${at})`);
    assert.equal(new Set(g.body).size, 1, `one body row height (${at})`);
    assert.equal(g.head[0], g.body[0], `header and body rows are as tall as each other (${at})`);
    assert.ok(Number.isFinite(g.headText) && Number.isFinite(g.bodyText) && Math.abs(g.headText - g.bodyText) <= 0.5, `the first column's text starts on one x in the header and the body (${at})`);
    assert.ok(Math.abs(g.selectorWidth - 40) <= 0.5, `the checkbox column is one fixed width, 40px (${at})`);
    assert.ok(Number.isFinite(g.boxMiddle) && g.boxMiddle <= 1, `the row checkbox is centred on its row (${at})`);
    assert.ok(Math.abs(g.boxInCell[0] - g.boxInCell[1]) <= 1, `the row checkbox is centred in its column (${at})`);
    assert.ok(g.overflow <= 0, `no horizontal page scroll (${at})`);
    if (g.description) for (const control of g.controls) assert.ok(g.description.top >= control.bottom - 0.5 || g.description.bottom <= control.top + 0.5, `the description line does not overlap ${control.name} (${at})`);
  }
  const shot = async (name, width, height, preserveViewport = false) => {
    // A relation scene sets metrics before opening its focus-owned overlay. Reapplying them can alter that scene.
    if (!preserveViewport) await call("Emulation.setDeviceMetricsOverride", { width, height, deviceScaleFactor: 1, mobile: !viewportOnly && width < 700 });
    else {
      // The emulated viewport settles a frame or two after the override; the exact-size assertion below stays and still fails loudly.
      await wait(`innerWidth === ${width} && innerHeight === ${height}`).catch(error => { if (!/^UI readiness failed/.test(String(error?.message))) throw error; });
      const actual = await evaluate('({width:innerWidth,height:innerHeight})');
      if (actual.width !== width || actual.height !== height) {
        const overflow = await evaluate(`([...document.querySelectorAll('body *')].filter(e => e.getBoundingClientRect().right > ${width} + 1 && getComputedStyle(e).display !== 'none').map(e => ({element:e.tagName+'.'+e.className, text:e.textContent.slice(0,80), rect:e.getBoundingClientRect().toJSON()})))`);
        const bar = await evaluate(`[...document.querySelectorAll('.erp-client > .app-bar, .erp-client > .app-bar *, .erp-client > .app-navigation')].filter(e => e.getBoundingClientRect().width > 0).map(e => ({element:e.tagName+'.'+String(e.className).slice(0,40), l:Math.round(e.getBoundingClientRect().left), r:Math.round(e.getBoundingClientRect().right), w:getComputedStyle(e).width, minw:getComputedStyle(e).minWidth}))`);
        await writeFile(path.join(output, name + ".viewport-failure.json"), JSON.stringify({actual, expected:{width,height}, bar, overflow}, null, 2));
        const diagnostic = await call("Page.captureScreenshot", {format:"png"});
        await writeFile(path.join(output, name + ".viewport-failure.png"), Buffer.from(diagnostic.data,"base64"));
      }
      assert.deepEqual(actual, {width,height}, "preserved capture viewport already matches the requested size");
    }
    await evaluate('new Promise(resolve=>requestAnimationFrame(()=>requestAnimationFrame(resolve)))');
    // Palette loading and viewport changes can start finite CSS transitions. Capture their settled paint.
    await wait('document.getAnimations().every(animation => animation.effect?.getComputedTiming().iterations === Infinity || animation.playState === "finished" || animation.playState === "idle")');
    const image = await call("Page.captureScreenshot", { format: "png", captureBeyondViewport: false });
    const png = Buffer.from(image.data, "base64");
    const canvas = { width: png.readUInt32BE(16), height: png.readUInt32BE(20) };
    assert.deepEqual(canvas, { width, height }, "actual PNG canvas matches the requested capture");
    const viewport = await evaluate('({innerWidth,innerHeight,clientWidth:document.documentElement.clientWidth,clientHeight:document.documentElement.clientHeight,devicePixelRatio})');
    await writeFile(path.join(output, name + ".png"), png); screenshots.push({ name: name + ".png", ...canvas, viewport });
    if (officeBlue) {
      const measured = await evaluate(officeMeasurementExpression);
      await writeFile(path.join(output, name + ".measurements.json"), JSON.stringify(measured, null, 2) + "\n");
      if (measured.office) {
        for (const pair of measured.pairs) visualCheck(pair.ratio >= pair.minimum, `${name} semantic contrast: ${JSON.stringify(pair)}`);
        for (const text of measured.texts.filter(text => !text.disabled && text.unresolved.length === 0)) visualCheck(text.ratio >= 4.5, `${name} text contrast: ${JSON.stringify(text)}`);
        for (const cell of measured.controls.filter(control => control.element.startsWith('td.'))) visualCheck(parseFloat(cell.fontSize) >= 13, `${name} table data is readable: ${cell.fontSize}`);
        visualCheck(measured.scrollWidth <= width, `${name} has no page overflow`);
        const listAction = measured.controls.find(control => control.element.includes('action-toggle-btn'));
        if (name.includes('list-selected') && listAction) visualCheck(listAction.rect.height >= (width <= 768 ? 44 : 32), `${name} list action meets control size: ${listAction.rect.height}`);
        const heading = measured.controls.find(control => control.element.includes('addon-info-title'));
        if (heading) visualCheck(heading.rect.width >= Math.min(200, width / 2), `${name} addon heading has room for its long label`);
      }
    }
    const logos = await evaluate(`(() => { const images=[...document.querySelectorAll('.brand-logo-light,.brand-logo-dark')].filter(node=>{const r=node.getBoundingClientRect();return r.width>0 && r.height>0 && !node.closest('[inert]');});return images.map(node=>node.getAttribute('src')); })()`);
    const drawerCoversShell = await evaluate('document.querySelector(".mobile-drawer:not(.hidden)") !== null || document.querySelector(".command-palette-dialog") !== null');
    assert.equal(logos.length, drawerCoversShell ? 0 : 1, `${name} shows one brand logo unless the modal drawer makes the shell inert: ${JSON.stringify(logos)}`);
    appearanceChecks.push({ name, surfaces: await appearance() });
  };
  // The grouped list, end to end in the compiled client: the Group By menu loads its choices, groups by a declared
  // item, group rows start folded, Enter opens one (its records are read with the group's domain), the page never
  // scrolls sideways, and the group row is as tall as a record row. Nested and temporal groups survive reload;
  // the probe removes its grouping before returning to the remaining scenes.
  // The study compares the same mounted surface at desktop, laptop, tablet and phone widths.
  // Keep the previous viewport: later interaction assertions continue in their original context.
  const qualityShots = async scene => {
    const previous = await evaluate('({width:innerWidth,height:innerHeight,mobile:matchMedia("(pointer: coarse)").matches})');
    for (const [width, height] of [[1440,900],[1280,900],[768,1024],[390,844]]) {
      await shot(`fixture-quality-${scene}-${width}`, width, height);
      assert.equal(await evaluate('document.documentElement.scrollWidth <= innerWidth'), true, `${scene} page fits ${width}`);
    }
    await call("Emulation.setDeviceMetricsOverride", { ...previous, deviceScaleFactor: 1 });
    await evaluate('new Promise(resolve=>requestAnimationFrame(()=>requestAnimationFrame(resolve)))');
  };
  const groupedListProbe = async prefix => {
    const savedPriority = priorityValue; priorityValue = "normal";
    await evaluate('[...document.querySelectorAll(".client-notification-close")].forEach(button => button.click())');
    try {
      await call("Emulation.setDeviceMetricsOverride", { width: 1440, height: 900, deviceScaleFactor: 1, mobile: false });
      await evaluate('document.querySelector(".search-menu-toggle").click()');
      await wait('document.querySelectorAll(".erp-groupby-section .erp-search-menu-item").length === 2');
      assert.equal(await evaluate('document.querySelector(".erp-groupby-section .erp-search-menu-item").textContent'), "Priority");
      assert.match(await evaluate('document.querySelector(".erp-groupby-section").textContent'), /Created by month/);
      assert.deepEqual(await evaluate('[...document.querySelectorAll(".erp-custom-group-select option")].map(option => option.textContent)'), ["Add Custom Group", "Name", "Priority", "Year", "Quarter", "Month", "Week", "Day", "Print run"]);
      await evaluate('document.querySelector(".erp-groupby-section .erp-search-menu-item").click()');
      await wait('document.querySelectorAll(".list-group-row").length === 3');
      assert.deepEqual(groupReads.at(-1).groupby, ["priority"]);
      assert.equal(await evaluate('[...document.querySelectorAll(".list-group-toggle")].map(button => button.getAttribute("aria-expanded")).join()'), "false,false,false");
      assert.equal(await evaluate('document.querySelectorAll(".list-table tbody tr[data-id]").length'), 0, "groups start folded");
      assert.equal(await evaluate('document.querySelector(".erp-facet-groupBy .erp-facet-remove").getAttribute("aria-label")'), "Remove grouping by Priority");
      await call("Input.dispatchKeyEvent", { type: "keyDown", key: "Escape", code: "Escape" });
      await wait('document.querySelector(".erp-search-menu-panel").style.display === "none"');
      await shot(`${prefix}-grouped-folded-1440`, 1440, 900);
      await evaluate('document.querySelector(".list-group-toggle").focus()');
      await call("Input.dispatchKeyEvent", { type: "keyDown", key: "Enter", code: "Enter", windowsVirtualKeyCode: 13, text: "\r" });
      await call("Input.dispatchKeyEvent", { type: "keyUp", key: "Enter", code: "Enter", windowsVirtualKeyCode: 13 });
      await wait('document.querySelector(".list-group-toggle").getAttribute("aria-expanded") === "true" && document.querySelectorAll(".list-table tbody tr[data-id]").length >= 1');
      assert.deepEqual(pageReads.at(-1).domain, [["priority", "=", "normal"]], "an opened group reads its own records");
      const heights = await evaluate('({ group: document.querySelector(".list-group-row").getBoundingClientRect().height, record: document.querySelector(".list-table tbody tr[data-id]").getBoundingClientRect().height })');
      assert.equal(heights.group, heights.record, `a group row is as tall as a record row: ${JSON.stringify(heights)}`);
      await shot(`${prefix}-grouped-open-1440`, 1440, 900);
      for (const [width, height] of [[1024, 900], [800, 1024], [390, 844]]) {
        await call("Emulation.setDeviceMetricsOverride", { width, height, deviceScaleFactor: 1, mobile: !viewportOnly && width < 700 });
        await evaluate('new Promise(resolve=>requestAnimationFrame(()=>requestAnimationFrame(resolve)))');
        assert.equal(await evaluate('document.documentElement.scrollWidth <= innerWidth'), true, `${prefix} grouped list fits ${width}`);
        await shot(`${prefix}-grouped-open-${width}`, width, height);
      }
      await evaluate('document.querySelector(".list-group-toggle").focus()');
      await call("Input.dispatchKeyEvent", { type: "keyDown", key: "ArrowLeft", code: "ArrowLeft" });
      await wait('document.querySelector(".list-group-toggle").getAttribute("aria-expanded") === "false" && document.querySelectorAll(".list-table tbody tr[data-id]").length === 0');
      // A second dimension loads groups under its parent; the state survives a real reload.
      await evaluate('document.querySelector(".search-menu-toggle").click()');
      await evaluate('(() => { const s=document.querySelector(".erp-custom-group-select"); s.value="name"; s.dispatchEvent(new Event("change")); })()');
      await wait('JSON.parse(new URL(location.href).searchParams.get("search_groupby"))?.by?.length === 2');
      await call("Input.dispatchKeyEvent", { type: "keyDown", key: "Escape", code: "Escape" });
      await evaluate('document.querySelector(".list-group-toggle").click()');
      await wait('document.querySelectorAll(".list-group-row").length === 4');
      assert.deepEqual(groupReads.at(-1).groupby, ["name"]);
      await shot(`${prefix}-grouped-nested-1440`, 1440, 900);
      // The old page also shows four rows and one open group: a marker that only the old document carries says the reload happened.
      await evaluate('window.__beforeReload = true');
      await call("Page.reload");
      await wait('window.__beforeReload === undefined && document.querySelectorAll(".list-group-row").length === 4 && document.querySelector(".erp-facet-groupBy .erp-facet-remove") !== null');
      assert.equal(await evaluate('JSON.parse(new URL(location.href).searchParams.get("search_groupby")).open.length'), 1);
      await evaluate('document.querySelector(".erp-facet-groupBy .erp-facet-remove").click()');
      await wait('document.querySelector(".list-group-row") === null');
      await evaluate('document.querySelector(".search-menu-toggle").click()');
      await wait('document.querySelectorAll(".erp-groupby-section .erp-search-menu-item").length === 2');
      await evaluate('[...document.querySelectorAll(".erp-groupby-section .erp-search-menu-item")].find(b=>b.textContent.includes("Created by month")).click()');
      await wait('document.querySelector(".list-group-label")?.textContent === "November 2026"');
      await call("Input.dispatchKeyEvent", { type: "keyDown", key: "Escape", code: "Escape" });
      await shot(`${prefix}-grouped-month-390`, 390, 844);
      await evaluate('document.querySelector(".erp-facet-groupBy .erp-facet-remove").click()');
      await wait('document.querySelector(".list-group-row") === null');
    } finally { priorityValue = savedPriority; }
    await call("Emulation.setDeviceMetricsOverride", { width: 1440, height: 900, deviceScaleFactor: 1, mobile: false });
    await call("Page.reload");
    await wait('document.querySelector(".list-table tbody tr[data-id]") !== null && document.querySelector(".list-group-row") === null');
  };
  // The pivot, end to end in the compiled client: reached from the view switcher of its action, one grouped read, the table with totals;
  // a header expands by a field chosen from its menu (one read narrowed to that header, focus stays on it); the keyboard folds and asks again;
  // the rows are sorted by a measure and the axes flipped; each state is captured at 1440 and 390, and the page never scrolls sideways.
  const pivotProbe = async prefix => {
    const back = await evaluate('location.pathname + location.search'); // the scenes that follow expect the page they had
    const desktop = () => call("Emulation.setDeviceMetricsOverride", { width: 1440, height: 900, deviceScaleFactor: 1, mobile: false });
    const phone = async name => {
      await call("Emulation.setDeviceMetricsOverride", { width: 390, height: 844, deviceScaleFactor: 1, mobile: !viewportOnly });
      await evaluate('new Promise(resolve=>requestAnimationFrame(()=>requestAnimationFrame(resolve)))');
      assert.equal(await evaluate('document.documentElement.scrollWidth <= innerWidth'), true, `${prefix} pivot ${name} fits 390`);
      await shot(`${prefix}-pivot-${name}-390`, 390, 844);
      await desktop();
    };
    const key = async (name) => call("Input.dispatchKeyEvent", { type: "keyDown", key: name, code: name });
    const rowButton = label => `[...document.querySelectorAll('.pivot-row-header .pivot-toggle')].find(button => button.textContent.trim() === ${JSON.stringify(label)})`;
    const rowHeaders = () => evaluate('[...document.querySelectorAll(".pivot-row-header")].map(cell => cell.textContent.trim()).join("|")');
    await desktop();
    await call("Page.navigate", { url: base + "/erp/action-25" });
    await wait('document.querySelector(".list-table tbody tr[data-id]") !== null');
    await evaluate('document.querySelector(\'.view-btn[aria-label="pivot view"]\').click()');
    await wait('document.querySelector(".pivot-table") !== null');
    assert.deepEqual(pivotReads.at(-1).grouping_sets, [[], ["format"], ["priority"], ["priority", "format"]]);
    assert.deepEqual(pivotReads.at(-1).aggregates, ["__count", "print_run:sum"]);
    assert.equal(await evaluate('document.querySelector(".pivot-row-total .pivot-toggle").getAttribute("aria-expanded")'), "true");
    assert.equal(await rowHeaders(), "Total|Normal|High|Draft|None");
    assert.equal(await evaluate('document.querySelector(".pivot-row-total .pivot-cell:last-child").textContent'), "196,010");
    assert.equal(await evaluate('getComputedStyle(document.querySelector(".pivot-cell-link")).fontVariantNumeric.includes("tabular-nums")'), true, "figures are tabular");
    assert.equal(await evaluate('getComputedStyle(document.querySelector(".pivot-cell")).textAlign'), "end", "figures are aligned to the end of the cell");
    assert.equal(await evaluate('document.querySelector("[role=toolbar]").getAttribute("aria-label")'), "Main actions");
    await shot(`${prefix}-pivot-1440`, 1440, 900);
    if (prefix === "fixture") await qualityShots("pivot");
    await phone("plain");
    // every header is a node (closed or opened), a level is indented 24px; the search's Group By sets the rows.
    assert.equal(await evaluate('document.querySelector(".pivot-row-total .pivot-row-header").classList.contains("pivot-node-opened")'), true);
    assert.equal(await evaluate(`${rowButton("Normal")}.closest("th").classList.contains("pivot-node-closed")`), true);
    await evaluate('new Promise(resolve=>requestAnimationFrame(()=>requestAnimationFrame(resolve)))'); // the desktop width is applied
    assert.equal(await evaluate(`getComputedStyle(${rowButton("Normal")}.closest("th")).paddingInlineStart`), "36px", "a row level is indented 24px");
    await evaluate('document.querySelector(".search-menu-toggle").click()');
    await wait('document.querySelector(".erp-custom-group-select") !== null');
    await evaluate('(() => { const select = document.querySelector(".erp-custom-group-select"); select.value = "pub_year"; select.dispatchEvent(new Event("change")); })()');
    await wait('document.querySelector(".erp-facet-groupBy") !== null');
    await wait(`[...document.querySelectorAll(".pivot-row-header")].map(cell => cell.textContent.trim()).join("|") === "Total|1974|1980|1951|2005|2006|None"`);
    assert.deepEqual(pivotReads.at(-1).grouping_sets, [[], ["format"], ["pub_year"], ["pub_year", "format"]]);
    assert.equal(await evaluate('new URL(location.href).searchParams.get("pivot_rows")'), "pub_year");
    await shot(`${prefix}-pivot-groupby-1440`, 1440, 900);
    await evaluate('document.querySelector(".erp-facet-groupBy .erp-facet-remove").click()');
    await wait(`[...document.querySelectorAll(".pivot-row-header")].map(cell => cell.textContent.trim()).join("|") === "Total|Normal|High|Draft|None"`);
    assert.equal(await evaluate('new URL(location.href).searchParams.has("pivot_rows")'), false, "the arch's rows leave the address clean");
    await key("Escape");
    await wait('document.querySelector(".erp-search-menu-panel").style.display === "none"');
    // Expand Normal by the publication year from the menu: one read, narrowed to the header, and the header keeps focus.
    await evaluate(`${rowButton("Normal")}.click()`);
    await wait('document.querySelector(".pivot-field-menu.open") !== null');
    assert.equal(await evaluate('document.activeElement.closest(".pivot-field-menu") !== null'), true, "the menu takes focus");
    await shot(`${prefix}-pivot-menu-1440`, 1440, 900);
    await evaluate('[...document.querySelectorAll(".pivot-field-menu [role=menuitem]")].find(item => item.textContent === "Publication year").click()');
    await wait('document.querySelectorAll(".pivot-table tbody tr").length === 7');
    assert.deepEqual(pivotReads.at(-1).domain, [["priority", "=", "normal"]]);
    assert.deepEqual(pivotReads.at(-1).grouping_sets, [["priority", "pub_year"], ["priority", "pub_year", "format"]]);
    assert.equal(await rowHeaders(), "Total|Normal|1974|1980|High|Draft|None");
    assert.equal(await evaluate('document.activeElement.dataset.pivotHeader'), 'row:"normal"', "the expanded header keeps focus");
    assert.equal(await evaluate('new URL(location.href).searchParams.get("pivot_rows")'), "priority,pub_year");
    await shot(`${prefix}-pivot-nested-1440`, 1440, 900);
    await phone("nested");
    // The keyboard: ArrowLeft folds Normal, which was the only header using the publication year, so the next ArrowRight asks again; Escape returns to the header.
    await key("ArrowLeft");
    await wait('document.querySelectorAll(".pivot-table tbody tr").length === 5');
    assert.equal(await evaluate('document.activeElement.dataset.pivotHeader'), 'row:"normal"', "a fold keeps the focus on the header");
    await key("ArrowRight");
    await wait('document.querySelector(".pivot-field-menu.open") !== null');
    await key("Escape");
    await wait('document.querySelector(".pivot-field-menu.open") === null');
    assert.equal(await evaluate('document.activeElement.dataset.pivotHeader'), 'row:"normal"', "Escape returns to the header");
    await evaluate(`${rowButton("Normal")}.click()`);
    await wait('document.querySelector(".pivot-field-menu.open") !== null');
    await evaluate('[...document.querySelectorAll(".pivot-field-menu [role=menuitem]")].find(item => item.textContent === "Publication year").click()');
    await wait('document.querySelectorAll(".pivot-table tbody tr").length === 7');
    // Sort by a measure column, then flip.
    await evaluate('document.querySelector(".pivot-head-measures th .pivot-sort").click()');
    await wait('document.querySelector(".pivot-head-measures th").getAttribute("aria-sort") === "ascending"');
    // Ascending by the first column (hardcover): Draft and None have none (0, server order), then High 800, then Normal 12,000 with its own years sorted too.
    assert.equal(await rowHeaders(), "Total|Draft|None|High|Normal|1980|1974");
    await shot(`${prefix}-pivot-sorted-1440`, 1440, 900);
    await phone("sorted");
    await evaluate('document.querySelector(".pivot-tool").click()');
    await wait('document.querySelectorAll(".pivot-table tbody tr").length === 4');
    assert.equal(await evaluate('new URL(location.href).searchParams.get("pivot_cols")'), "priority,pub_year");
    assert.equal(await rowHeaders(), "Total|Hardcover|Paperback|E-book");
    assert.equal(await evaluate('document.documentElement.scrollWidth <= innerWidth'), true, `${prefix} flipped pivot fits 1440`);
    await shot(`${prefix}-pivot-flipped-1440`, 1440, 900);
    await phone("flipped");
    await desktop();
    await call("Page.navigate", { url: base + back });
    await wait('document.querySelector(".list-table tbody tr[data-id]") !== null');
  };
  const pointerClick = async selector => {
    const at = await evaluate(`(() => { const r = document.querySelector(${JSON.stringify(selector)}).getBoundingClientRect(); return { x: r.x + r.width / 2, y: r.y + r.height / 2 }; })()`);
    await call("Input.dispatchMouseEvent", { type: "mouseMoved", x: at.x, y: at.y });
    await call("Input.dispatchMouseEvent", { type: "mousePressed", x: at.x, y: at.y, button: "left", clickCount: 1 });
    await call("Input.dispatchMouseEvent", { type: "mouseReleased", x: at.x, y: at.y, button: "left", clickCount: 1 });
  };
  const settingsPanelProbes = async () => {
    const open = async () => { await call("Page.navigate", { url: base + "/erp/action-21?menu=11" }); await wait('document.querySelector(`[data-setting-key="base.company_info"]`) !== null'); };
    await open();
    assert.equal(await evaluate('document.querySelector(".settings-app-tab").dataset.app'), "base");
    assert.equal(await evaluate('document.querySelector(".settings-app-name").textContent'), "Settings");
    assert.equal(await evaluate('Boolean(document.querySelector(".settings-app-tab .settings-app-icon [data-icon=settings]"))'), true);
    for (const [width, height] of [[1440,900],[800,1024],[390,844]]) {
      await shot(`settings-general-${width}`, width, height);
      assert.equal(await evaluate('document.documentElement.scrollWidth <= innerWidth'), true, `General settings fit ${width}`);
      const links = await evaluate('([...document.querySelectorAll("[data-app-section=base] .settings-action-link")].map(node=>({type:node.type,label:node.textContent.trim(),height:node.getBoundingClientRect().height})))');
      assert.equal(links.length, 4); assert.ok(links.every(link => link.type === "button" && link.height >= (width < 700 ? 44 : 24)));
    }
    await call("Emulation.setDeviceMetricsOverride", { width:1440, height:900, deviceScaleFactor:1, mobile:false });
    await evaluate('document.querySelector(`[data-setting-key="base.manage_users"] button`).focus()');
    await call("Input.dispatchKeyEvent", { type:"keyDown", key:"Enter", code:"Enter", windowsVirtualKeyCode:13, text:"\r" });
    await call("Input.dispatchKeyEvent", { type:"keyUp", key:"Enter", code:"Enter", windowsVirtualKeyCode:13 });
    await wait('location.pathname.includes("action-130") && document.querySelector(".list-table") !== null');
    await open();
    await evaluate('fetch("/fixture/deny-settings-link",{method:"POST",body:"{}"}).then(response=>{if(!response.ok)throw new Error("Settings denial injection failed");return response.json();})');
    await evaluate('document.querySelector(`[data-setting-key="base.manage_companies"] button`).click()');
    await wait('document.querySelector(".client-error-dialog") !== null');
    assert.equal(await evaluate('document.querySelector("#client-error-message").textContent'), "Settings company action refused by fixture.");
    assert.equal(await evaluate('Boolean(document.querySelector(".settings-form"))'), true);
    await evaluate('document.querySelector("[data-error-close]").click()');
    await evaluate('document.querySelector(`[data-setting-key="base.manage_companies"] button`).click()');
    await wait('location.pathname.includes("action-132") && document.querySelector(".list-table") !== null');
    await open();
    await evaluate('document.querySelector(`[data-setting-key="base.company_update"] button`).click()');
    await wait('location.pathname.includes("action-132") && new URLSearchParams(location.search).get("id") === "1" && document.querySelector(".record-form") !== null');
    await open();
    await evaluate('document.querySelector(".settings-app-tab[data-app=acceptance_notes]").click()');
    for (const [width,height] of [[1440,900],[800,1024],[390,844]]) await shot(`settings-mixed-${width}`, width, height);
    await evaluate('document.querySelector(".settings-toggle input").click(); document.querySelector(".settings-app-tab[data-app=base]").click(); document.querySelector(`[data-setting-key="base.manage_users"] button`).click()');
    await wait('document.querySelector(".choice-dialog") !== null');
    await evaluate('[...document.querySelectorAll(".choice-dialog button")].find(button=>button.textContent==="Cancel").click()');
    assert.equal(await evaluate('location.pathname.includes("action-21") && document.querySelector(".settings-toggle input").checked === false'), true);
  };
  const shellProbes = async () => {
  await settingsPanelProbes();
  await call("Page.navigate", { url: base + "/erp/action-21?menu=11" });
  await wait('document.querySelector(".settings-toggle input") !== null');
  await evaluate('document.querySelector(".settings-app-tab[data-app=acceptance_notes]").click()');
  if (officeBlue) await wait('document.documentElement.dataset.officeTheme === "office-blue"');
  // Shell measurements (UI-SHELL-SETTINGS): top bar spacing and centres, Technical dropdown panel, user menu and the Settings
  // page, per width. Everything is measured in the page (getBoundingClientRect); nothing is typed in.
  for (const [width, height] of [[1440, 900], [1280, 800], [1024, 768], [800, 900], [390, 844]]) {
    await call("Emulation.setDeviceMetricsOverride", { width, height, deviceScaleFactor: 1, mobile: width < 700 });
    await evaluate('new Promise(resolve=>requestAnimationFrame(()=>requestAnimationFrame(resolve)))');
    const probe = String.raw`(() => {
      const box = element => { const r = element.getBoundingClientRect(); return { x: r.x, y: r.y, right: r.right, bottom: r.bottom, width: r.width, height: r.height, cy: r.y + r.height / 2 }; };
      const visible = element => { const r = element.getBoundingClientRect(); return r.width > 0 && r.height > 0 && getComputedStyle(element).visibility !== "hidden"; };
      const rgb = value => { const m = value.match(/rgba?\(([^)]+)\)/); if (!m) return null; const p = m[1].split(/[ ,/]+/).filter(Boolean).map(Number); return { r: p[0], g: p[1], b: p[2], a: p.length > 3 ? p[3] : 1 }; };
      const lum = c => { const f = v => { v /= 255; return v <= .03928 ? v / 12.92 : ((v + .055) / 1.055) ** 2.4; }; return .2126 * f(c.r) + .7152 * f(c.g) + .0722 * f(c.b); };
      const background = element => { for (let node = element; node; node = node.parentElement) { const c = rgb(getComputedStyle(node).backgroundColor); if (c && c.a > .5) return c; } return { r: 255, g: 255, b: 255, a: 1 }; };
      const contrast = element => { const fg = rgb(getComputedStyle(element).color), bg = background(element); if (!fg) return null; const a = lum(fg), b = lum(bg); return +((Math.max(a, b) + .05) / (Math.min(a, b) + .05)).toFixed(2); };
      const bar = [".app-bar .brand", ".app-switcher-btn", ".app-brand-btn", ".app-sections > .nav-item", ".app-sections > .nav-dropdown > .nav-dropdown-toggle", ".systray .company-menu-button", ".systray .user-button"]
        .flatMap(selector => [...document.querySelectorAll(selector)].filter(visible).map(element => ({ selector, text: element.textContent.trim().slice(0, 24), ...box(element), fontSize: getComputedStyle(element).fontSize, fontWeight: getComputedStyle(element).fontWeight })));
      const ordered = [...bar].sort((a, b) => a.x - b.x);
      const gaps = ordered.slice(1).map((item, index) => ({ from: ordered[index].selector + ":" + ordered[index].text, to: item.selector + ":" + item.text, gap: +(item.x - ordered[index].right).toFixed(1) }));
      const active = document.querySelector("[data-app-section=acceptance_notes]"), band = active.querySelector(".settings-section-title"), groupTitle = active.querySelector(".settings-block-title"), firstBlock = active.querySelector(".setting-block");
      const rhythm = band && groupTitle && firstBlock ? { bandHeight: +band.getBoundingClientRect().height.toFixed(1), bandToGroupTitle: +(groupTitle.getBoundingClientRect().top - band.getBoundingClientRect().bottom).toFixed(1), groupTitleToFirstBlock: +(firstBlock.getBoundingClientRect().top - groupTitle.getBoundingClientRect().bottom).toFixed(1), blocks: active.querySelectorAll(".setting-block").length, blockColumns: [...new Set([...active.querySelectorAll(".setting-block")].map(b => Math.round(b.getBoundingClientRect().left)))].length } : null;
      const search = document.querySelector(".settings-search-box"), container0 = document.querySelector(".control-panel");
      const searchCentre = search && container0 ? { searchCentreX: +((search.getBoundingClientRect().left + search.getBoundingClientRect().right) / 2).toFixed(1) } : null;
      const block = document.querySelector(".setting-block:has(.settings-toggle)");
      const check = block?.querySelector("input[type=checkbox]"), label = block?.querySelector(".setting-label-text");
      const container = document.querySelector(".settings-container");
      const offenders = [...document.querySelectorAll("body *")].filter(e => e.getBoundingClientRect().right > Math.min(innerWidth, document.documentElement.clientWidth) + 1 && getComputedStyle(e).position !== "fixed" && visible(e)).slice(0, 8).map(e => e.tagName.toLowerCase() + "." + String(e.className).slice(0, 50) + "@" + Math.round(e.getBoundingClientRect().right));
      return { rhythm, searchCentre, viewport: { innerWidth, innerHeight }, horizontalScroll: document.documentElement.scrollWidth > innerWidth, offenders,
        bar: { items: bar, gaps, centres: [...new Set(bar.map(item => +item.cy.toFixed(1)))] },
        settings: { checkboxCentreMinusLabelCentre: check && label ? +(box(check).cy - box(label).cy).toFixed(1) : null,
          containerBottom: container ? +container.getBoundingClientRect().bottom.toFixed(1) : null, rail: [...document.querySelectorAll(".settings-app-tab")].map(tab => ({ text: tab.textContent, hasTile: Boolean(tab.querySelector(".settings-app-icon")) })),
          scopeSelectCount: document.querySelectorAll(".settings-toolbar select, .control-panel select").length, reloadButtons: [...document.querySelectorAll(".settings-toolbar button")].filter(b => b.textContent.trim() === "Reload").length } };
    })()`;
    const entry = { width, height, ...(await evaluate(probe)) };
    await shot(`shell-settings-${width}`, width, height);
    const toggle = await evaluate('(() => { const t = [...document.querySelectorAll(".nav-dropdown-toggle")].find(b => b.textContent.trim() === "Technical"); return Boolean(t && t.getBoundingClientRect().width > 0); })()');
    if (toggle) {
      await evaluate('[...document.querySelectorAll(".nav-dropdown-toggle")].find(b => b.textContent.trim() === "Technical").dataset.probe = "technical"');
      await pointerClick('[data-probe=technical]');
      await wait('document.querySelector(".nav-dropdown-menu.open") !== null');
      entry.pointerOpen = await evaluate('({ ringed: document.querySelector(".nav-dropdown-menu.open :focus-visible") !== null, focusInPanel: document.querySelector(".nav-dropdown-menu.open")?.contains(document.activeElement) })');
      assert.deepEqual(entry.pointerOpen, { ringed: false, focusInPanel: false }, `opening the Technical menu with the pointer shows no focus ring at ${width}px`);
      entry.technical = await evaluate(String.raw`(() => {
        const panel = document.querySelector(".nav-dropdown-menu.open"), r = panel.getBoundingClientRect(), s = getComputedStyle(panel);
        const header = panel.querySelector(".dropdown-header"), item = panel.querySelector(".dropdown-item");
        const rgb = value => { const m = value.match(/rgba?\(([^)]+)\)/); const p = m[1].split(/[ ,/]+/).filter(Boolean).map(Number); return { r: p[0], g: p[1], b: p[2] }; };
        const lum = c => { const f = v => { v /= 255; return v <= .03928 ? v / 12.92 : ((v + .055) / 1.055) ** 2.4; }; return .2126 * f(c.r) + .7152 * f(c.g) + .0722 * f(c.b); };
        const ratio = element => { const a = lum(rgb(getComputedStyle(element).color)), b = lum(rgb(s.backgroundColor)); return +((Math.max(a, b) + .05) / (Math.min(a, b) + .05)).toFixed(2); };
        const rows = [...panel.querySelectorAll(".dropdown-item")].map(row => row.getBoundingClientRect().height);
        return { top: r.top, bottom: r.bottom, left: r.left, right: r.right, height: r.height, bottomMargin: innerHeight - r.bottom, scrollHeight: panel.scrollHeight, clientHeight: panel.clientHeight, overflowY: s.overflowY, scrolls: panel.scrollHeight > panel.clientHeight,
          headerTransform: getComputedStyle(header).textTransform, headerLetterSpacing: getComputedStyle(header).letterSpacing, headerFontSize: getComputedStyle(header).fontSize, itemFontSize: getComputedStyle(item).fontSize,
          headerContrast: ratio(header), itemContrast: ratio(item), headerIndentX: header.getBoundingClientRect().x, firstItemTextIndent: parseFloat(getComputedStyle(item).paddingLeft), rowHeightsDistinct: [...new Set(rows.map(h => +h.toFixed(1)))], insideViewport: r.right <= innerWidth && r.left >= 0 };
      })()`);
      await shot(`shell-technical-${width}`, width, height);
      await pointerClick('[data-probe=technical]');
      await evaluate('document.querySelector("[data-probe=technical]").focus()');
      await call("Input.dispatchKeyEvent", { type: "keyDown", key: "Enter", code: "Enter", windowsVirtualKeyCode: 13, text: "\r" });
      await call("Input.dispatchKeyEvent", { type: "keyUp", key: "Enter", code: "Enter", windowsVirtualKeyCode: 13 });
      await wait('document.querySelector(".nav-dropdown-menu.open") !== null');
      entry.keyboardOpen = await evaluate('({ ringed: document.querySelector(".nav-dropdown-menu.open .dropdown-item:focus-visible") !== null })');
      assert.deepEqual(entry.keyboardOpen, { ringed: true }, `opening the Technical menu with the keyboard shows the focus ring at ${width}px`);
      await call("Input.dispatchKeyEvent", { type: "keyDown", key: "ArrowDown", code: "ArrowDown", windowsVirtualKeyCode: 40 });
      await call("Input.dispatchKeyEvent", { type: "keyUp", key: "ArrowDown", code: "ArrowDown", windowsVirtualKeyCode: 40 });
      assert.equal(await evaluate('document.activeElement.classList.contains("dropdown-item") && document.activeElement.matches(":focus-visible")'), true, "arrow keys keep roving focus with the ring");
      await call("Input.dispatchKeyEvent", { type: "keyDown", key: "Escape", code: "Escape", windowsVirtualKeyCode: 27 });
      await call("Input.dispatchKeyEvent", { type: "keyUp", key: "Escape", code: "Escape", windowsVirtualKeyCode: 27 });
      await wait('document.querySelector(".nav-dropdown-menu.open") === null');
    }
    if (await evaluate('document.querySelector(".company-menu-button") !== null')) {
      await pointerClick(".company-menu-button");
      await wait('document.querySelector(".company-dropdown.open") !== null');
      assert.equal(await evaluate('document.querySelector(".company-dropdown.open :focus-visible") === null'), true, `opening the company panel with the pointer shows no focus ring at ${width}px`);
      entry.companyMenu = await evaluate('(() => { const r = document.querySelector(".company-dropdown.open").getBoundingClientRect(), b = document.querySelector(".company-menu-button").getBoundingClientRect(), u = document.querySelector(".user-button").getBoundingClientRect(); return { top: r.top, bottom: r.bottom, left: r.left, right: r.right, insideViewport: r.right <= innerWidth && r.left >= 0 && r.bottom <= innerHeight, triggerCentreY: b.y + b.height / 2, userButtonCentreY: u.y + u.height / 2, triggerHeight: b.height, userButtonHeight: u.height, rows: document.querySelectorAll(".company-dropdown .company-row").length }; })()');
      await shot(`shell-company-${width}`, width, height);
      await pointerClick(".company-menu-button");
    }
    await pointerClick(".activity-menu-trigger");
    await wait('document.querySelector(".activity-group-list") !== null');
    assert.equal(await evaluate('document.querySelector(".activity-menu-trigger").classList.contains("is-failed") || document.body.textContent.includes("Activities could not be loaded")'), false, `the activity menu loaded without a failure notice at ${width}px`);
    entry.activityTargets = await evaluate('(() => { const R = n => n.getBoundingClientRect(), rows = [...document.querySelectorAll(".activity-group-item")].map(R); return { controls: [...document.querySelectorAll(".activity-menu-dropdown button.activity-count, .activity-menu-dropdown button.activity-group-title")].map(n => ({ w: R(n).width, h: R(n).height })), pitch: rows.length > 1 ? rows[1].top - rows[0].top : null, overlaps: [...document.querySelectorAll(".activity-menu-dropdown button.activity-count, .activity-menu-dropdown button.activity-group-title")].flatMap((n, i, all) => all.slice(i + 1).filter(m => { const a = R(n), b = R(m); return a.left < b.right - 0.01 && b.left < a.right - 0.01 && a.top < b.bottom - 0.01 && b.top < a.bottom - 0.01; }).map(m => `${n.textContent} / ${m.textContent}`)) }; })()');
    assert.ok(entry.activityTargets.controls.length > 0 && entry.activityTargets.controls.every(c => c.h >= 24 && c.w >= 24), `every activity count and name button is at least 24x24 px at ${width}px: ${JSON.stringify(entry.activityTargets)}`);
    assert.deepEqual(entry.activityTargets.overlaps, [], `no activity name or count button box overlaps another at ${width}px`);
    await shot(`shell-activity-menu-${width}`, width, height);
    await evaluate('document.querySelector(".activity-menu-trigger").dispatchEvent(new KeyboardEvent("keydown", { key: "Escape", bubbles: true, cancelable: true }))');
    await wait('document.querySelector(".activity-menu-dropdown.open") === null');
    await evaluate('document.querySelector(".user-button").click()');
    await wait('document.querySelector(".user-dropdown.open") !== null');
    entry.userMenu = await evaluate('(() => { const r = document.querySelector(".user-dropdown.open").getBoundingClientRect(); return { top: r.top, bottom: r.bottom, left: r.left, right: r.right, insideViewport: r.right <= innerWidth && r.left >= 0 && r.bottom <= innerHeight }; })()');
    await shot(`shell-usermenu-${width}`, width, height);
    if (width === 1440 || width === 390) {
      await evaluate('document.querySelector(".user-dropdown-preferences").click()');
      await wait('document.querySelector("dialog[open]") !== null');
      await evaluate('new Promise(resolve => setTimeout(resolve, 500))');
      entry.preferences = await evaluate(`(() => { const d = document.querySelector("dialog[open]"), r = d.getBoundingClientRect(), s = getComputedStyle(d), tabs = d.querySelector("[role=tablist]"), foot = [...d.querySelectorAll("button")].map(b => b.textContent.trim()); return { rect: { x: r.x, y: r.y, width: r.width, height: r.height }, radius: s.borderTopLeftRadius, shadow: s.boxShadow !== "none", title: d.querySelector("h2")?.textContent, dialogClass: d.className, dialogText: d.textContent.trim().slice(0, 160), hasTabs: Boolean(tabs), tabLabels: tabs ? [...tabs.querySelectorAll("[role=tab]")].map(t => t.textContent.trim()) : [], buttonsInOrder: foot, hasAvatar: Boolean(d.querySelector(".user-avatar, .avatar, img")), fieldLabels: [...d.querySelectorAll("label, .field-label > span")].map(n => n.textContent.trim()).filter(Boolean).slice(0, 8), insideViewport: r.right <= innerWidth && r.bottom <= innerHeight }; })()`);
      await shot(`shell-preferences-${width}`, width, height);
      assert.equal(await evaluate('document.documentElement.scrollWidth <= innerWidth'), true, `Preferences dialog adds no horizontal page scroll at ${width}px`);
      if (width === 1440) {
        await call("Emulation.setDeviceMetricsOverride", { width: 768, height: 1024, deviceScaleFactor: 1, mobile: false });
        await shot("shell-preferences-768", 768, 1024);
        assert.equal(await evaluate('document.documentElement.scrollWidth <= innerWidth'), true, "Preferences dialog adds no horizontal page scroll at 768px");
        await call("Emulation.setDeviceMetricsOverride", { width, height, deviceScaleFactor: 1, mobile: false });
      }
      entry.preferences.controls = await evaluate(`(() => [...document.querySelectorAll('dialog[open] .form-tab-panel:not([hidden]) select')].map(control => { const r = control.getBoundingClientRect(); const hit = document.elementFromPoint(r.x + r.width / 2, r.y + r.height / 2); const style = getComputedStyle(control), canvas = document.createElement("canvas").getContext("2d"); canvas.font = style.font; const requiredWidth = canvas.measureText(control.selectedOptions[0]?.textContent ?? "").width + parseFloat(style.paddingLeft) + parseFloat(style.paddingRight); const ancestors = []; for (let p = control; p && p !== document.body; p = p.parentElement) { const b = p.getBoundingClientRect(), s = getComputedStyle(p); ancestors.push({ className: p.className, x: b.x, y: b.y, width: b.width, height: b.height, display: s.display, overflow: s.overflow, columns: s.gridTemplateColumns }); } return { name: control.name, visible: r.width >= requiredWidth && r.height > 0 && (hit === control || control.contains(hit)), requiredWidth, ancestors }; }))()`);
      assert.ok(entry.preferences.controls.length > 0 && entry.preferences.controls.every(control => control.visible), `Preferences selection controls are visible and reachable at ${width}px: ${JSON.stringify(entry.preferences.controls)}`);
      // An invalid e-mail keeps its message visible under the row (typed, not committed: the dialog stays clean).
      await evaluate('(() => { const input = document.querySelector(".wizard-dialog .form-title-row input[type=email]"); input.value = "foo"; input.dispatchEvent(new Event("input", { bubbles: true })); })()');
      assert.equal(await evaluate('(() => { const e = document.querySelector(".wizard-dialog .form-title-row .field-error"); const r = e.getBoundingClientRect(); return !e.hidden && e.textContent.trim() !== "" && r.width > 20 && r.height > 8; })()'), true, `the invalid e-mail message is visible at ${width}px`);
      await shot(`shell-preferences-invalid-${width}`, width, height);
      await evaluate('[...document.querySelectorAll(".wizard-dialog button")].find(b => b.textContent.trim() === "Cancel").click()');
      await wait('document.querySelector(".wizard-dialog h2") === null');
    } else await evaluate('document.querySelector(".user-button").click()');
    shellMeasurements.push(entry);
  }
    if (!officeBlue) {
      await call("Page.navigate", { url: base + "/erp/action-21?menu=15" });
      await wait('document.querySelector(".settings-toggle input") !== null');
      await call("Emulation.setDeviceMetricsOverride", { width: 1440, height: 900, deviceScaleFactor: 1, mobile: false });
      shellMeasurements.push({ width: 1440, usersActive: await evaluate('({ current: [...document.querySelectorAll("[aria-current=page]")].map(n => n.textContent.trim()), pillBackground: getComputedStyle(document.querySelector(".nav-item[aria-current=page]") ?? document.body).backgroundColor })') });
      await shot("shell-users-active-1440", 1440, 900);
      await call("Emulation.setDeviceMetricsOverride", { width: 390, height: 844, deviceScaleFactor: 1, mobile: !viewportOnly });
    }
  await writeFile(path.join(output, "shell-measurements.json"), JSON.stringify({ theme, office_blue: officeBlue, measurements: shellMeasurements }, null, 2) + "\n");
  };
    const scene = async (name, selector, scroll = false) => {
      await wait(`document.querySelector(${JSON.stringify(selector)}) !== null`);
      for (const [width, height] of [[1440, 900], [1280, 900], [1024, 900], [800, 1024], [390, 844]]) {
        await call("Emulation.setDeviceMetricsOverride", {width, height, deviceScaleFactor:1, mobile:!viewportOnly && width<700});
        await evaluate(scroll ? `document.querySelector(${JSON.stringify(selector)}).scrollIntoView({block:'center'})` : 'window.scrollTo(0,0)');
        await shot(`office-blue-${name}-${width}`, width, height, true);
        visualCheck(await evaluate('document.documentElement.scrollWidth <= innerWidth'), `${name} fits ${width}`);
      }
    };
  if (officeBlue) {
    await shellProbes();
    // The Apps screen lays out the whole client as a column; the phone bar and the page must still follow the shared grid.
    await call("Page.navigate", { url: base + "/erp/action-22" });
    await wait('document.querySelector(".addons-view") !== null');
    for (const [width, height] of [[768, 900], [390, 844]]) {
      await call("Emulation.setDeviceMetricsOverride", { width, height, deviceScaleFactor: 1, mobile: width < 500 });
      await wait(`innerWidth === ${width}`);
      const g = await assertPhoneBarRow(evaluate, "Office Blue Apps screen", width);
      g.children = await evaluate(`[...document.querySelector('.erp-client').children].map(e => ({ tag: e.tagName + '.' + String(e.className).slice(0, 30) + (e.id ? '#' + e.id : ''), top: Math.round(e.getBoundingClientRect().top), bottom: Math.round(e.getBoundingClientRect().bottom), position: getComputedStyle(e).position, row: getComputedStyle(e).gridRowStart }))`);
      assert.ok(g.mainBottom >= g.innerHeight - 1, `the Apps page fills the window: ${JSON.stringify(g)}`);
      await shot(`office-blue-apps-${width}`, width, height, true);
    }
    await call("Page.navigate", { url: base + "/erp/action-20?view_type=list" });
    await wait('document.documentElement.dataset.officeTheme === "office-blue" && document.querySelector(".list-table") !== null');
    assert.equal(await evaluate('document.querySelector(".app-bar .office-header-search input") !== null'), true, "resolved database theme mounts the functioning header search");
    assert.equal(await evaluate('document.querySelector(".company-menu-button")?.textContent'), "Rendering fixture company", "company has its own shared top-bar control");
    await wait('document.querySelector(".list-state-tabs")?.getAttribute("aria-busy") === "false"');
    assert.equal(await evaluate('document.querySelector("[data-control-categories] .list-state-tabs") !== null'), true, "category placement belongs to the extensible XML template");
    assert.equal(await evaluate('document.querySelector(".list-page-size .choice-dropdown-trigger").value'), "80", "declared default is selected");
    // The page, not the list, scrolls in Office Blue too (review S6): the same pinned-header assertions as the default theme.
    await assertListHeaderPinned("Office Blue list", 'document.querySelector(".list-table tbody tr") !== null && document.querySelector(".list-state-tabs")?.getAttribute("aria-busy") === "false"');
    await call("Emulation.setDeviceMetricsOverride", { width: 1440, height: 900, deviceScaleFactor: 1, mobile: false });
    await call("Page.reload");
    await wait('document.documentElement.dataset.officeTheme === "office-blue" && document.querySelector(".list-table") !== null && document.querySelector(".list-state-tabs")?.getAttribute("aria-busy") === "false"');
    for (const [width, height] of [[1440, 900], [1280, 900], [1024, 900], [800, 1024], [390, 844]]) {
      await shot(`office-blue-list-${width}`, width, height);
      // The panel's height reaches the sticky header's offset on the next resize notification, one frame after the layout settles.
      await wait("parseFloat(getComputedStyle(document.documentElement).getPropertyValue('--control-panel-height')) === document.querySelector('.control-panel-host')?.offsetHeight");
      assert.equal(await evaluate('document.documentElement.scrollWidth <= innerWidth'), true, `list fits ${width}`);
      await assertListGeometry('Office Blue list', width);
      const tableEdges = await evaluate(`(() => { const box=document.querySelector('.list-results'), table=box.querySelector('table'), head=table.querySelector('th');return { wide:box.hasAttribute('data-wide'), boxClient:box.clientWidth, tableOffset:table.offsetWidth, cols:[...table.querySelectorAll('col')].map(c=>c.style.width), tableStyle:table.getAttribute('style'), box:box.getBoundingClientRect().toJSON(), table:table.getBoundingClientRect().toJSON(), head:head.getBoundingClientRect().toJSON() }; })()`);
      if (!tableEdges.wide) visualCheck(tableEdges.table.right >= tableEdges.box.right - 2, `list fills available width at ${width}: ${JSON.stringify(tableEdges)}`);
      visualCheck(Math.abs(tableEdges.head.top - tableEdges.table.top) < 2, `header stays above records at ${width}: ${JSON.stringify(tableEdges)}`);
      const listLayout = await evaluate(`(() => {
        const bounds = selector => { const r=document.querySelector(selector).getBoundingClientRect(); return {x:r.x,y:r.y,width:r.width,height:r.height,right:r.right,bottom:r.bottom}; };
        return { title:bounds('.control-panel-list h1'), search:bounds('.control-panel-list [data-control-search]'), actions:bounds('.control-panel-list [data-control-actions]'), table:bounds('.list-results'), pager:bounds('.control-panel-list .pager'), category:bounds('.list-state-tabs'), header:bounds('.list-table thead tr'), row:bounds('.list-table tbody tr'), checkbox:bounds('.list-table input[type=checkbox]'), current:document.querySelector('.pager-menu [aria-current="page"]')?.textContent };
      })()`);
      listLayouts.push({width, height, ...listLayout});
      assert.ok(listLayout.search.bottom <= listLayout.table.y, `list table below search ${width}`);
      assert.ok(listLayout.pager.bottom <= listLayout.table.y, `list paging in control panel ${width}`);
      assert.equal(listLayout.current, "1", `actual current page ${width}`);
      if (width >= 1280) {
        assert.ok(listLayout.actions.right <= listLayout.title.x + 1, "New before title");
        assert.ok(listLayout.title.right <= listLayout.search.x + 1, "title before centered search");
      }
      if (width === 1440) {
        assert.equal(await evaluate('getComputedStyle(document.querySelector(".office-navigation")).display'), "flex");
        assert.equal(await evaluate('getComputedStyle(document.querySelector(".app-navigation")).backgroundColor'), "rgb(14, 30, 50)");
      } else if (width <= 768) {
        await evaluate('document.querySelector("[data-burger-menu]").click()');
        await wait('document.querySelector(".mobile-drawer")?.classList.contains("hidden") === false');
        assert.equal(await evaluate('document.activeElement.classList.contains("drawer-close-btn")'), true);
        await call("Input.dispatchKeyEvent", { type: "keyDown", key: "Escape", code: "Escape" });
        await wait('document.querySelector(".mobile-drawer")?.classList.contains("hidden") === true');
      }
      if (width > 768) visualCheck(await evaluate('getComputedStyle(document.querySelector(".office-navigation")).display === "flex"'), `Office navigation remains available above the shared drawer breakpoint at ${width}`);
    }
    await evaluate('(() => { const search=document.querySelector(".office-header-search input"); search.focus(); search.value="Notes";search.dispatchEvent(new Event("input",{bubbles:true})); })()');
    await wait('document.querySelector(".office-menu-option") !== null');
    await call("Input.dispatchKeyEvent",{type:"keyDown",key:"ArrowDown",code:"ArrowDown"});
    await scene("menu-search", ".office-menu-results");
    await call("Input.dispatchKeyEvent",{type:"keyDown",key:"Escape",code:"Escape"});
    await evaluate('document.querySelector(".office-header-search input").blur()');
    await evaluate('document.querySelector(".search-menu-toggle").click()');
    await wait('document.querySelector(".erp-search-menu-panel").style.display !== "none"');
    assert.equal(await evaluate('document.activeElement.closest(".erp-search-menu-panel") !== null'), true, "separate More filters control opens the existing accessible menu");
    await scene("search-menu", ".erp-search-menu-panel");
    await call("Input.dispatchKeyEvent", { type: "keyDown", key: "Escape", code: "Escape" });
    await wait('document.querySelector(".erp-search-menu-panel").style.display === "none"');
    let resumeRead;
    readPause = new Promise(resolve => { resumeRead = resolve; });
    try {
      await evaluate(`document.querySelector('[data-tab-token="value:high"]').click()`);
      await wait('document.querySelector(".list-state-tabs")?.getAttribute("aria-busy") === "true"');
      await scene("list-loading", '.list-state-tabs[aria-busy="true"]');
    } finally { readPause = undefined; resumeRead(); }
    await wait('document.querySelector(".list-results .empty-state") !== null');
    await scene("list-empty", ".list-results .empty-state");
    assert.deepEqual(pageReads.at(-1).domain, [["priority", "=", "high"]]);
    assert.deepEqual(groupDomains.at(-1), [], "counts exclude the category's own filter");
    assert.equal(await evaluate(`document.querySelector('[data-tab-token="all"] .list-tab-count').textContent`), "2");
    await evaluate(`document.querySelector('[data-tab-token="all"]').click()`);
    await wait('document.querySelector(".list-table tbody tr") !== null');
    await evaluate('document.querySelector(".pager-options").focus()');
    for (const key of ["ArrowDown", "End", "ArrowDown"]) await call("Input.dispatchKeyEvent", {type:"keyDown",key,code:key});
    assert.equal(await evaluate('document.activeElement.dataset.value'), "20", "nested page-size keyboard opens on its first choice");
    for (const key of ["ArrowDown", "ArrowDown"]) await call("Input.dispatchKeyEvent", {type:"keyDown",key,code:key});
    assert.equal(await evaluate('document.activeElement.dataset.value'), "50", "one key moves one nested choice");
    await call("Input.dispatchKeyEvent", {type:"keyDown",key:"Enter",code:"Enter",windowsVirtualKeyCode:13,text:"\r"});
    await call("Input.dispatchKeyEvent", {type:"keyUp",key:"Enter",code:"Enter",windowsVirtualKeyCode:13});
    await evaluate('document.body.dispatchEvent(new Event("pointerdown", {bubbles:true}))');
    await wait('document.querySelector(".list-state-tabs")?.getAttribute("aria-busy") === "false"');
    assert.equal(pageReads.at(-1).limit, 50); assert.equal(pageReads.at(-1).offset, 0);
    rejectNextCount = true;
    await evaluate(`document.querySelector('[data-tab-token="value:normal"]').click()`);
    await wait('document.querySelector(".list-load-error") !== null && document.querySelector(".client-error-dialog")?.open === true');
    assert.equal(await evaluate('document.querySelector(".pager-count").textContent'), "Results unavailable");
    assert.equal(await evaluate('document.querySelector(".pager-prev").disabled && document.querySelector(".pager-next").disabled && !document.querySelector(".pager-pages [aria-current]")'), true);
    errorLayouts.push({phase:"before_capture", ...await evaluate(`(() => { const d=document.querySelector('.client-error-dialog'),r=d.getBoundingClientRect(),s=getComputedStyle(d); return {open:d.open,x:r.x,y:r.y,width:r.width,height:r.height,position:s.position,top:s.top,visibility:s.visibility}; })()`)});
    assert.equal(await evaluate('document.querySelector(".list-table tbody tr") === null'), true, "failed count cannot leave stale records");
    assert.equal(await evaluate(`document.querySelector('[data-tab-token="all"] .list-tab-count').textContent`), "—");
    await shot("office-blue-list-count-failure", 1440, 900);
    await scene("count-error", ".client-error-dialog");
    errorLayouts.push({phase:"after_capture", ...await evaluate(`(() => { const d=document.querySelector('.client-error-dialog'),r=d.getBoundingClientRect(),s=getComputedStyle(d); return {open:d.open,x:r.x,y:r.y,width:r.width,height:r.height,position:s.position,top:s.top,visibility:s.visibility}; })()`)});
    if (!dialogProbe) assertDialogVisible(await dialogGeometry('.client-error-dialog'));
    if (dialogProbe) {
      errorLayouts.push({phase:"error_original_computed", ...await dialogGeometry('.client-error-dialog')});
      await centerProbeDialog('.client-error-dialog');
      await shot("diagnostic-error-explicit-center",1440,900);
      errorLayouts.push({phase:"error_centered_probe", ...await dialogGeometry('.client-error-dialog')});
      await evaluate('document.querySelector("[data-error-close]").click()');
      await wait('document.querySelector(".client-error-dialog") === null');
      await evaluate(`(() => {const d=document.createElement('dialog');d.id='native-dialog-probe';d.textContent='Isolated native dialog geometry probe';document.body.append(d);d.showModal();})()`);
      await shot("diagnostic-native-dialog-unstyled",1440,900);
      errorLayouts.push({phase:"native_unstyled", ...await dialogGeometry('#native-dialog-probe')});
      await centerProbeDialog('#native-dialog-probe');
      await shot("diagnostic-native-dialog-centered",1440,900);
      errorLayouts.push({phase:"native_centered_probe", ...await dialogGeometry('#native-dialog-probe')});
      await evaluate(`(() => {const d=document.querySelector('#native-dialog-probe');d.close();d.remove();})()`);
    }
    await call("Page.navigate", { url: base + "/erp/action-20?view_type=list" });
    await wait('document.querySelector(".list-state-tabs")?.getAttribute("aria-busy") === "false" && document.querySelector(".list-table tbody tr") !== null');
    await groupedListProbe("office-blue");
    await pivotProbe("office-blue");
    {
      await evaluate('document.querySelector(".list-table tbody input[type=checkbox]").click();document.querySelector(".action-toggle-btn").click()');
      await scene("list-selected", ".action-menu.open");
      await evaluate('document.querySelector(".action-delete-btn").click()');
      await wait('document.querySelector(".choice-dialog")?.open === true');
      await shot(dialogProbe ? "diagnostic-real-choice-original" : "office-blue-delete-confirmation",1440,900);
      if (!dialogProbe) await scene("delete-dialog", ".choice-dialog");
      const choiceLayout=await dialogGeometry('.choice-dialog');
      errorLayouts.push({phase:"real_choice_original", ...choiceLayout});
      if (!dialogProbe) {
        assertDialogVisible(choiceLayout);
        await shot("office-blue-delete-confirmation-desktop",1440,900);
        // One dialog anatomy and one overlay scale: close button, confirm width, danger action last, shared radius/shadow/padding.
        await assertDialogAnatomy(evaluate);
        await shot("office-blue-delete-confirmation-narrow",375,812);
        assertDialogVisible(await dialogGeometry('.choice-dialog'));
      }
      if (dialogProbe) {
      await centerProbeDialog('.choice-dialog');
      await shot("diagnostic-real-choice-centered",1440,900);
      errorLayouts.push({phase:"real_choice_centered_probe", ...await dialogGeometry('.choice-dialog')});
      await writeFile(path.join(output,'dialog-probe.json'),JSON.stringify({status:'DIAGNOSTIC_NOT_ACCEPTANCE',source_revision:(await readFile(path.join(root,'REVISION'),'utf8')).trim(),time_utc:new Date().toISOString(),hostname:hostname(),browser:version,layouts:errorLayouts},null,2)+'\n');
      }
      await evaluate('[...document.querySelectorAll(".choice-dialog button")].find(b=>b.textContent.trim()==="Cancel").click()');
      await wait('document.querySelector(".choice-dialog") === null');
      if (!dialogProbe) {
        // The command palette: the dialog overlay scale, a hint (not "No commands found") before typing, the ellipsis placeholder.
        await evaluate(`document.querySelector('[data-client-content]').dispatchEvent(new KeyboardEvent('keydown',{key:'k',code:'KeyK',ctrlKey:true,bubbles:true,cancelable:true}))`);
        await wait('document.querySelector(".command-palette-dialog") !== null && document.querySelector(".command-palette-loading").style.display === "none"');
        const palette = await evaluate(`(()=>{const d=document.querySelector('.command-palette-dialog'),cs=getComputedStyle(d),probe=document.createElement('div');probe.style.boxShadow='var(--shadow-dialog)';document.body.append(probe);const expected=getComputedStyle(probe).boxShadow;probe.remove();return {radius:cs.borderTopLeftRadius,padding:cs.padding,shadow:cs.boxShadow,expected,placeholder:document.querySelector('.command-palette-input').placeholder,empty:document.querySelector('.command-palette-empty').style.display!=='none'?document.querySelector('.command-palette-empty').textContent:null,items:document.querySelectorAll('.command-palette-item').length}})()`);
        await shot("fixture-command-palette",375,812);
        assert.equal(palette.radius, "10px", "palette uses --radius-overlay");
        assert.equal(palette.padding, "6px", "palette uses --overlay-padding");
        assert.equal(palette.shadow, palette.expected, "palette uses an overlay shadow token");
        assert.equal(palette.placeholder, "Search commands…");
        assert.notEqual(palette.empty, "No commands found", "no empty message before anything is typed");
        assert.ok(palette.items > 0 || palette.empty !== null, "the palette lists menus or hints");
        await evaluate(`document.querySelector('.command-palette-input').dispatchEvent(new KeyboardEvent('keydown',{key:'Escape',bubbles:true,cancelable:true}))`);
        await wait('document.querySelector(".command-palette-dialog") === null');
      }
    }
    if (!listOnly) {
    await evaluate('document.querySelector(".office-navigation [aria-current=page]").closest("details").open = false');
    await evaluate('document.querySelector("[data-burger-menu]").click()');
    await wait('document.querySelector(".mobile-drawer")?.classList.contains("hidden") === false');
    // A rendered frame is required before reading geometry after a metrics override.
    // Moli can deliver resize/media events before its last painted layout has been refreshed.
    await shot("office-blue-resize-desktop", 1440, 900);
    await wait('document.querySelector(".mobile-drawer")?.classList.contains("hidden") === true');
    assert.equal(await evaluate(`(() => { const active=document.activeElement; return active.matches('.office-navigation [aria-current="page"],button.brand') && active.getBoundingClientRect().width>0 && getComputedStyle(active).visibility!=="hidden" && !active.closest("[inert]"); })()`), true, "resize restores visible Office Blue focus");
    assert.equal(await evaluate('document.body.style.overflow'), "");
    for (const width of [769, 860, 861]) {
      await call("Emulation.setDeviceMetricsOverride", {width:768,height:900,deviceScaleFactor:1,mobile:false});
      await evaluate(`document.querySelector('.office-navigation [aria-current="page"]').closest('details').open=false; document.querySelector('[data-burger-menu]').click()`);
      await wait('document.querySelector(".mobile-drawer:not(.hidden)") !== null');
      await call("Emulation.setDeviceMetricsOverride", {width,height:900,deviceScaleFactor:1,mobile:false});
      await wait('document.querySelector(".mobile-drawer:not(.hidden)") === null');
      assert.ok(await evaluate(`(() => { const a=document.activeElement; return a.matches('.office-navigation [aria-current="page"]') && a.getBoundingClientRect().width>0 && !a.closest('details:not([open])'); })()`), `visible sidebar focus at ${width}`);
    }
    await call("Page.navigate", { url: base + "/erp/action-20?id=5&view_type=form" });
    await wait('document.querySelector("input[name=name]") !== null');
    // The record name in the control row is the base's 14px block in both themes; it stays smaller than the sheet's title.
    assert.ok(await evaluate('parseFloat(getComputedStyle(document.querySelector(".form-breadcrumb-record")).fontSize) <= 14 && parseFloat(getComputedStyle(document.querySelector(".form-breadcrumb-record")).fontSize) < parseFloat(getComputedStyle(document.querySelector(".content-sheet input[name=name]")).fontSize)'), "breadcrumb remains secondary to the sheet title");
    assert.equal(await evaluate('document.querySelectorAll(".content-sheet .record-form input[name=name]").length === 1 && document.querySelector(".control-panel input[name=name]") === null'), true, "one editable record title stays inside the declared form sheet");
    assert.equal(await evaluate('document.querySelector(".record-form-viewport > .form-record-header .form-header-btn") !== null && document.querySelector(".content-sheet .form-header") === null'), true, "real declared object actions head the sheet");
    assert.equal(await evaluate('document.querySelector(".form-record-tabs") === null && document.querySelector(".record-form [role=tablist]") !== null'), true, "notebook navigation remains inside declared sheet content");
    for (const [width, height] of [[1536, 1024], [1440, 900], [800, 1024], [390, 844]]) {
      await shot(`office-blue-form-${width}`, width, height);
      assert.equal(await evaluate('document.documentElement.scrollWidth <= innerWidth'), true, `form fits ${width}: ` + await evaluate(`[...document.querySelectorAll('body *')].filter(e => e.getBoundingClientRect().right > innerWidth + 1).slice(0, 6).map(e => e.tagName + '.' + e.className + ' ' + Math.round(e.getBoundingClientRect().right)).join(' | ')`));
      const aside = await evaluate(`(() => { const side=document.querySelector('.record-form-aside').getBoundingClientRect(),sheet=document.querySelector('.record-form-viewport > .content-sheet').getBoundingClientRect(); return {side:side.toJSON(),sheet:sheet.toJSON()}; })()`);
      assert.ok(width > 1120 ? aside.side.left >= aside.sheet.right : aside.side.top >= aside.sheet.bottom, `real record information rail has responsive placement ${width}`);
      const smart = await evaluate(`(() => { const box=document.querySelector('.form-button-box').getBoundingClientRect(),sheet=document.querySelector('.form-sheet-layout').getBoundingClientRect(),button=document.querySelector('.form-stat-button');return {box:box.toJSON(),sheet:sheet.toJSON(),name:button.getAttribute('aria-label'),nested:button.querySelectorAll('input,select,textarea,button').length}; })()`);
      assert.ok(smart.box.bottom <= smart.sheet.top, `smart strip precedes the sheet ${width}: ${JSON.stringify(smart)}`);
      assert.equal(smart.name, "17 Lines"); assert.equal(smart.nested, 0, "smart statistic is not an editable nested control");
      const geometry = await evaluate(`(() => {
        const field = document.querySelector('select[name="priority"]').closest('.field-label');
        const label = field.querySelector('span').getBoundingClientRect();
        const control = field.querySelector('select').getBoundingClientRect();
        const section = field.closest('.form-group');
        const title = document.querySelector('.record-form input[name="name"]');
        const heading = title.closest('.form-title-block,.form-title-field').getBoundingClientRect();
        const actions = document.querySelector('[data-control-actions]').getBoundingClientRect();
        return { labelRight: label.right, controlLeft: control.left, labelBottom: label.bottom, controlTop: control.top, controlHeight: control.height,
          sectionBorder: getComputedStyle(section).borderTopWidth, headingLeft: heading.left,
          headingTop: heading.top, actionsBottom: actions.bottom, headingInSheet: title.closest(".content-sheet .record-form") !== null,
          actionsLeft: actions.left, tabBackground: getComputedStyle(document.querySelector('[role="tab"][aria-selected="true"]')).backgroundColor };
      })()`);
      assert.ok(width >= 800 ? geometry.labelRight <= geometry.controlLeft + 1 : geometry.labelBottom <= geometry.controlTop + 1, `responsive shared field alignment at ${width}: ${JSON.stringify(geometry)}`);
      assert.equal(geometry.sectionBorder, "0px", "field groups inherit unboxed base structure");
      if (width === 1440) {
        assert.equal(geometry.headingInSheet, true, "record title remains in its form sheet");
        assert.ok(geometry.actionsBottom <= geometry.headingTop, "header commands remain above the sheet title");
        assert.ok(geometry.controlHeight >= 28 && geometry.controlHeight <= 36, `compact desktop controls: ${JSON.stringify(geometry)}`);
      } else if (width <= 640) visualCheck(geometry.controlHeight >= 40, `phone controls preserve touch target height at ${width}: ${JSON.stringify(geometry)}`);
      assert.equal(await evaluate(`getComputedStyle(document.querySelector('[role="tab"][aria-selected="true"]')).borderTopWidth`), "2px", "notebook inherits the base active edge");
      await evaluate(`(() => { const title=document.querySelector('input[name="name"]'); title.value += ' edited'; title.dispatchEvent(new Event('change')); })()`);
      await wait('document.querySelector(".status-actions").style.display !== "none"');
      await shot(`office-blue-form-dirty-${width}`, width, height);
      const commands = await evaluate(`([...document.querySelectorAll('.status-btn-save,.status-btn-discard,.form-header-btn,[data-control-actions] > button,.action-toggle-btn,.form-pager,.view-switcher')]
        .map(node=>({label:node.getAttribute('aria-label')||node.textContent,rect:node.getBoundingClientRect().toJSON()})).filter(item=>item.rect.width && item.rect.height))`);
      for (const [index, command] of commands.entries()) {
        assert.ok(command.rect.left >= 0 && command.rect.right <= width, `dirty command fits ${width}: ${JSON.stringify(command)}`);
        for (const other of commands.slice(index + 1)) {
          const overlapX = Math.min(command.rect.right, other.rect.right) - Math.max(command.rect.left, other.rect.left);
          const overlapY = Math.min(command.rect.bottom, other.rect.bottom) - Math.max(command.rect.top, other.rect.top);
          assert.ok(overlapX <= 0 || overlapY <= 0, `dirty commands do not overlap at ${width}: ${JSON.stringify([command, other])}`);
        }
      }
      await evaluate(`(() => { const title=document.querySelector('input[name="name"]'); title.value=''; document.querySelector('.status-btn-save').click(); })()`);
      await wait('document.querySelector(".record-form input[name=name][aria-invalid=true]") !== null');
      assert.equal(writes.length, 0, "invalid sheet title never reaches web_save");
      await shot(`office-blue-form-invalid-${width}`, width, height);
      assert.equal(await evaluate('(() => { const s=getComputedStyle(document.querySelector("select[name=priority]")); return s.backgroundImage !== "none" || s.appearance === "auto"; })()'), true, "selection keeps its dropdown chevron");
      assert.deepEqual(await evaluate(`(() => { const field=document.querySelector('[data-field="reference"]');
        return { readonly:field.classList.contains('field-readonly'), value:field.querySelector('.field-read-value')?.textContent,
          label:field.querySelector('.field-read-value')?.getAttribute('aria-label'), editors:field.querySelectorAll('input,select,textarea,[contenteditable=true]').length };
      })()`), { readonly:true, value:"Readonly fixture reference", label:"Reference", editors:0 }, "readonly reference is labelled static text, never an editor");
      const calendarControl = await evaluate(`(() => { const input=document.querySelector('input[name=scheduled_date]');
        const button=input.closest('.datetime-input-area').querySelector('.datetime-open');
        const calendar=button.querySelector('[data-icon=calendar]').getBoundingClientRect(), bounds=button.getBoundingClientRect();
        return { label:button.getAttribute('aria-label'), disabled:button.disabled,
          contained:calendar.width>0 && calendar.left>=bounds.left && calendar.right<=bounds.right,
          separate:input.getBoundingClientRect().right<=bounds.left+1 };
      })()`);
      assert.ok(calendarControl.label && !calendarControl.disabled && calendarControl.contained && calendarControl.separate,
        `date uses a labelled calendar button beside its text editor: ${JSON.stringify(calendarControl)}`);
      await evaluate('document.querySelector(".status-btn-discard").click()');
      await wait('document.querySelector(".status-actions").style.display === "none"');
      await evaluate('document.querySelectorAll(".client-notification-close").forEach(button=>button.click())');
    }
    for (const [width,height] of [[1440,900],[800,1024],[390,844]]) {
      await call("Emulation.setDeviceMetricsOverride",{width,height,deviceScaleFactor:1,mobile:!viewportOnly&&width<700});
      await evaluate('(()=>{const input=document.querySelector("input[name=owner_id]");input.scrollIntoView({block:"center"});input.focus();input.value="Rendering";input.dispatchEvent(new Event("input",{bubbles:true}));})()');
      await wait('document.querySelector(".editor-autocomplete-list")?.textContent.includes("Rendering fixture user")');
      await shot(`office-blue-relation-${width}`,width,height,true);
      const bounds=await evaluate('document.querySelector(".editor-autocomplete-list").getBoundingClientRect().toJSON()');
      assert.ok(bounds.left>=0 && bounds.right<=width, `relation fits ${width}`);
      await evaluate('document.querySelector("input[name=owner_id]").dispatchEvent(new KeyboardEvent("keydown",{key:"Escape",bubbles:true}))');
    }
    await evaluate(`(() => { const title=document.querySelector('input[name="name"]');title.value='Quarterly equipment allocation and return schedule for the distributed field operations team';title.dispatchEvent(new Event('change')); })()`);
    await scene('form-long-title', '.record-form input[name="name"]');
    await evaluate('document.querySelector(".status-btn-discard").click()');
    await wait('document.querySelector(".status-actions").style.display === "none"');
    {
      // Both appearances must satisfy the same rendered header balance contract.
      const balances = {};
      for (const [width, height] of [[1440, 900], [1280, 900], [1024, 900]]) {
        await call("Emulation.setDeviceMetricsOverride", { width, height, deviceScaleFactor: 1, mobile: false });
        await wait(`document.querySelector('.form-statusbar') !== null`);
        await shot(`office-blue-form-header-${width}`, width, height);
        balances[width] = await evaluate(formBalanceGeometry);
        assert.equal(balances[width].overflow, false, `Office Blue's header band does not overflow at ${width}`);
        // the chatter column's numbers in this theme, recorded with the checks that do not hold yet.
        await wait(`document.querySelectorAll('.record-form-chatter .erp-mail-message').length === 3`);
        const chatterColumn = await evaluate(chatterColumnGeometry);
        balances[width].chatterColumn = chatterColumn;
        balances[width].chatterColumnFailing = Object.entries(chatterColumnChecks(chatterColumn, width, "office")).filter(([, ok]) => !ok).map(([name]) => name);
        // Nothing is expected to fail in Office Blue: the same checks as the default theme, with the top compared to the sheet's top.
        assert.deepEqual(balances[width].chatterColumnFailing, [], `Office Blue's chatter column checks hold at ${width}: ${JSON.stringify(chatterColumn)}`);
        // One action area in both themes: the same probe and the same assertions as the default run.
        if (width === 1440) assertFormActionGeometry(balances[width].actions = await evaluate(formActionProbe), "Office Blue");
        if (width === 1440) {
          const states = balances[width].chatterStates = await evaluate(chatterStateProbe), failing = Object.entries(chatterStateChecks(states)).filter(([, v]) => !v).map(([k]) => k);
          assert.deepEqual(failing, [], `Office Blue's chatter states hold at ${width}: ${JSON.stringify(states)}`);
        }
        await shot(`office-blue-form-header-${width}`, width, height);
      }
      // The stacked widths too: the default run checks the chatter column at five widths, this run at the same five.
      const stackedColumns = {};
      for (const [width, height] of [[800, 900], [390, 844]]) {
        await call("Emulation.setDeviceMetricsOverride", { width, height, deviceScaleFactor: 1, mobile: width < 500 });
        await wait(`document.querySelectorAll('.record-form-chatter .erp-mail-message').length === 3`);
        // Buttons transition their padding when the container query flips: measure the settled layout, as the screenshots do.
        await wait('document.getAnimations().every(animation => animation.effect?.getComputedTiming().iterations === Infinity || animation.playState === "finished" || animation.playState === "idle")');
        const g = stackedColumns[width] = { chatterColumn: await evaluate(chatterColumnGeometry) };
        g.kindPadding = await evaluate(`(() => { const k = document.querySelector('.record-form-chatter .erp-mail-kind'); return { padding: getComputedStyle(k).paddingLeft, container: document.querySelector('.record-form-chatter').getBoundingClientRect().width, inner: innerWidth }; })()`);
        await evaluate(`document.querySelector('.record-form-chatter').scrollIntoView({ block: 'start' })`);
        await shot(`office-blue-chatter-column-${width}`, width, height, true);
        g.chatterColumnFailing = Object.entries(chatterColumnChecks(g.chatterColumn, width)).filter(([, ok]) => !ok).map(([name]) => name);
        assert.deepEqual(g.chatterColumnFailing, [], `Office Blue's chatter column checks hold at ${width}: ${JSON.stringify({ ...g.chatterColumn, kindPadding: g.kindPadding })}`);
      }
      for (const [width, height] of [[768, 900], [390, 844]]) {
        await call("Emulation.setDeviceMetricsOverride", { width, height, deviceScaleFactor: 1, mobile: width < 500 });
        await wait(`innerWidth === ${width}`);
        await wait('document.getAnimations().every(animation => animation.effect?.getComputedTiming().iterations === Infinity || animation.playState === "finished" || animation.playState === "idle")');
        stackedColumns[`bar-${width}`] = await assertPhoneBarRow(evaluate, "Office Blue form", width);
      }
      await writeFile(path.join(output, "form-balance-office-blue.json"), JSON.stringify({ ...balances, stacked: stackedColumns }, null, 1));
      for (const [width, geometry] of Object.entries(balances)) {
        for (const [check, passed] of Object.entries(formBalanceChecks(geometry, Number(width)))) {
          assert.ok(passed, `Office Blue ${width}: ${check}: ${JSON.stringify(geometry)}`);
        }
      }
      await call("Emulation.setDeviceMetricsOverride", { width: 1440, height: 900, deviceScaleFactor: 1, mobile: false });
    }
    await evaluate('document.querySelector(".form-stat-button").focus()');
    assert.equal(await evaluate('document.activeElement.classList.contains("form-stat-button")'),true,"smart action receives keyboard focus");
    await evaluate('document.querySelector(".form-stat-button").click()');
    await wait('document.querySelector(".form-stat-button")?.getAttribute("aria-label") === "18 Lines"');
    assert.equal(inspectCalls.length,1,"smart action dispatches the declared record method once and refreshes its actual fixture count");
    const changesBeforeStatus = calls.filter(path => path.endsWith("/onchange")).length;
    await evaluate(`document.querySelector('.form-statusbar [data-value="high"]').click(); document.querySelector('.form-statusbar [data-value="high"]').dispatchEvent(new Event('click'));`);
    assert.equal(await evaluate('document.querySelector(".form-statusbar [aria-current=step]").textContent'), "Normal", "default statusbar cannot mutate the record");
    assert.equal(calls.filter(path => path.endsWith("/onchange")).length, changesBeforeStatus, "statusbar sends no change RPC");
    await wait('document.querySelector(".form-header-actions .form-header-btn")?.disabled === false');
    await evaluate('document.querySelector(".form-header-actions .form-header-btn").focus()');
    await call("Input.dispatchKeyEvent", {type:"keyDown",key:"Enter",code:"Enter",windowsVirtualKeyCode:13,text:"\r"});
    await call("Input.dispatchKeyEvent", {type:"keyUp",key:"Enter",code:"Enter",windowsVirtualKeyCode:13});
    await wait('document.querySelector(".choice-dialog")?.open === true');
    await evaluate('[...document.querySelectorAll(".choice-dialog button")].find(button => button.textContent === "Lend").click()');
    await wait('document.querySelector(".form-statusbar [aria-current=step]")?.textContent === "High"');
    assert.equal(lendCalls.length, 1, "the declared object action remains the state transition path");
    assert.equal(inspectCalls.length, 1, "workflow activation does not repeat the smart action");
    await evaluate('document.querySelector("[role=tab]").focus()');
    assert.equal(await evaluate('document.activeElement.getAttribute("role")'), "tab");
    // The same mounted components as Default, reached through the real Office startup path.
    await wait('document.querySelectorAll(".record-form-chatter .erp-mail-message").length === 3');
    await scene('chatter', '.record-form-chatter', true);
    await evaluate('document.querySelector(".record-form-chatter .erp-mail-kind[data-kind=note]").click()');
    await scene('chatter-note', '.erp-chatter-composer', true);
    await evaluate('document.querySelector(".record-form-chatter .erp-mail-followers-button").click()');
    await wait('!document.querySelector(".erp-mail-followers-popover").hidden && document.querySelector(".erp-mail-followers-status")?.textContent === "No followers yet."');
    assert.equal(await evaluate('document.querySelector(".erp-mail-followers-error") === null'), true, "followers fixture is complete");
    await scene('followers', '.erp-mail-followers-popover', true);
    await call("Page.navigate", { url: base + "/erp" });
    await scene('home', '.home-grid');
    await evaluate('document.querySelector(".user-button").click()');
    await wait('document.querySelector(".user-dropdown")?.classList.contains("open") === true');
    await scene('user-menu', '.user-dropdown');
    await call("Page.navigate", { url: base + "/erp/action-21" });
    await scene('settings', '.settings-form');
    await call("Page.navigate", { url: base + "/erp/action-22" });
    await scene('addons', '.addon-tile');
    await evaluate('document.querySelector(".addon-info-btn").click()');
    await wait('document.querySelector(".addon-info-dialog")?.open === true');
    await scene('addon-info', '.addon-info-dialog');
    await call("Page.navigate", { url: base + "/erp/login" });
    await scene('login-boundary', '.login-card');
    assert.equal(await evaluate('document.documentElement.hasAttribute("data-office-theme")'), false, 'anonymous login does not perform authenticated theme selection');
    await call("Page.navigate", { url: base + "/erp/action-21" });
    await wait('document.querySelector(".settings-form") !== null');
    await evaluate(`document.querySelector('[data-app="theme_office_blue"]').click()`);
    await evaluate(`document.querySelector('[name="theme_office_blue.appearance"]').click(); document.querySelector('[data-setting-key="theme_office_blue.appearance"] [data-value="default"]').click();`);
    await evaluate('[...document.querySelectorAll("button")].find(b=>b.textContent==="Save").click()');
    await wait('document.querySelector("[data-client-notice]").textContent.includes("saved")');
    await call("Page.reload");
    await wait('document.querySelector(".settings-form") !== null');
    assert.equal(await evaluate('document.documentElement.hasAttribute("data-office-theme")'), false);
    assert.equal(await evaluate('document.querySelector("#database-appearance-stylesheet") !== null'), false);
    officeInstalled = false;
    await call("Page.navigate", { url: base + "/erp" });
    await wait('document.querySelector(".home-grid") !== null');
    assert.equal(await evaluate('document.documentElement.hasAttribute("data-office-theme")'), false);
    officeInstalled = true; officeChoice = "office-blue"; missingOfficeCss = true;
    await call("Page.navigate", { url: base + "/erp" });
    await wait('document.querySelector("#client-error-message")?.textContent.includes("Office Blue stylesheet could not be loaded")');
    assert.equal(await evaluate('document.documentElement.hasAttribute("data-office-theme")'), false);
    await shot("office-blue-missing-stylesheet", 1440, 900);
    }
  } else {
  await call("Page.navigate", { url: base + "/erp/login" }); await wait('document.querySelector("form[action=\\"/erp/login\\"]") !== null');
  assert.equal(await evaluate(`[...document.querySelectorAll("img")].every(img=>["/erp/brand/logo.svg","/erp/brand/logo-dark.svg"].includes(img.getAttribute("src")))`), true, "only the pinned brand logos render as images");
  assert.equal(await evaluate('document.querySelector("input[name=password]").autocomplete'), "current-password");
  assert.equal(await evaluate('document.querySelector("input[name=csrf_token]").value'), "fixture-not-a-real-token");
  // Native autofocus runs after input connection; retain the exact expected-focus assertion.
  await wait('document.activeElement.id === "login-name"');
  assert.equal(await evaluate('document.activeElement.id'),"login-name","the login error view keeps keyboard focus on the login input");
  assert.equal(await evaluate('document.querySelector(".login-error")?.getAttribute("role")'),"alert","login failure is announced");
  await shot("fixture-login-desktop", 1440, 900); await shot("fixture-login-mobile", 390, 844);
  await call("Page.navigate", { url: base + "/erp" });
  await wait('document.querySelector(".home-grid") !== null');
  await shot("fixture-home-menu-desktop", 1440, 900); await shot("fixture-home-menu-mobile", 390, 844);
  await shellProbes();
  await call("Page.navigate", { url: base + "/erp" });
  await wait('document.querySelector(".home-grid") !== null');
  assert.equal(await evaluate(`(()=>{const visible=[...document.querySelectorAll('.app-tile')].filter(n=>n.getBoundingClientRect().width&&n.getBoundingClientRect().height&&!n.closest('[inert]'));const names=visible.map(n=>n.textContent);return visible.length===4&&new Set(names).size===4;})()`), true, "one accessible launcher per app across the complete mounted shell");
  await evaluate('document.querySelector("[data-burger-menu]").click()');
  await wait('document.querySelector(".mobile-drawer")?.classList.contains("hidden") === false');
  assert.equal(await evaluate('document.activeElement.classList.contains("drawer-close-btn")'), true);
  await shot("fixture-navigation-drawer-mobile",390,844);
  await evaluate('document.querySelector(".drawer-close-btn").click()');
  assert.equal(await evaluate('document.activeElement.hasAttribute("data-burger-menu")'), true);
  await evaluate('document.querySelector(".user-button").click()');
  await wait('document.querySelector(".user-dropdown")?.classList.contains("open") === true');
  await shot("fixture-user-menu-mobile",390,844);
  const menuScale = await evaluate(`(()=>{const d=getComputedStyle(document.querySelector('.user-dropdown')),probe=document.createElement('div');probe.style.boxShadow='var(--shadow-menu)';document.body.append(probe);const expected=getComputedStyle(probe).boxShadow;probe.remove();return {radius:d.borderTopLeftRadius,shadow:d.boxShadow,expected}})()`);
  assert.equal(menuScale.radius, "10px", "menus use --radius-overlay");
  assert.equal(menuScale.shadow, menuScale.expected, "menus use --shadow-menu");
  await evaluate('document.querySelector(".user-button").click()');
  assert.equal(await evaluate(`[...document.querySelectorAll('.home-grid .app-tile')].every(tile=>{const svg=tile.querySelector('.app-icon svg');return svg?.getAttribute('aria-hidden')==='true'&&svg.getAttribute('focusable')==='false'&&svg.getBoundingClientRect().width>0&&tile.querySelector('.app-caption').textContent.length>0})`), true, "visible decorative launcher SVGs and captions");
  assert.equal(await evaluate(`document.querySelector('.home-grid [data-icon="notebook-pen"]')!==null&&document.querySelector('.home-grid [data-icon="settings"]')!==null`), true, "Notes and Settings use pinned icons");
  const homeAlignment=await evaluate(`(()=>{const grid=document.querySelector('.home-grid').getBoundingClientRect(),tiles=[...document.querySelectorAll('.app-tile')].map(e=>e.getBoundingClientRect());return {grid: {x:grid.x,width:grid.width}, left:Math.min(...tiles.map(r=>r.left)),right:Math.max(...tiles.map(r=>r.right))};})()`);
  assert.ok(Math.abs((homeAlignment.left+homeAlignment.right)/2-(homeAlignment.grid.x+homeAlignment.grid.width/2))<2,"launcher row centered");
  await call("Emulation.setDeviceMetricsOverride", {width:1440,height:900,deviceScaleFactor:1,mobile:false});
  await call("Page.navigate", {url:base+"/erp/action-20?view_type=list"});
  await wait('document.querySelectorAll(".list-table tbody tr").length === 2');
  assert.equal(await evaluate('document.querySelectorAll(".list-table tbody tr").length'),2,"selection fixture provides two records");
  for (const [width,height] of [[1440,900],[800,900],[390,844]]) {
    await call("Emulation.setDeviceMetricsOverride", {width,height,deviceScaleFactor:1,mobile:!viewportOnly&&width<700});
    await call("Page.navigate", {url:base+"/erp/action-20?view_type=list"});
    await wait('document.querySelectorAll(".list-table tbody tr").length === 2');
    const panelBox = '(()=>{const h=document.querySelector(".control-panel h1").getBoundingClientRect();return {titleX:h.x,titleY:h.y,panelHeight:document.querySelector(".control-panel").getBoundingClientRect().height};})()';
    const beforeSelect = await evaluate(panelBox);
    await evaluate('[...document.querySelectorAll(".list-row-select")].forEach(input=>input.click())');
    await wait('document.querySelector(".selection-count")?.textContent === "2 selected"');
    assert.deepEqual(await evaluate(panelBox), beforeSelect, `selecting rows moves neither the title nor the control panel's height at ${width}px`);
    assert.equal(await evaluate('(()=>{const b=document.querySelector(".selection-box");const s=getComputedStyle(b);return s.borderTopWidth==="0px"&&s.backgroundColor==="rgba(0, 0, 0, 0)";})()'), true, "the selection summary is not a box");
    assert.equal(await evaluate('document.querySelectorAll(".list-table tbody tr.row-selected").length'),2,"selected row tint matches the selected count");
    await assertListGeometry('default list with 2 rows selected', width);
    await evaluate('document.querySelector(".action-toggle-btn").click()');
    await wait('document.querySelector(".action-menu")?.classList.contains("open")');
    assert.equal(await evaluate('Boolean(document.querySelector(".action-delete-btn"))'),true,"selected-list actions are available");
    assert.equal(await evaluate('document.documentElement.scrollWidth <= innerWidth'),true,`selected list does not overflow at ${width}px`);
    await shot(`fixture-list-selection-${width}`,width,height);
  }
  {
    // Dialog anatomy of the default theme (420px, 10px radius, 20px padding, right-aligned footer, 32px buttons): the delete confirmation of the selected list.
    await call("Emulation.setDeviceMetricsOverride", {width:1440,height:900,deviceScaleFactor:1,mobile:false});
    await call("Page.navigate", {url:base+"/erp/action-20?view_type=list"});
    await wait('document.querySelectorAll(".list-table tbody tr").length === 2');
    await evaluate('document.querySelector(".list-row-select").click();document.querySelector(".action-toggle-btn").click()');
    await wait('document.querySelector(".action-menu")?.classList.contains("open")');
    await evaluate('document.querySelector(".action-delete-btn").click()');
    await wait('document.querySelector(".choice-dialog")?.open === true');
    await assertDialogAnatomy(evaluate);
    await evaluate('document.querySelector(".choice-dialog .message-dialog-close").click()');
    await wait('document.querySelector(".choice-dialog")?.open !== true');
  }
  await call("Page.navigate", { url: base + "/erp/action-20?id=5&view_type=form" });
  await wait('document.querySelector(".acceptance-note-title input") !== null && document.querySelector(".acceptance-note-guide") !== null');
  assert.equal(await evaluate('document.querySelector(".acceptance-note-title input").value'), note);
  assert.equal(await evaluate(`document.querySelector('[aria-current="page"]')?.textContent`), "Notes");
  await shot("fixture-form-groups-desktop",1440,900);
  const geometry=await evaluate(`(()=>{
   const groups=[...document.querySelectorAll('.record-form .form-group-title')].map(e=>e.parentElement.getBoundingClientRect());
   const title=document.querySelector('.form-title-block,.form-title-field').getBoundingClientRect();
   const sheet=document.querySelector('.content-sheet').getBoundingClientRect();
   const required=[...document.querySelectorAll('.field-required > span')].map(e=>({text:e.textContent,after:getComputedStyle(e,'::after').content}));
   return {groups:groups.map(r=>({x:r.x,y:r.y,width:r.width})),title:{width:title.width},sheet:{width:sheet.width},required};
  })()`);
  assert.equal(geometry.groups.length,2);
  assert.ok(geometry.groups[0].x < geometry.groups[1].x && Math.abs(geometry.groups[0].y-geometry.groups[1].y)<2,"declared groups share desktop row");
  assert.ok(geometry.title.width>geometry.sheet.width*.8,"title spans the sheet");
  assert.ok(geometry.required.every(x=>x.text.split('*').length===2 && ["none","normal"].includes(x.after)),"one required marker per field");
    // the geometry of the sheet (button box, title, groups, fields, notebook) read from the rendered DOM.
  const formSheetGeometry = `(() => {
    const px = (e, p) => parseFloat(getComputedStyle(e)[p]) || 0;
    const box = e => { const b = e.getBoundingClientRect(); return { l: +b.left.toFixed(2), r: +b.right.toFixed(2), t: +b.top.toFixed(2), b: +b.bottom.toFixed(2), w: +b.width.toFixed(2), h: +b.height.toFixed(2) }; };
    const textLeft = e => { const w = document.createTreeWalker(e, NodeFilter.SHOW_TEXT); for (let n = w.nextNode(); n; n = w.nextNode()) if (n.textContent.trim()) { const r = document.createRange(); r.setStart(n, n.textContent.search(/\\S/)); r.setEnd(n, n.textContent.search(/\\S/) + 1); return +r.getBoundingClientRect().left.toFixed(2); } return null; };
    const sheet = document.querySelector('.record-form-viewport > .content-sheet');
    const sb = sheet.getBoundingClientRect();
    const inner = { l: +(sb.left + px(sheet, 'borderLeftWidth') + px(sheet, 'paddingLeft')).toFixed(2), r: +(sb.right - px(sheet, 'borderRightWidth') - px(sheet, 'paddingRight')).toFixed(2) };
    const buttonBox = document.querySelector('.form-button-box');
    const stats = buttonBox ? [...buttonBox.querySelectorAll('.form-stat-button')].map(b => ({ ...box(b), value: box(b.querySelector('.form-stat-value')), label: box(b.querySelector('.form-stat-label')), icon: !!b.querySelector(':scope > svg.erp-icon'), valueText: textLeft(b.querySelector('.form-stat-value')), labelText: textLeft(b.querySelector('.form-stat-label')) })) : [];
    const heading = document.querySelector('.form-title-block, .form-title-field, .acceptance-note-title');
    const titleInput = document.querySelector('.record-form input[name="name"]');
    const help = heading && heading.querySelector('small, .field-help');
    const groups = [...document.querySelectorAll('.record-form .form-group')].filter(g => g.getClientRects().length).map(g => ({ group: box(g), title: g.querySelector('.form-group-title') ? box(g.querySelector('.form-group-title')) : null, titleText: g.querySelector('.form-group-title') ? textLeft(g.querySelector('.form-group-title')) : null }));
    const fields = [...document.querySelectorAll('.record-form .form-group .field-label')].filter(f => f.getClientRects().length).map(f => {
      const caption = f.querySelector(':scope > span, :scope > label'), control = [...f.children].find(c => c !== caption && !c.matches('.field-error,.field-help,small'));
      const input = control && (control.matches('input,select,textarea') ? control : control.querySelector('input,select,textarea'));
      const cb = control ? box(control) : null, lb = caption ? box(caption) : null;
      return { name: f.dataset.field || f.querySelector('input,select,textarea')?.name || null, readonly: f.classList.contains('field-readonly') || !!(input && input.readOnly), column: box(f.closest('.form-group')).l, label: lb, labelText: caption ? textLeft(caption) : null, labelWeight: caption ? getComputedStyle(caption).fontWeight : null, control: cb, valueLeft: input ? +(input.getBoundingClientRect().left + px(input, 'borderLeftWidth') + px(input, 'paddingLeft')).toFixed(2) : (control ? textLeft(control) : null), centreOffset: cb && lb ? +((cb.t + cb.b) / 2 - (lb.t + lb.b) / 2).toFixed(2) : null, row: box(f) };
    });
    const tabs = document.querySelector('.form-record-tabs [role=tablist], .form-tabs');
    const panel = document.querySelector('.form-tab-panel:not([hidden])');
    const panelField = panel && panel.querySelector('.field-label');
    const diag = [...document.querySelectorAll('.form-title-block, .form-title-block *, .form-title-field, .form-title-field *')].slice(0, 14).map(e => { const c = getComputedStyle(e); return { tag: e.tagName, cls: e.className, display: c.display, flex: c.flex, w: +e.getBoundingClientRect().width.toFixed(1), ta: c.textAlign, fw: c.fontWeight, ai: c.alignItems, fs: c.fontSize, pad: c.padding }; });
    return { diag, sheet: box(sheet), inner, buttonBox: buttonBox ? box(buttonBox) : null, stats, title: heading ? box(heading) : null, titleInput: titleInput ? { ...box(titleInput), textLeft: +(titleInput.getBoundingClientRect().left + px(titleInput, 'borderLeftWidth') + px(titleInput, 'paddingLeft')).toFixed(2), fontSize: px(titleInput, 'fontSize') } : null, help: help ? { ...box(help), text: textLeft(help), align: getComputedStyle(help).textAlign } : null, groups, fields, tabs: tabs ? box(tabs.closest('.form-tabs-bar') ?? tabs) : null, panel: panel ? box(panel) : null, panelField: panelField ? box(panelField) : null };
  })()`;
  for (const [width,height] of [[1440,900],[800,900],[390,844]]) {
    await shot(`default-form-rail-${width}`,width,height);
    const sheetGeometry=await evaluate(formSheetGeometry); await writeFile(path.join(output,`form-geometry-${width}.json`),JSON.stringify(sheetGeometry,null,1));
    {
      // measurable symmetry of the sheet. Tolerance is sub-pixel layout rounding only.
      const g=sheetGeometry, near=(x,y,what)=>assert.ok(Math.abs(x-y)<=0.6,`${what} at ${width}: ${x} vs ${y}`);
      const tiles=g.stats; assert.ok(tiles.length>=3,`the fixture shows its smart buttons directly at ${width}`);
      // Smart buttons are one joined strip in the control row (never tiles in the sheet): one height, shared borders, an icon and two short lines each.
      assert.ok(g.buttonBox.b<=g.sheet.t,`the smart-button strip sits above the sheet at ${width}: ${JSON.stringify(g.buttonBox)}`);
      for (const tile of tiles) { near(tile.h,tiles[0].h,"smart buttons share one height"); near(tile.t,tiles[0].t,"smart buttons share one top"); assert.ok((width<=640?tile.h>=44&&tile.h<=56:tile.h>=30&&tile.h<=34),`smart button height at ${width}: ${tile.h}`); assert.equal(tile.icon,true,"each smart button carries an icon"); assert.ok(tile.label.t>=tile.value.b-1,"value and label are two short lines"); }
      for (const [index,tile] of tiles.slice(1).entries()) assert.ok(tile.l-tiles[index].r<=1&&tile.l-tiles[index].r>=-1,`smart buttons are joined at ${width}`);
      assert.ok(g.title.t>=g.sheet.t,`the title opens the sheet at ${width}`);
      assert.ok(g.titleInput.l>=g.inner.l&&g.titleInput.l-g.inner.l<=48,`the title rule starts after the favourite star at ${width}: ${g.titleInput.l} vs ${g.inner.l}`); near(g.titleInput.r,g.inner.r,"title rule ends on the sheet edge");
      assert.ok(g.titleInput.h>=44,`title field height at ${width}`);
      assert.ok(g.help&&["start","left"].includes(g.help.align),"title help is start aligned"); assert.ok(g.help.l>=g.inner.l&&g.help.l-g.inner.l<=48,"title help starts at the title");
      const titled=g.groups.filter(group=>group.title&&group.group.w<g.inner.r-g.inner.l-1);
      if (width>640) {
        assert.equal(titled.length,2,"two titled groups"); const [left,right]=titled;
        near(left.group.l,g.inner.l,"left column starts on the sheet edge"); near(right.group.r,g.inner.r,"right column ends on the sheet edge");
        near(left.group.t,right.group.t,"columns start on one line"); near(left.group.w,right.group.w,"columns have one width"); near(right.group.l-left.group.r,32,"column gutter equals the sheet's side padding");
        near(left.title.h,right.title.h,"group titles share one height"); near(left.title.t,right.title.t,"group titles share one line");
      }
      const columns=[...new Set(g.fields.map(field=>field.column))];
      // Density: a 32px pitch between consecutive rows of a column (28px row + 4px gap) on a desktop pointer.
      if (width>768) for (const column of [...new Set(g.fields.map(field=>field.column))]) { const own=g.fields.filter(field=>field.column===column&&field.row.h<40).sort((x,y)=>x.row.t-y.row.t); for (const [index,field] of own.slice(1).entries()) near(field.row.t-own[index].row.t,32,`row pitch of ${field.name} at ${width}`); }
      for (const column of columns) {
        const own=g.fields.filter(field=>field.column===column); const plain=own.filter(field=>field.name!=="active");
        for (const field of own) { near(field.label.l,column,`label of ${field.name} starts on its column edge`); if (width>640) { near(field.row.h,own[0].row.h,`row of ${field.name} has the common height`); assert.ok(field.row.h>=28,`row height ${field.name}`); } }
        if (width>640) for (const field of own) { near(field.control.l-column,own[0].control.l-column,`value column of ${field.name} is shared`); assert.ok(Math.abs(field.centreOffset)<=0.6,`label and value of ${field.name} share one centre line at ${width}: ${field.centreOffset}`); }
        // Read-only text and editable text start on one x, so a record does not shift between modes.
        for (const field of plain) near(field.valueLeft,plain[0].valueLeft,`${field.readonly?"read-only":"editable"} value of ${field.name} starts where the others do`);
      }
      assert.ok(g.fields.some(field=>field.readonly)&&g.fields.some(field=>!field.readonly),"the fixture covers read-only and editable values");
      near(g.tabs.l,g.inner.l,"notebook tabs start on the sheet edge"); near(g.tabs.r,g.inner.r,"notebook tabs end on the sheet edge"); near(g.panelField.l,g.inner.l,"tab content starts on the sheet edge");
    }
    const layout=await evaluate(`(() => { const side=document.querySelector('.record-form-aside').getBoundingClientRect(), sheet=document.querySelector('.record-form-viewport > .content-sheet').getBoundingClientRect(); return {side:side.toJSON(),sheet:sheet.toJSON(),overflow:document.documentElement.scrollWidth>innerWidth}; })()`);
    assert.ok(width>1120 ? layout.side.left>=layout.sheet.right : layout.side.top>=layout.sheet.bottom, `declared aside placement at ${width}`);
    assert.equal(layout.overflow,false,`form fits ${width}`);
    // leads the side column and the aside rail follows it, elsewhere it follows the sheet. It adds no overflow.
    const chatter=await evaluate(`(() => { const slot=document.querySelector('.record-form-viewport > .record-form-chatter'), sheet=document.querySelector('.record-form-viewport > .content-sheet').getBoundingClientRect(), side=document.querySelector('.record-form-aside').getBoundingClientRect(); if(!slot) return null; const r=slot.getBoundingClientRect(); return {rect:r.toJSON(),sheet:sheet.toJSON(),side:side.toJSON(),data:{...slot.firstElementChild.dataset},label:slot.getAttribute('aria-label'),count:document.querySelectorAll('.record-form-chatter').length}; })()`);
    assert.ok(chatter,`chatter host rendered at ${width}`);
    assert.equal(chatter.count,1,"one chatter host");
    assert.equal(chatter.label,"Chatter");
    assert.deepEqual(chatter.data,{reloadOnPost:"true",reloadOnAttachment:"false",reloadOnActivity:"false",reloadOnFollower:"false",openAttachments:"false",hasActivities:"true"});
    assert.ok(chatter.rect.top>=chatter.side.bottom-1 || chatter.side.top>=chatter.rect.bottom-1,`chatter does not overlap the aside at ${width}`);
    // Chatter panel layout (any theme): own column of 360-440px on wide viewports, under the sheet and never wider than it below.
    if (width>1120) { assert.ok(chatter.rect.left>=chatter.sheet.right && chatter.side.top>=chatter.rect.bottom-1,`chatter leads the side column and the aside rail follows it at ${width}`); assert.ok(chatter.rect.width>=360 && chatter.rect.width<=440,`chatter column width at ${width}: ${chatter.rect.width}`); }
    else { assert.ok(chatter.rect.top>=chatter.sheet.bottom-1,`chatter follows the sheet at ${width}`); assert.ok(chatter.rect.width<=chatter.sheet.width+1,`chatter is never wider than the sheet at ${width}`); }
    await wait('document.querySelectorAll(".record-form-chatter .erp-mail-message").length === 3 && document.querySelectorAll(".record-form-chatter .erp-mail-activity").length === 2');
    assert.equal(await evaluate('document.querySelectorAll(".record-form-chatter .erp-mail-message.is-note").length'),1,`the internal-subtype comment is styled as a note at ${width}`);
    assert.equal(await evaluate('Boolean(document.querySelector(".record-form-chatter .erp-mail-chatter-header .erp-mail-kinds"))'),true,`chatter header carries the post-type switch at ${width}`);
    assert.equal(await evaluate('document.querySelector(".record-form-chatter .erp-chatter-composer").classList.contains("is-collapsed")'),true,"composer starts collapsed");
    await shot(`chatter-${width}`,width,height);
    if (width===1440) {
      // Expanded composer in note mode, then the followers popover, so both states can be reviewed.
      await evaluate('document.querySelector(".record-form-chatter .erp-mail-kind[data-kind=note]").click()');
      await wait('document.querySelector(".record-form-chatter .erp-mail-chatter").dataset.kind === "note" && !document.querySelector(".record-form-chatter .erp-chatter-composer").classList.contains("is-collapsed")');
      assert.equal(await evaluate('document.querySelectorAll(".record-form-chatter .erp-mail-activity").length'),2,"two planned activities render");
      assert.equal(await evaluate('document.querySelector(".record-form-chatter .erp-mail-activity-group.is-overdue .erp-mail-activity-summary").textContent'),"Send the lending contract","the overdue activity is grouped first");
            await shot("chatter-note-expanded-1440",width,height);
      await evaluate('document.querySelector(".record-form-chatter .erp-mail-followers-button").click()');
      await wait('!document.querySelector(".record-form-chatter .erp-mail-followers-popover").hidden');
      await shot("chatter-followers-1440",width,height);
      await evaluate('document.querySelector(".record-form-chatter .erp-mail-followers-button").click(); document.querySelector(".record-form-chatter .erp-mail-kind[data-kind=message]").click(); document.activeElement.blur()');
    }
  }
  assert.equal(await evaluate('document.querySelector(".form-breadcrumb-path [aria-current=page]")?.textContent'), note);
  assert.equal(await evaluate('document.querySelector(".form-statusbar [aria-current=step]")?.textContent'), "Normal");
  for (const [width,height] of [[1440,900],[800,900],[390,844]]) {
    await call("Emulation.setDeviceMetricsOverride",{width,height,deviceScaleFactor:1,mobile:!viewportOnly&&width<700});
    await evaluate('(()=>{const input=document.querySelector("input[name=owner_id]");input.focus();input.value="Rendering";input.dispatchEvent(new Event("input",{bubbles:true}));})()');
    await wait('document.querySelector(".editor-autocomplete-list")?.textContent.includes("Rendering fixture user")');
    assert.ok(await evaluate('document.querySelectorAll(".editor-autocomplete-list [role=option]").length >= 1'),`relation suggestions render at ${width}px`);
    const menuBounds=await evaluate('(()=>{const r=document.querySelector(".editor-autocomplete-list").getBoundingClientRect();return {left:r.left,right:r.right,width:innerWidth};})()');
    assert.ok(menuBounds.left>=0&&menuBounds.right<=menuBounds.width,`relation dropdown fits ${width}px: ${JSON.stringify(menuBounds)}`);
    // On a pointer device the suggestion list lines up with the field box: same left edge, never narrower.
    const align=await evaluate('(()=>{const p=document.querySelector(".editor-popover");const c=document.querySelector("input[name=owner_id]")?.closest(".many2one-control");if(!p||!c)return null;const a=p.getBoundingClientRect(),b=c.getBoundingClientRect();return {popLeft:a.left,popWidth:a.width,fieldLeft:b.left,fieldWidth:b.width,popTop:a.top,fieldBottom:b.bottom};})()');
    if (align) assert.ok(Math.abs(align.popLeft-align.fieldLeft)<=1 && align.popWidth>=align.fieldWidth-1 && align.popTop-align.fieldBottom<=6,`relation dropdown is aligned to its field at ${width}px: ${JSON.stringify(align)}`);
    await shot(`fixture-relation-dropdown-${width}`,width,height,true);
    assert.ok(await evaluate('document.querySelectorAll(".editor-autocomplete-list [role=option]").length >= 1 && document.querySelector("input[name=owner_id]").getAttribute("aria-expanded") === "true"'), `relation scene remains open after capture at ${width}px`);
    await evaluate('document.querySelector("input[name=owner_id]").dispatchEvent(new KeyboardEvent("keydown",{key:"Escape",bubbles:true}))');
    if (width===1440) {
      // At rest a many2one shows only its value and the open-record icon; Clear and Search more appear with hover or focus.
      const icons=state=>evaluate(`(()=>{const c=document.querySelector("input[name=owner_id]").closest(".many2one-control");const v=s=>getComputedStyle(c.querySelector(s)).visibility;return {clear:v(".many2one-clear-btn"),more:v(".relation-search-more"),open:v(".many2one-open-btn")};})()`);
      await call("Input.dispatchMouseEvent",{type:"mouseMoved",x:2,y:2});
      await evaluate('document.activeElement.blur()');
      await wait('getComputedStyle(document.querySelector("input[name=owner_id]").closest(".many2one-control").querySelector(".many2one-clear-btn")).visibility==="hidden"').catch(()=>{});
      assert.deepEqual(await icons(),{clear:"hidden",more:"hidden",open:"visible"},`a many2one at rest shows only the open-record icon (hover media: ${await evaluate('matchMedia("(hover: hover) and (pointer: fine)").matches')})`);
      const box=await evaluate('(()=>{const r=document.querySelector("input[name=owner_id]").closest(".many2one-control").getBoundingClientRect();return {x:r.left+r.width/2,y:r.top+r.height/2};})()');
      await call("Input.dispatchMouseEvent",{type:"mouseMoved",x:box.x,y:box.y});
      await wait('getComputedStyle(document.querySelector("input[name=owner_id]").closest(".many2one-control").querySelector(".many2one-clear-btn")).visibility==="visible"').catch(()=>{});
      assert.deepEqual(await icons(),{clear:"visible",more:"visible",open:"visible"},"a hovered many2one shows its Clear and Search more icons");
      await call("Input.dispatchMouseEvent",{type:"mouseMoved",x:2,y:2});
    }
  }
  // Header object button, driven by the real keyboard: Enter opens the confirmation, Space accepts it.
  assert.equal(await evaluate(`document.querySelector('.form-header-actions')?.getAttribute('role')`), "toolbar");
  assert.deepEqual(await evaluate(`[...document.querySelectorAll('.form-header-actions .form-header-btn')].map(b=>b.textContent)`), ["Lend"]);
  assert.equal(await evaluate(`Boolean(document.querySelector('.record-form-viewport > .form-record-header .form-header')) && !document.querySelector('.content-sheet .form-header')`), true, "the action row is its own row above the sheet");
  await shot("fixture-form-header-desktop", 1440, 900);
  // One control height, one centre line per row, equal
  // gaps, and the outer edges on the sheet's own gutter; Archive is in the Actions menu, not beside the header action.
  formActionGeometry = await evaluate(formActionProbe);
  assertFormActionGeometry(formActionGeometry);
  // The balance of the whole header band, at three desktop widths: edges, tops, heights and the gaps between rows.
  const balances = {};
  for (const [width, height] of [[1440, 900], [1280, 900], [1024, 900]]) {
    await call("Emulation.setDeviceMetricsOverride", { width, height, deviceScaleFactor: 1, mobile: false });
    await wait(`document.querySelector('.form-statusbar') !== null`);
    await shot(`fixture-form-header-${width}`, width, height);
    const g = balances[width] = await evaluate(formBalanceGeometry), note = `${width}: ${JSON.stringify(g)}`;
    assert.equal(g.overflow, false, `the header band does not overflow at ${note}`);
    assert.ok(Math.abs(g.newLeft - g.actionLeft) <= 1 && Math.abs(g.actionLeft - g.sheetLeft) <= 1, `New, the first action button and the sheet share one left edge at ${note}`);
    assert.ok(Math.abs(g.barRight - g.sheetRight) <= 1, `the statusbar ends on the sheet's right edge at ${note}`);
    for (const group of ["control", "action"]) assert.ok(Math.max(...g.rowHeights[group]) - Math.min(...g.rowHeights[group]) <= 0.5, `every control of the ${group} row has one height at ${note}`);
    if (width > 1120) assert.ok(Math.abs(g.actionTop - g.chatterButtonTop) <= 1, `the action row and the chatter's button row share a top at ${note}`);
    assert.equal(g.stripClipped, false, `no smart button is truncated (the rest are in More) at ${note}`);
    assert.ok(Math.abs(g.gapControlToAction - g.gapActionToSheet) <= 4, `the gaps between the three rows are alike at ${note}`);
    for (const [name, ok] of Object.entries(formBalanceChecks(g, width))) assert.ok(ok, `balance check ${name} holds at ${note}`);
  }
  await writeFile(path.join(output, "form-balance-default.json"), JSON.stringify(balances, null, 1));
  {
    // the chatter column against the sheet and the action row, at five widths; numbers go to evidence.
    const columns = {};
    for (const [width, height] of [[1440, 900], [1280, 900], [1024, 900], [800, 900], [390, 844]]) {
      await call("Emulation.setDeviceMetricsOverride", { width, height, deviceScaleFactor: 1, mobile: width < 500 });
      await wait(`document.querySelectorAll('.record-form-chatter .erp-mail-message').length === 3`);
      const g = columns[width] = await evaluate(chatterColumnGeometry), note = `${width}: ${JSON.stringify(g)}`;
      for (const [name, ok] of Object.entries(chatterColumnChecks(g, width))) assert.ok(ok, `chatter column check ${name} holds at ${note}`);
      // The error line, drawn as the chatter draws it, sits between the composer and the thread and has the column's width.
      await evaluate(`(() => { const a = document.createElement('div'); a.className = 'erp-mail-thread-error erp-mail-chatter-error'; a.dataset.probe = '1'; a.textContent = 'Unable to connect'; document.querySelector('.erp-mail-chatter-status').append(a); })()`);
      const err = await evaluate(`(() => { const e = document.querySelector('[data-probe]'), c = document.querySelector('.record-form-chatter').getBoundingClientRect(), k = document.querySelector('.erp-chatter-composer').getBoundingClientRect(), t = document.querySelector('.erp-mail-chatter .erp-mail-thread').getBoundingClientRect(), act = document.querySelector('.erp-mail-chatter-activities'), r = e.getBoundingClientRect(); const after = act && !act.hidden ? act.getBoundingClientRect().top - r.bottom : null; e.remove(); return { l: r.left - c.left, r: c.right - r.right, belowComposer: r.top >= k.bottom - 0.5, aboveThread: r.bottom <= t.top + 0.5, activitiesPresent: after !== null, gapToActivities: after === null ? null : +after.toFixed(1) }; })()`);
      columns[width].errorLine = err;
      assert.ok(Math.abs(err.l) <= 1 && Math.abs(err.r) <= 1 && err.belowComposer && err.aboveThread && err.activitiesPresent && Math.abs(err.gapToActivities - 12) <= 0.5, `the error line has the column's width, directly below the composer, above the activities (with 12px under it) and the thread at ${width}: ${JSON.stringify(err)}`);
      if (width === 1440) {
        const states = columns[width].states = await evaluate(chatterStateProbe), failing = Object.entries(chatterStateChecks(states)).filter(([, v]) => !v).map(([k]) => k);
        assert.deepEqual(failing, [], `chatter states hold at ${width}: ${JSON.stringify(states)}`);
      }
      if (width === 1440 || width === 390) {
        // Scroll to the chatter itself, so a stacked layout (the phone) is captured and not only the top of the form.
        await evaluate(`document.querySelector('.record-form-chatter').scrollIntoView({ block: 'start' })`);
        await shot(`fixture-chatter-column-${width}`, width, height);
        await evaluate(`document.querySelector('.control-panel-form').scrollIntoView({ block: 'start' })`);
      }
    }
    await writeFile(path.join(output, "chatter-column-default.json"), JSON.stringify(columns, null, 1));
    await call("Emulation.setDeviceMetricsOverride", { width: 1440, height: 900, deviceScaleFactor: 1, mobile: false });
  }
  await call("Emulation.setDeviceMetricsOverride", { width: 1440, height: 900, deviceScaleFactor: 1, mobile: false });
  // A favourite declared in the title sits left of the heading, on the title's own line.
  const star = await evaluate(`(() => { const f = document.querySelector('.form-title-block > .field-label'), i = document.querySelector('.form-title-block input[name=name]'); if (!f || !i) return null; const a = f.getBoundingClientRect(), b = i.getBoundingClientRect(); return { starRight: a.right, titleLeft: b.left, starMid: a.top + a.height / 2, titleMid: b.top + b.height / 2, label: f.textContent.trim().length }; })()`);
  assert.ok(star && star.starRight <= star.titleLeft && Math.abs(star.starMid - star.titleMid) <= 6, `the favourite star sits left of the title: ${JSON.stringify(star)}`);
  // The Catalogue page: radio, copy button, integer year, tags. Measured on the rendered DOM; the mouse rests outside the sheet.
  await evaluate(`[...document.querySelectorAll('[role=tab]')].find(tab => tab.textContent.trim() === 'Catalogue').click()`);
  await wait(`document.querySelector('.field-radio-group') !== null && document.querySelector('.field-radio-group').getClientRects().length > 0`);
  await call("Input.dispatchMouseEvent", { type: "mouseMoved", x: 2, y: 2 });
  await evaluate('document.activeElement && document.activeElement.blur()');
  for (const [width, height] of [[1440, 900], [800, 900], [390, 844]]) {
    await call("Emulation.setDeviceMetricsOverride", { width, height, deviceScaleFactor: 1, mobile: width < 700 });
    await call("Input.dispatchMouseEvent", { type: "mouseMoved", x: 2, y: 2 });
    await wait(`getComputedStyle(document.querySelector('.field-copy-clipboard > button')).visibility === 'hidden'`).catch(() => {});
    const c = await evaluate(`(() => {
      const R = e => { const b = e.getBoundingClientRect(); return { l: b.left, r: b.right, t: b.top, b: b.bottom, h: b.height, w: b.width }; };
      const group = document.querySelector('.field-radio-group'), items = [...group.querySelectorAll('.field-radio-item')].map(R), label = R(group.closest('.field-label').querySelector(':scope > span'));
      const checked = group.querySelector('input:checked'), p = document.createElement('i'); p.style.color = 'var(--accent)'; document.body.append(p); const accent = getComputedStyle(p).color; p.remove();
      const copy = document.querySelector('.field-copy-clipboard > button'), year = document.querySelector('input[name=pub_year]'), run = document.querySelector('input[name=print_run]');
      const chips = [...document.querySelectorAll('.many2many-tags .relation-chip, .many2many-tags .tag')].map(R);
      const rows = [...document.querySelectorAll('.form-tab-panel:not([hidden]) .field-label')].filter(f => f.getClientRects().length).map(f => ({ name: f.querySelector('input,select')?.name || f.className, ...R(f) }));
      return { items, label, group: R(group), appearance: getComputedStyle(checked).appearance, checkedBorder: getComputedStyle(checked).borderTopColor, accent, copyVisibility: copy ? getComputedStyle(copy).visibility : null, year: year.value, yearType: year.type, run: run.value, chips, rows, overflow: document.documentElement.scrollWidth > innerWidth };
    })()`);
    const note = `${width}: ${JSON.stringify(c)}`;
    assert.equal(c.overflow, false, `the catalogue page does not overflow at ${note}`);
    assert.equal(c.appearance, "none", `the radio is our own control at ${width}`);
    assert.equal(c.checkedBorder, c.accent, `a checked radio is drawn in the accent colour at ${width}`);
    if (width > 640) assert.ok(c.items.every(item => Math.abs(item.t - c.items[0].t) <= 1), `the radio choices sit on one row at ${note}`);
    if (width > 640) assert.ok(Math.abs((c.label.t + c.label.h / 2) - (c.items[0].t + c.items[0].h / 2)) <= 2, `the radio's label is centred on its first row at ${note}`);
    assert.equal(c.year, "1974", `a year declared with type number has no thousands separator at ${width}`);
    assert.equal(c.run, "12,000", `a plain integer keeps its thousands separator at ${width}`);
    if (width > 640) assert.equal(c.copyVisibility, "hidden", `the Copy button is hidden while the field is at rest at ${width}; hover=${await evaluate('matchMedia("(hover: hover) and (pointer: fine)").matches')} over=${await evaluate('document.querySelector(".field-copy-clipboard").matches(":hover")+"/"+document.querySelector(".field-copy-clipboard").matches(":focus-within")')}`);
    assert.ok(c.chips.length >= 2 && c.chips.every(chip => chip.h <= 26), `tags are compact chips at ${width}: ${JSON.stringify(c.chips)}`);
    await shot(`fixture-form-catalogue-${width}`, width, height);
    if (width === 1440) {
      const box = await evaluate(`(() => { const r = document.querySelector('.field-copy-clipboard').getBoundingClientRect(); return { x: r.left + 20, y: r.top + r.height / 2 }; })()`);
      await call("Input.dispatchMouseEvent", { type: "mouseMoved", x: box.x, y: box.y });
      await wait(`getComputedStyle(document.querySelector('.field-copy-clipboard > button')).visibility === 'visible'`);
      await shot("fixture-form-catalogue-hover-1440", 1440, 900);
      await call("Input.dispatchMouseEvent", { type: "mouseMoved", x: 2, y: 2 });
    }
  }
  await call("Emulation.setDeviceMetricsOverride", { width: 1440, height: 900, deviceScaleFactor: 1, mobile: false });
  await evaluate(`[...document.querySelectorAll('[role=tab]')].find(tab => tab.textContent.trim() === 'Description').click()`);
  await evaluate('document.querySelector(".form-actions-dropdown .action-toggle-btn").click()');
  await wait('document.querySelector(".form-action-menu")?.classList.contains("open")');
  assert.deepEqual(await evaluate('[...document.querySelectorAll(".form-action-menu button")].filter(b=>!b.hidden).map(b=>b.textContent)'), ["Duplicate", "Archive", "Delete", "Reload"], "Duplicate, Archive and Delete lead the Actions menu, Reload follows");
  assert.deepEqual(await evaluate('[...document.querySelector(".form-action-menu").children].filter(c=>!c.hidden).map(c=>c.matches(".dropdown-divider")?"|":c.textContent)'), ["Duplicate", "Archive", "Delete", "|", "Reload"], "a divider sets the model's own actions (none yet) and Reload apart");
  assert.equal(await evaluate('document.querySelector(".status-badge.archived").hidden'), true, "an active record shows no Archived indicator");
  const menuGeometry = await evaluate(`(() => [...document.querySelectorAll(".form-action-menu button")].filter(b => !b.hidden).map(b => { const i = b.querySelector(":scope > .erp-icon"), t = document.createRange(); t.selectNodeContents(b); t.setStart(b, 1); return { icon: i && i.getAttribute("data-icon"), size: i && [Math.round(i.getBoundingClientRect().width), Math.round(i.getBoundingClientRect().height)], iconLeft: i && Math.round(i.getBoundingClientRect().left), labelLeft: Math.round(t.getBoundingClientRect().left), color: getComputedStyle(b).color }; }))()`);
  assert.ok(menuGeometry.every(row => row.icon && row.size[0] === 16 && row.size[1] === 16), `every Actions entry has a 16px icon: ${JSON.stringify(menuGeometry)}`);
  assert.equal(new Set(menuGeometry.map(row => row.iconLeft)).size, 1, `icons share one column: ${JSON.stringify(menuGeometry)}`);
  assert.equal(new Set(menuGeometry.map(row => row.labelLeft)).size, 1, `labels start on one x: ${JSON.stringify(menuGeometry)}`);
  assert.notEqual(menuGeometry.find(row => row.label === "Delete" || row.text === "Delete")?.color ?? menuGeometry[2].color, menuGeometry[0].color, `Delete keeps the danger colour: ${JSON.stringify(menuGeometry)}`);
  await shot("fixture-form-actions-menu-1440", 1440, 900, true);
  // Duplicate goes through the generic copy method with the open record's id and lands on the copy.
  await evaluate('document.querySelector(".form-duplicate-btn").click()');
  for (let attempt = 0; attempt < 50 && copyCalls.length === 0; attempt++) await new Promise(resolve => setTimeout(resolve, 100));
  assert.deepEqual(copyCalls.map(call => call.ids), [[5]], "Duplicate calls copy on the open record");
  await wait('!document.querySelector(".form-action-menu")?.classList.contains("open")');
  const press = async (key, code, keyCode, text) => {
    for (const type of ["keyDown", "keyUp"]) await call("Input.dispatchKeyEvent", { type, key, code, windowsVirtualKeyCode: keyCode, ...(type === "keyDown" ? { text } : {}) });
  };
  // The copy's form is rebuilt after Duplicate; focus the header button only once it has settled.
  await wait(`document.querySelector('.form-header-actions .form-header-btn') !== null`);
  await new Promise(resolve => setTimeout(resolve, 500));
  await evaluate(`document.querySelector('.form-header-actions .form-header-btn').focus()`);
  await press("Enter", "Enter", 13, "\r");
  await wait(`document.querySelector('dialog')?.textContent.includes('Lend this book?')`);
  await shot("fixture-confirmation-desktop",1440,900);
  assert.equal(lendCalls.length, 0, "the method waits for the confirmation");
  assert.equal(await evaluate(`document.activeElement.closest('dialog') !== null`), true, "focus moves into the confirmation dialog");
  await press(" ", "Space", 32, " ");
  await wait(`document.querySelector('.form-header-actions .form-header-btn') === null`);
  assert.equal(lendCalls.length, 1); assert.deepEqual(lendCalls[0].ids, [5]);
  assert.equal(await evaluate(`document.querySelector('.form-statusbar [aria-current=step]')?.textContent`), "High", "declared status remains visible when the action disappears");
  priorityValue = "normal";
  await evaluate(`[...document.querySelectorAll('[role=tab]')].find(tab => tab.textContent.trim() === 'Internal Memo').click()`);
  assert.equal(await evaluate(`document.querySelector('[role=tab][aria-selected=true]').textContent`),"Internal Memo");
  const callsBeforeTabs=calls.length;
  await evaluate(`document.querySelector('[role=tab][aria-selected=true]').dispatchEvent(new KeyboardEvent('keydown',{key:'Home',bubbles:true}))`);
  assert.equal(calls.length,callsBeforeTabs,"tab switch does not issue RPC or writes");
  const writesBeforeInvalid=writes.length;
  await evaluate(`(()=>{const input=document.querySelector('textarea[name=internal_memo]');input.required=true;document.querySelector('.record-form').requestSubmit();})()`);
  await wait(`document.querySelector('[role=tab][aria-selected=true]')?.textContent==='Internal Memo'`);
  assert.equal(writes.length,writesBeforeInvalid,"invalid hidden-page field must block save even from Enter/requestSubmit");
  await evaluate(`(()=>{document.querySelector('textarea[name=internal_memo]').required=false;document.querySelector('[role=tab]').click();})()`);
  for(const width of [800,390]) {
   await shot(`fixture-form-groups-${width}`,width,900);
   assert.equal(await evaluate('document.documentElement.scrollWidth <= innerWidth'),true,`form should fit ${width}px`);
   // A one2many table as wide as ten columns: it scrolls inside its own container and never pans the page. The table
   // is injected with the classes the real widget uses, into the visible notebook page, because this fixture has no line field.
   // Each cell carries a visually-hidden span: it is absolutely positioned, so only a positioned scroller clips it.
   const lines = await evaluate(`(()=>{
     const panel=document.querySelector('.form-tab-panel:not([hidden])'); const box=document.createElement('div'); box.className='x2many-field'; box.id='probe-x2many';
     const scroll=document.createElement('div'); scroll.className='x2many-scroll'; const table=document.createElement('table'); table.className='x2many-table';
     const cells=tag=>Array.from({length:10},(_,i)=>'<'+tag+'>Column value that does not wrap '+i+(tag==='td'?'<span class="visually-hidden">Open line</span>':'')+'</'+tag+'>').join('');
     table.innerHTML='<thead><tr>'+cells('th')+'</tr></thead><tbody><tr>'+cells('td')+'</tr></tbody>'; scroll.append(table); box.append(scroll); panel.append(box);
     const wide=[...document.querySelectorAll('body *')].filter(e=>!e.closest('.x2many-scroll')&&e.getBoundingClientRect().right>innerWidth+1).slice(0,6).map(e=>e.tagName+'.'+e.className);
     return {scrollWidth:document.documentElement.scrollWidth,innerWidth,wide,own:{client:scroll.clientWidth,scroll:scroll.scrollWidth}};})()`);
   assert.ok(lines.scrollWidth<=lines.innerWidth,`a wide one2many table must not pan the page at ${width}px: ${JSON.stringify(lines)}`);
   assert.ok(lines.own.scroll>lines.own.client,`the wide table scrolls inside its own container at ${width}px: ${JSON.stringify(lines)}`);
   // the lines of a form use the list's rhythm (36px header and rows, 12px cell padding) inside the sheet, with no shadow of their own.
   const rhythm = await evaluate(`(()=>{const q=s=>document.querySelector('#probe-x2many '+s); return {th:q('th').getBoundingClientRect().height,td:q('td').getBoundingClientRect().height,pad:getComputedStyle(q('th')).paddingLeft,shadow:getComputedStyle(q('.x2many-scroll')).boxShadow};})()`);
   assert.ok(Math.abs(rhythm.th-36)<=1&&Math.abs(rhythm.td-36)<=1&&rhythm.pad==='12px'&&rhythm.shadow==='none',`form lines use the list header, row and cell rhythm at ${width}px: ${JSON.stringify(rhythm)}`);
   await evaluate(`document.getElementById('probe-x2many').remove()`);
  }
  await shot("fixture-form-groups-desktop",1440,900);
  await shot("fixture-route-form-desktop", 1440, 900);
  await shot("fixture-route-form-mobile", 375, 812);
  await evaluate('document.querySelector("[data-skip-content]").click()');
  assert.equal(await evaluate('document.activeElement.id'), "erp-content"); assert.equal(await evaluate('location.hash'), "");
  await shot("fixture-addon-form-desktop", 1440, 900); await shot("fixture-addon-form-mobile", 390, 844);
  await evaluate('(()=>{ const input=document.querySelector(".acceptance-note-title input"); input.value="Compiled addon event"; input.dispatchEvent(new Event("change",{bubbles:true})); })()');
  await wait('document.querySelector(".acceptance-note-title input")?.value === "Compiled addon event" && document.querySelector(".acceptance-note-title input")?.disabled === false');
  await wait('document.querySelector(".status-badge.dirty")?.textContent === "Unsaved changes"');
  assert.equal(await evaluate('document.querySelector(".status-badge.dirty").hidden'),false,"dirty state is visibly marked");
  {
    await call("Emulation.setDeviceMetricsOverride", {width:1440,height:900,deviceScaleFactor:1,mobile:false});
    await evaluate('scrollTo(0,0)');
    await wait('innerWidth === 1440 && getComputedStyle(document.querySelector(".control-panel-form")).paddingLeft === "24px"');
    const dirty = await evaluate(`(() => { const sw = document.querySelector('.view-switcher').getBoundingClientRect(); return { top: sw.top, right: sw.right, panel: document.querySelector('.control-panel-form').getBoundingClientRect().height, save: document.querySelector('.status-btn-save').getBoundingClientRect().height, path: document.querySelector('.control-panel-form .breadcrumbs').getBoundingClientRect().height }; })()`);
    assert.ok(Math.abs(dirty.top - formActionGeometry.switcher.top) <= 0.5 && Math.abs(dirty.right - formActionGeometry.switcher.right) <= 0.5 && Math.abs(dirty.panel - formActionGeometry.panel) <= 0.5, `nothing moves when Save and Discard appear: ${JSON.stringify({dirty, clean: formActionGeometry})}`);
    assert.ok(Math.abs(dirty.save - 32) <= 0.5, `Save has the shared 32px control height: ${JSON.stringify(dirty)}`);
  }
  for (const [width,height] of [[1440,900],[800,900],[390,844]]) {
    await shot(`fixture-form-dirty-${width}`,width,height);
    if (width < 700) {
      assert.equal(await evaluate('[...document.querySelectorAll(".status-actions button")].filter(button=>button.getClientRects().length).length'),2,"phone exposes save and discard actions for the dirty form");
    } else {
      assert.equal(await evaluate('document.querySelector(".status-badge.dirty")?.getClientRects().length > 0'),true,`dirty status badge remains visible at ${width}px`);
    }
  }
  await qualityShots("form");
  await call("Emulation.setDeviceMetricsOverride",{width:1440,height:900,deviceScaleFactor:1,mobile:false});
  await evaluate('[...document.querySelectorAll("button")].find(button=>button.textContent==="Save").click()');
  await wait('[...document.querySelectorAll("[data-client-notice] .client-notification-message")].some(node => node.textContent === "Record saved.")');
  assert.equal(writes.length, 1); assert.equal(writes[0].vals.name, "Compiled addon event");
  await evaluate('[...document.querySelectorAll("[data-client-navigation] button")].find(button=>button.textContent==="Notes").click()');
  await wait('document.querySelector("table[aria-label=Records]") !== null');
  await shot("fixture-list-desktop", 1440, 900);
  await qualityShots("list");
  const gutters = await evaluate(`(() => { const box = s => document.querySelector(s).getBoundingClientRect(); const pad = (s, p) => parseFloat(getComputedStyle(document.querySelector(s))[p]); const cb = box('.list-table tbody .list-row-select'); const lastTd = [...document.querySelectorAll('.list-table tbody tr:first-child td')].pop(); const last = lastTd.getBoundingClientRect(); const panel = document.querySelector('.control-panel'); const cs = getComputedStyle(panel); const row = box('.list-table tbody tr:first-child'), head = box('.list-table thead tr'), mid = cb.top + cb.height / 2; return { checkboxLeft: Math.round(cb.left), panelLeft: Math.round(box('.control-panel').left + parseFloat(cs.paddingLeft)), lastPadEnd: pad('.list-table tbody tr:first-child td:last-child', 'paddingRight'), panelPadRight: parseFloat(cs.paddingRight), rowMid: Math.abs(mid - (row.top + row.height / 2)), rowHeight: Math.round(row.height), headHeight: Math.round(head.height) }; })()`);
  assert.ok(gutters.rowMid <= 1, `the row checkbox is centred in its row: ${JSON.stringify(gutters)}`);
  assert.equal(gutters.headHeight, gutters.rowHeight, `the header row is as tall as a record row: ${JSON.stringify(gutters)}`);
  await assertListGeometry('default list', 1440);
  await groupedListProbe("fixture");
  await pivotProbe("fixture");
  // The same page geometry at every delivered width (the Office Blue list runs the same loop in its own section).
  for (const [width, height] of [[1280, 900], [1024, 900], [800, 1024], [390, 844]]) {
    await call("Emulation.setDeviceMetricsOverride", { width, height, deviceScaleFactor: 1, mobile: !viewportOnly && width < 700 });
    await call("Page.reload"); await wait('document.querySelector("table tbody td button.link")?.textContent === "Compiled addon event"');
    await wait("parseFloat(getComputedStyle(document.documentElement).getPropertyValue('--control-panel-height')) === document.querySelector('.control-panel-host')?.offsetHeight");
    await assertListGeometry('default list', width);
  }
  await call("Emulation.setDeviceMetricsOverride", { width: 1440, height: 900, deviceScaleFactor: 1, mobile: false });
  const toolbarLayout = await evaluate('(()=>{const rect=s=>{const r=document.querySelector(s).getBoundingClientRect();return {left:r.left,right:r.right,y:r.y+r.height/2};};return {action:rect("[data-control-actions] button"),title:rect(".breadcrumbs"),search:rect("[data-control-search]"),tools:rect("[data-control-tools]")};})()');
  assert.ok(toolbarLayout.action.right <= toolbarLayout.title.left, "primary action must precede title");
  assert.ok(toolbarLayout.title.right <= toolbarLayout.search.left + 1, "title must not overlap search");
  assert.ok(Math.abs(toolbarLayout.action.y - toolbarLayout.search.y) < 10, "desktop actions and search must share one row");

  const shortNote = note;
  note = "Long record title " + "department and purchasing operations ".repeat(30);
  for (const [width, height] of [[1440, 900], [390, 844]]) {
    await call("Emulation.setDeviceMetricsOverride", { width, height, deviceScaleFactor: 1, mobile: !viewportOnly && width < 700 });
    await evaluate('document.documentElement.dataset.reloadProbe = "pending"');
    await call("Page.reload");
    await wait('document.documentElement !== null && document.documentElement.dataset.reloadProbe !== "pending" && document.querySelector("table tbody td button.link")?.textContent.startsWith("Long record title") && getComputedStyle(document.querySelector("table[aria-label=Records]")).tableLayout === "fixed"');
    // First frame after reload: table columns must have settled synchronously upon DOM attachment
    // without waiting for requestAnimationFrame.
    const firstFrame = await evaluate('(()=>{const table=document.querySelector("table[aria-label=Records]");return {layout:getComputedStyle(table).tableLayout,autoLayoutClass:table.classList.contains("list-auto-layout"),cols:[...table.querySelectorAll("col")].map(c=>c.style.width)};})()');
    assert.equal(firstFrame.layout, "fixed", `table must settle to fixed layout synchronously on reload without waiting for requestAnimationFrame: ${JSON.stringify(firstFrame)}`);
    assert.equal(firstFrame.autoLayoutClass, false, `table must not retain list-auto-layout class on reload: ${JSON.stringify(firstFrame)}`);
    // every data column but the last has its inline pixel width; the last has none and takes the spare room.
    assert.ok(firstFrame.cols.length > 1 && firstFrame.cols.slice(1, -1).every(w => /^[0-9]+px$/.test(w)) && firstFrame.cols.at(-1) === "", `columns must have settled inline pixel widths on first frame: ${JSON.stringify(firstFrame)}`);
    const longCell = await evaluate('(()=>{const cell=document.querySelector("table tbody td:not(.list-record-selector)"),link=cell.querySelector("button"),table=document.querySelector("table");return {page:document.documentElement.scrollWidth,viewport:innerWidth,table:table.getBoundingClientRect().width,cell:cell.getBoundingClientRect().width,link:link.getBoundingClientRect().width,full:cell.title===link.textContent,clipped:link.scrollWidth>link.clientWidth,layout:getComputedStyle(table).tableLayout};})()');
    assert.ok(longCell.page <= width && longCell.table <= width && longCell.link <= longCell.cell && longCell.clipped && longCell.full, JSON.stringify(longCell));
    assert.equal(longCell.layout, "fixed");
    await shot(`fixture-list-long-${width}`, width, height);
  }
  note = shortNote;
  await assertListHeaderPinned("Default list", 'document.querySelector("table tbody td button.link")?.textContent === "Compiled addon event"');
  await call("Emulation.setDeviceMetricsOverride", {width:1440,height:900,deviceScaleFactor:1,mobile:false});
  await call("Page.reload"); await wait('document.querySelector("table tbody td button.link")?.textContent === "Compiled addon event"');

  // Synchronize real rendering after reload before native pointer hit testing (Moli publishes layout on capture).
  await shot("fixture-list-before-resize", 1440, 900);
  // Exercise actual CDP pointer capture; a synthetic DOM event alone cannot prove dragging.
  await wait('document.querySelector(".column-resize-handle") !== null');
  const resize = await evaluate('(()=>{const h=document.querySelector(".column-resize-handle"),r=h.getBoundingClientRect();return {x:r.x+r.width/2,y:r.y+r.height/2,width:h.parentElement.getBoundingClientRect().width};})()');
  assert.ok(resize.width > 0 && resize.x > 0 && resize.y > 0, `resize handle has actual rendered geometry: ${JSON.stringify(resize)}`);
  await call("Input.dispatchMouseEvent", {type:"mousePressed",x:resize.x,y:resize.y,button:"left",clickCount:1});
  await call("Input.dispatchMouseEvent", {type:"mouseMoved",x:resize.x+60,y:resize.y,button:"left",buttons:1});
  await call("Input.dispatchMouseEvent", {type:"mouseReleased",x:resize.x+60,y:resize.y,button:"left",clickCount:1});
  await shot("fixture-list-after-resize", 1440, 900);
  const resized = await evaluate('document.querySelector(".column-resize-handle").parentElement.getBoundingClientRect().width');
  resizeLayouts.push({ before: resize, pointer_width: resized });
  assert.ok(resized >= resize.width + 50, `column drag must change actual layout width: ${JSON.stringify({resize,resized})}`);
  await evaluate('document.querySelector(".column-resize-handle").focus()');
  await press("ArrowLeft", "ArrowLeft", 37);
  await evaluate('new Promise(resolve=>requestAnimationFrame(()=>requestAnimationFrame(resolve)))');
  await shot("fixture-list-keyboard-resize", 1440, 900);
  const keyboardWidth = await evaluate('document.querySelector(".column-resize-handle").parentElement.getBoundingClientRect().width');
  resizeLayouts.at(-1).keyboard_width = keyboardWidth;
  assert.ok(keyboardWidth < resized, `keyboard resizing must change actual layout width: ${JSON.stringify({resize,resized,keyboardWidth,table:await evaluate('document.querySelector("table").outerHTML')})}`);

  // The last column draws the spare width: a keyboard resize starts from what is drawn and shows on screen.
  const lastHandle = '[...document.querySelectorAll(".column-resize-handle")].at(-1)';
  const lastBefore = await evaluate(`(()=>{const h=${lastHandle};h.focus();return {width:h.parentElement.getBoundingClientRect().width,now:Number(h.getAttribute("aria-valuenow"))};})()`);
  assert.ok(Math.abs(lastBefore.now - lastBefore.width) <= 1, `the last column announces its drawn width: ${JSON.stringify(lastBefore)}`);
  await press("ArrowLeft", "ArrowLeft", 37);
  await evaluate('new Promise(resolve=>requestAnimationFrame(()=>requestAnimationFrame(resolve)))');
  const lastAfter = await evaluate(`(()=>{const h=${lastHandle};return {width:h.parentElement.getBoundingClientRect().width,now:Number(h.getAttribute("aria-valuenow"))};})()`);
  assert.ok(Math.abs(lastAfter.width - (lastBefore.width - 10)) <= 1.5 && Math.abs(lastAfter.now - lastAfter.width) <= 1, `the last column's keyboard resize shows on screen: ${JSON.stringify({lastBefore,lastAfter})}`);

  // Hold a real fixture response: the compiled app keeps its list, focus and geometry.
  for (const [width, height] of [[1440, 900], [390, 844]]) {
    await call("Emulation.setDeviceMetricsOverride", { width, height, deviceScaleFactor: 1, mobile: !viewportOnly && width < 700 });
    await call("Emulation.setEmulatedMedia", { features: [{name: "prefers-reduced-motion", value: "reduce"}, {name: "prefers-color-scheme", value: theme === "auto-light" || theme === "dark" ? "light" : "dark"}] });
    delayNextRead = true;
    await evaluate('(()=>{const input=document.querySelector(".search-input");input.focus();input.value="Loading probe";input.dispatchEvent(new Event("input",{bubbles:true}));input.dispatchEvent(new KeyboardEvent("keydown",{key:"Enter",bubbles:true}));})()');
    // Search facets may legitimately wrap the toolbar before loading feedback appears.
    // Compare after their synchronous render; the loading status itself must add no layout.
    await evaluate('new Promise(resolve=>requestAnimationFrame(()=>requestAnimationFrame(resolve)))');
    await shot(`fixture-loading-baseline-${width}`, width, height);
    const before = await evaluate('document.querySelector("table").getBoundingClientRect().top');
    await wait('document.querySelector(".client-loading")?.hidden === false');
    await shot(`fixture-loading-visible-${width}`, width, height);
    const feedback = await evaluate('(()=>{const node=document.querySelector(".client-loading"),r=node.getBoundingClientRect(),style=getComputedStyle(node);return {top:document.querySelector("table").getBoundingClientRect().top,width:r.width,height:r.height,right:innerWidth-r.right,bottom:innerHeight-r.bottom,pointer:style.pointerEvents,animation:getComputedStyle(node,"::before").animationName,focus:document.activeElement.classList.contains("search-input"),rows:document.querySelectorAll("table tbody tr").length,notice:document.querySelector("[data-client-notice]").textContent,overflow:document.documentElement.scrollWidth>innerWidth,role:node.getAttribute("role"),hit:document.elementFromPoint(r.x+r.width/2,r.y+r.height/2)?.closest(".client-loading")!==null};})()');
    assert.equal(feedback.top, before, "loading must not move the table");
    assert.ok(feedback.width <= 180 && feedback.height <= 40 && feedback.right >= 10 && feedback.right <= 18 && feedback.bottom >= 10 && feedback.bottom <= 18, JSON.stringify(feedback));
    assert.equal(feedback.pointer, "none"); assert.equal(feedback.animation, "none");
    assert.ok(feedback.focus && !feedback.overflow && !feedback.hit, JSON.stringify(feedback));
    assert.equal(feedback.rows, 1); assert.notEqual(feedback.notice, "Loading…"); assert.equal(feedback.role, "status");
    await shot(`fixture-loading-${width}`, width, height);
    await wait('document.querySelector(".client-loading")?.hidden === true');
    await evaluate('document.querySelector(".search-clear-btn").click()');
    await wait('document.querySelectorAll("table tbody tr").length === 1');
  }

  await call("Emulation.setDeviceMetricsOverride", {width:1440,height:900,deviceScaleFactor:1,mobile:false});
  await call("Input.dispatchMouseEvent", {type:"mouseMoved",x:0,y:0});
  const searchBox = '(()=>{const r=e=>document.querySelector(e).getBoundingClientRect().height;return {form:r(".search-form"),panel:r(".control-panel")};})()';
  await evaluate('new Promise(resolve=>requestAnimationFrame(()=>requestAnimationFrame(resolve)))');
  const emptySearchBox = await evaluate(searchBox);
  assert.equal(await evaluate('document.querySelector("select.search-field-select, .search-form select") === null'), true, "the search field picker is not a native select");
  await evaluate('(()=>{ const input = document.querySelector(".search-input"); input.focus(); input.value = "Fixture"; input.dispatchEvent(new Event("input", { bubbles: true })); })()');
  await wait('document.querySelector(".erp-search-autocomplete .erp-autocomplete-item.active") !== null');
  await shot("fixture-search-autocomplete-desktop", 1440, 900);
  await evaluate('document.querySelector(".search-input").dispatchEvent(new KeyboardEvent("keydown", { key: "Enter", bubbles: true }))');
  await wait('document.querySelector(".search-facet-chip") !== null && document.querySelector(".empty-state")?.textContent.includes("No matching records")');
  assert.deepEqual(searchDomains.at(-1), [["name", "ilike", "Fixture"]]);
  assert.deepEqual(await evaluate(searchBox), emptySearchBox, "adding a facet changes neither the search box nor the control panel height");
  assert.equal(await evaluate('(()=>{const c=document.querySelector(".search-chips");return c.scrollHeight<=c.clientHeight+1;})()'), true, "facets stay on one line");
  assert.equal(await evaluate('(()=>{const t=document.querySelector(".search-facet-chip").textContent;return (t.match(/Title/g)||[]).length===1;})()'), true, "the facet label is not doubled");
  assert.equal(await evaluate('document.querySelectorAll("table tbody tr").length'), 0);
  const searchedUrl = await evaluate('location.href');
  await call("Page.navigate", { url: searchedUrl });
  await wait('document.querySelector(".search-facet-chip") !== null && document.querySelector(".empty-state")?.textContent.includes("No matching records")');
  assert.deepEqual(searchDomains.at(-1), [["name", "ilike", "Fixture"]]);
  await evaluate('document.querySelector(".search-menu-toggle").click()');
  await wait('document.querySelector(".erp-search-menu-panel")?.style.display !== "none"');
  await shot("fixture-search-menu-desktop", 1440, 900);
  await evaluate('document.querySelector(".erp-search-toggle-form-btn").click()');
  await wait('document.querySelector(".erp-btn-primary") !== null');
  const applyRect = await evaluate('(()=>{const r=document.querySelector(".erp-btn-primary").getBoundingClientRect();return {x:r.x+r.width/2,y:r.y+r.height/2};})()');
  await call("Input.dispatchMouseEvent", {type:"mouseMoved",x:applyRect.x,y:applyRect.y});
  await shot("fixture-search-apply-hover-desktop",1440,900);
  await call("Input.dispatchMouseEvent", {type:"mouseMoved",x:0,y:0});
  await evaluate('document.querySelector(".erp-search-toggle-form-btn").click()');
  const searchSections = await evaluate('[...document.querySelector(".erp-search-menu-sections").children].map(e=>{const r=e.getBoundingClientRect();return {x:r.x,y:r.y,width:r.width};})');
  assert.equal(searchSections.length, 2, "the search menu shows Filters and Group By, the sections that do something");
  assert.equal(await evaluate('/M[0-9]\\.[A-Z]|not available until/.test(document.querySelector(".erp-search-menu-panel").textContent)'), false, "no internal task id reaches the filter panel");
  for (const width of [800, 900]) {
    await shot(`fixture-search-menu-${width}`, width, 900);
    await wait('(()=>{const r=document.querySelector(".erp-search-menu-panel").getBoundingClientRect();return r.left>=0 && r.right<=innerWidth;})()');
    const bounds = await evaluate('(()=>{const r=document.querySelector(".erp-search-menu-panel").getBoundingClientRect();return {left:r.left,right:r.right,width:innerWidth};})()');
    assert.ok(bounds.left >= 0 && bounds.right <= bounds.width, `search menu must fit ${width}px viewport: ${JSON.stringify(bounds)}`);
  }
  await shot("fixture-search-menu-mobile", 375, 812);
  assert.ok(await evaluate('document.documentElement.scrollWidth <= innerWidth'), "mobile search menu must stay within viewport");
  await shot("fixture-search-menu-desktop", 1440, 900);
  await evaluate('document.querySelector(".search-menu-toggle").click()');
  await wait('document.querySelector(".erp-search-menu-panel")?.style.display === "none"');
  await evaluate('document.querySelector(".search-clear-btn")?.click()');
  await wait('document.querySelectorAll("table tbody tr").length === 1');
  assert.deepEqual(searchDomains.at(-1), []);
  assert.equal(await evaluate('[...new URLSearchParams(location.search).keys()].some(key => key.startsWith("search"))'), false);
  assert.equal(await evaluate('new URLSearchParams(location.search).get("menu")'), "10");
  await shot("fixture-list-mobile", 375, 812);
  await evaluate('[...document.querySelectorAll("[data-client-navigation] button")].find(button=>button.textContent==="Settings").click()');
  await wait('document.querySelector(".settings-toggle input") !== null');
  assert.equal(await evaluate(`document.querySelector('[aria-current="page"]')?.textContent`), "Settings");
  await shot("fixture-settings-desktop", 1440, 900); await shot("fixture-settings-tablet", 768, 1024); await shot("fixture-settings-mobile", 375, 812);
  rejectNextAction = true;
  // Use an unopened action: previously visited descriptors can legitimately be cached.
  await evaluate('[...document.querySelectorAll("[data-client-navigation] button")].find(button=>button.textContent==="Restricted").click()');
  await wait('document.querySelector(".client-error-dialog")?.open === true');
  assert.equal(await evaluate('document.querySelector("#client-error-message")?.textContent'), permissionMessage);
  assert.ok(deniedActions.length >= 1, "the denied action was requested instead of using a cached descriptor");
  assert.ok(deniedActions.every(call => call.ids.length === 1 && call.ids[0] === 23), "only the intended Restricted action is denied");
  assert.equal(await evaluate('document.querySelector(".settings-toggle input") !== null'), true, "permission failure preserves the current Settings view");
  await shot("fixture-permission-dialog-mobile",375,812);
  await evaluate('document.querySelector("[data-error-close]").click()');
  await wait('document.querySelector(".client-error-dialog") === null');
  assert.equal(await evaluate(`[...document.querySelectorAll('.settings-toolbar-actions button,.settings-scope-option:first-child')].every(node=>{const box=node.getBoundingClientRect();return document.elementFromPoint(box.x+box.width/2,box.y+box.height/2)===node})`), true);
  assert.equal(await evaluate('document.querySelector(".client-loading").hidden'), true);
  await shot("fixture-settings-recovered-mobile", 375, 812);
  await call("Page.navigate", { url: base + "/erp/action-22" });
  await wait('document.querySelector(".addon-tile") !== null');
  for (const [width,height] of [[1440,900],[768,1024],[375,812]]) {
    await shot(`fixture-addons-${width}`,width,height);
    assert.ok(await evaluate('document.documentElement.scrollWidth <= innerWidth'), `addons fit ${width}px`);
  }
  await evaluate('document.querySelector(".addon-tile").click()');
  await wait('document.querySelector(".addon-detail") !== null');
  for (const [width,height] of [[1440,900],[1280,800],[768,1024],[390,1400]]) {
    await shot(`fixture-addon-detail-${width}`,width,height);
    assert.ok(await evaluate('document.documentElement.scrollWidth <= innerWidth'), `addon detail fits ${width}px`);
    assert.equal(await evaluate('document.querySelector(".addon-detail").textContent.includes("a later release")'), false, "no task id on the addon detail page");
  }
  await evaluate('document.querySelector(".addon-detail-back").click()');
  await wait('document.querySelector(".addon-tile") !== null');
  await evaluate('document.querySelector(".addon-info-btn").click()');
  await wait('document.querySelector(".addon-info-dialog")?.open === true');
  assert.equal(await evaluate('document.activeElement.closest(".addon-info-dialog") !== null'), true);
  assert.equal(await evaluate('document.querySelector(".addon-info-dialog .message-dialog-close")?.getAttribute("aria-label")'), "Close", "the addon-info dialog has the shared close button");
  assert.equal(await evaluate('getComputedStyle(document.querySelector(".addon-info-dialog")).borderTopLeftRadius'), "10px", "the addon-info dialog uses --radius-overlay");
  await shot("fixture-addon-info-mobile",375,812);
  await evaluate('document.querySelector(".addon-info-dialog button").click()');
  await wait('document.querySelector(".addon-info-dialog") === null');
  assert.equal(await evaluate('document.activeElement.classList.contains("addon-info-btn")'), true);
  }
  assert.deepEqual(exceptions, []);
  if (officeBlue && !listOnly && !dialogProbe) {
    const expected = ['list','search-menu','menu-search','list-selected','list-empty','list-loading','form-long-title','count-error','delete-dialog','relation','chatter','chatter-note','followers','home','user-menu','settings','addons','addon-info','login-boundary'].flatMap(scene=>[1440,800,390].map(width=>`office-blue-${scene}-${width}.png`));
    expected.push(...['form','form-dirty','form-invalid'].flatMap(scene=>[1536,1440,800,390].map(width=>`office-blue-${scene}-${width}.png`)));
    expected.push('office-blue-grouped-folded-1440.png',...[1440,1024,800,390].map(width=>`office-blue-grouped-open-${width}.png`),'office-blue-pivot-1440.png','office-blue-pivot-menu-1440.png',...['plain','nested','sorted','flipped'].map(scene=>`office-blue-pivot-${scene}-390.png`),...['nested','sorted','flipped'].map(scene=>`office-blue-pivot-${scene}-1440.png`));
    for(const name of expected) assert.ok(screenshots.some(shot=>shot.name===name),`required scene missing: ${name}`);
  }
  const result = { list_geometry: listGeometries, status: officeBaseline ? "BASELINE_NOT_ACCEPTANCE" : dialogProbe ? "DIAGNOSTIC_NOT_ACCEPTANCE" : "PASS", visual_findings: visualFindings, shell_measurements: shellMeasurements, scope: `compiled-asset ${remoteCdp ? "remote CDP" : "Chrome"} rendering with explicit protocol fixtures; not backend/auth/database acceptance`, source_revision: (await readFile(path.join(root, "REVISION"), "utf8")).trim(), time_utc: new Date().toISOString(), hostname: hostname(), node: process.version, browser: version, theme, office_blue: officeBlue, viewport_only: viewportOnly, office_blue_scope: listOnly ? "list/category/page/count-error/delete-confirmation layout and narrow drawer close; resize/form/settings not run" : "enumerated screenshots; protocol fixture only", appearance_checks: appearanceChecks, manifest, screenshots, list_layouts: listLayouts, resize_layouts: resizeLayouts, error_layouts: errorLayouts, calls, search_domains: searchDomains, page_reads: pageReads, group_domains: groupDomains, observed_write: writes[0], denied_actions: deniedActions, unhandled_exceptions: exceptions };
  await writeFile(path.join(output, "result.json"), JSON.stringify(result, null, 2) + "\n"); console.log(JSON.stringify(result));
} catch (error) {
  try {
    await writeFile(path.join(output, "failure.json"), JSON.stringify({ status: "FAIL", time_utc: new Date().toISOString(), hostname: hostname(), node: process.version, source_revision: (await readFile(path.join(root, "REVISION"), "utf8")).trim(), message: String(error), unhandled_exceptions: exceptions, screenshots, calls, denied_actions: deniedActions }, null, 2) + "\n");
  } catch (evidenceError) { throw new AggregateError([error, evidenceError], "Browser fixture failed and its failure receipt could not be written"); }
  throw error;
} finally {
  for (const request of pending.values()) clearTimeout(request.timer);
  socket?.close();
  if (chrome?.pid && chrome.exitCode === null && chrome.signalCode === null) {
    const exited = once(chrome, "exit");
    chrome.kill("SIGTERM");
    let timer;
    try { await Promise.race([exited, new Promise(resolve => { timer = setTimeout(resolve, 3000); })]); }
    finally { clearTimeout(timer); }
    if (chrome.exitCode === null && chrome.signalCode === null) { chrome.kill("SIGKILL"); await exited; }
  }
  await new Promise(resolve => server.close(resolve)); await rm(profile, { recursive: true, force: true, maxRetries: 10, retryDelay: 100 });
}
