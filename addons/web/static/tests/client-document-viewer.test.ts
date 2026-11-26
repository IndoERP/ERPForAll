/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
import { afterEach, beforeEach, expect, test, vi } from "vitest";
import { DialogService } from "../src/client/dialog-service.js";
import { attachmentViewerFile, openDocumentViewer, readViewerLimits, reportViewerFile, viewerKind, type ViewerFile } from "../src/client/document-viewer.js";
import { PdfOpenError, type PdfDocument, type PdfPageSize } from "../src/client/pdf-document.js";
import { RpcClient } from "../src/client/transport.js";

let dialogs: DialogService;
beforeEach(() => {
  Object.defineProperty(HTMLDialogElement.prototype, "showModal", { configurable: true, value(this: HTMLDialogElement) { this.open = true; } });
  Object.defineProperty(HTMLDialogElement.prototype, "close", { configurable: true, value(this: HTMLDialogElement) { this.open = false; } });
  dialogs = new DialogService(() => {});
});
afterEach(() => { dialogs.dispose(); vi.restoreAllMocks(); vi.unstubAllGlobals(); document.body.replaceChildren(); });

const A4: PdfPageSize = { width: 595, height: 842 };
type Draw = { page: number; scale: number; rotation: number };
function fakePdf(pages: number, options: { sizes?: Record<number, PdfPageSize>; broken?: number } = {}) {
  const draws: Draw[] = [];
  const doc: PdfDocument = {
    pages,
    size: vi.fn(async (page: number) => options.sizes?.[page] ?? A4),
    draw: vi.fn(async (page: number, canvas: HTMLCanvasElement, scale: number, rotation: number, signal: AbortSignal) => {
      if (signal.aborted) throw new DOMException("cancelled", "AbortError");
      if (page === options.broken) throw new Error("the page stream is damaged");
      draws.push({ page, scale, rotation });
      canvas.width = 8; canvas.height = 8;
    }),
    picture: vi.fn(async (page: number) => new Blob([`page ${page}`], { type: "image/png" })),
    close: vi.fn(async () => {}),
  };
  return { doc, draws, open: vi.fn(async () => doc) };
}
function fakeFiles(fileName = "Order S00001.pdf") {
  const getFile = vi.fn(async (_url: string, _type: string) => ({ blob: new Blob(["%PDF-1.7"], { type: "application/pdf" }), fileName }));
  const call = vi.fn(async (_model: string, _method: string, _args: Record<string, unknown>) => settings(2000, 100));
  return { rpc: { getFile, call } as unknown as RpcClient, getFile, call };
}
/** The answer of `ir.config.setting.erp_read_values` for the viewer's two bounds. */
const settings = (pages: unknown, print: unknown) => ({ wire_major: 1, values: [{ key: "web.document_viewer_max_pages", value: pages }, { key: "web.document_viewer_print_max_pages", value: print }] });
const order: ViewerFile = reportViewerFile("/erp/report/pdf/sale.order.document/1,2", "Order");
const q = <T extends HTMLElement>(root: ParentNode, selector: string): T => {
  const node = root.querySelector<T>(selector);
  if (!node) throw new Error(`missing ${selector}`);
  return node;
};
const labelled = (root: ParentNode, label: string): HTMLButtonElement => q<HTMLButtonElement>(root, `[aria-label="${label}"]`);
const key = (dialog: HTMLElement, name: string, init: KeyboardEventInit = {}): void => { q(dialog, ".document-viewer-stage").dispatchEvent(new KeyboardEvent("keydown", { key: name, bubbles: true, cancelable: true, ...init })); };
const pageBox = (dialog: HTMLElement, page: number): HTMLElement => q(dialog, `.document-viewer-page[data-page="${page}"]`);
const drawn = (dialog: HTMLElement): number[] => [...dialog.querySelectorAll<HTMLElement>(".document-viewer-page")].filter(node => node.querySelector("canvas")).map(node => Number(node.dataset["page"]));

async function opened(pages = 3, options: Parameters<typeof fakePdf>[1] = {}) {
  const pdf = fakePdf(pages, options), files = fakeFiles();
  const report = vi.fn();
  const handle = openDocumentViewer({ rpc: files.rpc, dialogs, openPdf: pdf.open, report }, [order]);
  await vi.waitFor(() => expect(q(handle.dialog, ".document-viewer-page-total").textContent).toBe(`of ${pages}`));
  return { ...pdf, ...files, handle, dialog: handle.dialog, report };
}

