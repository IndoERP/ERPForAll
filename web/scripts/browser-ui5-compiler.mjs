/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
// Actual localized compiler output in a native browser. No authenticated-serving parity claim.
import { strict as assert } from 'node:assert';
import { readFile, mkdir } from 'node:fs/promises';
import { hostname, homedir } from 'node:os';
import { join, resolve } from 'node:path';
import { createRequire } from 'node:module';
import { fileURLToPath } from 'node:url';
import { build } from 'esbuild';

const require = createRequire(import.meta.url);
const { chromium } = require(join(homedir(), '.local/share/erpforall-qa/node_modules/playwright'));
const root = fileURLToPath(new URL('../../', import.meta.url));
const input = process.argv[2];
if (!input) throw new Error('Usage: browser-ui5-compiler.mjs <translation-fixture.json>');
const fixture = JSON.parse(await readFile(resolve(input), 'utf8'));
assert(Array.isArray(fixture.modules));
assert.equal(typeof fixture.payload, 'string');
const modules = new Map(fixture.modules.map(module => [module.name, module.code]));
assert.equal(modules.size, fixture.modules.length);
const output = join(root, 'logs/ui5-compiler-browser');
await mkdir(output, { recursive: true });
console.log(JSON.stringify({host: hostname(), utc: new Date().toISOString(), node: process.version,
  revision: (await readFile(join(root, 'REVISION'), 'utf8')).trim(), input: resolve(input)}));
const result = await build({
  stdin: { contents: `import {Component,mount} from '@erpforall/elang';
    import * as Main from 'web.Main'; import * as ChildTemplate from 'web.Child';
    class Child extends Component { static template='web.Child'; }
    class Root extends Component { static template='web.Main'; static components={Child}; }
    await mount(Root,document.querySelector('main'),{templates:[Main,ChildTemplate]});
    globalThis.fixtureReady=true;`, resolveDir: root, sourcefile: 'ui5-compiler-browser.ts', loader: 'ts' },
  bundle: true, write: false, format: 'esm', legalComments: 'inline',
  plugins: [{ name: 'actual-localized-output', setup(builder) {
    builder.onResolve({filter: /^web\./}, args => {
      if (!modules.has(args.path)) throw new Error(`Missing compiled module ${args.path}`);
      return {path: args.path, namespace: 'localized'};
    });
    builder.onLoad({filter: /.*/, namespace: 'localized'}, args => ({contents: modules.get(args.path), loader: 'js', resolveDir: root}));
    builder.onResolve({filter: /^@erpforall\/elang(?:\/abi)?$/}, args => ({path: join(root, args.path.endsWith('/abi') ? 'web/src/runtime/abi-impl.ts' : 'web/src/index.ts')}));
  }}],
});
const browser = await chromium.launch({executablePath: process.env.CHROME_BIN ?? '/usr/bin/google-chrome', headless: true});
try {
  console.log(`browser=${browser.version()}`);
  const page = await browser.newPage();
  const errors = [];
  page.on('pageerror', error => errors.push(error.message));
  await page.setContent('<!doctype html><html lang="fr"><head><title>Compiler translation safety</title></head><body><main></main></body></html>');
  await page.addScriptTag({type: 'module', content: result.outputFiles[0].text});
  await page.waitForFunction(() => globalThis.fixtureReady === true);
  assert.equal(await page.locator('#translated').textContent(), fixture.payload);
  assert.equal(await page.locator('#translated').getAttribute('title'), fixture.payload);
  assert.equal(await page.locator('#component').textContent(), fixture.payload);
  assert.equal(await page.locator('#original').textContent(), 'Pareil');
  assert.equal(await page.locator('#contributor').textContent(), 'Autre');
  assert.equal(await page.locator('main script').count(), 0);
  assert.equal(await page.evaluate(() => globalThis.injected), undefined);
  assert.deepEqual(errors, []);
  await page.screenshot({path: join(output, 'localized.png')});
  console.log('PASS: translated static text, attribute and component literal stay data; inherited addon context is isolated; no script insertion, execution or page errors');
} finally { await browser.close(); }
