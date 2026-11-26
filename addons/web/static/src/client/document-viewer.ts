/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
import { el, errorMessage } from "./dom.js";
import { icon, type IconName } from "./icons.js";
import type { DialogHandle, DialogService } from "./dialog-service.js";
import { saveFile } from "./file-save.js";
import { PdfOpenError, type PdfDocument, type PdfOpen, type PdfPageSize, type PdfRotation } from "./pdf-document.js";
import { ClientError, type RpcClient } from "./transport.js";

/**
 * The document viewer (docs/design/pdf-viewer.md, ADR 0231): a full-window dialog that shows PDF documents and
 * pictures of this server without leaving the page. PDF pages are drawn on canvases by the bundled renderer from
 * bytes the client fetched itself, so nothing of the document runs as a page of this origin.
 */

export type ViewerKind = "pdf" | "image";

/** One file the viewer can show. Build it with {@link attachmentViewerFile} or {@link reportViewerFile}. */
export interface ViewerFile {
  /** The name in the header and of the downloaded file. */
  readonly name: string;
  /** Where the bytes are read: a path of this server's attachment or report route, never an outside address. */
  readonly url: string;
  readonly kind: ViewerKind;
  /** The path that offers the file as a download; `url` when omitted. */
  readonly downloadUrl?: string;
}

export interface ViewerServices {
  readonly rpc: RpcClient;
  readonly dialogs: DialogService;
  /** Receives failures the viewer also shows in place (a page that cannot be drawn), for the client error log. */
  readonly report?: (error: unknown) => void;
  /** Opens PDF bytes; the bundled renderer when omitted. */
  readonly openPdf?: PdfOpen;
  /** The bounds of the viewer; the database settings ({@link readViewerLimits}) when omitted. */
  readonly limits?: () => Promise<ViewerLimits>;
  /** Sends pictures to the browser's print dialog; {@link printPictures} when omitted. `finished` runs once they are no longer needed. */
  readonly printPictures?: (urls: readonly string[], finished: () => void) => Promise<void>;
}

/**
 * The viewer's bounds, from Settings (`web.document_viewer_*`, declared by the web addon with a default, a floor
 * and a hard bound). A PDF states its own page count, so these are what stops a small file that claims millions of
 * pages from filling the tab.
 */
export interface ViewerLimits {
  /** The most pages of one document the viewer lays out. */
  readonly maxPages: number;
  /** The most pages of one document the viewer prepares for printing. */
  readonly printMaxPages: number;
}
const MAX_PAGES_KEY = "web.document_viewer_max_pages", PRINT_MAX_PAGES_KEY = "web.document_viewer_print_max_pages";

/** Read the viewer's bounds from the database settings. */
export async function readViewerLimits(rpc: RpcClient): Promise<ViewerLimits> {
  const answer: unknown = await rpc.call("ir.config.setting", "erp_read_values", { scope: { kind: "global" }, keys: [MAX_PAGES_KEY, PRINT_MAX_PAGES_KEY] });
  const response = typeof answer === "object" && answer !== null ? answer as Record<string, unknown> : {};
  if (response["wire_major"] !== 1) throw new ClientError("unsupported", "Unsupported settings response. Reload the page.");
  const values: unknown = response["values"];
  if (!Array.isArray(values)) throw new ClientError("server", "The server did not answer the document viewer settings with a list.");
  const read = (key: string): number => {
    const found = values.filter((entry: unknown) => typeof entry === "object" && entry !== null && (entry as Record<string, unknown>)["key"] === key);
    const value: unknown = found.length === 1 ? (found[0] as Record<string, unknown>)["value"] : undefined;
    if (typeof value !== "number" || !Number.isSafeInteger(value) || value < 1) throw new ClientError("server", `Invalid document viewer setting ${key}.`);
    return value;
  };
  return { maxPages: read(MAX_PAGES_KEY), printMaxPages: read(PRINT_MAX_PAGES_KEY) };
}

/** Picture types shown in place. SVG is left out: it is a document, and the content route offers it as a download. */
const PICTURE_TYPES: ReadonlySet<string> = new Set(["image/png", "image/jpeg", "image/gif", "image/webp"]);

