/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
import { afterEach, expect, test, vi } from "vitest";
import type { ClientActionEnvironment } from "@erpforall/client";
import { workspace } from "../src/workspace.js";
import { assistantSession, readHistoryPolicy } from "../src/assistant-session.js";
import { recordProposal, workspaceRecords } from "../src/workspace-records.js";

const records = { length: 3, records: [
  { id: 16, name: "<script>unsafe</script>", state: "draft", date_order: "2026-10-20 09:00:00" },
  { id: 14, name: "P0014", state: "purchase", date_order: false },
] };
const states = { state: { selection: [["draft", "RFQ"], ["purchase", "Purchase order"]] } };
afterEach(() => document.body.replaceChildren());
function build(call = vi.fn().mockResolvedValueOnce(records).mockResolvedValueOnce(states)) {
  const open = vi.fn().mockResolvedValue(undefined);
  const env = { companies: [{ id: 7, name: "Demo company" }], rpc: {call}, action: {params: {sources: ["purchase.order"], limit: 2}},
    open, run: (job: () => Promise<void>) => { void job(); } } as unknown as ClientActionEnvironment;
  const surface = workspaceRecords(env, 7); document.body.append(surface.node);
  return { ...surface, call, open };
}

/// (partial: the full workspace issues only native open-record actions for returned IDs)
test("lookup scopes company, reads native status labels, counts only displayed rows and opens the real form", async () => {
  const view = build();
  const search = view.node.querySelector("input")!;
  search.value = "P00";
  view.node.querySelector("form")!.requestSubmit();
  await vi.waitFor(() => expect(view.node.querySelector("table")).not.toBeNull());
  expect(view.call).toHaveBeenCalledWith("purchase.order","web_search_read",{
    domain:[["company_id","=",7],["name","ilike","P00"]], specification:{name:{},state:{},date_order:{}},
    context:{allowed_company_ids:[7]}, offset:0, limit:2, count_limit:3, order:"id desc",
  });
  expect(view.node.querySelector("[role=status]")?.textContent).toBe("Showing the latest 2 matching records.");
  expect(Array.from(view.node.querySelectorAll("li"), li => li.textContent)).toEqual(["RFQ1","Purchase order1"]);
  expect(view.node.querySelector("script")).toBeNull();
  const link = view.node.querySelector<HTMLButtonElement>("tbody button")!;
  expect(link.textContent).toBe("<script>unsafe</script>");
  link.click();
  expect(view.open).toHaveBeenCalledWith({type:"ir.actions.act_window",name:link.textContent,
    res_model:"purchase.order",res_id:16,views:[[false,"form"]],target:"new"});
});
test("new input makes a late result obsolete; a denied read is shown in place", async () => {
  let done!: (response: unknown) => void;
  const call = vi.fn().mockImplementationOnce(() => new Promise(resolve => { done = resolve; }))
    .mockResolvedValueOnce(states).mockRejectedValueOnce(new Error("Access denied")).mockResolvedValueOnce(states);
  const view = build(call);
  const form = view.node.querySelector("form")!, input = view.node.querySelector("input")!;
  form.requestSubmit();
  input.value = "other"; input.dispatchEvent(new Event("input"));
  done(records); await Promise.resolve(); await Promise.resolve();
  expect(view.node.querySelector("table")).toBeNull();
  form.requestSubmit();
  await vi.waitFor(() => expect(view.node.querySelector("[role=status]")?.textContent).toBe("Access denied"));
  expect(view.node.querySelector("table")).toBeNull();
});

test("a later question preserves the user's current slide and keyboard focus", async () => {
  const deck = { version: 1, type: "slides", title: "Shipment review", slides: [
    { heading: "Departures", points: ["Tea"] }, { heading: "Arrivals", points: ["Coffee"] },
  ] };
  const response = { text: "\x60\x60\x60erp-view\n" + JSON.stringify(deck) + "\n\x60\x60\x60" };
  const request = vi.fn().mockResolvedValue(new Response(JSON.stringify(response), { headers: { "content-type": "application/json" } }));
  const env = { currentCompanyId: 7, userId: 41, csrfToken: "fixture", host: document.body,
    rpc: {}, companies: [], action: {params:{company_id:7,sources:[],limit:2,history:{turns:32,bytes:262144,message_bytes:32768}}}, run: (job: () => Promise<void>) => { void job(); },
  } as unknown as ClientActionEnvironment;
  const session = assistantSession(env.rpc, 7, "fixture", { userId: 41, request, history: readHistoryPolicy(env.action.params["history"]) });
  const handle = await workspace(env);
  try {
    await session.send("Show slides");
    const slide = document.querySelector<HTMLElement>(".ai-answer-deck")!;
    slide.focus();
    slide.dispatchEvent(new KeyboardEvent("keydown", {key:"ArrowRight"}));
    expect(slide.textContent).toContain("Slide 2 of 2");
    request.mockResolvedValueOnce(new Response(JSON.stringify({text:"Received."}), {headers:{"content-type":"application/json"}}));
    await session.send("Thank you");
    expect(document.querySelector(".ai-answer-deck")).toBe(slide);
    expect(slide.textContent).toContain("Slide 2 of 2");
    expect(document.activeElement).toBe(slide);
  } finally { handle.dispose(); }
});