test("a PDF is read from the server's own route and its first pages are drawn", async () => {
  const { dialog, getFile, open, draws } = await opened(5);
  expect(getFile).toHaveBeenCalledWith("/erp/report/pdf/sale.order.document/1,2", "application/pdf");
  expect(open).toHaveBeenCalledTimes(1);
  expect(dialog.open).toBe(true);
  // The server's name of the printed document replaces the name the caller gave.
  expect(q(dialog, ".document-viewer-name").textContent).toBe("Order S00001.pdf");
  expect(q<HTMLInputElement>(dialog, ".document-viewer-page-input").value).toBe("1");
  await vi.waitFor(() => expect(drawn(dialog)).toEqual([1, 2]));
  // 100 % is 96 CSS pixels per inch; a page of 595 x 842 points is 793 x 1123 pixels.
  expect(draws[0]).toEqual({ page: 1, scale: expect.closeTo(96 / 72, 9), rotation: 0 });
  expect(pageBox(dialog, 1).style.width).toBe("793px");
  expect(pageBox(dialog, 1).style.height).toBe("1123px");
  expect(q(dialog, ".document-viewer-zoom").textContent).toBe("100%");
  expect(q(dialog, ".document-viewer-status").hidden).toBe(true);
  expect(labelled(dialog, "Previous page").disabled).toBe(true);
  expect(labelled(dialog, "Next page").disabled).toBe(false);
});

test("the loading line names the file until it is read", async () => {
  let release!: (value: { blob: Blob; fileName: string }) => void;
  const getFile = vi.fn(() => new Promise<{ blob: Blob; fileName: string }>(resolve => { release = resolve; }));
  const pdf = fakePdf(1);
  const { dialog } = openDocumentViewer({ rpc: { getFile } as unknown as RpcClient, dialogs, openPdf: pdf.open, limits: async () => ({ maxPages: 2000, printMaxPages: 100 }) }, [order]);
  const status = q(dialog, ".document-viewer-status");
  expect(status.getAttribute("role")).toBe("status");
  expect(status.hidden).toBe(false);
  expect(status.textContent).toBe("Loading Order.pdf…");
  expect(q(dialog, ".document-viewer-toolbar").hidden).toBe(true);
  expect(labelled(dialog, "Print").disabled).toBe(true);
  release({ blob: new Blob(["%PDF-1.7"], { type: "application/pdf" }), fileName: "download.xlsx" });
  await vi.waitFor(() => expect(status.hidden).toBe(true));
  // A response without a PDF name keeps the caller's name.
  expect(q(dialog, ".document-viewer-name").textContent).toBe("Order.pdf");
  expect(q(dialog, ".document-viewer-toolbar").hidden).toBe(false);
});

test("the page buttons, the page box and the page keys move through the document", async () => {
  const { dialog } = await opened(9);
  const input = q<HTMLInputElement>(dialog, ".document-viewer-page-input");
  labelled(dialog, "Next page").click();
  expect(input.value).toBe("2");
  key(dialog, "PageDown");
  expect(input.value).toBe("3");
  key(dialog, "ArrowRight");
  expect(input.value).toBe("4");
  key(dialog, "ArrowLeft"); key(dialog, "PageUp");
  expect(input.value).toBe("2");
  key(dialog, "End");
  expect(input.value).toBe("9");
  expect(labelled(dialog, "Next page").disabled).toBe(true);
  await vi.waitFor(() => expect(drawn(dialog)).toContain(9));
  key(dialog, "Home");
  expect(input.value).toBe("1");
  await vi.waitFor(() => expect(drawn(dialog)).toEqual([1, 2]));
  input.value = "7"; input.dispatchEvent(new Event("change"));
  expect(input.value).toBe("7");
  await vi.waitFor(() => expect(drawn(dialog)).toEqual([6, 7, 8]));
  // A page that does not exist is not gone to: the box shows the current page again.
  for (const wrong of ["0", "10", "two", "3.5"]) { input.value = wrong; input.dispatchEvent(new Event("change")); expect(input.value).toBe("7"); }
});

test("canvases far from the page in view are dropped", async () => {
  const { dialog } = await opened(40);
  await vi.waitFor(() => expect(drawn(dialog)).toEqual([1, 2]));
  key(dialog, "End");
  await vi.waitFor(() => expect(drawn(dialog)).toEqual([39, 40]));
});