/** What the viewer shows for a stored content type, `undefined` when it shows nothing for it. */
export function viewerKind(mimetype: string | null | undefined): ViewerKind | undefined {
  const type = (mimetype ?? "").split(";")[0]!.trim().toLowerCase();
  return type === "application/pdf" ? "pdf" : PICTURE_TYPES.has(type) ? "image" : undefined;
}

/** The viewer file of attachment `id`, `undefined` when its type is one the viewer does not show. */
export function attachmentViewerFile(attachment: { readonly id: number; readonly name: string; readonly mimetype?: string | null | undefined }): ViewerFile | undefined {
  const kind = viewerKind(attachment.mimetype);
  if (!kind) return undefined;
  if (!Number.isSafeInteger(attachment.id) || attachment.id <= 0) throw new ClientError("validation", "An attachment to show has no valid id.");
  return { name: attachment.name, kind, url: `/erp/content/${attachment.id}`, downloadUrl: `/erp/content/${attachment.id}?download=true` };
}

/** A saved binary field is always read again under the active user's field and record access. */
export function binaryPdfViewerFile(model: string, id: number, field: string, filename: string): ViewerFile {
  if (!/^[a-z][a-z0-9_]*(?:\.[a-z][a-z0-9_]*)*$/.test(model) || !/^[a-z_][a-z0-9_]*$/.test(field) || !Number.isSafeInteger(id) || id <= 0) {
    throw new ClientError("validation", "The PDF field has no valid saved record.");
  }
  const url = servedPath(`/erp/content/${model}/${id}/${field}`);
  return { name: filename, kind: "pdf", url, downloadUrl: `${url}?download=true` };
}

/** The viewer file of a printed report: `url` is the address the server gave for the print (`/erp/report/pdf/...`). */
export function reportViewerFile(url: string, name: string): ViewerFile {
  const path = servedPath(url);
  return { name: /\.pdf$/i.test(name) ? name : `${name}.pdf`, kind: "pdf", url: path, downloadUrl: `${path}${path.includes("?") ? "&" : "?"}download=1` };
}

// No percent sign: the paths the server builds need none, and an encoded dot segment would leave the two routes.
const SERVED = /^\/erp\/(content|report)\/[A-Za-z0-9._~,\/-]+(\?[A-Za-z0-9._~,=&-]*)?$/;
/** The viewer reads the attachment and report routes of this server only; the routes check who may read. */
function servedPath(url: unknown): string {
  if (typeof url !== "string" || url.length > 2048 || !SERVED.test(url) || url.includes("..") || url.includes("//")) {
    throw new ClientError("validation", "The viewer shows files of this server's attachment and report routes only.");
  }
  return url;
}

/** CSS pixels per PDF point at 100 %. */
const CSS_PX_PER_POINT = 96 / 72;
/** Picture pixels per point for printing (150 dots per inch, what the reference viewer prints at). */
const PRINT_PX_PER_POINT = 150 / 72;
const ZOOM_MIN = 0.25, ZOOM_MAX = 5, ZOOM_STEP = 1.25;
/** The opening zoom fits the width but does not enlarge a page past this. */
const OPENING_ZOOM_MAX = 1.25;
/** Padding of the page column; the stylesheet uses the same value. */
const STAGE_PADDING = 16;
/** How long an address opened in another tab stays valid: long enough for that tab to load it. */
const TAB_ADDRESS_MS = 60_000;

type ZoomMode = "opening" | "width" | "page" | "custom";
interface Shown {
  readonly file: ViewerFile;
  blob?: Blob;
  name: string;
  doc?: PdfDocument;
  limits?: ViewerLimits;
  image?: HTMLImageElement;
  pages: HTMLElement[];
  sizes: PdfPageSize[];
  /** Pages whose real size has been read; the others carry the first page's size until they are drawn. */
  known: boolean[];
  /** Page number to the zoom and rotation its canvas was drawn for. */
  drawn: Map<number, string>;
  draws: AbortController;
}

function tool(label: string, name: IconName, action: () => void): HTMLButtonElement {
  const node = el("button", undefined, "document-viewer-btn");
  node.type = "button"; node.title = label; node.setAttribute("aria-label", label);
  node.append(icon(name));
  node.addEventListener("click", action);
  return node;
}
const clampZoom = (value: number): number => Math.min(ZOOM_MAX, Math.max(ZOOM_MIN, value));
const isAbort = (error: unknown): boolean => error instanceof DOMException && error.name === "AbortError";

