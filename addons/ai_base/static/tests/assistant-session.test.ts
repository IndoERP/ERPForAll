/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
import { afterEach, expect, test, vi } from "vitest";
import { assistantSession } from "../src/assistant-session.js";
import { clearProtectedTabState } from "../../../web/static/src/client/session-boundary.js";

afterEach(() => clearProtectedTabState());
test("a fresh shell restores replies and dock state in the same user/company scope only", async () => {
  const request = vi.fn<typeof fetch>().mockImplementation(async () => Response.json({ text: "Received 6" }));
  const options = { userId: 51, request, history: { turns: 4, bytes: 4096, messageBytes: 4096 } };
  const session = assistantSession({}, 7, "token-1", options);
  session.updateDock({ mode: "minimized", draft: "What remains?", scroll: 86 });
  await session.send("Show receipt", "Format the response as requested. Show receipt");
  expect(session.messages.map(m => m.text)).toEqual(["Show receipt","Received 6"]);
  expect(JSON.parse(String(request.mock.calls[0]![1]?.body)).message).toBe("Format the response as requested. Show receipt");
  const restored = assistantSession({}, 7, "rotated-token", options);
  expect(restored.messages).toEqual(session.messages);
  expect(restored.readDock()).toEqual({ mode: "minimized", draft: "What remains?", scroll: 86 });
  expect(assistantSession({}, 8, "other-company", options).messages).toEqual([]);
  expect(assistantSession({}, 7, "other-user", { ...options, userId: 52 }).readDock().draft).toBe("");
  clearProtectedTabState();
  expect(restored.messages).toEqual([]);
  expect(assistantSession({}, 7, "new-login", options).messages).toEqual([]);
});

test("a reload interrupts a pending exchange without replaying a provider call", async () => {
  let finish!: (value: Response) => void;
  const request = vi.fn<typeof fetch>().mockImplementation(() => new Promise(resolve => { finish = resolve; }));
  const options = { userId: 51, request, history: { turns: 4, bytes: 4096, messageBytes: 4096 } };
  const first = assistantSession({}, 7, "token", options);
  const sending = first.send("Summarize");
  const recovered = assistantSession({}, 7, "rotated-token", options);
  expect(recovered.messages.at(-1)?.state).toBe("error");
  expect(recovered.busy).toBe(false);
  expect(request).toHaveBeenCalledTimes(1);
  clearProtectedTabState();
  finish(Response.json({ text: "A late reply" }));
  await sending;
  expect(assistantSession({}, 7, "new-login", options).messages).toEqual([]);
});

test("tab history evicts whole exchanges and restores only the configured number", async () => {
  const options = { userId: 9, request: vi.fn(async () => Response.json({ text: "Received" })),
    history: { turns: 2, bytes: 4096, messageBytes: 4096 } };
  const session = assistantSession({}, 6, "token", options);
  for (const question of ["One", "Two", "Three"]) await session.send(question);
  expect(session.messages.map(item => item.text)).toEqual(["Two", "Received", "Three", "Received"]);
  const restored = assistantSession({}, 6, "token", options);
  expect(restored.messages).toEqual(session.messages);
  expect(restored.persistenceWarning).toContain("most recent");
});

test("an oversized reply remains readable now and never replays on reload", async () => {
  const text = "詳".repeat(1600);
  const request = vi.fn(async () => Response.json({ text }));
  const options = { userId: 5, request, history: { turns: 2, bytes: 4096, messageBytes: 4096 } };
  const session = assistantSession({}, 7, "token", options);
  await session.send("Show all");
  expect(session.messages.at(-1)?.text).toBe(text);
  expect(session.persistenceWarning).toContain("too large");
  expect(assistantSession({}, 7, "token", options).messages).toEqual([]);
  expect(request).toHaveBeenCalledTimes(1);
  request.mockImplementationOnce(async () => Response.json({ text: "Ready" }));
  await session.send("Short reply");
  expect(session.persistenceWarning).toContain("most recent");
  expect(assistantSession({}, 7, "token", options).messages).toEqual(session.messages);
});