test("zoom steps, the zoom keys and the fit buttons redraw at the new size", async () => {
  const { dialog, draws } = await opened(2);
  await vi.waitFor(() => expect(drawn(dialog)).toEqual([1, 2]));
  labelled(dialog, "Zoom in").click();
  expect(q(dialog, ".document-viewer-zoom").textContent).toBe("125%");
  expect(pageBox(dialog, 1).style.width).toBe("992px");
  await vi.waitFor(() => expect(draws.at(-1)).toEqual({ page: 2, scale: expect.closeTo(1.25 * 96 / 72, 9), rotation: 0 }));
  key(dialog, "-"); key(dialog, "-");
  expect(q(dialog, ".document-viewer-zoom").textContent).toBe("80%");
  key(dialog, "+");
  expect(q(dialog, ".document-viewer-zoom").textContent).toBe("100%");
  // The stage of this test has no size, so a fitted zoom is 100 %; the pressed state still says which fit is on.
  labelled(dialog, "Fit width").click();
  expect(labelled(dialog, "Fit width").getAttribute("aria-pressed")).toBe("true");
  expect(labelled(dialog, "Fit page").getAttribute("aria-pressed")).toBe("false");
  labelled(dialog, "Fit page").click();
  expect(labelled(dialog, "Fit page").getAttribute("aria-pressed")).toBe("true");
  key(dialog, "0");
  expect(labelled(dialog, "Fit page").getAttribute("aria-pressed")).toBe("false");
});

test("a fitted zoom uses the width and height the stage has", async () => {
  const { dialog } = await opened(1);
  const stage = q(dialog, ".document-viewer-stage");
  Object.defineProperty(stage, "clientWidth", { configurable: true, value: 628 });
  Object.defineProperty(stage, "clientHeight", { configurable: true, value: 594 });
  labelled(dialog, "Fit width").click();
  // (628 - 32) / (595 * 96 / 72) = 0.751
  expect(q(dialog, ".document-viewer-zoom").textContent).toBe("75%");
  labelled(dialog, "Fit page").click();
  // (594 - 32) / (842 * 96 / 72) = 0.5006
  expect(q(dialog, ".document-viewer-zoom").textContent).toBe("50%");
  expect(pageBox(dialog, 1).style.height).toBe("562px");
});

test("zoom stops at its bounds", async () => {
  const { dialog } = await opened(1);
  for (let step = 0; step < 12; step++) key(dialog, "+");
  expect(q(dialog, ".document-viewer-zoom").textContent).toBe("500%");
  expect(labelled(dialog, "Zoom in").disabled).toBe(true);
  for (let step = 0; step < 20; step++) key(dialog, "-");
  expect(q(dialog, ".document-viewer-zoom").textContent).toBe("25%");
  expect(labelled(dialog, "Zoom out").disabled).toBe(true);
});

test("Rotate turns every page a quarter and swaps the page box", async () => {
  const { dialog, draws } = await opened(1);
  await vi.waitFor(() => expect(drawn(dialog)).toEqual([1]));
  labelled(dialog, "Rotate").click();
  expect(pageBox(dialog, 1).style.width).toBe("1123px");
  expect(pageBox(dialog, 1).style.height).toBe("793px");
  await vi.waitFor(() => expect(draws.at(-1)).toEqual({ page: 1, scale: expect.closeTo(96 / 72, 9), rotation: 90 }));
  key(dialog, "r"); key(dialog, "r"); key(dialog, "r");
  await vi.waitFor(() => expect(draws.at(-1)).toEqual({ page: 1, scale: expect.closeTo(96 / 72, 9), rotation: 0 }));
});

test("a page of another size takes its own size when it is drawn", async () => {
  const { dialog } = await opened(2, { sizes: { 2: { width: 842, height: 595 } } });
  await vi.waitFor(() => expect(drawn(dialog)).toEqual([1, 2]));
  expect(pageBox(dialog, 2).style.width).toBe("1123px");
  expect(pageBox(dialog, 2).style.height).toBe("793px");
});

