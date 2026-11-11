/*! SPDX-License-Identifier: MIT OR Apache-2.0 */

/** The longest text after a delimiter that still triggers suggestions. */
export const MAX_TERM_LENGTH = 64;

/** A trigger found before the caret. */
export interface Trigger {
  readonly delimiter: string;
  /** The text typed after the delimiter. */
  readonly term: string;
  /** The index of the delimiter in the draft. */
  readonly start: number;
}

/**
 * Find the suggestion trigger the caret is in: the text from the last whitespace before the caret to the caret must
 * start with a delimiter (so `a@b.com` is not a trigger). The longest matching delimiter wins (`::` before `:`).
 *
 * @param text - the draft
 * @param caret - the caret index
 * @param delimiters - the delimiters of the registered providers
 * @returns the trigger, or `null` when the caret is not in one
 */
export function findTrigger(text: string, caret: number, delimiters: readonly string[]): Trigger | null {
  let start = caret;
  while (start > 0 && !/\s/.test(text.charAt(start - 1))) start -= 1;
  const token = text.slice(start, caret);
  const matching = delimiters.filter((d) => token.startsWith(d)).sort((a, b) => b.length - a.length);
  for (const delimiter of matching) {
    const term = token.slice(delimiter.length);
    if (term.length <= MAX_TERM_LENGTH) return { delimiter, term, start };
  }
  return null;
}
