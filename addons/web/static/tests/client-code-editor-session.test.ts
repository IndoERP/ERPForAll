/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
import { expect, test, vi } from "vitest";
import { CodeSessions, codeEditorLimits, codeEditorText, codeSessionId } from "../src/client/code-editor-session.js";
import { markup } from "@erpforall/elang";

const position = (row: number, column: number) => ({ row, column });

test("code document admission accepts primitive source text and refuses markup and coercible values", () => {
  expect(codeEditorText("<script>notExecuted()</script>", 100)).toBe("<script>notExecuted()</script>");
  for (const value of [markup("<p>trusted</p>"), new String("text"), { toString: () => "text" }, false, null]) {
    expect(() => codeEditorText(value, 100)).toThrowError(expect.objectContaining({ kind: "validation", message: "Code editor value must be primitive text." }));
  }
  expect(codeEditorText("é", 2)).toBe("é");
  expect(() => codeEditorText("é", 1)).toThrowError(expect.objectContaining({ kind: "validation", message: "Code editor value must be at most 1 UTF-8 bytes." }));
});

test("code session identities preserve type and reject unsafe identities and widened limits", () => {
  expect(codeSessionId(1)).toBe(1); expect(codeSessionId("1")).toBe("1"); expect(codeSessionId("__proto__")).toBe("__proto__");
  for (const value of [NaN, Infinity, 1.5, Number.MAX_SAFE_INTEGER + 1, {}, null]) expect(() => codeSessionId(value)).toThrowError(expect.objectContaining({ kind: "validation", message: "Code editor sessionId must be bounded text or a safe integer." }));
  expect(codeEditorLimits({ maxDocumentBytes: 4 }).maxDocumentBytes).toBe(4);
  for (const maxDocumentBytes of [0, -1, Infinity, 1048577]) expect(() => codeEditorLimits({ maxDocumentBytes })).toThrowError(expect.objectContaining({ kind: "validation", message: "Code editor maxDocumentBytes must be a positive safe integer no greater than 1048576." }));
  expect(() => codeEditorLimits({ unknown: 1 } as never)).toThrowError(expect.objectContaining({ kind: "validation", message: "Unknown code editor limit: unknown." }));
});

test("code sessions isolate drafts and history while matching echoes preserve state", () => {
  const sessions = new CodeSessions(codeEditorLimits(), vi.fn());
  sessions.activate({ sessionId: 1, value: "first", mode: "xml" });
  expect(sessions.canUndo).toBe(false); expect(sessions.canRedo).toBe(false);
  sessions.edit("first edit", position(0, 10)); sessions.selection = position(0, 6);
  sessions.activate({ sessionId: "1", value: "second", mode: "python" });
  sessions.edit("second edit", position(0, 11));
  sessions.activate({ sessionId: 1, value: "first edit", mode: "javascript" });
  expect(sessions.value).toBe("first edit"); expect(sessions.selection).toEqual(position(0, 6)); expect(sessions.mode).toBe("javascript");
  expect(sessions.undo()).toBe(true); expect(sessions.value).toBe("first"); expect(sessions.canRedo).toBe(true);
  expect(sessions.redo()).toBe(true); expect(sessions.value).toBe("first edit");
  sessions.replace("first edit"); expect(sessions.canUndo).toBe(true);
  sessions.replace("server"); expect(sessions.value).toBe("server"); expect(sessions.canUndo).toBe(false); expect(sessions.canRedo).toBe(false);
  sessions.activate({ sessionId: "1", value: "second edit" }); expect(sessions.canUndo).toBe(true); expect(sessions.undo()).toBe(true); expect(sessions.value).toBe("second");
  sessions.edit("new branch", position(0, 10)); expect(sessions.canRedo).toBe(false);
});

test("session admission refuses aggregate overflow without evicting or changing the active draft", () => {
  const sessions = new CodeSessions(codeEditorLimits({ maxSessions: 2, maxDocumentBytes: 4, maxTotalDocumentBytes: 6 }), vi.fn());
  sessions.activate({ sessionId: "a", value: "aaaa" });
  sessions.activate({ sessionId: "b", value: "bb" });
  expect(() => sessions.edit("bbb", position(0, 3))).toThrowError(expect.objectContaining({ kind: "validation", message: "Code editor documents must total at most 6 UTF-8 bytes." }));
  expect(sessions.value).toBe("bb"); expect(sessions.id).toBe("b");
  expect(() => sessions.activate({ sessionId: "c", value: "" })).toThrowError(expect.objectContaining({ kind: "validation", message: "Code editor retains at most 2 sessions; release an inactive session first." }));
  expect(sessions.id).toBe("b");
  expect(() => sessions.release("b")).toThrowError(expect.objectContaining({ kind: "validation", message: "The active code editor session cannot be released." }));
  sessions.release("a"); sessions.activate({ sessionId: "c", value: "cccc" }); expect(sessions.value).toBe("cccc");
});

