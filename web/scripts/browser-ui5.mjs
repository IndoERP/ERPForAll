/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
// Primitive-only native browser proof. No backend or whole-view parity claim.
import { strict as assert } from 'node:assert';
import { hostname } from 'node:os';
import { readFile, mkdir } from 'node:fs/promises';
import { build } from 'esbuild';
import { chromium } from 'playwright';
import { auditPage } from '../testing/axe.ts';

const output = new URL('../../logs/ui5-browser/', import.meta.url);
await mkdir(output,{recursive:true});
console.log(JSON.stringify({host:hostname(),utc:new Date().toISOString(),revision:(await readFile(new URL('../../REVISION',import.meta.url),'utf8')).trim(),node:process.version}));
const bundle = await build({entryPoints:[new URL('../src/runtime/accessibility.ts',import.meta.url).pathname],bundle:true,write:false,format:'iife',globalName:'UI5',legalComments:'inline'});
const browser=await chromium.launch({executablePath:process.env.CHROME_BIN ?? '/usr/bin/google-chrome',headless:true});
try {
  console.log(`browser=${browser.version()}`);
  const page=await browser.newPage();
  await page.setContent('<!doctype html><html lang="en"><head><title>UI5 primitives</title></head><body><main><button id="outside">Open</button><section id="dialog" role="dialog" aria-modal="true" aria-label="Focus test"><button id="a" tabindex="2">A</button><button id="b" tabindex="1">B</button><button id="c">C</button><label><input id="r1" type="radio" name="choice">One</label><label><input id="r2" type="radio" name="choice" checked>Two</label></section></main></body></html>');
  await page.addScriptTag({content:bundle.outputFiles[0].text});
  await page.locator('#outside').focus();
  await page.evaluate(()=>{window.trap=window.UI5.createFocusTrap(document.querySelector('#dialog'));});
  const active=()=>page.evaluate(()=>document.activeElement.id);
  assert.equal(await active(),'b');
  for (const id of ['a','c','r2','b']) {await page.keyboard.press('Tab');assert.equal(await active(),id);}
  for (const id of ['r2','c','a','b']) {await page.keyboard.press('Shift+Tab');assert.equal(await active(),id);}
  await page.locator('#outside').focus();assert.equal(await active(),'b');
  // Positive tabindex is deliberately an axe violation: remove the order fixture
  // after testing native traversal, before the clean-audit fixture.
  await page.locator('#a').evaluate(el=>el.tabIndex=0);
  await page.locator('#b').evaluate(el=>el.tabIndex=0);
  const report=await auditPage(page);console.log(`axe clean serious/critical=${report.violations.filter(v=>['serious','critical'].includes(v.impact)).length}`);
  await page.evaluate(()=>{const b=document.createElement('button');b.id='unlabelled';document.querySelector('#dialog').append(b);});
  await assert.rejects(auditPage(page),/button-name/);
  await page.locator('#unlabelled').evaluate(el=>el.remove());
  await page.screenshot({path:new URL('focus.png',output).pathname});
  await page.evaluate(()=>window.trap.dispose());assert.equal(await active(),'outside');
  console.log('PASS: native Tab, Shift+Tab, radio group, outside containment, focus restore, real axe pass/fail');
} finally { await browser.close(); }
