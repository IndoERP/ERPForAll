/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
import { expect, test } from "vitest";
import { HtmlEditorHistory } from "../src/client/html-editor-history.js";
import { htmlEditorLimits } from "../src/client/html-editor-document.js";
import { ClientError } from "../src/client/transport.js";
const bytes = (value: { html: string }): number => new TextEncoder().encode(value.html).length;
const snap = (html: string) => ({ html, selection: { anchor: [0], anchorOffset: 0 } });
/// (partial: isolated snapshots, explicit undo and redo transitions)
test("history baseline is not a transition; reads and admissions isolate snapshots", () => {
  const initial = snap("a"); const history = new HtmlEditorHistory(initial, htmlEditorLimits(), bytes);
  initial.html = "mutated"; expect(history.canUndo).toBe(false); expect(history.canRedo).toBe(false); expect(history.undo()).toBeUndefined();
  const next = snap("b"); history.commit(next); next.selection.anchor[0] = 9;
  const undone = history.undo(); expect(undone?.html).toBe("a"); undone!.html = "mutated";
  const redone = history.redo(); expect(redone).toEqual(snap("b")); redone!.selection.anchor[0] = 8;
  expect(history.undo()).toEqual(snap("a")); expect(history.redo()).toEqual(snap("b"));
});
test("new edits clear redo and repeated undo/redo never skips reachable states", () => {
  const history = new HtmlEditorHistory(snap("a"), htmlEditorLimits(), bytes);
  history.commit(snap("b")); history.commit(snap("c")); expect(history.undo()?.html).toBe("b");
  history.commit(snap("d")); expect(history.canRedo).toBe(false); expect(history.redo()).toBeUndefined();
  expect(history.undo()?.html).toBe("b"); expect(history.undo()?.html).toBe("a"); expect(history.undo()).toBeUndefined();
  expect(history.redo()?.html).toBe("b"); expect(history.redo()?.html).toBe("d"); expect(history.redo()).toBeUndefined();
});
test("entry and total byte pressure evict oldest reachable undo prefixes including baseline bytes", () => {
  const history = new HtmlEditorHistory(snap("aa"), htmlEditorLimits({ historyEntries: 2, historyBytes: 5 }), bytes);
  expect(history.commit(snap("bb"))).toEqual({ shortened: false });
  expect(history.commit(snap("cc"))).toEqual({ shortened: true });
  expect(history.undo()?.html).toBe("bb"); expect(history.undo()).toBeUndefined(); expect(history.redo()?.html).toBe("cc");
  expect(history.commit(snap("dddd"))).toEqual({ shortened: true }); expect(history.canUndo).toBe(false);
  expect(history.undo()).toBeUndefined();
});
test("oversized snapshot refuses atomically preserving undo and redo", () => {
  const history = new HtmlEditorHistory(snap("a"), htmlEditorLimits({ historyBytes: 3 }), bytes);
  history.commit(snap("b")); history.undo();
  try { history.commit(snap("four")); throw new Error("Expected refusal"); }
  catch (error) { expect(error).toBeInstanceOf(ClientError); expect(error).toMatchObject({ kind: "validation", message: "HTML editor snapshot exceeds historyBytes limit." }); }
  expect(history.redo()?.html).toBe("b"); expect(history.undo()?.html).toBe("a");
});
test("reset creates a new baseline; clear releases all retained transitions", () => {
  const history = new HtmlEditorHistory(snap("a"), htmlEditorLimits(), bytes); history.commit(snap("b")); history.reset(snap("c"));
  expect(history.canUndo).toBe(false); expect(history.canRedo).toBe(false); history.commit(snap("d")); expect(history.undo()?.html).toBe("c");
  history.clear(); expect(history.canUndo).toBe(false); expect(history.canRedo).toBe(false); expect(history.redo()).toBeUndefined();
  history.reset(snap("e")); history.commit(snap("f")); expect(history.undo()?.html).toBe("e");
});

test("replaceCurrent stages isolated selection without adding or disconnecting transitions", () => {
  const history = new HtmlEditorHistory(snap("a"), htmlEditorLimits(), bytes); history.commit(snap("b")); history.undo();
  const staged = snap("a"); staged.selection.anchorOffset = 1; history.replaceCurrent(staged); staged.selection.anchorOffset = 9;
  expect(history.canUndo).toBe(false); expect(history.canRedo).toBe(true); expect(history.redo()?.html).toBe("b");
  expect(history.undo()?.selection.anchorOffset).toBe(1);
});
test("replaceCurrent refuses total byte pressure without sacrificing redo", () => {
  const history = new HtmlEditorHistory(snap("a"), htmlEditorLimits({ historyBytes: 3 }), bytes); history.commit(snap("b")); history.undo();
  try { history.replaceCurrent(snap("aaa")); throw new Error("Expected refusal"); }
  catch (error) { expect(error).toBeInstanceOf(ClientError); expect(error).toMatchObject({ kind: "validation", message: "HTML editor history exceeds historyBytes limit." }); }
  expect(history.redo()?.html).toBe("b"); expect(history.undo()?.html).toBe("a");
});
test("caller byte accounting includes selection serialization", () => {
  const serialized = (value: ReturnType<typeof snap>) => new TextEncoder().encode(JSON.stringify(value)).length;
  const initial = snap("a"); const max = serialized(initial); const history = new HtmlEditorHistory(initial, htmlEditorLimits({ historyBytes: max }), serialized);
  expect(history.commit(snap("b"))).toEqual({ shortened: true }); expect(history.undo()).toBeUndefined();
});

test("historyEntries counts every retained snapshot including baseline", () => {
  const history = new HtmlEditorHistory(snap("a"), htmlEditorLimits({ historyEntries: 2 }), bytes);
  expect(history.commit(snap("b"))).toEqual({ shortened: false });
  expect(history.commit(snap("c"))).toEqual({ shortened: true });
  expect(history.undo()?.html).toBe("b"); expect(history.undo()).toBeUndefined();
  const single = new HtmlEditorHistory(snap("a"), htmlEditorLimits({ historyEntries: 1 }), bytes);
  expect(single.commit(snap("b"))).toEqual({ shortened: true }); expect(single.canUndo).toBe(false);
});