/**
 * Print pictures through a hidden frame of this page, one picture per sheet. Resolves when the browser has been
 * asked to print; `finished` runs when the print dialog is closed and the pictures are no longer needed.
 */
export async function printPictures(urls: readonly string[], finished: () => void): Promise<void> {
  const frame = el("iframe", undefined, "document-viewer-print-frame");
  frame.setAttribute("aria-hidden", "true"); frame.tabIndex = -1;
  document.body.append(frame);
  let done = false;
  const finish = (): void => { if (done) return; done = true; frame.remove(); finished(); };
  try {
    const inner = frame.contentDocument, view = frame.contentWindow;
    if (!inner || !view) throw new ClientError("unsupported", "This browser cannot print from the viewer. Download the file and print it from another application.");
    inner.body.style.margin = "0";
    await Promise.all(urls.map(url => new Promise<void>((resolve, reject) => {
      const picture = inner.createElement("img");
      picture.style.cssText = "display:block;width:100%;break-after:page";
      picture.onload = () => resolve();
      picture.onerror = () => reject(new ClientError("server", "A page could not be prepared for printing."));
      picture.src = url;
      inner.body.append(picture);
    })));
    view.addEventListener("afterprint", finish, { once: true });
    view.focus();
    view.print();
  } catch (error) { finish(); throw error; }
}

/**
 * Open `files` in the viewer, starting with `files[start]`. The header moves through the files (it wraps round);
 * Escape or Close ends it and returns focus to the control that opened it.
 */
