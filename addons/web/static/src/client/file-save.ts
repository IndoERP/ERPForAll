/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
/** Offer `blob` to the user as a file download named `fileName` (a hidden link pressed once). */
export function saveFile(blob: Blob, fileName: string): void {
  const url = URL.createObjectURL(blob);
  const link = document.createElement("a");
  link.href = url;
  link.download = fileName;
  link.hidden = true;
  document.body.append(link);
  link.click();
  link.remove();
  // The browser has started the download by the time this fires; the address is not needed after it.
  setTimeout(() => URL.revokeObjectURL(url), 30_000);
}
