/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
import { icon, type Command, type IconName, type SystrayEnvironment } from "@erpforall/client";

import { renderChatReply } from "./presentation.js";
import { assistantSession, messageCanvases, readHistoryPolicy, type AssistantSession, type AssistantMessage } from "./assistant-session.js";
import { renderOperations } from "./operations.js";
import { sessionPanel, type SessionPanelHandle } from "./session-panel.js";

function control(label: string, glyph: IconName, style: string): HTMLButtonElement {
  const button = document.createElement("button");
  button.type = "button";
  button.className = style;
  button.setAttribute("aria-label", label);
  button.title = label;
  button.append(icon(glyph));
  return button;
}

function object(value: unknown): value is Record<string, unknown> {
  return value !== null && typeof value === "object" && !Array.isArray(value);
}

/** The command palette and toolbar open the same active company window. */
export function assistantCommands({ searchValue }: { searchValue: string }): Command[] {
  const button = document.querySelector<HTMLButtonElement>(".ai-chat-trigger:not([hidden]), .ai-chat-trigger[data-workspace-active=true]");
  const name = "Open AI Assistant";
  if (!button || !name.toLowerCase().includes(searchValue.trim().toLowerCase())) return [];
  return [{ id: "ai_base.open_assistant", name, action() {
    if (!button.isConnected) return;
    if (button.dataset["workspaceActive"] === "true") { button.click(); return; }
    if (button.hidden) return;
    if (button.getAttribute("aria-expanded") === "true") {
      document.getElementById("ai-assistant-dock")?.querySelector("textarea")?.focus();
    } else {
      button.click();
    }
  } }];
}