export function openDocumentViewer(services: ViewerServices, files: readonly ViewerFile[], start = 0): DialogHandle {
  if (!files.length) throw new ClientError("validation", "There is no file to show.");
  if (!Number.isInteger(start) || start < 0 || start >= files.length) throw new ClientError("validation", "The file to show first is not one of the files.");
  for (const file of files) {
    servedPath(file.url);
    if (file.downloadUrl !== undefined) servedPath(file.downloadUrl);
    if (file.kind !== "pdf" && file.kind !== "image") throw new ClientError("validation", "The viewer shows PDF documents and pictures only.");
    if (typeof file.name !== "string" || !file.name) throw new ClientError("validation", "A file to show has no name.");
  }
  // Without a reporter the failure is still kept: the reader sees it in place, the console has the detail.
  const report = services.report ?? ((error: unknown) => console.error("Document viewer:", error));

  const dialog = el("dialog", undefined, "document-viewer");
  const heading = el("h2", undefined, "document-viewer-name");
  heading.id = `document-viewer-name-${++nextViewer}`;
  dialog.setAttribute("aria-labelledby", heading.id);

  // Header: the file, the move between files, and what can be done with the file.
  const previousFile = tool("Previous file", "chevron-left", () => showFile(index - 1));
  const nextFile = tool("Next file", "chevron-right", () => showFile(index + 1));
  const fileCount = el("span", undefined, "document-viewer-count");
  const fileNav = el("div", undefined, "document-viewer-files");
  fileNav.append(previousFile, fileCount, nextFile);
  fileNav.hidden = files.length < 2;
  const downloadLink = el("a", undefined, "document-viewer-btn document-viewer-download");
  downloadLink.title = "Download"; downloadLink.setAttribute("aria-label", "Download");
  downloadLink.append(icon("download"));
  downloadLink.addEventListener("click", event => {
    // The loaded bytes are saved as they are: a printed report is not rendered a second time for the download.
    if (!shown?.blob) return;
    event.preventDefault();
    saveFile(shown.blob, shown.name);
  });
  const printButton = tool("Print", "printer", () => void print());
  const tabButton = tool("Open in a new tab", "external-link", () => openTab());
  const closeButton = tool("Close", "x", () => handle.close());
  const fileActions = el("div", undefined, "document-viewer-actions");
  fileActions.append(downloadLink, printButton, tabButton, closeButton);
  const header = el("header", undefined, "document-viewer-header");
  header.append(heading, fileNav, fileActions);

  // Toolbar: pages, zoom, rotation.
  const previousPage = tool("Previous page", "chevron-up", () => goToPage(page - 1));
  const nextPage = tool("Next page", "chevron-down", () => goToPage(page + 1));
  const pageInput = el("input", undefined, "document-viewer-page-input");
  pageInput.type = "text"; pageInput.inputMode = "numeric"; pageInput.autocomplete = "off"; pageInput.setAttribute("aria-label", "Page");
  const pageTotal = el("span", undefined, "document-viewer-page-total");
  const pageGroup = el("div", undefined, "document-viewer-group document-viewer-paging");
  pageGroup.append(previousPage, pageInput, pageTotal, nextPage);
  const zoomOut = tool("Zoom out", "zoom-out", () => zoomBy(1 / ZOOM_STEP));
  const zoomIn = tool("Zoom in", "zoom-in", () => zoomBy(ZOOM_STEP));
  const zoomLabel = el("span", undefined, "document-viewer-zoom");
  const fitWidth = tool("Fit width", "move-horizontal", () => setMode("width"));
  const fitPage = tool("Fit page", "maximize", () => setMode("page"));
  const zoomGroup = el("div", undefined, "document-viewer-group");
  zoomGroup.append(zoomOut, zoomLabel, zoomIn, fitWidth, fitPage);
  const rotate = tool("Rotate", "rotate-cw", () => turn());
  const toolbar = el("div", undefined, "document-viewer-toolbar");
  toolbar.setAttribute("role", "toolbar"); toolbar.setAttribute("aria-label", "Pages and zoom");
  toolbar.append(pageGroup, zoomGroup, rotate);

  // A strip for what an action reports: print progress (with Cancel) or a failed action.
  const noticeText = el("span");
  const noticeCancel = el("button", "Cancel", "document-viewer-notice-cancel");
  noticeCancel.type = "button";
  const notice = el("div", undefined, "document-viewer-notice");
  notice.append(noticeText, noticeCancel); notice.hidden = true;

  // Stage: the pages or the picture, the loading line, the failure.
  const column = el("div", undefined, "document-viewer-pages");
  const status = el("p", undefined, "document-viewer-status"); status.setAttribute("role", "status");
  const problemText = el("p");
  const problemLink = el("a", "Download the file", "document-viewer-problem-link");
  const problem = el("div", undefined, "document-viewer-problem");
  problem.setAttribute("role", "alert"); problem.append(problemText, problemLink); problem.hidden = true;
  const stage = el("div", undefined, "document-viewer-stage");
  stage.tabIndex = 0; stage.setAttribute("autofocus", ""); stage.setAttribute("aria-label", "Document");
  stage.append(status, problem, column);
  dialog.append(header, toolbar, notice, stage);

  let index = start;
  let shown: Shown | undefined;
  let page = 1, zoom = 1, mode: ZoomMode = "opening", rotation: PdfRotation = 0;
  let pumping = false, pumpAgain = false, scrollQueued = false, closed = false;
  /** The scroll position our own page move left: a scroll event at that position is not the reader scrolling. */
  let movedTo: number | undefined;
  let printing: AbortController | undefined;
  const printAddresses = new Set<string>();

  const turned = (size: PdfPageSize): PdfPageSize => rotation === 90 || rotation === 270 ? { width: size.height, height: size.width } : size;
  const live = (candidate: Shown): boolean => !closed && shown === candidate;

  function say(text: string, kind: "status" | "alert", cancel?: () => void): void {
    noticeText.textContent = text;
    notice.setAttribute("role", kind);
    notice.classList.toggle("is-problem", kind === "alert");
    noticeCancel.hidden = !cancel;
    noticeCancel.onclick = cancel ?? null;
    notice.hidden = false;
  }
  function fail(candidate: Shown, message: string): void {
    if (!live(candidate)) return;
    status.hidden = true; column.replaceChildren(); column.hidden = true;
    problemText.textContent = message;
    problem.hidden = false;
    toolbar.hidden = true;
    printButton.disabled = true;
  }
  /** What to tell the reader when a file does not open; the download stays offered next to it. */
  function openFailure(error: unknown): string {
    if (error instanceof PdfOpenError) {
      if (error.reason === "password") return "This document is protected by a password, which the viewer does not ask for. Download it and open it in another application.";
      if (error.reason === "invalid") return "This file could not be read as a PDF document. Download it and open it in another application.";
      return `The document could not be opened: ${error.message}`;
    }
    return errorMessage(error);
  }

  function refreshControls(): void {
    const current = shown;
    const pdf = current?.doc, total = pdf?.pages ?? 0;
    pageGroup.hidden = !pdf;
    pageInput.value = String(page);
    pageTotal.textContent = `of ${total}`;
    previousPage.disabled = page <= 1; nextPage.disabled = page >= total;
    const fitted = Boolean(current?.image) && mode !== "custom";
    zoomLabel.textContent = fitted ? "Fit" : `${Math.round(zoom * 100)}%`;
    zoomOut.disabled = !fitted && zoom <= ZOOM_MIN; zoomIn.disabled = !fitted && zoom >= ZOOM_MAX;
    fitWidth.setAttribute("aria-pressed", String(mode === "width"));
    fitPage.setAttribute("aria-pressed", String(mode === "page" || (Boolean(current?.image) && mode === "opening")));
  }

  /** The zoom the current mode gives in the space the stage has; 100 % while the stage has no size yet. */
  function modeZoom(current: Shown): number {
    if (mode === "custom") return zoom;
    const size = turned(current.sizes[page - 1] ?? current.sizes[0]!);
    const width = stage.clientWidth - 2 * STAGE_PADDING, height = stage.clientHeight - 2 * STAGE_PADDING;
    if (width <= 0 || height <= 0) return 1;
    const byWidth = width / (size.width * CSS_PX_PER_POINT);
    if (mode === "width") return clampZoom(byWidth);
    if (mode === "page") return clampZoom(Math.min(byWidth, height / (size.height * CSS_PX_PER_POINT)));
    return clampZoom(Math.min(byWidth, OPENING_ZOOM_MAX));
  }
  function sizePage(current: Shown, number: number): void {
    const size = turned(current.sizes[number - 1]!), node = current.pages[number - 1]!;
    node.style.width = `${Math.round(size.width * zoom * CSS_PX_PER_POINT)}px`;
    node.style.height = `${Math.round(size.height * zoom * CSS_PX_PER_POINT)}px`;
  }
  /** Apply zoom and rotation: every page takes its new size at once, canvases are drawn again as they come into view. */
  function layout(): void {
    const current = shown;
    if (!current) return;
    if (current.image) { layoutImage(current.image); refreshControls(); return; }
    if (!current.doc) return;
    zoom = modeZoom(current);
    current.draws.abort(); current.draws = new AbortController();
    for (let number = 1; number <= current.pages.length; number++) sizePage(current, number);
    refreshControls();
    moveTo(page);
    void pump();
  }
  function layoutImage(image: HTMLImageElement): void {
    const fitted = mode !== "custom";
    image.classList.toggle("is-fit-page", fitted && mode !== "width");
    image.classList.toggle("is-fit-width", mode === "width");
    image.style.width = fitted || !image.naturalWidth ? "" : `${Math.round(image.naturalWidth * zoom)}px`;
    image.style.transform = rotation ? `rotate(${rotation}deg)` : "";
  }

  /** The first and last page that show in the stage; the current page alone while the stage has no size. */
  function visiblePages(current: Shown): [number, number] {
    const height = stage.clientHeight, pages = current.pages;
    if (height <= 0 || !pages.length) return [page, page];
    const top = stage.scrollTop, bottom = top + height;
    let low = 0, high = pages.length - 1;
    while (low < high) {
      const middle = (low + high) >> 1, node = pages[middle]!;
      if (node.offsetTop + node.offsetHeight > top) high = middle; else low = middle + 1;
    }
    let last = low;
    while (last + 1 < pages.length && pages[last + 1]!.offsetTop < bottom) last++;
    return [low + 1, last + 1];
  }
  function moveTo(number: number): void {
    const node = shown?.pages[number - 1];
    if (!node) return;
    stage.scrollTop = Math.max(0, node.offsetTop - STAGE_PADDING);
    movedTo = stage.scrollTop;
  }
  function goToPage(number: number): void {
    const total = shown?.doc?.pages ?? 0;
    if (!total) return;
    page = Math.min(total, Math.max(1, Math.trunc(number)));
    refreshControls();
    moveTo(page);
    void pump();
  }
  /** The reader scrolled: the page that shows most becomes the current page. */
  function followScroll(): void {
    scrollQueued = false;
    const current = shown;
    if (!current?.doc || stage.clientHeight <= 0) return;
    if (movedTo !== undefined && Math.abs(stage.scrollTop - movedTo) < 1) return;
    movedTo = undefined;
    const [first, last] = visiblePages(current);
    const top = stage.scrollTop, bottom = top + stage.clientHeight;
    let best = first, most = -1;
    for (let number = first; number <= last; number++) {
      const node = current.pages[number - 1]!;
      const seen = Math.min(bottom, node.offsetTop + node.offsetHeight) - Math.max(top, node.offsetTop);
      if (seen > most) { most = seen; best = number; }
    }
    if (best !== page) { page = best; refreshControls(); }
    void pump();
  }

  function release(current: Shown, number: number): void {
    const node = current.pages[number - 1]!;
    // A canvas keeps its pixels until its size is dropped.
    for (const canvas of node.querySelectorAll("canvas")) { canvas.width = 0; canvas.height = 0; }
    node.replaceChildren();
    current.drawn.delete(number);
  }
  async function drawPage(current: Shown, number: number, key: string): Promise<void> {
    const node = current.pages[number - 1]!, pdf = current.doc!;
    const signal = current.draws.signal;
    try {
      if (!current.known[number - 1]) {
        const size = await pdf.size(number);
        if (!live(current) || signal.aborted) return;
        current.known[number - 1] = true;
        const before = current.sizes[number - 1]!;
        if (size.width !== before.width || size.height !== before.height) { current.sizes[number - 1] = size; sizePage(current, number); }
      }
      const canvas = document.createElement("canvas");
      await pdf.draw(number, canvas, zoom * CSS_PX_PER_POINT * (window.devicePixelRatio || 1), rotation, signal);
      if (!live(current) || signal.aborted) { canvas.width = 0; canvas.height = 0; return; }
      release(current, number);
      node.append(canvas);
      current.drawn.set(number, key);
    } catch (error) {
      if (isAbort(error) || !live(current)) return;
      // Marked as handled so the page is not tried again in a loop; the reader sees which page is missing.
      release(current, number);
      const missing = el("p", `Page ${number} could not be drawn.`, "document-viewer-page-problem");
      missing.setAttribute("role", "alert");
      node.append(missing);
      current.drawn.set(number, key);
      report(error);
    }
  }
  /** Draw the pages in view (and one on each side), the current page first, one at a time; drop canvases far from view. */
  async function pump(): Promise<void> {
    if (pumping) { pumpAgain = true; return; }
    pumping = true;
    try {
      for (;;) {
        pumpAgain = false;
        const current = shown;
        if (closed || !current?.doc) return;
        const [first, last] = visiblePages(current);
        const from = Math.max(1, first - 1), to = Math.min(current.doc.pages, last + 1);
        for (const number of [...current.drawn.keys()]) if (number < from - 2 || number > to + 2) release(current, number);
        const key = `${zoom}:${rotation}`;
        const order = [page, ...Array.from({ length: to - from + 1 }, (_, offset) => from + offset)];
        const next = order.find(number => number >= from && number <= to && current.drawn.get(number) !== key);
        if (next === undefined) { if (pumpAgain) continue; return; }
        await drawPage(current, next, key);
      }
    } finally { pumping = false; }
  }

  function setMode(next: ZoomMode): void { mode = next; layout(); }
  function zoomBy(factor: number): void {
    const current = shown;
    if (!current) return;
    if (current.image && mode !== "custom") {
      // A fitted picture starts from the size it shows at, so one step is one visible step.
      const image = current.image;
      zoom = image.naturalWidth && image.clientWidth ? image.clientWidth / image.naturalWidth : 1;
    }
    zoom = clampZoom(zoom * factor);
    mode = "custom";
    layout();
  }
  function turn(): void { rotation = ((rotation + 90) % 360) as PdfRotation; layout(); }

  async function load(current: Shown): Promise<void> {
    const file = current.file;
    if (file.kind === "image") {
      const image = el("img", undefined, "document-viewer-image");
      image.alt = file.name;
      image.addEventListener("load", () => { if (!live(current)) return; status.hidden = true; current.image = image; printButton.disabled = false; layout(); }, { once: true });
      image.addEventListener("error", () => fail(current, "The picture could not be loaded. It may have been removed, or you may not be allowed to read it."), { once: true });
      image.src = file.url;
      column.append(image);
      return;
    }
    try {
      const [{ blob, fileName }, limits] = await Promise.all([services.rpc.getFile(file.url, "application/pdf"), (services.limits ?? (() => readViewerLimits(services.rpc)))()]);
      if (!live(current)) return;
      const open = services.openPdf ?? (await import("./pdf-engine.js")).openPdf;
      const pdf = await open(await blob.arrayBuffer());
      if (!live(current)) { void pdf.close().catch(report); return; }
      // The page count is the document's own claim: checked before anything is made per page.
      if (!Number.isSafeInteger(pdf.pages) || pdf.pages < 1 || pdf.pages > limits.maxPages) {
        void pdf.close().catch(report);
        fail(current, Number.isSafeInteger(pdf.pages) && pdf.pages > limits.maxPages
          ? `This document has ${pdf.pages} pages. The viewer shows documents of up to ${limits.maxPages} pages (setting ${MAX_PAGES_KEY}). Download it and open it in another application.`
          : "This document does not say how many pages it has. Download it and open it in another application.");
        return;
      }
      current.limits = limits;
      const first = await pdf.size(1);
      if (!live(current)) { void pdf.close().catch(report); return; }
      current.blob = blob;
      // The server names a printed report; a response without a PDF name keeps the name the caller gave.
      if (/\.pdf$/i.test(fileName)) { current.name = fileName; heading.textContent = fileName; downloadLink.download = fileName; }
      current.doc = pdf;
      current.sizes = Array.from({ length: pdf.pages }, () => first);
      current.known = Array.from({ length: pdf.pages }, (_, at) => at === 0);
      current.pages = current.sizes.map((_, at) => {
        const node = el("div", undefined, "document-viewer-page");
        node.dataset["page"] = String(at + 1);
        node.setAttribute("role", "img"); node.setAttribute("aria-label", `Page ${at + 1} of ${pdf.pages}`);
        return node;
      });
      column.append(...current.pages);
      status.hidden = true; toolbar.hidden = false; printButton.disabled = false;
      layout();
    } catch (error) {
      fail(current, openFailure(error));
    }
  }

  function leave(current: Shown | undefined): void {
    if (!current) return;
    current.draws.abort();
    for (const canvas of column.querySelectorAll("canvas")) { canvas.width = 0; canvas.height = 0; }
    // A failed close only leaves memory to the page's end; it is reported, not shown.
    if (current.doc) void current.doc.close().catch(report);
  }
  function showFile(next: number): void {
    printing?.abort();
    leave(shown);
    index = (next + files.length) % files.length;
    const file = files[index]!;
    const current: Shown = { file, name: file.name, pages: [], sizes: [], known: [], drawn: new Map(), draws: new AbortController() };
    shown = current;
    page = 1; zoom = 1; mode = "opening"; rotation = 0; movedTo = undefined;
    heading.textContent = file.name;
    fileCount.textContent = `${index + 1} of ${files.length}`;
    const download = file.downloadUrl ?? file.url;
    downloadLink.href = download; downloadLink.download = file.name;
    problemLink.href = download; problemLink.download = file.name;
    column.replaceChildren(); column.hidden = false;
    problem.hidden = true; notice.hidden = true;
    status.textContent = `Loading ${file.name}…`; status.hidden = false;
    toolbar.hidden = file.kind === "pdf"; printButton.disabled = true;
    stage.scrollTop = 0;
    refreshControls();
    void load(current);
  }

  function openTab(): void {
    const current = shown;
    if (!current) return;
    if (current.file.kind === "pdf" && current.blob) {
      // The type is set here, not taken from the response: the other tab can only treat the bytes as a PDF.
      const address = URL.createObjectURL(new Blob([current.blob], { type: "application/pdf" }));
      window.open(address, "_blank", "noopener");
      setTimeout(() => URL.revokeObjectURL(address), TAB_ADDRESS_MS);
      return;
    }
    window.open(current.file.url, "_blank", "noopener");
  }
  function forget(addresses: readonly string[]): void {
    for (const address of addresses) { if (printAddresses.delete(address)) URL.revokeObjectURL(address); }
  }
  async function print(): Promise<void> {
    const current = shown;
    if (!current || printing || (!current.doc && !current.image)) return;
    const mine = printing = new AbortController();
    const made: string[] = [];
    try {
      let pictures: string[];
      if (current.doc) {
        const total = current.doc.pages, most = current.limits?.printMaxPages ?? 0;
        // Every page becomes a picture held until the print dialog closes, so the count is bounded.
        if (total > most) {
          say(`This document has ${total} pages. The viewer prints documents of up to ${most} pages (setting ${PRINT_MAX_PAGES_KEY}). Download it, or open it in a new tab, and print from there.`, "alert");
          return;
        }
        for (let number = 1; number <= total; number++) {
          say(`Preparing page ${number} of ${total} for printing…`, "status", () => mine.abort());
          const picture = await current.doc.picture(number, PRINT_PX_PER_POINT, rotation, mine.signal);
          if (mine.signal.aborted || !live(current)) throw new DOMException("Printing was cancelled.", "AbortError");
          const address = URL.createObjectURL(picture);
          made.push(address); printAddresses.add(address);
        }
        pictures = made;
      } else pictures = [current.file.url];
      await (services.printPictures ?? printPictures)(pictures, () => forget(made));
      if (live(current)) notice.hidden = true;
    } catch (error) {
      forget(made);
      if (!live(current)) return;
      if (isAbort(error)) notice.hidden = true; else say(`The document was not printed: ${errorMessage(error)}`, "alert");
    } finally { if (printing === mine) printing = undefined; }
  }

  pageInput.addEventListener("change", () => {
    const wanted = Number(pageInput.value.trim());
    if (Number.isInteger(wanted) && wanted >= 1 && wanted <= (shown?.doc?.pages ?? 0)) goToPage(wanted);
    else pageInput.value = String(page);
  });
  pageInput.addEventListener("keydown", event => { if (event.key === "Enter") { event.preventDefault(); pageInput.blur(); stage.focus(); } });
  stage.addEventListener("scroll", () => {
    if (scrollQueued) return;
    scrollQueued = true;
    if (typeof requestAnimationFrame === "function") requestAnimationFrame(followScroll); else queueMicrotask(followScroll);
  }, { passive: true });
  dialog.addEventListener("keydown", event => {
    if (event.defaultPrevented || event.ctrlKey || event.metaKey || event.altKey) return;
    const target = event.target;
    if (target instanceof HTMLInputElement || target instanceof HTMLTextAreaElement || target instanceof HTMLSelectElement) return;
    const pdf = Boolean(shown?.doc), several = files.length > 1;
    const act = (action: () => void): void => { event.preventDefault(); action(); };
    switch (event.key) {
      case "PageDown": if (pdf) act(() => goToPage(page + 1)); break;
      case "PageUp": if (pdf) act(() => goToPage(page - 1)); break;
      case "Home": if (pdf) act(() => goToPage(1)); break;
      case "End": if (pdf) act(() => goToPage(shown!.doc!.pages)); break;
      case "ArrowRight": if (pdf && !event.shiftKey) act(() => goToPage(page + 1)); else if (several) act(() => showFile(index + 1)); break;
      case "ArrowLeft": if (pdf && !event.shiftKey) act(() => goToPage(page - 1)); else if (several) act(() => showFile(index - 1)); break;
      case "+": case "=": act(() => zoomBy(ZOOM_STEP)); break;
      case "-": act(() => zoomBy(1 / ZOOM_STEP)); break;
      case "0": act(() => setMode("opening")); break;
      case "r": case "R": act(() => turn()); break;
      default: break;
    }
  });
  // A fitted zoom follows the window; a zoom the reader chose stays.
  const resize = typeof ResizeObserver === "function" ? new ResizeObserver(() => { if (shown && mode !== "custom") layout(); }) : undefined;
  resize?.observe(stage);

  const handle = services.dialogs.attach(dialog, {
    onClose: () => {
      closed = true;
      printing?.abort();
      resize?.disconnect();
      leave(shown);
      forget([...printAddresses]);
      for (const frame of document.querySelectorAll(".document-viewer-print-frame")) frame.remove();
    },
  });
  showFile(start);
  return handle;
}
let nextViewer = 0;
