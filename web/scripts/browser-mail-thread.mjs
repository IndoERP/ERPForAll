/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
// End-to-end proof of the shared thread, message and composer components in Chromium (
// ). A real MessagingStore holds fixture records; the transport is a
// fixture that answers like thread_post. No server, no bus and no authenticated session are involved.
//
//   MAIL_THREAD_BROWSER_OUTPUT=<evidence dir> CHROME_BIN=<chrome> node web/scripts/browser-mail-thread.mjs
import assert from 'node:assert/strict';
import { createServer } from 'node:http';
import { hostname, homedir } from 'node:os';
import { createRequire } from 'node:module';
import path from 'node:path';
import { readFile, writeFile, mkdir, stat } from 'node:fs/promises';
import { createHash } from 'node:crypto';
import { build } from 'esbuild';

import { auditPage } from '../testing/axe.ts';
const root = new URL('../../', import.meta.url).pathname;
const qa = path.join(homedir(), '.local/share/erpforall-qa');
assert.equal((await stat(qa)).mode & 0o077, 0, 'Operator QA directory must be private (mode 700)');
const { chromium } = createRequire(path.join(qa, 'package.json'))('playwright');
const output = process.env.MAIL_THREAD_BROWSER_OUTPUT;
assert.ok(output, 'Set MAIL_THREAD_BROWSER_OUTPUT to a task-owned evidence directory');
await mkdir(output, { recursive: true });

const entry = `
import { RegistryHub, createTranslator } from '@erpforall/elang';
import { MessagingStore } from '@erpforall/mail-store';
import { Thread, Composer, defineThreadViewRegistries, registerBuiltins } from ${JSON.stringify(root + 'web/packages/mail-thread-view/src/index.ts')};
const kind = (k, extra = {}) => ({ kind: k, ...extra });
const schema = JSON.stringify({ wire: 1, schema: 'e2e', models: {
  'res.partner': { fields: { name: kind('string') } },
  'mail.thread': { fields: { name: kind('string') } },
  'mail.message': { fields: { body: kind('html'), message_type: kind('selection'), date: kind('datetime'), author_id: kind('many2one', { target: 'res.partner' }), subject: kind('string'), tracking: kind('string'), starred: kind('bool'), pinned_at: kind('datetime') } },
} });
const errors = [];
const store = new MessagingStore({ reportError: e => errors.push(String(e && e.message || e)), schema, config: { collectOnSettle: true } });
let xmax = 10;
const answer = records => JSON.stringify({ wire: 1, schema: 'e2e', asof: { xmin: '1', xmax: String(xmax += 10), xip: [], own: null }, records });
const ref = { model: 'mail.thread', id: 7 };
const track = JSON.stringify([{ f: 3, n: 'stage_id', t: 'many2one', o: 1, v: 2, od: 'New', vd: 'Won' }]);
store.applyAnswer(answer({
  'res.partner': [{ id: 1, name: 'Ann Lee' }, { id: 2, name: 'Bob' }, { id: 3, name: 'Anders' }],
  'mail.message': [
    { id: 1, body: '<p>Quote sent to the customer.</p>', message_type: 'comment', date: '2026-11-07 09:00:00', author_id: 1, subject: 'Quarterly plan' },
    { id: 2, body: '<p>Looks good, <b>please confirm</b> the delivery date.</p>', message_type: 'comment', date: '2026-11-08 08:30:00', author_id: 3 },
    { id: 3, body: '', message_type: 'notification', date: '2026-11-08 09:00:00', author_id: 2, tracking: track },
  ],
}));
store.windows.replace(ref, [1, 2, 3], false);
const posts = [];
const state = { failNext: false, ready: false };
const escape = s => s.replace(/[&<>'"]/g, c => ({ '&': '&amp;', '<': '&lt;', '>': '&gt;', "'": '&#39;', '"': '&#34;' }[c]));
// What the server's thread_post renders from a text body: escaped text, mention anchors for the byte spans, <br> for line breaks.
const renderText = dto => {
  const bytes = new TextEncoder().encode(dto.body), decode = (a, b) => new TextDecoder().decode(bytes.slice(a, b));
  let html = '', at = 0;
  for (const m of [...(dto.mentions ?? [])].sort((a, b) => a.start - b.start)) {
    html += escape(decode(at, m.start)) + '<a class="o_mail_redirect" data-oe-model="res.partner" data-oe-id="' + m.partner_id + '">' + escape(decode(m.start, m.end)) + '</a>';
    at = m.end;
  }
  return (html + escape(decode(at, bytes.length))).replace(/\\r?\\n/g, '<br>');
};
let nextId = 10;
const transport = {
  post: async (r, dto) => {
    posts.push(dto);
    if (state.failNext) { state.failNext = false; throw new Error('Network unreachable'); }
    const id = nextId++;
    return { message_id: id, replayed: posts.filter(p => p.client_uid === dto.client_uid).length > 1,
      store: answer({ 'mail.message': [{ id, body: renderText(dto), message_type: 'comment', date: '2026-11-08 10:00:00', author_id: 2 }] }) };
  },
};
const pager = { loadLatest: async () => {}, loadOlder: async () => {} };
const hub = new RegistryHub();
const registries = defineThreadViewRegistries(hub);
registerBuiltins(registries);
const common = { store, ref, registries, transport, pager, self: { partnerId: 2 }, translate: createTranslator({ language: 'en_US' })._t,
  reportError: e => errors.push(String(e && e.message || e)), services: { now: () => new Date('2026-11-08T12:00:00Z'), timeZone: 'UTC', locale: 'en-US' } };
const thread = new Thread({ ...common, threadName: () => 'Order 42' });
const composer = new Composer({ ...common, platform: 'other' });
document.querySelector('#thread').append(thread.element);
document.querySelector('#composer').append(composer.element);
window.fixture = { posts, errors, state, setTheme: t => { document.documentElement.dataset.theme = t; }, ready: true };
`;
const bundle = await build({ stdin: { contents: entry, loader: 'ts', resolveDir: root + 'web' }, bundle: true, write: false, format: 'esm', target: 'es2023',
  alias: { '@erpforall/elang': root + 'web/src/index.ts', '@erpforall/mail-store': root + 'web/packages/mail-store/src/index.ts', '@erpforall/store-wire': root + 'web/packages/store-wire/src/index.ts', '@erpforall/expr': root + 'web/packages/expr/src/index.ts' },
  legalComments: 'inline' });
