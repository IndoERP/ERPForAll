/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
import { afterEach, expect, test, vi } from "vitest";
import { assistantChat, assistantCommands } from "../src/assistant-chat.js";

const environment = { csrfToken: "session-test", context: { allowed_company_ids: [7, 12] } };
function json(body: unknown, status = 200) {
  return new Response(JSON.stringify(body), { status, headers: { "content-type": "application/json" } });
}
afterEach(() => document.body.replaceChildren());

test.each([401, 403, 404])("unavailable status %s leaves the launcher hidden", async status => {
  const request = vi.fn<typeof fetch>().mockResolvedValue(json({}, status));
  const chat = assistantChat(environment, request);
  await vi.waitFor(() => expect(request).toHaveBeenCalledOnce());
  await Promise.resolve();
  expect(chat.node.hidden).toBe(true);
  chat.dispose();
});

test("one company-scoped request, plain text reply, minimize and keyboard close", async () => {
  let finish!: (reply: Response) => void;
  const request = vi.fn<typeof fetch>().mockResolvedValueOnce(json({ available: true, history: { turns: 4, bytes: 4096, message_bytes: 4096 } }))
    .mockImplementationOnce(() => new Promise(resolve => { finish = resolve; }));
  const chat = assistantChat(environment, request);
  document.body.append(chat.node);
  await vi.waitFor(() => expect(chat.node.hidden).toBe(false));
  const command = assistantCommands({ searchValue: "Assistant" })[0]!;
  command.action!();
  command.action!();
  expect(document.querySelectorAll(".ai-chat-dock")).toHaveLength(1);
  const dock = document.getElementById(chat.node.getAttribute("aria-controls")!)!;
  const input = dock.querySelector("textarea")!;
  const form = dock.querySelector("form")!;
  expect(document.activeElement).toBe(input);
  input.value = "  Summarize receipts  ";
  form.dispatchEvent(new Event("submit", { cancelable: true }));
  input.value = "Another question";
  form.dispatchEvent(new Event("submit", { cancelable: true }));
  expect(request).toHaveBeenCalledTimes(2);
  expect(request.mock.calls[1]).toEqual(["/erp/ai/assistant/turn", expect.objectContaining({
    method: "POST", credentials: "same-origin", headers: { "Content-Type": "application/json", "X-CSRF-Token": "session-test" },
    body: JSON.stringify({ message: "Summarize receipts", company_id: 7, history: [] }),
  })]);
  finish(json({ text: "<img src=x> is text" }));
  await vi.waitFor(() => expect(dock.querySelector('[role="log"]')?.textContent).toContain("<img src=x> is text"));
  expect(dock.querySelector("img")).toBeNull();
  dock.querySelector<HTMLButtonElement>('[aria-label="Minimize chat"]')!.click();
  expect(form.hidden).toBe(true);
  chat.node.click();
  expect(form.hidden).toBe(false);
  input.dispatchEvent(new KeyboardEvent("keydown", { key: "Escape", bubbles: true }));
  expect(dock.hidden).toBe(true);
  expect(document.activeElement).toBe(chat.node);
  chat.dispose();
  expect(dock.isConnected).toBe(false);
});

test("an unconfigured company sees the server's message in the conversation", async () => {
  const request = vi.fn<typeof fetch>().mockResolvedValueOnce(json({ available: true, history: { turns: 4, bytes: 4096, message_bytes: 4096 } }))
    .mockResolvedValueOnce(json({ message: "erp.ai.provider_unconfigured: configure a provider for this company." }, 422));
  const chat = assistantChat(environment, request);
  await vi.waitFor(() => expect(chat.node.hidden).toBe(false));
  chat.node.click();
  const form = document.querySelector("form")!;
  form.querySelector("textarea")!.value = "Question";
  form.dispatchEvent(new Event("submit", { cancelable: true }));
  await vi.waitFor(() => expect(document.querySelector(".ai-chat-error")?.textContent).toBe("Configure a provider for this company."));
  expect(request).toHaveBeenCalledTimes(2);
  chat.dispose();
});

test("dock keeps its draft, scroll and pending turn when the shell header remounts", async () => {
  const env = { ...environment, userId: 42 };
  let answer!: (value: Response) => void;
  const request = vi.fn<typeof fetch>().mockImplementation(async url => {
    if (url === "/erp/ai/assistant/status") return json({ available: true, history: { turns: 4, bytes: 4096, message_bytes: 4096 } });
    return new Promise(resolve => { answer = resolve; });
  });
  const first = assistantChat(env, request);
  document.body.append(first.node);
  await vi.waitFor(() => expect(first.node.hidden).toBe(false));
  first.node.click();
  const input = document.querySelector("textarea")!;
  input.value = "Existing order?";
  input.form!.dispatchEvent(new Event("submit", { cancelable: true }));
  input.value = "Second question";
  input.dispatchEvent(new Event("input"));
  first.dispose();
  const second = assistantChat(env, request);
  document.body.append(second.node);
  await vi.waitFor(() => expect(document.querySelector("textarea")?.value).toBe("Second question"));
  answer(json({ text: "The receipt is ready." }));
  await vi.waitFor(() => expect(document.querySelector('[role="log"]')?.textContent).toContain("The receipt is ready."));
  expect(request.mock.calls.filter(([url]) => url === "/erp/ai/assistant/turn")).toHaveLength(1);
  second.dispose();
});

test("the dock restores a workspace draft proposal as readable fields", async () => {
  const config = {turns:4,bytes:4096,message_bytes:4096};
  const key = "erp.session.ai.91.7.messages";
  const text = ["```erp-view",JSON.stringify({version:1,type:"draft",title:"RFQ proposal",form:"rfq",fields:{origin:"DEMO-A"}}),"```"].join("\n");
  sessionStorage.setItem(key, JSON.stringify([{role:"user",state:"ready",text:"Prepare an RFQ"},{role:"assistant",state:"ready",text}]));
  const chat = assistantChat({...environment,userId:91}, vi.fn(async () => json({available:true,history:config})));
  try {
    await vi.waitFor(() => expect(chat.node.hidden).toBe(false));
    chat.node.click();
    const log = document.querySelector(".ai-chat-thread")!;
    expect(log.querySelector("dd")?.textContent).toBe("DEMO-A");
    expect(log.textContent).not.toContain("erp-view");
    expect(log.querySelector("button")).toBeNull();
  } finally { chat.dispose();sessionStorage.removeItem(key); }
});

test("the dock adds only the selected record reference to the editable draft", async () => {
  const context = {active_model:"sale.order",active_id:18,private_payload:"not for the assistant"};
  const request = vi.fn<typeof fetch>().mockResolvedValueOnce(json({available:true,history:{turns:4,bytes:4096,message_bytes:4096}}));
  const chat = assistantChat({...environment,assistantContext:context},request);
  document.body.append(chat.node);
  await vi.waitFor(() => expect(chat.node.hidden).toBe(false));
  chat.node.click();
  const include = document.querySelector<HTMLButtonElement>('[aria-label="Include current record"]')!;
  include.click();
  expect(document.querySelector("textarea")?.value).toBe("sale.order #18 ");
  expect(request).toHaveBeenCalledTimes(1);
  chat.dispose();
});
