// SPDX-License-Identifier: MIT OR Apache-2.0
// Execute the real WASM in Node and Chromium; neither host implements a barcode parser.
const { readFileSync } = require('node:fs');
const { resolve, join } = require('node:path');
const { createServer } = require('node:http');
const assert = require('node:assert/strict');
const { BarcodeParser } = require(resolve(process.argv[2]));
const cases = JSON.parse(readFileSync('addons/barcodes/tests/fixtures/core-corpus.json', 'utf8'));
const errorText = e => `${e.kind}: ${e.message}`;
for (const c of cases) {
    if (c.compile_error) {
        assert.throws(() => new BarcodeParser(JSON.stringify(c.snapshot)), error => String(error) === errorText(c.compile_error));
        continue;
    }
    const parser = new BarcodeParser(JSON.stringify(c.snapshot));
    try {
        if (c.error) assert.throws(() => parser.parse(c.code), error => String(error) === errorText(c.error));
        else assert.deepEqual(JSON.parse(parser.parse(c.code)), c.result);
    } finally { parser.free(); }
}
async function browserCorpus() {
    const { chromium } = require(join(process.env.HOME, '.local/share/erpforall-qa/node_modules/playwright'));
    const directory = resolve(process.argv[3]);
    const server = createServer((request, response) => {
        const path = new URL(request.url, 'http://localhost').pathname;
        if (path === '/') {
            response.writeHead(200, { 'Content-Type': 'text/html' });
            response.end('<!doctype html><title>Barcode engine conformance</title>');
        } else if (path === '/erp_addon_barcodes.js' || path === '/erp_addon_barcodes_bg.wasm') {
            response.writeHead(200, { 'Content-Type': path.endsWith('.wasm') ? 'application/wasm' : 'text/javascript' });
            response.end(readFileSync(join(directory, path.slice(1))));
        } else { response.writeHead(404); response.end(); }
    });
    let browser;
    try {
        await new Promise((done, reject) => { server.once('error', reject); server.listen(0, '127.0.0.1', done); });
        const executablePath = process.env.CHROME_BIN || (process.platform === 'darwin' ? '/Applications/Google Chrome.app/Contents/MacOS/Google Chrome' : '/usr/bin/google-chrome');
        browser = await chromium.launch({ headless: true, executablePath });
        const page = await browser.newPage();
        await page.goto(`http://127.0.0.1:${server.address().port}/`);
        const actual = await page.evaluate(async corpus => {
            const module = await import('/erp_addon_barcodes.js');
            await module.default();
            return corpus.map(c => {
                let parser;
                try {
                    parser = new module.BarcodeParser(JSON.stringify(c.snapshot));
                    return { result: JSON.parse(parser.parse(c.code)) };
                } catch (error) { return { error: String(error) }; }
                finally { if (parser) parser.free(); }
            });
        }, cases);
        cases.forEach((c, i) => {
            const expectedError = c.compile_error || c.error;
            assert.deepEqual(actual[i], expectedError ? { error: errorText(expectedError) } : { result: c.result });
        });
        console.log(`barcode WASM corpus passed in Node and Chromium: ${cases.length} cases`);
    } finally {
        if (browser) await browser.close();
        await new Promise(done => server.close(done));
    }
}
browserCorpus().catch(error => { console.error(error); process.exitCode = 1; });