/// (partial: a proposed business reference resolves to a permitted standard form)
test("a proposed reference resolves inside the current company only after a click", async () => {
  const call = vi.fn().mockResolvedValue({length: 1, records: [{id: 31, name: "PO00001"}]});
  const open = vi.fn().mockResolvedValue(undefined);
  const env = {rpc:{call}, open, action:{params:{sources:["purchase.order"]}}} as unknown as ClientActionEnvironment;
  const lookup = recordProposal(env, 7);
  expect(lookup("res.users", "admin")).toBeUndefined();
  const openPurchase = lookup("purchase.order", "PO00001")!;
  expect(call).not.toHaveBeenCalled();
  await openPurchase();
  expect(call).toHaveBeenCalledWith("purchase.order", "web_search_read", expect.objectContaining({
    domain:[["company_id","=",7],["name","=","PO00001"]],context:{allowed_company_ids:[7]},limit:2,
  }));
  expect(open).toHaveBeenCalledWith(expect.objectContaining({res_model:"purchase.order",res_id:31,target:"new"}));
  for (const result of [{length:0,records:[]},{length:2,records:[{id:31,name:"PO00001"},{id:32,name:"PO00001"}]}]) {
    call.mockResolvedValueOnce(result);
    await expect(openPurchase()).rejects.toThrow(/reference/);
  }
  expect(open).toHaveBeenCalledTimes(1);
});

test("the workspace shows a usable conversation and shares the draft without opening the dock", async () => {
  const env = { currentCompanyId: 7, userId: 42, companies: [], csrfToken: "fixture", host: document.body, rpc: {},
    action: { params: { company_id: 7, sources: [], limit: 2, history: { turns: 4, bytes: 4096, message_bytes: 2048 } } },
    run: (job: () => Promise<void>) => { void job(); },
  } as unknown as ClientActionEnvironment;
  const chat = assistantSession(env.rpc, 7, "fixture", { userId: 42, request: async () => Response.json({ text: "Draft ready" }),
    history: readHistoryPolicy(env.action.params["history"]) });
  chat.updateDock({ draft: "Prepare a draft" });
  const view = await workspace(env);
  try {
    expect(chat.readDock().mode).toBe("closed");
    const input = document.querySelector("textarea")!;
    expect(input.value).toBe("Prepare a draft");
    input.form!.requestSubmit();
    await vi.waitFor(() => expect(document.querySelector("[role=log]")?.textContent).toContain("Draft ready"));
    expect(document.querySelector<HTMLElement>("[role=log]")?.style.display).not.toBe("none");
    expect(chat.readDock().draft).toBe("");
  } finally { view.dispose(); }
});

test("opening Canvas transfers an expanded dock, its pending response and draft into a single composer", async () => {
  const {assistantChat, assistantCommands} = await import("../src/assistant-chat.js");
  const rpc = {}, host = document.createElement("main");
  document.body.append(host);
  const limits = {turns:8,bytes:16384,message_bytes:2048,saved_sessions:true,workspace_action_id:23};
  let answer!: (value: Response) => void;
  const request = vi.fn<typeof fetch>(async url => url === "/erp/ai/assistant/status"
    ? Response.json({available:true,history:limits})
    : new Promise(resolve => { answer = resolve; }));
  const chat = assistantChat({userId:133,csrfToken:"fixture",context:{allowed_company_ids:[7]},rpc} as Parameters<typeof assistantChat>[0],request);
  document.body.append(chat.node);
  await vi.waitFor(() => expect(chat.node.hidden).toBe(false));
  chat.node.click();
  const dock = document.querySelector<HTMLElement>("#ai-assistant-dock")!;
  const input = dock.querySelector("textarea")!;
  input.value = "Review this order";
  input.form!.requestSubmit();
  input.value = "Follow up on the receipt";
  input.dispatchEvent(new Event("input"));
  const view = await workspace({currentCompanyId:7,userId:133,csrfToken:"fixture",rpc,host,
    action:{params:{company_id:7,sources:[],history:limits}},run:(job:()=>Promise<void>)=>void job()} as unknown as ClientActionEnvironment);
  try {
    expect(dock.isConnected).toBe(false);
    expect(chat.node.hidden).toBe(true);
    const composer = host.querySelector("textarea")!;
    expect(composer.value).toBe("Follow up on the receipt");
    assistantCommands({searchValue:"Assistant"})[0]!.action!();
    expect(document.activeElement).toBe(composer);
    answer(Response.json({text:"Order reviewed",session_id:35}));
    await vi.waitFor(() => expect(host.querySelector('[role="log"]')?.textContent).toContain("Order reviewed"));
    expect(request.mock.calls.filter(([url])=>url === "/erp/ai/assistant/turn")).toHaveLength(1);
  } finally { view.dispose(); }
  expect(chat.node.hidden).toBe(false);
  expect(dock.isConnected).toBe(false);
  chat.node.click();
  expect(input.value).toBe("Follow up on the receipt");
  expect(dock.querySelector('[role="log"]')?.textContent).toContain("Order reviewed");
  chat.dispose();
});