test("scrolling makes the page that shows most the current page", async () => {
  const { dialog } = await opened(4);
  const stage = q(dialog, ".document-viewer-stage");
  Object.defineProperty(stage, "clientHeight", { configurable: true, value: 600 });
  Object.defineProperty(stage, "clientWidth", { configurable: true, value: 900 });
  [...dialog.querySelectorAll<HTMLElement>(".document-viewer-page")].forEach((node, at) => {
    Object.defineProperty(node, "offsetTop", { configurable: true, value: 16 + at * 1000 });
    Object.defineProperty(node, "offsetHeight", { configurable: true, value: 988 });
  });
  stage.scrollTop = 2700;
  stage.dispatchEvent(new Event("scroll"));
  // Page 3 (2016 to 3004) shows 304 pixels, page 4 (3016 to 4004) shows 284.
  await vi.waitFor(() => expect(q<HTMLInputElement>(dialog, ".document-viewer-page-input").value).toBe("3"));
  await vi.waitFor(() => expect(drawn(dialog)).toContain(4));
});

test("a page that cannot be drawn says so in its place and is reported; the others still show", async () => {
  const { dialog, report } = await opened(3, { broken: 2 });
  key(dialog, "PageDown");
  await vi.waitFor(() => expect(pageBox(dialog, 2).textContent).toBe("Page 2 could not be drawn."));
  expect(q(pageBox(dialog, 2), "[role=alert]").textContent).toBe("Page 2 could not be drawn.");
  await vi.waitFor(() => expect(drawn(dialog)).toEqual([1, 3]));
  expect(report).toHaveBeenCalledTimes(1);
  expect((report.mock.calls[0]![0] as Error).message).toBe("the page stream is damaged");
});

test("Close and Escape end the viewer and release the document", async () => {
  const first = await opened(1);
  labelled(first.dialog, "Close").click();
  expect(first.dialog.isConnected).toBe(false);
  expect(first.doc.close).toHaveBeenCalledTimes(1);
  const second = await opened(1);
  second.dialog.dispatchEvent(new Event("cancel", { cancelable: true }));
  await vi.waitFor(() => expect(second.dialog.isConnected).toBe(false));
  expect(second.doc.close).toHaveBeenCalledTimes(1);
});

test("focus moves into the viewer and returns to the opener", async () => {
  const outside = document.createElement("button");
  document.body.append(outside); outside.focus();
  const { dialog } = await opened(1);
  expect(dialog.contains(document.activeElement)).toBe(true);
  expect(q(dialog, ".document-viewer-stage").hasAttribute("autofocus")).toBe(true);
  labelled(dialog, "Close").click();
  expect(document.activeElement).toBe(outside);
});

test("a user who may not read the file sees the server's refusal and keeps the download link", async () => {
  const fetcher = vi.fn(async (input: RequestInfo | URL) => String(input).startsWith("/erp/content/")
    ? new Response("Not found.", { status: 404, headers: { "Content-Type": "text/plain" } })
    : new Response(JSON.stringify(settings(2000, 100)), { headers: { "Content-Type": "application/json" } }));
  const pdf = fakePdf(1);
  const file = attachmentViewerFile({ id: 77, name: "contract.pdf", mimetype: "application/pdf" })!;
  const { dialog } = openDocumentViewer({ rpc: new RpcClient(fetcher, () => undefined), dialogs, openPdf: pdf.open }, [file]);
  const problem = q(dialog, ".document-viewer-problem");
  await vi.waitFor(() => expect(problem.hidden).toBe(false));
  expect(problem.getAttribute("role")).toBe("alert");
  expect(q(problem, "p").textContent).toBe("Not found.");
  expect(fetcher).toHaveBeenCalledWith("/erp/content/77", expect.objectContaining({ method: "GET", credentials: "same-origin" }));
  expect(pdf.open).not.toHaveBeenCalled();
  expect(dialog.querySelector("canvas")).toBeNull();
  expect(q(dialog, ".document-viewer-toolbar").hidden).toBe(true);
  expect(labelled(dialog, "Print").disabled).toBe(true);
  const link = q<HTMLAnchorElement>(problem, "a");
  expect(link.getAttribute("href")).toBe("/erp/content/77?download=true");
  expect(link.getAttribute("download")).toBe("contract.pdf");
  expect(q<HTMLAnchorElement>(dialog, ".document-viewer-download").getAttribute("href")).toBe("/erp/content/77?download=true");
});

