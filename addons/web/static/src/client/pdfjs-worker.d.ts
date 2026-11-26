/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
// pdfjs-dist ships no types for its parser module; the client only hands it to the renderer (pdf-engine.ts).
declare module "pdfjs-dist/legacy/build/pdf.worker.mjs" {
  export const WorkerMessageHandler: unknown;
}