const js = bundle.outputFiles[0].contents;
const css = (await readFile(root + 'addons/web/static/src/client/client.css', 'utf8')) + '\n' + (await readFile(root + 'web/packages/mail-thread-view/src/thread-view.css', 'utf8'));
const html = `<!doctype html><html lang="en"><head><meta charset="utf-8"><title>Thread view fixture</title><style>${css}</style></head><body><main aria-label="Order 42"><button id="before" type="button">Before the thread</button><div id="thread"></div><div id="composer"></div></main><script type="module" src="/fixture.js"></script></body></html>`;
const server = createServer((request, response) => { response.writeHead(200, { 'Content-Type': request.url === '/fixture.js' ? 'text/javascript' : 'text/html' }); response.end(request.url === '/fixture.js' ? js : html); });
await new Promise((resolve, reject) => { server.once('error', reject); server.listen(0, '127.0.0.1', resolve); });
let browser;
try {
  browser = await chromium.launch({ executablePath: process.env.CHROME_BIN ?? '/usr/bin/google-chrome', headless: true });
  const stamp = { host: hostname(), utc: new Date().toISOString(), revision: (await readFile(root + 'REVISION', 'utf8')).trim(), node: process.version, platform: process.platform, browser: browser.version(), command: 'node web/scripts/browser-mail-thread.mjs', bundle_sha256: createHash('sha256').update(js).digest('hex'), scope: 'fixture store and fixture transport; no server, bus or session' };
  console.log(JSON.stringify(stamp));
  for (const viewport of [{ width: 1280, height: 900 }, { width: 390, height: 844 }]) {
    const page = await browser.newPage({ viewport });
    const pageErrors = [];
    page.on('pageerror', error => pageErrors.push(String(error)));
    try {
      await page.goto(`http://127.0.0.1:${server.address().port}/`);
      await page.waitForFunction(() => window.fixture?.ready);
      const ids = () => page.$$eval('article[data-message-id]', els => els.map(e => e.dataset.messageId));
      assert.deepEqual(await ids(), ['3', '2', '1']);
      const days = await page.$$eval('article[data-message-id]', els => els.map(e => e.querySelector('.erp-mail-day')?.textContent ?? null));
      assert.deepEqual(days, ['Today', null, 'Yesterday']);
      assert.match(await page.locator('article[data-message-id="3"]').innerText(), /Stage\s+New\s+\u2192\s+changed to\s+Won/);
      assert.ok(await page.locator('article[data-message-id="2"] b').count() === 1);
      for (const theme of ['light', 'dark']) {
        await page.evaluate(t => window.fixture.setTheme(t), theme);
        await auditPage(page);
      }
      await page.evaluate(() => window.fixture.setTheme('light'));

      // Keyboard: one tab stop for the feed, then its toolbar, then back with Escape.
      await page.locator('#before').focus();
      await page.keyboard.press('Tab');
      assert.equal(await page.evaluate(() => document.activeElement?.dataset.messageId), '3');
      await page.keyboard.press('ArrowDown');
      assert.equal(await page.evaluate(() => document.activeElement?.dataset.messageId), '2');
      await page.keyboard.press('Tab');
      const firstAction = await page.evaluate(() => document.activeElement?.dataset.action);
      assert.ok(firstAction, 'the toolbar of the current article is the next tab stop');
      await page.keyboard.press('ArrowRight');
      assert.notEqual(await page.evaluate(() => document.activeElement?.dataset.action), firstAction);
      await page.keyboard.press('Escape');
      assert.equal(await page.evaluate(() => document.activeElement?.dataset.messageId), '2');
      await page.keyboard.press('End');
      assert.equal(await page.evaluate(() => document.activeElement?.dataset.messageId), '1');

      // Mention with the keyboard, send with Ctrl+Enter.
      await page.locator('.erp-mail-draft').click();
      await page.keyboard.type('Hello @an');
      await page.waitForSelector('.erp-mail-suggestions:not([hidden])');
      assert.deepEqual(await page.$$eval('.erp-mail-suggestion', els => els.map(e => e.textContent)), ['Anders', 'Ann Lee']);
      await auditPage(page);
      await page.keyboard.press('ArrowDown');
      await page.keyboard.press('Enter');
      await page.keyboard.type('thanks');
      assert.equal(await page.locator('.erp-mail-draft').inputValue(), 'Hello @Ann Lee thanks');
      await page.keyboard.press('Control+Enter');
      await page.waitForFunction(() => window.fixture.posts.length === 1 && document.querySelectorAll('article[data-message-id]').length === 4);
      const first = await page.evaluate(() => window.fixture.posts[0]);
      assert.match(first.client_uid, /^[0-9a-f-]{36}$/);
      assert.deepEqual({ ...first, client_uid: 'uid' }, { body: 'Hello @Ann Lee thanks', message_type: 'comment', subtype: 'comment', mentions: [{ start: 6, end: 14, partner_id: 1 }], client_uid: 'uid' });
      assert.equal(await page.locator('.erp-mail-pending').count(), 0);
      // The server's anchor is drawn as the renderer's own mention element, without a link.
      assert.equal(await page.locator('article[data-message-id="10"] .o-mail-mention[data-partner-id="1"]').innerText(), '@Ann Lee');
      assert.equal(await page.locator('article[data-message-id="10"] .erp-mail-body a').count(), 0);
      assert.equal(await page.locator('.erp-mail-draft').inputValue(), '');
      assert.equal(await page.evaluate(() => document.activeElement?.className), 'erp-mail-draft');

      // A failed post stays visible and is retried with the same client_uid.
      await page.evaluate(() => { window.fixture.state.failNext = true; });
      await page.keyboard.type('Second try');
      await page.keyboard.press('Shift+Enter');
      await page.keyboard.type('on two lines');
      await page.keyboard.press('Control+Enter');
      await page.waitForSelector('.erp-mail-pending.is-failed');
      assert.match(await page.locator('.erp-mail-pending-error').innerText(), /Network unreachable/);
      await auditPage(page);
      await page.locator('.erp-mail-retry').click();
      await page.waitForFunction(() => window.fixture.posts.length === 3 && !document.querySelector('.erp-mail-pending'));
      const [, failed, retried] = await page.evaluate(() => window.fixture.posts);
      assert.equal(failed.client_uid, retried.client_uid);
      assert.equal(await page.locator('article[data-message-id]').count(), 5);
      // The line break the user typed is a <br> in the stored message, and the two lines are laid out on separate rows.
      assert.equal(await page.locator('article[data-message-id="11"] .erp-mail-body br').count(), 1);
      const lines = await page.$$eval('article[data-message-id="11"] .erp-mail-body', els => els[0].innerText.split('\n'));
      assert.deepEqual(lines, ['Second try', 'on two lines']);
      await auditPage(page);
      assert.deepEqual(pageErrors, []);
      assert.deepEqual(await page.evaluate(() => window.fixture.errors), ['Network unreachable']);
      await page.screenshot({ path: output + `/thread-${viewport.width}.png`, fullPage: true });
      console.log(`PASS thread view fixture viewport=${viewport.width}: render, days, tracking, axe light and dark with contrast, keyboard traversal, keyboard mention, send chord, failed post retry`);
    } finally { await page.close(); }
  }
  await writeFile(output + '/receipt.json', JSON.stringify({ ...stamp, result: 'PASS' }, null, 2) + '\n');
} finally {
  await browser?.close();
  await new Promise((resolve, reject) => server.close(error => (error ? reject(error) : resolve())));
}
