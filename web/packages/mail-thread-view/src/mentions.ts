/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
// Structured mentions (6): spans next to a plain-text draft. Offsets inside the field are UTF-16 code
// units; the server wants UTF-8 byte spans of the text it receives (thread_post, "mentions"), converted at send.
import { ThreadViewError } from "./errors";

/** The most mentions one post may carry (the server refuses more). */
export const MAX_MENTIONS = 100;
/** The longest body the server accepts, in UTF-8 bytes. */
export const MAX_BODY_BYTES = 1_048_576;

/** A mention recorded in the draft: the partner and the literal text it occupies (`@Ann Lee`). */
export interface MentionRange {
  readonly start: number;
  readonly end: number;
  readonly partnerId: number;
  /** The exact draft text of the span; checked against the draft at send. */
  readonly text: string;
}

/** The wire form of a mention: `thread_post` `mentions[]`, byte offsets of the UTF-8 text. */
export interface MentionSpan {
  readonly start: number;
  readonly end: number;
  readonly partner_id: number;
}

/**
 * Keep the mentions valid across an edit. The edit is reduced to one replaced range (common prefix and suffix of the
 * two texts); a mention before it stays, one after it shifts by the change in length, one that overlaps it is dropped
 * (its text remains in the draft as plain text).
 *
 * @param ranges - the mentions of `before`
 * @param before - the draft text before the edit
 * @param after - the draft text after the edit
 */
export function reconcileMentions(ranges: readonly MentionRange[], before: string, after: string): MentionRange[] {
  if (before === after || ranges.length === 0) return [...ranges];
  const shortest = Math.min(before.length, after.length);
  let prefix = 0;
  while (prefix < shortest && before.charCodeAt(prefix) === after.charCodeAt(prefix)) prefix += 1;
  let suffix = 0;
  while (suffix < shortest - prefix && before.charCodeAt(before.length - 1 - suffix) === after.charCodeAt(after.length - 1 - suffix)) {
    suffix += 1;
  }
  const oldEnd = before.length - suffix;
  const delta = after.length - before.length;
  const kept: MentionRange[] = [];
  for (const range of ranges) {
    if (range.end <= prefix) kept.push(range);
    else if (range.start >= oldEnd) kept.push({ ...range, start: range.start + delta, end: range.end + delta });
  }
  return kept;
}

/** The number of UTF-8 bytes of a text. */
export function utf8Length(text: string): number {
  let bytes = 0;
  for (let i = 0; i < text.length; i += 1) {
    const unit = text.charCodeAt(i);
    if (unit < 0x80) bytes += 1;
    else if (unit < 0x800) bytes += 2;
    else if (unit >= 0xd800 && unit <= 0xdbff && i + 1 < text.length && text.charCodeAt(i + 1) >= 0xdc00 && text.charCodeAt(i + 1) <= 0xdfff) {
      bytes += 4;
      i += 1;
    } else bytes += 3;
  }
  return bytes;
}

/**
 * Convert the draft's mentions to the wire spans.
 *
 * @param text - the exact text that will be sent
 * @param ranges - the mentions of that text
 * @throws {@link ThreadViewError} `mention` for more than 100 mentions, a span outside the text, one that splits a
 *   character, overlapping spans, or a span whose recorded text is no longer what the draft holds there
 */
export function toSpans(text: string, ranges: readonly MentionRange[]): MentionSpan[] {
  if (ranges.length > MAX_MENTIONS) throw new ThreadViewError("mention", `a post carries at most ${MAX_MENTIONS} mentions, this draft has ${ranges.length}`);
  const sorted = [...ranges].sort((a, b) => a.start - b.start);
  let previousEnd = 0;
  for (const range of sorted) {
    const where = `mention of partner ${range.partnerId} at ${range.start}..${range.end}`;
    if (!Number.isInteger(range.start) || !Number.isInteger(range.end) || range.start < 0 || range.end <= range.start || range.end > text.length) {
      throw new ThreadViewError("mention", `${where} is outside the draft text (length ${text.length})`);
    }
    if (range.start < previousEnd) throw new ThreadViewError("mention", `${where} overlaps the previous mention`);
    if (text.slice(range.start, range.end) !== range.text) {
      throw new ThreadViewError("mention", `${where} no longer matches its text ${JSON.stringify(range.text)}: the draft holds ${JSON.stringify(text.slice(range.start, range.end))}`);
    }
    for (const index of [range.start, range.end]) {
      const low = text.charCodeAt(index);
      const high = index > 0 ? text.charCodeAt(index - 1) : 0;
      if (low >= 0xdc00 && low <= 0xdfff && high >= 0xd800 && high <= 0xdbff) {
        throw new ThreadViewError("mention", `${where} splits a character at index ${index}`);
      }
    }
    previousEnd = range.end;
  }
  const spans: MentionSpan[] = [];
  let bytes = 0;
  let cursor = 0;
  for (const range of sorted) {
    bytes += utf8Length(text.slice(cursor, range.start));
    const start = bytes;
    bytes += utf8Length(text.slice(range.start, range.end));
    spans.push({ start, end: bytes, partner_id: range.partnerId });
    cursor = range.end;
  }
  return spans;
}