test.each([
  ["invalid", "This file could not be read as a PDF document. Download it and open it in another application."],
  ["password", "This document is protected by a password, which the viewer does not ask for. Download it and open it in another application."],
  ["failed", "The document could not be opened: the cross-reference table is missing"],
] as const)("a file the renderer refuses (%s) says what failed", async (reason, message) => {
  const open = vi.fn(async () => { throw new PdfOpenError(reason, "the cross-reference table is missing"); });
  const { dialog } = openDocumentViewer({ rpc: fakeFiles().rpc, dialogs, openPdf: open }, [order]);
  await vi.waitFor(() => expect(q(dialog, ".document-viewer-problem").hidden).toBe(false));
  expect(q(dialog, ".document-viewer-problem p").textContent).toBe(message);
  expect(q<HTMLAnchorElement>(dialog, ".document-viewer-problem a").getAttribute("href")).toBe("/erp/report/pdf/sale.order.document/1,2?download=1");
});

test("the viewer reads this server's attachment and report routes only", () => {
  const rpc = fakeFiles();
  for (const url of ["https://elsewhere.example/a.pdf", "//elsewhere.example/a.pdf", "/erp/content/../json/2/res.users/read", "/erp/content/%2e%2e/%2e%2e/erp/json/2/res.users/read", "/erp/content/.%2E/x", "/erp/content/5?next=%2f", "/erp/json/2/res.users/read", "/erp/content/5\\..", "javascript:alert(1)", "/erp/content/5#x", ""]) {
    expect(() => openDocumentViewer({ rpc: rpc.rpc, dialogs }, [{ name: "a.pdf", kind: "pdf", url }]), url).toThrow("The viewer shows files of this server's attachment and report routes only.");
    expect(() => openDocumentViewer({ rpc: rpc.rpc, dialogs }, [{ name: "a.pdf", kind: "pdf", url: "/erp/content/5", downloadUrl: url }]), url).toThrow("attachment and report routes only");
  }
  expect(() => reportViewerFile("/report/pdf/sale.order.document/1", "Order")).toThrow("attachment and report routes only");
  expect(() => openDocumentViewer({ rpc: rpc.rpc, dialogs }, [])).toThrow("There is no file to show.");
  expect(() => openDocumentViewer({ rpc: rpc.rpc, dialogs }, [order], 1)).toThrow("The file to show first is not one of the files.");
  expect(() => openDocumentViewer({ rpc: rpc.rpc, dialogs }, [{ name: "a.html", kind: "html" as "pdf", url: "/erp/content/5" }])).toThrow("The viewer shows PDF documents and pictures only.");
  expect(rpc.getFile).not.toHaveBeenCalled();
  expect(rpc.call).not.toHaveBeenCalled();
  expect(document.querySelector("dialog")).toBeNull();
});

test("a document that claims more pages than the setting allows is not laid out", async () => {
  const pdf = fakePdf(2_000_000), files = fakeFiles();
  files.call.mockResolvedValue(settings(500, 100));
  const { dialog } = openDocumentViewer({ rpc: files.rpc, dialogs, openPdf: pdf.open }, [order]);
  await vi.waitFor(() => expect(q(dialog, ".document-viewer-problem").hidden).toBe(false));
  expect(q(dialog, ".document-viewer-problem p").textContent).toBe("This document has 2000000 pages. The viewer shows documents of up to 500 pages (setting web.document_viewer_max_pages). Download it and open it in another application.");
  expect(files.call).toHaveBeenCalledWith("ir.config.setting", "erp_read_values", { scope: { kind: "global" }, keys: ["web.document_viewer_max_pages", "web.document_viewer_print_max_pages"] });
  expect(dialog.querySelectorAll(".document-viewer-page")).toHaveLength(0);
  expect(pdf.doc.size).not.toHaveBeenCalled();
  expect(pdf.doc.close).toHaveBeenCalledTimes(1);
  expect(q<HTMLAnchorElement>(dialog, ".document-viewer-problem a").getAttribute("href")).toBe("/erp/report/pdf/sale.order.document/1,2?download=1");
});

test.each([0, -3, 1.5, Number.NaN])("a document with an unusable page count (%s) is refused", async pages => {
  const pdf = fakePdf(pages);
  const { dialog } = openDocumentViewer({ rpc: fakeFiles().rpc, dialogs, openPdf: pdf.open }, [order]);
  await vi.waitFor(() => expect(q(dialog, ".document-viewer-problem").hidden).toBe(false));
  expect(q(dialog, ".document-viewer-problem p").textContent).toBe("This document does not say how many pages it has. Download it and open it in another application.");
  expect(dialog.querySelectorAll(".document-viewer-page")).toHaveLength(0);
});

