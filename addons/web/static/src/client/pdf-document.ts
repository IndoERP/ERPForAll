/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
// What the document viewer needs from a PDF renderer. `pdf-engine.ts` implements it with the bundled renderer
// (ADR 0231); this file names no renderer, so the viewer and its tests load without one.

/** Size of a page in PDF points (1/72 inch), with the page's own rotation applied. */
export interface PdfPageSize { readonly width: number; readonly height: number }
/** Quarter turns the reader adds to every page. */
export type PdfRotation = 0 | 90 | 180 | 270;

/** An open document. Pages are numbered from 1. */
export interface PdfDocument {
  readonly pages: number;
  size(page: number): Promise<PdfPageSize>;
  /** Draw `page` into `canvas` at `scale` canvas pixels per point. Rejects with an `AbortError` when `signal` aborts. */
  draw(page: number, canvas: HTMLCanvasElement, scale: number, rotation: PdfRotation, signal: AbortSignal): Promise<void>;
  /** `page` as a PNG picture at `scale` pixels per point, for printing. */
  picture(page: number, scale: number, rotation: PdfRotation, signal: AbortSignal): Promise<Blob>;
  close(): Promise<void>;
}

/** Why a file did not open: `password` (protected), `invalid` (not a readable PDF) or `failed` (anything else). */
export class PdfOpenError extends Error {
  constructor(readonly reason: "password" | "invalid" | "failed", message: string) { super(message); this.name = "PdfOpenError"; }
}

/** Opens PDF bytes. The viewer's default is {@link openPdf}; tests give their own. */
export type PdfOpen = (data: ArrayBuffer) => Promise<PdfDocument>;
