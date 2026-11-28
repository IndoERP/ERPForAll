/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
import { type ClientActionEnvironment, type ClientActionHandle } from "@erpforall/client";
import { assistantSession, messageCanvases, readHistoryPolicy, type AssistantMessage } from "./assistant-session.js";
import { createCanvasStage } from "./canvas/canvas-stage.js";
import { renderChatReply } from "./presentation.js";
import { renderOperations } from "./operations.js";
import { recordProposal } from "./workspace-records.js";
import { sessionPanel, type SessionPanelHandle } from "./session-panel.js";

export async function workspace(env: ClientActionEnvironment): Promise<ClientActionHandle> {
  const { currentCompanyId: company, csrfToken: token, userId } = env;
  if (!company || company !== env.action.params["company_id"] || !token || !userId) {
    throw new Error("Reopen AI Assistant in your current company.");
  }
  const chat = assistantSession(env.rpc, company, token, {
    userId, history: { ...readHistoryPolicy(env.action.params["history"]), savedSessions: true },
  });
  const shell = document.createElement("div");
  shell.className = "ai-workspace-shell ai-workspace-fullpage";
  const conversation = document.createElement("section");
  conversation.className = "ai-workspace-conversation";
  const toolbar = document.createElement("header");
  const heading = document.createElement("h2");
  heading.textContent = "Conversation";
  toolbar.append(heading);
  function button(label: string, action: () => void) {
    const el = document.createElement("button");
    el.type = "button";
    el.className = "erp-button erp-button-secondary";
    el.textContent = label;
    el.onclick = action;
    return el;
  }
  let panel: SessionPanelHandle | undefined;
  function closeHistory() {
    panel?.dispose();
    panel = undefined;
    thread.hidden = form.hidden = false;
    input.focus();
  }
  const history = button("History", () => {
    if (panel) { closeHistory(); return; }
    panel = sessionPanel({ rpc: env.rpc, onClose: closeHistory, onResume: async (_summary, resume) => {
      await chat.resume(resume.id);
      closeHistory();
      thread.scrollTop = thread.scrollHeight;
    } });
    thread.hidden = form.hidden = true;
    jump.hidden = true;
    conversation.append(panel.node);
    panel.node.querySelector<HTMLInputElement>("input")?.focus();
  });
  const newChat = button("New chat", () => { chat.startNew(); closeHistory(); });
  toolbar.append(history, newChat);
  const thread = document.createElement("div");
  thread.className = "ai-workspace-thread";
  thread.tabIndex = 0;
  thread.setAttribute("role", "log");
  thread.setAttribute("aria-live", "polite");
  thread.setAttribute("aria-label", "Assistant conversation history");
  const nearLatest = () => thread.scrollHeight - thread.scrollTop - thread.clientHeight <= 8;
  const jump = button("Ke pesan terbaru", () => {
    thread.scrollTop = thread.scrollHeight;
    thread.focus({ preventScroll: true });
    jump.hidden = true;
  });
  jump.classList.add("ai-jump-latest");
  jump.hidden = true;
  thread.onscroll = () => { if (nearLatest()) jump.hidden = true; };
  const form = document.createElement("form");
  const input = document.createElement("textarea");
  input.rows = 2;
  input.setAttribute("aria-label", "Message to AI Assistant");
  input.placeholder = "Ask a question or describe a task…";
  const send = document.createElement("button");
  send.textContent = "Send";
  send.type = "submit";
  send.className = "erp-button erp-button-primary";
  const update = () => {
    send.disabled = chat.busy || !input.value.trim();
    newChat.disabled = history.disabled = chat.busy;
  };
  input.oninput = () => { chat.updateDock({ draft: input.value }); update(); };
  input.onkeydown = event => {
    if (event.key === "Enter" && !event.shiftKey && !event.isComposing) {
      event.preventDefault();
      if (!send.disabled) form.requestSubmit();
    }
  };
  form.onsubmit = event => {
    event.preventDefault();
    const prompt = input.value.trim();
    if (!prompt || chat.busy) return;
    chat.updateDock({ draft: "" });
    env.run(() => chat.send(prompt));
    input.focus();
  };
  form.append(input, send);
  conversation.append(toolbar, thread, jump, form);
  const canvas = createCanvasStage(prompt => { showPane("chat"); chat.updateDock({ draft: prompt }); input.focus(); },
    env.openDraft ? (name, definition) => env.run(() => env.openDraft!("ai_base.insight_action", {name,definition})) : undefined,
    (name, model, domain) => env.run(() => env.open({
      type:"ir.actions.act_window", name, res_model:model, domain,
      views:[[false,"list"],[false,"form"]], target:"current",
    }, { sourceCompanyId: company })));
  const panels = document.createElement("div");
  panels.className = "ai-workspace-switch";
  panels.setAttribute("role", "group");
  panels.setAttribute("aria-label","Workspace view");
  const chatTab = button("Chat", () => showPane("chat"));
  const canvasTab = button("Canvas", () => showPane("canvas"));
  panels.append(chatTab, canvasTab);
  function showPane(pane: "chat" | "canvas") {
    shell.dataset["pane"] = pane;
    chatTab.setAttribute("aria-pressed", String(pane === "chat"));
    canvasTab.setAttribute("aria-pressed", String(pane === "canvas"));
  }
  showPane("chat");
  const artifactIds = new WeakMap<object,string>();
  function display(artifact: unknown, select: boolean, background = false) {
    if (!artifact || typeof artifact !== "object") throw new Error("Invalid Canvas output.");
    const known = artifactIds.get(artifact);
    if (!known || (select && !canvas.selectArtifact(known))) {
      const handle = canvas.renderArtifact(artifact, background ? {activate: false} : undefined);
      artifactIds.set(artifact, handle);
    }
    if (select) {
      showPane("canvas");
      canvas.node.tabIndex = -1;
      canvas.node.focus();
    }
  }
  const lookup = recordProposal(env, company);
  const rendered = new Map<AssistantMessage, HTMLElement>();
  let previousConversation = chat.conversationId;
  function render() {
    if (previousConversation !== chat.conversationId) { canvas.clear(); previousConversation = chat.conversationId; }
    const atBottom = nearLatest();
    let newReply = false;
    const messages = new Set(chat.messages);
    for (const [item, node] of rendered) {
      if (!messages.has(item)) { node.remove(); rendered.delete(item); }
    }
    let previous: HTMLElement | undefined;
    for (const item of messages) {
      const existing = rendered.get(item);
      if (existing) { previous = existing; continue; }
      const part = document.createElement("article");
      part.className = "ai-workspace-message";
      part.dataset["role"] = item.role;
      const author = document.createElement("strong");
      author.textContent = item.role === "user" ? "You" : "AI Assistant";
      const body = document.createElement("div");
      body.className = item.state === "error" ? "ai-workspace-error" : "ai-workspace-text";
      part.append(author, body);
      if (item.role === "assistant" && item.state === "ready") {
        renderChatReply(body, item.text, env, lookup);
        newReply = true;
        const artifacts = messageCanvases(item);
        const kindOf = (artifact: unknown) => artifact && typeof artifact === "object" && "canvas_type" in artifact
          ? artifact.canvas_type : undefined;
        const hasChart = artifacts.some(artifact => kindOf(artifact) === "chart");
        for (const artifact of artifacts) {
          try {
            display(artifact, false, hasChart && kindOf(artifact) === "table");
            const title = artifact && typeof artifact === "object" && "title" in artifact && typeof artifact.title === "string"
              ? artifact.title : "Canvas";
            const link = button("Open " + title, () => display(artifact, true));
            link.classList.add("ai-artifact-link");
            part.append(link);
          } catch (error) {
            const notice = document.createElement("p");
            notice.setAttribute("role","status");
            notice.textContent = error instanceof Error ? error.message : "This Canvas output could not be displayed.";
            part.append(notice);
          }
        }
        renderOperations(part, item.operations, chat, id => env.run(() => env.open(id)));
      } else { body.textContent = item.text; }
      thread.insertBefore(part, previous ? previous.nextSibling : thread.firstChild);
      previous = part;
      rendered.set(item, part);
    }
    if (atBottom) thread.scrollTop = thread.scrollHeight;
    else if (newReply && !thread.hidden) jump.hidden = false;
    update();
  }
  const resources = document.createElement("nav");
  resources.setAttribute("aria-label", "Assistant tools");
  if (env.openMenuAction) {
    const addLink = (label: string, xmlId: string) => resources.append(button(label, () =>
      env.run(() => env.openMenuAction!(xmlId))));
    addLink("Saved Insights", "ai_base.insight_action");
    if (env.action.params["implementation_access"] === true) addLink("Implementation projects", "ai_base.implementation_projects");
  }
  resources.prepend(panels);
  shell.append(resources, conversation, canvas.node);
  env.host.append(shell);
  const unsubscribe = chat.subscribe(render);
  const unsubscribeDraft = chat.subscribeDraft(value => { input.value = value; update(); });
  render();
  const takeFocus = chat.readDock().mode === "expanded";
  const focusChat = () => { showPane("chat"); if (panel) closeHistory(); input.focus(); };
  const releaseWorkspace = chat.attachWorkspace(focusChat, artifact => display(artifact, true));
  if (takeFocus) {
    if (input.isConnected && shell.dataset["pane"] === "chat") focusChat();
    else requestAnimationFrame(() => { if (input.isConnected && shell.dataset["pane"] === "chat") focusChat(); });
  }
  return { dispose() { unsubscribe(); unsubscribeDraft(); releaseWorkspace(); panel?.dispose(); canvas.dispose(); shell.remove(); } };
}