test("a document at the limit opens", async () => {
  const pdf = fakePdf(500), files = fakeFiles();
  files.call.mockResolvedValue(settings(500, 100));
  const { dialog } = openDocumentViewer({ rpc: files.rpc, dialogs, openPdf: pdf.open }, [order]);
  await vi.waitFor(() => expect(q(dialog, ".document-viewer-page-total").textContent).toBe("of 500"));
  expect(dialog.querySelectorAll(".document-viewer-page")).toHaveLength(500);
});

test("a document longer than the print setting is not prepared for printing", async () => {
  const printPictures = vi.fn(async () => {});
  const pdf = fakePdf(4), files = fakeFiles();
  files.call.mockResolvedValue(settings(2000, 3));
  const { dialog } = openDocumentViewer({ rpc: files.rpc, dialogs, openPdf: pdf.open, printPictures }, [order]);
  await vi.waitFor(() => expect(labelled(dialog, "Print").disabled).toBe(false));
  labelled(dialog, "Print").click();
  const notice = q(dialog, ".document-viewer-notice");
  await vi.waitFor(() => expect(notice.hidden).toBe(false));
  expect(notice.getAttribute("role")).toBe("alert");
  expect(q(notice, "span").textContent).toBe("This document has 4 pages. The viewer prints documents of up to 3 pages (setting web.document_viewer_print_max_pages). Download it, or open it in a new tab, and print from there.");
  expect(pdf.doc.picture).not.toHaveBeenCalled();
  expect(printPictures).not.toHaveBeenCalled();
});

test("the viewer's settings are read strictly", async () => {
  const rpc = (answer: unknown): RpcClient => ({ call: async () => answer }) as unknown as RpcClient;
  await expect(readViewerLimits(rpc(settings(2000, 100)))).resolves.toEqual({ maxPages: 2000, printMaxPages: 100 });
  await expect(readViewerLimits(rpc({ wire_major: 2, values: [] }))).rejects.toThrow("Unsupported settings response. Reload the page.");
  await expect(readViewerLimits(rpc(null))).rejects.toThrow("Unsupported settings response. Reload the page.");
  await expect(readViewerLimits(rpc({ wire_major: 1, values: "all" }))).rejects.toThrow("The server did not answer the document viewer settings with a list.");
  await expect(readViewerLimits(rpc({ wire_major: 1, values: [] }))).rejects.toThrow("Invalid document viewer setting web.document_viewer_max_pages.");
  for (const wrong of [0, -1, 2.5, "2000", null]) await expect(readViewerLimits(rpc(settings(2000, wrong)))).rejects.toThrow("Invalid document viewer setting web.document_viewer_print_max_pages.");
});

test("settings that cannot be read stop the document from opening", async () => {
  const pdf = fakePdf(1), files = fakeFiles();
  files.call.mockRejectedValue(new Error("You are not allowed to read these settings."));
  const { dialog } = openDocumentViewer({ rpc: files.rpc, dialogs, openPdf: pdf.open }, [order]);
  await vi.waitFor(() => expect(q(dialog, ".document-viewer-problem").hidden).toBe(false));
  expect(q(dialog, ".document-viewer-problem p").textContent).toBe("You are not allowed to read these settings.");
  expect(pdf.open).not.toHaveBeenCalled();
});

test("stored types map to what the viewer shows; SVG and other documents are not shown", () => {
  expect(viewerKind("application/pdf")).toBe("pdf");
  expect(viewerKind("Application/PDF; charset=binary")).toBe("pdf");
  for (const type of ["image/png", "image/jpeg", "image/gif", "image/webp"]) expect(viewerKind(type)).toBe("image");
  for (const type of ["image/svg+xml", "text/html", "application/xhtml+xml", "text/plain", "", null, undefined]) expect(viewerKind(type)).toBeUndefined();
  expect(attachmentViewerFile({ id: 9, name: "page.html", mimetype: "text/html" })).toBeUndefined();
  expect(attachmentViewerFile({ id: 9, name: "scan.png", mimetype: "image/png" })).toEqual({ name: "scan.png", kind: "image", url: "/erp/content/9", downloadUrl: "/erp/content/9?download=true" });
  expect(() => attachmentViewerFile({ id: 0, name: "scan.png", mimetype: "image/png" })).toThrow("An attachment to show has no valid id.");
  expect(reportViewerFile("/erp/report/pdf/sale.order.document/1,2", "Order")).toEqual({ name: "Order.pdf", kind: "pdf", url: "/erp/report/pdf/sale.order.document/1,2", downloadUrl: "/erp/report/pdf/sale.order.document/1,2?download=1" });
});

