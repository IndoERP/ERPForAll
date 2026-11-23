/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
/** The English sources of the `t()` calls in `text`; a call without one plain string literal is refused, naming `file`. */
export function extractClientStrings(text: string, file?: string): string[];
/** Every `t()` text of an addon's client sources (default `web`), as a catalog skeleton: source to empty translation, sorted. */
export function clientCatalogSkeleton(addon?: string): Record<string, "">;