test("editors share draft updates without re-rendering the transcript", () => {
  const options = { userId: 50, history: { turns: 4, bytes: 4096, messageBytes: 80 } };
  const session = assistantSession({}, 8, "token", options);
  const transcript = vi.fn(), editor = vi.fn();
  session.subscribe(transcript);
  const dispose = session.subscribeDraft(editor);
  session.updateDock({ draft: "PO00001" });
  session.updateDock({ scroll: 4 });
  expect(editor.mock.calls).toEqual([[""], ["PO00001"]]);
  expect(transcript).not.toHaveBeenCalled();
  dispose();
  session.updateDock({ draft: "Later" });
  expect(editor).toHaveBeenCalledTimes(2);
});

test("each question resends the completed transcript as history", async () => {
  const request = vi.fn<typeof fetch>().mockImplementation(async () => Response.json({ text: "Second answer" }));
  const options = { userId: 51, request, history: { turns: 4, bytes: 4096, messageBytes: 4096 } };
  const session = assistantSession({}, 7, "token-1", options);
  await session.send("First question");
  expect(JSON.parse(String(request.mock.calls[0]![1]?.body)).history).toEqual([]);
  await session.send("Follow up");
  const body = JSON.parse(String(request.mock.calls[1]![1]?.body));
  expect(body.message).toBe("Follow up");
  expect(body.company_id).toBe(7);
  expect(body.history).toEqual([
    { role: "user", text: "First question" },
    { role: "assistant", text: "Second answer" },
  ]);
});

test("history carries only completed exchanges, never failed replies or their questions", async () => {
  const request = vi.fn<typeof fetch>()
    .mockImplementationOnce(async () => new Response(JSON.stringify({ message: "erp.ai.provider_failed: no" }), { status: 502, headers: { "content-type": "application/json" } }))
    .mockImplementationOnce(async () => Response.json({ text: "Ready" }));
  const options = { userId: 51, request, history: { turns: 4, bytes: 4096, messageBytes: 4096 } };
  const session = assistantSession({}, 7, "token", options);
  await session.send("Question one");
  expect(session.messages.at(-1)?.state).toBe("error");
  await session.send("Question two");
  const body = JSON.parse(String(request.mock.calls[1]![1]?.body));
  // The unanswered question stays visible but is not resent, so the history never sends
  // two adjacent user messages the provider protocols cannot represent.
  expect(body.history).toEqual([]);
});

test("history drops an exchange whose reply exceeds the per-message limit", async () => {
  const oversized = "詳".repeat(5000);
  const request = vi.fn<typeof fetch>().mockImplementation(async () => Response.json({ text: oversized }));
  const options = { userId: 51, request, history: { turns: 4, bytes: 4096, messageBytes: 4096 } };
  const session = assistantSession({}, 7, "token", options);
  await session.send("Show everything");
  request.mockImplementationOnce(async () => Response.json({ text: "Fine" }));
  await session.send("Now shorter");
  const body = JSON.parse(String(request.mock.calls[1]![1]?.body));
  // A question whose reply cannot be resent is dropped with it, so the history never carries
  // two adjacent user messages.
  expect(body.history).toEqual([]);
});

test("history keeps later exchanges when an earlier reply is too large to resend", async () => {
  const oversized = "詳".repeat(5000);
  const request = vi.fn<typeof fetch>()
    .mockImplementationOnce(async () => Response.json({ text: oversized }))
    .mockImplementationOnce(async () => Response.json({ text: "Small one" }))
    .mockImplementationOnce(async () => Response.json({ text: "Small two" }));
  const options = { userId: 51, request, history: { turns: 4, bytes: 32768, messageBytes: 4096 } };
  const session = assistantSession({}, 7, "token", options);
  await session.send("First big");
  await session.send("Second small");
  // The oversized exchange drops out of the resent history instead of leaving an orphan
  // question that would create two adjacent user messages.
  expect(JSON.parse(String(request.mock.calls[1]![1]?.body)).history).toEqual([]);
  await session.send("Third small");
  const body = JSON.parse(String(request.mock.calls[2]![1]?.body));
  expect(body.history).toEqual([
    { role: "user", text: "Second small" },
    { role: "assistant", text: "Small one" },
  ]);
});

test("a durable conversation continues by server ID without resubmitting browser history", async () => {
  const request = vi.fn<typeof fetch>().mockImplementation(async () => Response.json({ text: "Ready", session_id: 36 }));
  const chat = assistantSession({}, 7, "token", { userId: 51, request, history: { turns: 4, bytes: 4096, messageBytes: 2048 } });
  await chat.send("First");
  await chat.send("Next");
  const [first, next] = request.mock.calls.map(call => JSON.parse(String(call[1]?.body)));
  expect(first.request_id).toMatch(/^[a-f0-9-]{36}$/);
  expect(next.request_id).not.toBe(first.request_id);
  expect(next.session_id).toBe(36);
  expect(next.history).toBeUndefined();
});

