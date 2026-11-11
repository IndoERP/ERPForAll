/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
/** Document scroll lock shared by the full-screen layers (home menu, mobile drawer).
 *
 * While a layer covers the page, a swipe or wheel on the layer must not scroll the page under it: the header would
 * slide away and the page would show above the layer. `overflow: hidden` on `<body>` alone does not stop touch
 * scrolling on every mobile browser, so the lock sets a class on `<html>` (see client.css `erp-scroll-locked`).
 * Holders are counted, so two layers can hold the lock at once; each release function works once. */
let holders = 0;

export const scrollLockClass = "erp-scroll-locked";

export function lockDocumentScroll(): () => void {
  if (holders === 0) document.documentElement.classList.add(scrollLockClass);
  holders += 1;
  let released = false;
  return () => {
    if (released) return;
    released = true;
    holders -= 1;
    if (holders === 0) document.documentElement.classList.remove(scrollLockClass);
  };
}

/** The element that scrolls the document (standards mode: `<html>`). */
export function documentScroller(): Element {
  return document.scrollingElement ?? document.documentElement;
}
