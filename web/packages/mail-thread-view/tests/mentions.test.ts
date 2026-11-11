/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
// Structured mentions (6): spans that follow the draft through edits and become UTF-8 byte spans on send.
import { expect, test } from "vitest";
import { MAX_MENTIONS, reconcileMentions, toSpans, utf8Length, type MentionRange } from "../src/index";
import { Rig, failsWith, type } from "./support";

const mention = (start: number, end: number, partnerId = 1, text = ""): MentionRange => ({ start, end, partnerId, text });

test("a mention follows edits before it, survives edits after it and is dropped, text kept, when an edit touches it", () => {
  const before = "Hi @Ann ok";
  const ann = [mention(3, 7, 1, "@Ann")];
  const after = (text: string): MentionRange[] => reconcileMentions(ann, before, text);
  expect(after("Oh, Hi @Ann ok")).toEqual([mention(7, 11, 1, "@Ann")]);
  expect(after("Hi @Ann ok, thanks")).toEqual(ann);
  expect(after("Hi @Ann\nok")).toEqual(ann);
  expect(after("@Ann ok")).toEqual([mention(0, 4, 1, "@Ann")]);
  expect(after("Hi X@Ann ok")).toEqual([mention(4, 8, 1, "@Ann")]);
  expect(after("Hi @Annx ok")).toEqual(ann);
  expect(after("Hi @Axnn ok")).toEqual([]);
  expect(after("Hi @Bob")).toEqual([]);
  expect(after("Hi ok")).toEqual([]);
  expect(after(before)).toEqual(ann);
  expect(after(before)).not.toBe(ann);
  expect(reconcileMentions([], "a", "ab")).toEqual([]);
  // Two mentions: an edit between them moves only the second; an edit across both drops both.
  const two = [mention(0, 2, 1, "@A"), mention(5, 7, 2, "@B")];
  expect(reconcileMentions(two, "@A is @B", "@A was @B")).toEqual([mention(0, 2, 1, "@A"), mention(6, 8, 2, "@B")]);
  expect(reconcileMentions(two, "@A is @B", "all")).toEqual([]);
  // A surrogate pair counts as two units in the draft, so shifts are in code units.
  expect(reconcileMentions([mention(2, 6, 1, "@Ann")], "😀@Ann", "😀😀@Ann")).toEqual([mention(4, 8, 1, "@Ann")]);
});

test("mention offsets become UTF-8 byte spans of the exact text sent, whatever characters precede them", () => {
  const text = "héllo 😀 @Ann Lee ok €";
  // "héllo " is 7 bytes, the emoji 4, the space 1: the mention starts at byte 12 and is 8 bytes long.
  const start = text.indexOf("@Ann Lee");
  expect(toSpans(text, [mention(start, start + 8, 7, "@Ann Lee")])).toEqual([{ start: 12, end: 20, partner_id: 7 }]);
  expect(utf8Length(text)).toBe(new TextEncoder().encode(text).length);
  for (const sample of ["", "plain", "é", "€", "😀", "a\u{10FFFF}b", "x\uD800y", "\uDC00"]) {
    expect(utf8Length(sample)).toBe(new TextEncoder().encode(sample).length);
  }
  // Spans come out in text order whatever the order they were recorded in.
  const two = toSpans("@a @b", [mention(3, 5, 2, "@b"), mention(0, 2, 1, "@a")]);
  expect(two).toEqual([{ start: 0, end: 2, partner_id: 1 }, { start: 3, end: 5, partner_id: 2 }]);
  expect(toSpans("none", [])).toEqual([]);

  failsWith(() => toSpans("@a", [mention(0, 5, 1, "@a")]), "mention", "mention of partner 1 at 0..5 is outside the draft text (length 2)");
  failsWith(() => toSpans("@a", [mention(1, 1, 1, "")]), "mention", "mention of partner 1 at 1..1 is outside the draft text (length 2)");
  failsWith(() => toSpans("@a", [mention(-1, 1, 1, "@")]), "mention", "mention of partner 1 at -1..1 is outside the draft text (length 2)");
  failsWith(() => toSpans("@abc", [mention(0, 3, 1, "@ab"), mention(2, 4, 2, "bc")]), "mention", "mention of partner 2 at 2..4 overlaps the previous mention");
  failsWith(() => toSpans("@abc", [mention(0, 3, 1, "@xy")]), "mention", 'mention of partner 1 at 0..3 no longer matches its text "@xy": the draft holds "@ab"');
  failsWith(() => toSpans("a😀b", [mention(2, 3, 1, "\uDE00")]), "mention", "mention of partner 1 at 2..3 splits a character at index 2");
  failsWith(() => toSpans("a😀b", [mention(0, 2, 1, "a\uD83D")]), "mention", "mention of partner 1 at 0..2 splits a character at index 2");
  const many = Array.from({ length: MAX_MENTIONS + 1 }, (_, i) => mention(i * 2, i * 2 + 1, i + 1, "@"));
  failsWith(() => toSpans("@ ".repeat(MAX_MENTIONS + 1), many), "mention", `a post carries at most ${MAX_MENTIONS} mentions, this draft has ${MAX_MENTIONS + 1}`);
});

test("the composer keeps the draft's mentions in step with what the user types", () => {
  const rig = new Rig();
  const composer = rig.composer();
  const state = rig.store.local("mail_ui.thread");
  type(composer.field, "Hi @Ann ok");
  state.set("mail.thread:7", { mentions: [mention(3, 7, 1, "@Ann")] });
  type(composer.field, "Oh, Hi @Ann ok");
  expect(state.get("mail.thread:7", "mentions")).toEqual([mention(7, 11, 1, "@Ann")]);
  type(composer.field, "Oh, Hi @Ann ok!");
  expect(state.get("mail.thread:7", "mentions")).toEqual([mention(7, 11, 1, "@Ann")]);
  type(composer.field, "Oh, Hi @Aan ok!");
  expect(state.get("mail.thread:7", "mentions")).toEqual([]);
  expect(composer.field.value).toBe("Oh, Hi @Aan ok!");
});