test("a resumed approval updates the shared transcript without caching private results in the browser", async () => {
  const pending = {name:"Confirm order",model:"sale.order",ids:[4],status:"awaiting_approval",approval_request_id:12};
  const completed = {...pending,status:"completed"};
  const request = vi.fn<typeof fetch>()
    .mockResolvedValueOnce(Response.json({text:"Approval required",session_id:22,operations:[pending]}))
    .mockResolvedValueOnce(Response.json(completed));
  const chat = assistantSession({}, 7, "token", {userId:51,request,history:{savedSessions:true,turns:4,bytes:4096,messageBytes:2048}});
  await chat.send("Confirm my order");
  expect(chat.messages.at(-1)?.operations?.[0]?.status).toBe("awaiting_approval");
  await chat.resumeOperation(12);
  expect(chat.messages.at(-1)?.operations?.[0]?.status).toBe("completed");
  expect(window.sessionStorage.getItem("erp.session.ai.51.7.messages")).toBe("[]");
});

test("an active durable request stores no transcript before the server responds", async () => {
  let finish!: (value: Response) => void;
  const request = vi.fn<typeof fetch>().mockImplementation(() => new Promise(resolve => { finish = resolve; }));
  const chat = assistantSession({}, 7, "token", {userId:51,request,history:{savedSessions:true,turns:4,bytes:4096,messageBytes:2048}});
  const waiting = chat.send("Private question");
  expect(window.sessionStorage.getItem("erp.session.ai.51.7.messages")).toBe("[]");
  finish(Response.json({text:"Ready",session_id:22}));
  await waiting;
});

test("one assistant turn can deliver an ordered set of different Canvas outputs", async () => {
  const first = {canvas_type:"document",title:"Brief",data:{sections:[{kind:"paragraph",text:"Confirm dates"}]}};
  const second = {canvas_type:"timeline",title:"Pilot",data:{items:[{label:"Launch",when:"Week one"}]}};
  const request = vi.fn<typeof fetch>().mockResolvedValue(Response.json({text:"Ready",canvas:second,canvases:[first,second]}));
  const chat = assistantSession({}, 4, "token", {request,history:{turns:4,bytes:8192,messageBytes:2048}});
  await chat.send("Prepare an implementation brief and a timeline");
  expect(chat.messages.at(-1)?.canvases).toEqual([first,second]);
});

test("a first turn with a lost response is found by its submission receipt on reload, without a second mutation", async () => {
  const restored = [{role:"user",text:"Create a quotation"},{role:"assistant",text:"Quotation recorded"}];
  const call = vi.fn(async (_model:string, method:string) => method === "locate_submission"
    ? {session_id:55} : {id:55,messages:restored});
  const request = vi.fn<typeof fetch>().mockRejectedValueOnce(new TypeError("Disconnected"));
  const options = {userId:73,request,history:{savedSessions:true,turns:4,bytes:4096,messageBytes:2048}};
  const first = assistantSession({}, 7, "token", options);
  await first.send("Create a quotation");
  const next = assistantSession({call}, 7, "token",options);
  await vi.waitFor(() => expect(next.messages.at(-1)?.text).toBe("Quotation recorded"));
  expect(next.conversationId).toBe(55);
  expect(request).toHaveBeenCalledOnce();
  expect(call.mock.calls.map(row=>row[1])).toEqual(["locate_submission","conversation_history"]);
});

test("a cancelled workspace navigation restores the currently mounted workspace owner", () => {
  const chat = assistantSession({}, 7, "token", {history:{turns:4,bytes:4096,messageBytes:2048}});
  const first = vi.fn(), staged = vi.fn();
  const leave = chat.attachWorkspace(first,vi.fn());
  const cancel = chat.attachWorkspace(staged,vi.fn());
  expect(chat.focusWorkspaceChat()).toBe(true);
  expect(staged).toHaveBeenCalledOnce();
  cancel();
  expect(chat.workspaceActive).toBe(true);
  expect(chat.focusWorkspaceChat()).toBe(true);
  expect(first).toHaveBeenCalledOnce();
  leave();
  expect(chat.workspaceActive).toBe(false);
});
