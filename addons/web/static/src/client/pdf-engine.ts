/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
/// <reference path="./pdfjs-worker.d.ts" />
// The only module that names the bundled PDF renderer (pdf.js, Apache-2.0; ADR 0231). The viewer talks to the
// small interface below, so its behaviour is tested without a canvas and the renderer can change behind it.
// The renderer's "legacy" build: the same library with its own polyfills. Its default build calls language
// features only the newest browsers have (Chromium 141 lacks `Map.prototype.getOrInsertComputed`, and every page
// failed to draw there: web/scripts/browser-document-viewer.mjs, 2026-11-18).
import { getDocument, InvalidPDFException, PasswordException, RenderingCancelledException } from "pdfjs-dist/legacy/build/pdf.mjs";
import * as pdfWorker from "pdfjs-dist/legacy/build/pdf.worker.mjs";
import { PdfOpenError, type PdfOpen, type PdfRotation } from "./pdf-document.js";

/**
 * Largest canvas drawn, in pixels. Browsers refuse or blank larger canvases (the lowest common limit is about
 * 16.7 million pixels); a page that would be larger is drawn at a lower resolution and stretched.
 */
const CANVAS_PIXELS_MAX = 16_000_000;

let prepared = false;
function prepare(): void {
  if (prepared) return;
  // The parser runs on the page's own thread: the client is published as one script, so there is no separate
  // worker file to start (ADR 0231 names the follow-up). pdf.js takes the handler from this global.
  (globalThis as { pdfjsWorker?: unknown }).pdfjsWorker = pdfWorker;
  prepared = true;
}

function aborted(): DOMException { return new DOMException("Drawing the page was cancelled.", "AbortError"); }

/** Open `data` with the bundled renderer. Scripts, forms and links of the document are never run or followed. */
export const openPdf: PdfOpen = async data => {
  prepare();
  // No address is ever given to the renderer: it gets bytes only. Fonts and colour profiles a document does
  // not embed, and JPEG 2000 / JBIG2 decoders, are not shipped (useWasm off, no cMapUrl): see ADR 0231.
  const task = getDocument({ data, useWasm: false, useWorkerFetch: false, enableXfa: false, stopAtErrors: false });
  const opened = await task.promise.catch((error: unknown) => {
    void task.destroy();
    if (error instanceof PasswordException) throw new PdfOpenError("password", "The document is protected by a password.");
    if (error instanceof InvalidPDFException) throw new PdfOpenError("invalid", "The file is not a readable PDF document.");
    throw new PdfOpenError("failed", error instanceof Error ? error.message : "The document could not be opened.");
  });
  const render = async (number: number, canvas: HTMLCanvasElement, scale: number, rotation: PdfRotation, signal: AbortSignal, intent: "display" | "print"): Promise<void> => {
    if (signal.aborted) throw aborted();
    const page = await opened.getPage(number);
    if (signal.aborted) throw aborted();
    let viewport = page.getViewport({ scale, rotation: (page.rotate + rotation) % 360 });
    const pixels = viewport.width * viewport.height;
    if (pixels > CANVAS_PIXELS_MAX) viewport = page.getViewport({ scale: scale * Math.sqrt(CANVAS_PIXELS_MAX / pixels), rotation: (page.rotate + rotation) % 360 });
    canvas.width = Math.max(1, Math.floor(viewport.width));
    canvas.height = Math.max(1, Math.floor(viewport.height));
    const rendering = page.render({ canvas, viewport, intent });
    const cancel = (): void => rendering.cancel();
    signal.addEventListener("abort", cancel, { once: true });
    try { await rendering.promise; }
    catch (error) { if (error instanceof RenderingCancelledException || signal.aborted) throw aborted(); throw error; }
    finally { signal.removeEventListener("abort", cancel); }
  };
  return {
    pages: opened.numPages,
    async size(number) {
      const viewport = (await opened.getPage(number)).getViewport({ scale: 1 });
      return { width: viewport.width, height: viewport.height };
    },
    draw: (number, canvas, scale, rotation, signal) => render(number, canvas, scale, rotation, signal, "display"),
    async picture(number, scale, rotation, signal) {
      const canvas = document.createElement("canvas");
      try {
        await render(number, canvas, scale, rotation, signal, "print");
        return await new Promise<Blob>((resolve, reject) => canvas.toBlob(blob => blob ? resolve(blob) : reject(new Error(`Page ${number} could not be prepared for printing.`)), "image/png"));
      } finally { canvas.width = 0; canvas.height = 0; }
    },
    close: () => task.destroy(),
  };
};