test("bounded history accounts both stacks, prunes visibly and never loses current source", () => {
  const status = vi.fn(), sessions = new CodeSessions(codeEditorLimits({ maxHistoryGroups: 2, maxHistoryBytes: 1024 }), status);
  sessions.activate({ sessionId: 0, value: "start" });
  sessions.edit("one", position(0, 3)); sessions.edit("two", position(0, 3)); sessions.edit("three", position(0, 5));
  expect(status).toHaveBeenCalledWith("Some undo and redo history was removed to stay within the editor's safety limits.");
  expect(sessions.value).toBe("three"); expect(sessions.undo()).toBe(true); expect(sessions.value).toBe("two");
  expect(sessions.undo()).toBe(true); expect(sessions.value).toBe("one"); expect(sessions.undo()).toBe(false);
  expect(sessions.historyBytes).toBeLessThanOrEqual(1024); expect(sessions.redo()).toBe(true); expect(sessions.value).toBe("two");
  sessions.dispose(); expect(sessions.historyBytes).toBe(0);
  expect(() => sessions.replace("late")).toThrowError(expect.objectContaining({ kind: "validation", message: "Code editor is disposed." }));
});

test("history restoration rechecks the aggregate document bound without consuming a refused undo", () => {
  const sessions = new CodeSessions(codeEditorLimits({ maxDocumentBytes: 5, maxTotalDocumentBytes: 6 }), vi.fn());
  sessions.activate({ sessionId: "a", value: "aaaa" }); sessions.edit("a", position(0, 1));
  sessions.activate({ sessionId: "b", value: "bbbbb" });
  sessions.activate({ sessionId: "a", value: "a" });
  expect(() => sessions.undo()).toThrowError(expect.objectContaining({ kind: "validation", message: "Code editor documents must total at most 6 UTF-8 bytes." }));
  expect(sessions.value).toBe("a"); expect(sessions.canUndo).toBe(true); expect(sessions.canRedo).toBe(false);
  sessions.release("b"); expect(sessions.undo()).toBe(true); expect(sessions.value).toBe("aaaa");
});

test("snapshot byte admission charges both stacks and a single oversized retained group", () => {
  const status = vi.fn(), sessions = new CodeSessions(codeEditorLimits({ maxHistoryBytes: 128 }), status);
  sessions.activate({ sessionId: "a", value: "" }); sessions.edit("é", position(0, 1));
  expect(sessions.value).toBe("é"); expect(sessions.historyBytes).toBe(0); expect(sessions.canUndo).toBe(false);
  expect(status).toHaveBeenCalledWith("Some undo and redo history was removed to stay within the editor's safety limits.");
});

test("pruning an undone transition also removes its dependent future redo chain", () => {
  const status = vi.fn(), sessions = new CodeSessions(codeEditorLimits({ maxHistoryBytes: 700 }), status);
  sessions.activate({ sessionId: "a", value: "" }); sessions.edit("a", position(0, 1)); sessions.edit("ab", position(0, 2));
  expect(sessions.undo()).toBe(true); expect(sessions.undo()).toBe(true); expect(sessions.value).toBe("");
  sessions.activate({ sessionId: "b", value: "" }); sessions.edit("b".repeat(160), position(0, 160));
  expect(sessions.value).toBe("b".repeat(160)); expect(sessions.canUndo).toBe(true); expect(sessions.historyBytes).toBeLessThanOrEqual(700);
  sessions.activate({ sessionId: "a", value: "" }); expect(sessions.value).toBe(""); expect(sessions.canRedo).toBe(false);
  expect(sessions.redo()).toBe(false); expect(sessions.value).toBe("");
  expect(status).toHaveBeenCalledWith("Some undo and redo history was removed to stay within the editor's safety limits.");
});

test("snapshot byte admission reduces retained undo depth for larger documents without losing current source", () => {
  const run = (length: number) => {
    const notice = vi.fn(), sessions = new CodeSessions(codeEditorLimits({ maxHistoryBytes: 900, maxHistoryGroups: 10 }), notice);
    sessions.activate({ sessionId: "a", value: "a".repeat(length) });
    for (const character of ["b", "c", "d"]) sessions.edit(character.repeat(length), position(0, length));
    expect(sessions.value).toBe("d".repeat(length));
    const charged = sessions.historyBytes, restored: string[] = [];
    while (sessions.undo()) restored.push(sessions.value);
    expect(sessions.historyBytes).toBe(charged); // Moving a group between stacks retains its charge.
    expect(charged).toBeLessThanOrEqual(900);
    return { restored, notice };
  };
  const short = run(10), large = run(100);
  expect(short.restored).toEqual(["c".repeat(10), "b".repeat(10), "a".repeat(10)]);
  expect(short.notice).not.toHaveBeenCalled();
  expect(large.restored).toEqual(["c".repeat(100)]);
  expect(large.notice).toHaveBeenCalledWith("Some undo and redo history was removed to stay within the editor's safety limits.");
});
