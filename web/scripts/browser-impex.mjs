// SPDX-License-Identifier: MIT OR Apache-2.0
// A real download is filled and uploaded again. The only storage is the test's temporary directory.
import assert from "node:assert/strict";
import { execFileSync } from "node:child_process";
import { mkdtemp, readFile, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import path from "node:path";
import { chromium } from "playwright";

let config = "";
for await (const chunk of process.stdin) config += chunk;
const { origin, cookies, contacts, tags } = JSON.parse(config);
const dir = await mkdtemp(path.join(tmpdir(), "erp-impex-browser-"));
const browser = await chromium.launch({
  executablePath:
    process.env.CHROME_BIN ??
    (process.platform === "darwin"
      ? "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome"
      : "/usr/bin/google-chrome"),
});
const context = await browser.newContext({
  viewport: { width: 1280, height: 860 },
});
await context.addCookies(
  cookies.split(/;\s*/).map((pair) => {
    const sep = pair.indexOf("=");
    return {
      url: origin,
      name: pair.slice(0, sep),
      value: pair.slice(sep + 1),
    };
  }),
);
const page = await context.newPage();
page.setDefaultTimeout(15000);
const dialog = page.getByRole("dialog");
async function capture(name) {
  const output = process.env.ERP_IMPEX_SCREENSHOTS;
  if (output) await page.screenshot({ path: path.join(output, `${name}.png`) });
}
async function assertInView(control, message) {
  assert.ok(
    await control.evaluate((node) => {
      const box = node.getBoundingClientRect();
      const scroller = node.closest(".message-dialog-body")?.getBoundingClientRect();
      return (
        box.top >= Math.max(0, scroller?.top ?? 0) &&
        box.bottom <= Math.min(window.innerHeight, scroller?.bottom ?? window.innerHeight) &&
        box.left >= 0 && box.right <= window.innerWidth &&
        box.height > 0 && box.width > 0
      );
    }),
    message,
  );
}
async function assertExportLayout(format) {
  await assertInView(format, "file format is visible before scrolling the field picker");
  assert.ok(
    await dialog.locator(".record-export-scope").evaluate((scope) =>
      [...scope.querySelectorAll("label")].every((label) => {
        const radio = label.querySelector("input").getBoundingClientRect();
        const text = label.querySelector("span").getBoundingClientRect();
        const gap = text.left - radio.right;
        return (
          radio.width >= 14 && radio.width <= 20 &&
          radio.height >= 14 && radio.height <= 20 &&
          gap >= 4 && gap <= 12 &&
          radio.left - scope.getBoundingClientRect().left <= 16
        );
      }),
    ),
    "each compact radio sits next to its left-aligned label",
  );
}
const contact = "Workbook browser contact";
const mail = "workbook@example.test";
const tag = "CSV browser tag";
async function openAction(action) {
  await page
    .getByRole("button", { name: "Import / Export", exact: true })
    .click();
  await page.getByRole("menuitem", { name: action, exact: true }).click();
  await dialog.waitFor();
}
async function download(button, filename) {
  const [file] = await Promise.all([
    page.waitForEvent("download"),
    button.click(),
  ]);
  const target = path.join(dir, filename);
  await file.saveAs(target);
  return target;
}
async function count(model, name) {
  return page.evaluate(
    async ({ model, name }) => {
      const response = await fetch(`/erp/json/2/${model}/search_count`, {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify({ domain: [["name", "=", name]] }),
      });
      if (!response.ok)
        throw new Error(`Failed to count fixture records: ${response.status}`);
      return response.json();
    },
    { model, name },
  );
}
const workbook = (mode, file, output) =>
  JSON.parse(
    execFileSync(
      "python3",
      [
        "-I",
        "-c",
        `
import json, sys, re, zipfile
import xml.etree.ElementTree as ET
mode, src, dst = sys.argv[1:]
ns = "{http://schemas.openxmlformats.org/spreadsheetml/2006/main}"
with zipfile.ZipFile(src) as book:
    sheets = ET.fromstring(book.read("xl/workbook.xml")).findall(".//"+ns+"sheet")
    shared = []
    if "xl/sharedStrings.xml" in book.namelist():
        for item in ET.fromstring(book.read("xl/sharedStrings.xml")):
            shared.append("".join(el.text or "" for el in item.iter(ns+"t")))
    sheet = ET.fromstring(book.read("xl/worksheets/sheet1.xml"))
    data = sheet.find(ns+"sheetData")
    def value(cell):
        raw = cell.findtext(ns+"v", "")
        return shared[int(raw)] if cell.get("t") == "s" else "".join(cell.itertext()) if cell.get("t") == "inlineStr" else raw
    header = [(re.match("[A-Z]+", c.get("r"))[0],value(c)) for c in data[0]]
    rows = [[value(c) for c in row] for row in data]
    if mode == "fill":
        assert len(rows) == 1 and len(sheets) == 2, "expected empty data and a separate Contacts guide"
        row = ET.SubElement(data, ns+"row", {"r":"2"})
        for key, val in [("name", "Workbook browser contact"), ("email","workbook@example.test")]:
            col, = [col for col, label in header if label.casefold() == key]
            c=ET.SubElement(row,ns+"c",{"r":col+"2","t":"inlineStr"})
            ET.SubElement(ET.SubElement(c,ns+"is"),ns+"t").text=val
        with zipfile.ZipFile(dst,"w",zipfile.ZIP_DEFLATED) as out:
            for item in book.namelist():
                out.writestr(item, ET.tostring(sheet) if item=="xl/worksheets/sheet1.xml" else book.read(item))
    print(json.dumps(rows))
`,
        mode,
        file,
        output ?? "",
      ],
      { encoding: "utf8" },
    ),
  );

async function importFile(file, model, name) {
  const [chooser] = await Promise.all([
    page.waitForEvent("filechooser"),
    dialog.getByRole("button", { name: "Choose file", exact: true }).click(),
  ]);
  await chooser.setFiles(file);
  assert.equal(
    await dialog.getByText(path.basename(file), { exact: true }).count(), 1,
    "the selected filename is displayed once",
  );
  const test = dialog.getByRole("button", { name: "Test import", exact: true });
  await test.click();
  const status = dialog.getByText(/Test passed for 1 record/i);
  await status.waitFor();
  await assertInView(status, "the test result is visible beside the import actions");
  assert.ok(
    await status.evaluate((node) => node.closest(".message-dialog-footer") !== null),
    "the test result remains visible when the preview scrolls",
  );
  await capture(
    model === "res.partner" ? "contacts-import-preview" : "tags-import-mobile",
  );
  assert.equal(await count(model, name), 0, "testing must roll back");
  await dialog.getByRole("button", { name: "Import", exact: true }).click();
  await dialog.waitFor({ state: "hidden" });
  assert.equal(await count(model, name), 1);
}
try {
  await page.goto(`${origin}/erp/action-${contacts}`);
  await openAction("Import records");
  const blank = await download(
    dialog.getByRole("button", { name: /download.*xlsx.*template/i }),
    "template.xlsx",
  );
  await capture("contacts-import-template");
  const filled = path.join(dir, "contact.xlsx");
  workbook("fill", blank, filled);
  await importFile(filled, "res.partner", contact);
  const row = page.locator("tr[data-id]").filter({ hasText: contact });
  await row.locator("input.list-row-select").check();
  await openAction("Export");
  await dialog.locator('[data-remove="email"]').waitFor();
  for (const key of await dialog
    .locator("[data-remove]")
    .evaluateAll((nodes) => nodes.map((n) => n.dataset.remove))) {
    await dialog.locator(`[data-remove="${key}"]`).click();
  }
  await dialog.locator('[data-field="name"]').click();
  await dialog.locator('[data-field="email"]').click();
  const move = dialog.getByRole("button", {
    name: "Move Email up",
    exact: true,
  });
  await move.press("Enter");
  const format = dialog.getByLabel("File format");
  await assertExportLayout(format);
  await format.selectOption("xlsx");
  await capture("contacts-export");
  const selected = await download(
    dialog.locator("[data-export]"),
    "selected.xlsx",
  );
  const rows = workbook("read", selected);
  assert.deepEqual(
    rows.map((r) => r.map(String)),
    [
      ["Email", "Name"],
      [mail, contact],
    ],
  );
  await page.setViewportSize({ width: 390, height: 844 });
  await dialog
    .getByRole("radio", { name: /matching current filters/i })
    .check();
  await assertExportLayout(format);
  await format.selectOption("csv");
  await capture("contacts-export-mobile");
  const all = await download(dialog.locator("[data-export]"), "filtered.csv");
  const csv = await readFile(all, "utf8");
  assert.ok(csv.includes(mail + "," + contact));
  assert.ok(
    csv.trim().split(/\r?\n/).length > 2,
    "filtered scope includes other fixture contacts",
  );
  assert.ok(
    await dialog.evaluate(
      (node) => node.getBoundingClientRect().width <= window.innerWidth,
    ),
  );
  await dialog
    .getByRole("button", { name: "Close", exact: true })
    .last()
    .click();
  await page.goto(`${origin}/erp/action-${tags}`);
  await openAction("Import records");
  const blankCsv = await download(
    dialog.getByRole("button", { name: /download.*csv.*template/i }),
    "tags.csv",
  );
  const header = (await readFile(blankCsv, "utf8")).trim().split(",");
  const values = header.map((h) =>
    h.toLowerCase() === "name"
      ? tag
      : h.toLowerCase() === "active"
        ? "true"
        : "",
  );
  assert.ok(values.includes(tag), "tag template includes Name");
  await writeFile(
    blankCsv,
    header.join(",") + "\r\n" + values.join(",") + "\r\n",
  );
  await importFile(blankCsv, "res.partner.category", tag);
  console.log(
    "Contacts XLSX and tags CSV round trips, dry-run, ordered downloads, selection, filtered scope and mobile passed.",
  );
} finally {
  await browser.close();
  await rm(dir, { recursive: true, force: true });
}