test("several files: the header moves between them and wraps round", async () => {
  const first = fakePdf(2), second = fakePdf(4);
  const open = vi.fn().mockResolvedValueOnce(first.doc).mockResolvedValueOnce(second.doc).mockResolvedValueOnce(first.doc);
  const files = [attachmentViewerFile({ id: 1, name: "a.pdf", mimetype: "application/pdf" })!, attachmentViewerFile({ id: 2, name: "b.pdf", mimetype: "application/pdf" })!];
  const rpc = fakeFiles("download.xlsx");
  const { dialog } = openDocumentViewer({ rpc: rpc.rpc, dialogs, openPdf: open }, files, 0);
  const total = q(dialog, ".document-viewer-page-total");
  expect(q(dialog, ".document-viewer-files").hidden).toBe(false);
  expect(q(dialog, ".document-viewer-count").textContent).toBe("1 of 2");
  await vi.waitFor(() => expect(total.textContent).toBe("of 2"));
  labelled(dialog, "Next file").click();
  expect(q(dialog, ".document-viewer-name").textContent).toBe("b.pdf");
  expect(q(dialog, ".document-viewer-count").textContent).toBe("2 of 2");
  expect(first.doc.close).toHaveBeenCalledTimes(1);
  await vi.waitFor(() => expect(total.textContent).toBe("of 4"));
  expect(rpc.getFile).toHaveBeenLastCalledWith("/erp/content/2", "application/pdf");
  key(dialog, "ArrowRight", { shiftKey: true });
  expect(q(dialog, ".document-viewer-name").textContent).toBe("a.pdf");
  expect(q(dialog, ".document-viewer-count").textContent).toBe("1 of 2");
  expect(second.doc.close).toHaveBeenCalledTimes(1);
  await vi.waitFor(() => expect(total.textContent).toBe("of 2"));
});

test("one file shows no file navigation", async () => {
  const { dialog } = await opened(1);
  expect(q(dialog, ".document-viewer-files").hidden).toBe(true);
});

test("a picture shows in place with zoom and rotation and without page controls", async () => {
  const rpc = fakeFiles();
  const files = [attachmentViewerFile({ id: 5, name: "scan.png", mimetype: "image/png" })!, attachmentViewerFile({ id: 6, name: "photo.jpg", mimetype: "image/jpeg" })!];
  const { dialog } = openDocumentViewer({ rpc: rpc.rpc, dialogs }, files);
  const image = q<HTMLImageElement>(dialog, ".document-viewer-image");
  expect(image.getAttribute("src")).toBe("/erp/content/5");
  expect(image.alt).toBe("scan.png");
  expect(rpc.getFile).not.toHaveBeenCalled();
  Object.defineProperty(image, "naturalWidth", { configurable: true, value: 400 });
  image.dispatchEvent(new Event("load"));
  expect(q(dialog, ".document-viewer-status").hidden).toBe(true);
  expect(q(dialog, ".document-viewer-paging").hidden).toBe(true);
  expect(q(dialog, ".document-viewer-zoom").textContent).toBe("Fit");
  expect(image.classList.contains("is-fit-page")).toBe(true);
  labelled(dialog, "Zoom in").click();
  expect(q(dialog, ".document-viewer-zoom").textContent).toBe("125%");
  expect(image.style.width).toBe("500px");
  expect(image.classList.contains("is-fit-page")).toBe(false);
  labelled(dialog, "Rotate").click();
  expect(image.style.transform).toBe("rotate(90deg)");
  // For a picture the arrow keys move between the files.
  key(dialog, "ArrowRight");
  expect(q<HTMLImageElement>(dialog, ".document-viewer-image").getAttribute("src")).toBe("/erp/content/6");
  expect(q(dialog, ".document-viewer-name").textContent).toBe("photo.jpg");
});

test("a picture that does not load says so and keeps the download link", () => {
  const { dialog } = openDocumentViewer({ rpc: fakeFiles().rpc, dialogs }, [attachmentViewerFile({ id: 5, name: "scan.png", mimetype: "image/png" })!]);
  q(dialog, ".document-viewer-image").dispatchEvent(new Event("error"));
  expect(q(dialog, ".document-viewer-problem p").textContent).toBe("The picture could not be loaded. It may have been removed, or you may not be allowed to read it.");
  expect(q<HTMLAnchorElement>(dialog, ".document-viewer-problem a").getAttribute("href")).toBe("/erp/content/5?download=true");
  expect(dialog.querySelector(".document-viewer-image")).toBeNull();
});

