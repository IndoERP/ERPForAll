/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
import { expect, test } from "vitest";
import { renderChatReply } from "../src/presentation.js";

const markdown = [
  "| Month | Currency | Confirmed Sales | Orders |",
  "| --- | --- | ---: | ---: |",
  "| November 2026 | USD | 53,047.92 | 17 |",
  "| **Total** | **USD** | **53,047.92** | **17** |",
].join("\n");
const html = "<table><thead><tr><th>Month</th><th>Currency</th><th>Confirmed Sales</th><th>Orders</th></tr></thead>" +
  "<tbody><tr><td>November 2026</td><td>USD</td><td>53,047.92</td><td>17</td></tr>" +
  "<tr><td><strong>Total</strong></td><td>USD</td><td>53,047.92</td><td>17</td></tr></tbody></table>";

function show(text: string) {
  const host = document.createElement("div");
  renderChatReply(host, text);
  return host;
}
function fence(lang: string, source: string) { return "```" + lang + "\n" + source + "\n```"; }

test.each([markdown, html])("sales figures have semantic rows in a mixed reply", source => {
  const reply = show("**Monthly Sales by Currency**\n\nHere are the orders.\n\n" + source + "\n\nEnd.");
  const table = reply.querySelector("table")!;
  expect(Array.from(table.tHead!.rows[0]!.cells, c => c.textContent)).toEqual(["Month", "Currency", "Confirmed Sales", "Orders"]);
  expect(Array.from(table.tBodies[0]!.rows[0]!.cells, c => c.textContent)).toEqual(["November 2026", "USD", "53,047.92", "17"]);
  expect(table.tBodies[0]!.rows[1]?.querySelector("strong")?.textContent).toBe("Total");
  expect(reply.querySelector("p > strong")?.textContent).toBe("Monthly Sales by Currency");
  expect(reply.textContent).toContain("End.");
});

test.each([["md", markdown], ["markdown", markdown], ["html", html]])("only a standalone fenced %s table gets a preview and literal source", (lang, source) => {
  const reply = show(fence(lang!, source!));
  expect(reply.querySelectorAll("table")).toHaveLength(1);
  expect(reply.querySelector("details")?.open).toBe(false);
  expect(reply.querySelector("summary")?.textContent).toBe("Lihat sumber");
  expect(reply.querySelector("details code")?.textContent).toBe(source);
  const mixed = show(fence(lang!, source + "\n\n<p>Example</p>"));
  expect(mixed.querySelector("table")).toBeNull();
  expect(mixed.querySelector("pre code")?.textContent).toContain("<p>Example</p>");
});

test("formats nested lists, ordered starts, inline emphasis, quotes, links and ordinary code", () => {
  const reply = show("### Review\n\n3. *First*\n   - **Nested**\n4. Second\n\n> See [docs](https://example.com/) and `<tag>`.\n\n" + fence("ts", "const x = '<table>';"));
  expect(reply.querySelector("h3")?.textContent).toBe("Review");
  expect(reply.querySelector("ol")?.start).toBe(3);
  expect(reply.querySelector("ol em")?.textContent).toBe("First");
  expect(reply.querySelector("ol ul li strong")?.textContent).toBe("Nested");
  expect(reply.querySelector("blockquote a")?.getAttribute("href")).toBe("https://example.com/");
  expect(reply.querySelector("blockquote code")?.textContent).toBe("<tag>");
  expect(reply.querySelector("pre code")?.textContent?.trimEnd()).toBe("const x = '<table>';");
  expect(reply.querySelector("details,table")).toBeNull();
});

test("only allowed formatting reaches the document and oversized replies stay readable", () => {
  const reply = show('<p class="app-bar" data-action="open" style="color:red"><strong>Safe</strong> <a href="javascript:void(0)">link</a><img src="/tracker"><button>Run</button></p>');
  expect(reply.querySelector("strong")?.textContent).toBe("Safe");
  expect(reply.querySelector("[href], [style], [data-action], .app-bar, img, button")).toBeNull();
  const large = "# Heading\n" + "a".repeat(65536);
  expect(show(large).textContent).toContain(large);
  const sparse = "| Value ".repeat(100) + "|\n" + "| --- ".repeat(100) + "|\n" + "| 1 |\n".repeat(80);
  expect(show(sparse).querySelector("table")).toBeNull();
  expect(show(sparse).textContent).toContain(sparse);
});
