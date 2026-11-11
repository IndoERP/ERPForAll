/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
/// <reference types="node" />
import { readFileSync, readdirSync } from "node:fs";
import { resolve } from "node:path";
import { expect, test } from "vitest";

const SINK = /\b\w+\.(?:innerHTML|outerHTML)\s*=|\[\s*["'`](?:innerHTML|outerHTML)["'`]\s*\]\s*=|\binsertHTML\b|\bsetHTMLUnsafe\b|\bparseHTMLUnsafe\b|\bsrcdoc\b|\binsertAdjacentHTML\s*\(|\bcreateContextualFragment\s*\(|\bDOMParser\b|\b(?:adoptNode|importNode)\s*\(|\bmarkup\s*\(|\bdocument\s*\.\s*write(?:ln)?\s*\(|\beval\s*\(|\bnew\s+Function\b|\bset(?:Timeout|Interval)\s*\(\s*["'`]/g;
const hits = (source: string): string[] => [...source.matchAll(SINK)].map(match => match[0]);
test("HTML editor owns exactly one inert template parsing sink and no live HTML sinks", () => {
  const dir = resolve(process.cwd(), "../addons/web/static/src/client") + "/";
  const sinks: string[] = [];
  for (const name of readdirSync(dir).filter(name => /^html-editor(?:-.+)?\.ts$/.test(name))) {
    const source = readFileSync(dir + name, "utf8");
    for (const sink of hits(source)) sinks.push(`${name}:${sink}`);
  }
  expect(sinks).toEqual(["html-editor-document.ts:template.innerHTML ="]);
});


test("sink guard detects active document writes and string evaluation", () => {
  for (const source of ['document.write("<p>x</p>")', 'document.writeln("x")', 'document . write ("x")', 'eval("x")', 'window.eval("x")', 'new Function("x")', 'setTimeout("x", 1)', 'setInterval(`x`, 1)', 'node.innerHTML = value', 'node.outerHTML = value', 'node.insertAdjacentHTML("beforeend", value)', 'new DOMParser()', 'node.createContextualFragment(value)', 'document.execCommand("insertHTML", false, value)', 'node.setHTMLUnsafe(value)', 'Document.parseHTMLUnsafe(value)', 'node["innerHTML"] = value', "node['outerHTML'] = value", 'frame.srcdoc = value', 'frame.setAttribute("srcdoc", value)']) expect(hits(source).length, source).toBeGreaterThan(0);
  expect(hits('const template = document.createElement("template"); template.innerHTML = source;')).toEqual(['template.innerHTML =']);
  expect(hits('node.textContent = source; node.append(document.createTextNode(source));')).toEqual([]);
});