test("Download saves the bytes already read; Open in a new tab hands them over as a PDF", async () => {
  const made: Blob[] = [];
  vi.stubGlobal("URL", Object.assign(URL, { createObjectURL: vi.fn((blob: Blob) => { made.push(blob); return `blob:test/${made.length}`; }), revokeObjectURL: vi.fn() }));
  const opener = vi.spyOn(window, "open").mockReturnValue(null);
  const { dialog, getFile } = await opened(1);
  let saved: HTMLAnchorElement | undefined;
  vi.spyOn(HTMLAnchorElement.prototype, "click").mockImplementation(function (this: HTMLAnchorElement) { saved = this; });
  const click = new MouseEvent("click", { bubbles: true, cancelable: true });
  q(dialog, ".document-viewer-download").dispatchEvent(click);
  expect(click.defaultPrevented).toBe(true);
  expect(saved?.download).toBe("Order S00001.pdf");
  expect(getFile).toHaveBeenCalledTimes(1);
  labelled(dialog, "Open in a new tab").click();
  expect(opener).toHaveBeenCalledWith("blob:test/2", "_blank", "noopener");
  expect(made[1]!.type).toBe("application/pdf");
});

test("Print prepares every page at print resolution and hands the pictures to the browser", async () => {
  let serial = 0;
  vi.stubGlobal("URL", Object.assign(URL, { createObjectURL: vi.fn(() => `blob:test/${++serial}`), revokeObjectURL: vi.fn() }));
  const printPictures = vi.fn(async (_urls: readonly string[], finished: () => void) => { finished(); });
  const pdf = fakePdf(3), files = fakeFiles();
  const { dialog } = openDocumentViewer({ rpc: files.rpc, dialogs, openPdf: pdf.open, printPictures }, [order]);
  await vi.waitFor(() => expect(labelled(dialog, "Print").disabled).toBe(false));
  labelled(dialog, "Print").click();
  expect(q(dialog, ".document-viewer-notice").textContent).toContain("Preparing page 1 of 3 for printing…");
  await vi.waitFor(() => expect(printPictures).toHaveBeenCalledTimes(1));
  expect(printPictures.mock.calls[0]![0]).toEqual(["blob:test/1", "blob:test/2", "blob:test/3"]);
  expect(pdf.doc.picture).toHaveBeenNthCalledWith(2, 2, 150 / 72, 0, expect.any(AbortSignal));
  await vi.waitFor(() => expect(q(dialog, ".document-viewer-notice").hidden).toBe(true));
  expect(URL.revokeObjectURL).toHaveBeenCalledTimes(3);
});

test("a print that fails says why", async () => {
  vi.stubGlobal("URL", Object.assign(URL, { createObjectURL: vi.fn(() => "blob:test/1"), revokeObjectURL: vi.fn() }));
  const printPictures = vi.fn(async () => { throw new Error("The print dialog could not be opened."); });
  const pdf = fakePdf(1);
  const { dialog } = openDocumentViewer({ rpc: fakeFiles().rpc, dialogs, openPdf: pdf.open, printPictures }, [order]);
  await vi.waitFor(() => expect(labelled(dialog, "Print").disabled).toBe(false));
  labelled(dialog, "Print").click();
  const notice = q(dialog, ".document-viewer-notice");
  await vi.waitFor(() => expect(notice.getAttribute("role")).toBe("alert"));
  expect(notice.textContent).toContain("The document was not printed: The print dialog could not be opened.");
  expect(URL.revokeObjectURL).toHaveBeenCalledWith("blob:test/1");
});

test("the keys leave a text box alone", async () => {
  const { dialog } = await opened(5);
  const input = q<HTMLInputElement>(dialog, ".document-viewer-page-input");
  input.dispatchEvent(new KeyboardEvent("keydown", { key: "ArrowRight", bubbles: true, cancelable: true }));
  input.dispatchEvent(new KeyboardEvent("keydown", { key: "-", bubbles: true, cancelable: true }));
  expect(input.value).toBe("1");
  expect(q(dialog, ".document-viewer-zoom").textContent).toBe("100%");
});