/** Authorized dock for the active company, sharing turns with the full-page workspace. */
export function assistantChat(env: SystrayEnvironment, request: typeof fetch = (url, options) => globalThis.fetch(url, options)) {
  const companies = env.context?.["allowed_company_ids"];
  const company: unknown = Array.isArray(companies) ? companies[0] : undefined;
  const token = env.csrfToken;
  const trigger = control("AI Assistant", "message-square", "icon-button ai-chat-trigger");
  trigger.hidden = true;
  trigger.setAttribute("aria-expanded", "false");
  const lifetime = new AbortController();
  let session: AssistantSession | undefined;
  let workspaceId: number | undefined;
  let unsubscribe: (() => void) | undefined;
  let unsubscribeDraft: (() => void) | undefined;
  const dock = document.createElement("section");
  dock.className = "ai-chat-dock";
  dock.id = "ai-assistant-dock";
  trigger.setAttribute("aria-controls", dock.id);
  dock.setAttribute("role", "region");
  dock.setAttribute("aria-label", "AI Assistant");
  dock.hidden = true;
  const header = document.createElement("header");
  const title = document.createElement("h2");
  title.textContent = "AI Assistant";
  const sessionsButton = control("Sessions", "book-open", "icon-button ai-chat-sessions-trigger");
  sessionsButton.hidden = true;
  sessionsButton.setAttribute("aria-expanded", "false");
  sessionsButton.setAttribute("aria-controls", "ai-session-panel");
  const fold = control("Minimize chat", "chevron-down", "icon-button");
  const close = control("Close chat", "x", "icon-button");
  const studio = control("Open Canvas Studio", "panels-top-left", "icon-button");
  studio.hidden = true;
  studio.onclick = () => {
    if (workspaceId && env.navigate && env.run) env.run(() => env.navigate!({action:String(workspaceId)}));
  };
  header.append(icon("message-square"), title, sessionsButton, studio, fold, close);

  const thread = document.createElement("div");
  thread.className = "ai-chat-thread";
  thread.setAttribute("role", "log");
  thread.setAttribute("aria-label", "Messages");
  thread.setAttribute("aria-live", "polite");
  const form = document.createElement("form");
  const input = document.createElement("textarea");
  input.rows = 2;
  input.placeholder = "Ask a question…";
  input.setAttribute("aria-label", "Message to AI Assistant");
  const hint = document.createElement("p");
  hint.className = "ai-chat-hint";
  hint.textContent = "Conversations are scoped to your account and active company.";
  const send = control("Send message", "send", "erp-button erp-button-primary");
  send.type = "submit";
  send.disabled = true;
  const compose = document.createElement("div");
  compose.className = "ai-chat-compose";
  compose.append(input, send);
  const include = document.createElement("button");
  include.type = "button";
  include.className = "erp-button erp-button-secondary";
  include.setAttribute("aria-label", "Include current record");
  function currentRecord(): string | undefined {
    const model = env.assistantContext?.["active_model"];
    const id = env.assistantContext?.["active_id"];
    if (typeof model === "string" && /^[a-z][a-z0-9_.]*$/.test(model) && typeof id === "number" && Number.isSafeInteger(id) && id > 0) return model + " #" + id;
    return undefined;
  }
  function updateRecord() {
    const reference = currentRecord();
    include.hidden = !reference;
    include.textContent = reference ? "Include " + reference : "";
  }
  include.onclick = () => {
    const reference = currentRecord();
    if (reference) session?.updateDock({ draft: [input.value.trimEnd(), reference].filter(Boolean).join(" ") + " " });
    updateRecord();
    input.focus();
  };
  form.addEventListener("focusin", updateRecord);
  form.append(hint, include, compose);
  dock.append(header, thread, form);

  // The session panel replaces the thread and form while it is open; the fold hides both surfaces.
  let panel: SessionPanelHandle | undefined;
  let panelOpen = false;
  let minimized = false;
  const renderViews = () => {
    thread.hidden = form.hidden = panelOpen || minimized;
    if (panel) panel.node.hidden = !panelOpen || minimized;
    sessionsButton.setAttribute("aria-expanded", String(panelOpen && !minimized));
  };

  const ready = async () => {
    if (typeof company !== "number" || !Number.isSafeInteger(company) || company <= 0 || !token) return;
    const response = await request("/erp/ai/assistant/status", {
      credentials: "same-origin", redirect: "error", cache: "no-store", signal: lifetime.signal,
    });
    if ([401, 403, 404].includes(response.status)) return;
    if (!response.ok) throw new Error("Unable to check whether AI Assistant is available.");
    if (!response.headers.get("content-type")?.includes("application/json")) return;
    const data: unknown = await response.json();
    if (!object(data) || typeof data["available"] !== "boolean") throw new Error("Invalid assistant availability response.");
    if (lifetime.signal.aborted || !data["available"]) return;
    const action = object(data["history"]) ? data["history"]["workspace_action_id"] : undefined;
    if (typeof action === "number" && Number.isSafeInteger(action) && action > 0) workspaceId = action;
    session = assistantSession(env.rpc ?? request, company, token, { ...(env.userId !== undefined && { userId: env.userId }), request, history: readHistoryPolicy(data["history"]) });
    trigger.hidden = false;
    sessionsButton.hidden = !env.rpc;
    studio.hidden = !(workspaceId && env.run && env.navigate);
    unsubscribeDraft = session.subscribeDraft(text => {
      if (input.value !== text) input.value = text;
      hint.textContent = session?.persistenceWarning || "Conversations are scoped to your account and active company.";
      update();
    });
    unsubscribe = session.subscribe(renderMessages);
    renderMessages();
    const saved = session.readDock();
    if (!session.workspaceActive && saved.mode !== "closed") {
      expand(false);
      if (saved.mode === "minimized") minimize(false);
      thread.scrollTop = saved.scroll;
    }
  };
  void ready().catch(() => {
    if (!lifetime.signal.aborted) {
      env.messages?.notifications.add("AI Assistant availability could not be checked.", { level: "warning" });
    }
  });

  const expand = (focus = true) => {
    if (session?.focusWorkspaceChat()) return;
    updateRecord();
    if (!dock.isConnected) document.body.append(dock);
    dock.hidden = false;
    minimized = false;
    fold.setAttribute("aria-label", "Minimize chat");
    fold.title = "Minimize chat";
    fold.setAttribute("aria-expanded", "true");
    trigger.setAttribute("aria-expanded", "true");
    renderViews();
    if (focus) {
      if (panelOpen) panel?.node.querySelector<HTMLInputElement>(".ai-session-search")?.focus();
      else input.focus();
      session?.updateDock({ mode: "expanded" });
    }
  };
  const minimize = (focus = true) => {
    minimized = true;
    fold.setAttribute("aria-label", "Expand chat");
    fold.title = "Expand chat";
    fold.setAttribute("aria-expanded", "false");
    trigger.setAttribute("aria-expanded", "false");
    renderViews();
    if (focus) { fold.focus(); session?.updateDock({ mode: "minimized" }); }
  };
  const closePanel = () => {
    if (!panelOpen) return;
    panelOpen = false;
    renderViews();
    input.focus();
  };
  const openPanel = () => {
    if (!env.rpc || !session) return;
    if (!panel) {
      panel = sessionPanel({
        rpc: env.rpc,
        context: env.assistantContext,
        onResume: async (_summary, resume) => { await session!.resume(resume.id); closePanel(); },
        onClose: () => closePanel(),
      });
    }
    if (!panel.node.isConnected) dock.insertBefore(panel.node, form);
    panelOpen = true;
    minimized = false;
    renderViews();
    fold.setAttribute("aria-label", "Minimize chat");
    fold.title = "Minimize chat";
    fold.setAttribute("aria-expanded", "true");
    trigger.setAttribute("aria-expanded", "true");
    panel.node.querySelector<HTMLInputElement>(".ai-session-search")?.focus();
    session.updateDock({ mode: "expanded" });
  };
  sessionsButton.onclick = () => { if (panelOpen) closePanel(); else openPanel(); };
  trigger.onclick = () => { if (dock.hidden || form.hidden) expand(); else minimize(); };
  fold.onclick = () => { if (form.hidden && !panelOpen) expand(); else if (panelOpen && minimized) expand(); else minimize(); };
  close.onclick = () => {
    panelOpen = false;
    panel?.dispose();
    panel = undefined;
    session?.updateDock({ mode: "closed" });
    dock.hidden = true;
    trigger.setAttribute("aria-expanded", "false");
    trigger.focus();
  };
  dock.addEventListener("keydown", event => {
    if (event.key === "Escape") { event.preventDefault(); event.stopPropagation(); close.click(); }
  });
  const update = () => { send.disabled = !session || session.busy || !input.value.trim(); };
  input.addEventListener("input", () => { session?.updateDock({ draft: input.value }); update(); });
  thread.addEventListener("scroll", () => {
    if (!dock.hidden && !thread.hidden) session?.updateDock({ scroll: Math.max(0, thread.scrollTop) });
  });
  input.addEventListener("keydown", event => {
    if (event.key === "Enter" && !event.shiftKey && !event.isComposing) {
      event.preventDefault();
      if (!send.disabled) form.requestSubmit();
    }
  });
  const emptyPromptContainer = document.createElement("div");
  emptyPromptContainer.className = "ai-chat-empty-intro";
  const introP = document.createElement("p");
  introP.textContent = "Tanyakan analisis bisnis atau visualisasi data:";
  const chipList = document.createElement("div");
  chipList.className = "ai-chat-chip-list";
  const chips = [
    { label: "5 Customer Terloyal", prompt: "Siapa 5 customer terloyal tahun ini? Tampilkan grafiknya di canvas." },
    { label: "Tren Omzet Bulanan", prompt: "Tampilkan omzet penjualan bulanan tahun ini dalam bentuk chart." },
    { label: "Pivot Omzet & Status", prompt: "Tampilkan pivot table omzet penjualan per customer dan status pesanan." },
    { label: "Ringkasan Piutang Jatuh Tempo", prompt: "Tampilkan tabel ringkasan faktur piutang yang jatuh tempo." },
  ];
  chips.forEach(c => {
    const chipBtn = document.createElement("button");
    chipBtn.type = "button";
    chipBtn.className = "ai-chat-chip";
    chipBtn.textContent = c.label;
    chipBtn.addEventListener("click", () => {
      input.value = c.prompt;
      update();
      form.requestSubmit();
    });
    chipList.append(chipBtn);
  });
  emptyPromptContainer.append(introP, chipList);

  const rendered = new Map<AssistantMessage, HTMLElement>();
  function renderMessages() {
    if (!session) return;
    trigger.hidden = session.workspaceActive;
    trigger.dataset["workspaceActive"] = String(session.workspaceActive);
    if (session.workspaceActive) {
      panel?.dispose(); panel = undefined; panelOpen = false;
      dock.hidden = true;
      dock.remove();
      trigger.setAttribute("aria-expanded","false");
      return;
    }
    const atBottom = thread.scrollTop + thread.clientHeight >= thread.scrollHeight - 1;
    const visible = new Set(session.messages);
    for (const [item, part] of rendered) {
      if (!visible.has(item)) { part.remove(); rendered.delete(item); }
    }
    if (visible.size === 0) {
      if (!emptyPromptContainer.isConnected) thread.append(emptyPromptContainer);
    } else {
      emptyPromptContainer.remove();
    }
    let previous: HTMLElement | undefined;
    for (const item of visible) {
      const existing = rendered.get(item);
      if (existing) { previous = existing; continue; }
      const part = document.createElement("div");
      const author = document.createElement("strong");
      author.textContent = item.role === "user" ? "You" : "AI Assistant";
      part.className = "ai-chat-message " + (item.role === "user" ? "ai-chat-question" : "ai-chat-answer");
      const content = document.createElement("div");
      content.className = "ai-chat-content";
      content.classList.toggle("ai-chat-error", item.state === "error");
      if (item.role === "assistant" && item.state === "ready") {
        renderChatReply(content, item.text);
        renderOperations(content, item.operations, session, env.openRecord && env.run
          ? action => env.run!(() => env.openRecord!(action.res_model, action.res_id))
          : undefined);
        for (const c of messageCanvases(item)) {
          if (!object(c)) continue;
          const canvasCard = document.createElement("div");
          canvasCard.className = "ai-chat-canvas-card";
          const badge = document.createElement("div");
          badge.className = "ai-chat-canvas-badge";
          const iconEl = icon("panels-top-left");
          const titleSpan = document.createElement("strong");
          titleSpan.textContent = String(c["title"] || "Visualisasi Canvas");
          badge.append(iconEl, titleSpan);

          const descP = document.createElement("p");
          descP.className = "ai-chat-canvas-desc";
          descP.textContent = String(c["description"] || `Tipe: ${c["canvas_type"] || "chart"}`);

          const viewBtn = document.createElement("button");
          viewBtn.type = "button";
          viewBtn.className = "erp-button erp-button-secondary ai-chat-canvas-view-btn";
          viewBtn.textContent = "Lihat di Canvas Studio";
          viewBtn.addEventListener("click", () => {
            session?.showCanvas(c);
            if (!document.querySelector(".ai-workspace-canvas-stage") && workspaceId && env.navigate && env.run) {
              env.run(() => env.navigate!({action:String(workspaceId)}));
            }
          });
          canvasCard.append(badge, descP);
          if (workspaceId && env.navigate) canvasCard.append(viewBtn);
          content.append(canvasCard);
        }
      } else {
        content.textContent = item.text;
      }
      part.append(author, content);
      thread.insertBefore(part, previous ? previous.nextSibling : thread.firstChild);
      previous = part;
      rendered.set(item, part);
    }
    hint.textContent = session.persistenceWarning || "Conversations are scoped to your account and active company.";
    if (atBottom) thread.scrollTop = thread.scrollHeight;
    update();
  }
  form.addEventListener("submit", event => {
    event.preventDefault();
    const prompt = input.value.trim();
    if (!session || session.busy || !prompt) return;
    input.value = "";
    session.updateDock({ draft: "" });
    void session.send(prompt);
    thread.scrollTop = thread.scrollHeight;
  });
  return { node: trigger, dispose() { lifetime.abort(); panel?.dispose(); unsubscribe?.(); unsubscribeDraft?.(); dock.remove(); trigger.remove(); } };
}
