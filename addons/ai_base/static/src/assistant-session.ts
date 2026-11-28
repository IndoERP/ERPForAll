/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
import { conversationHistory, locateSubmission, type RpcLike } from "./session-contract.js";
import { operations, type Operation } from "./operations.js";

export interface AssistantMessage {
  readonly operations?: readonly Operation[];
  readonly role: "user" | "assistant";
  readonly text: string;
  readonly state: "ready" | "pending" | "error";
  readonly canvas?: unknown;
  readonly canvases?: readonly unknown[];
}

export function messageCanvases(message: AssistantMessage): readonly unknown[] {
  return message.canvases ?? (message.canvas ? [message.canvas] : []);
}

export interface AssistantSession {
  readonly workspaceActive: boolean;
  attachWorkspace(focusChat: () => void, showArtifact: (artifact: unknown) => void): () => void;
  focusWorkspaceChat(): boolean;
  showCanvas(artifact: unknown): void;
  readonly messages: readonly AssistantMessage[];
  readonly busy: boolean;
  readonly persistenceWarning: string;
  readonly conversationId: number | undefined;
  resume(id: number): Promise<void>;
  startNew(): void;
  resumeOperation(approval: number): Promise<Operation>;
  send(prompt: string, providerText?: string): Promise<void>;
  subscribe(changed: () => void): () => void;
  subscribeDraft(changed: (text: string) => void): () => void;
  readDock(): DockState;
  updateDock(patch: Partial<DockState>): void;
}

interface DockState { mode: "closed" | "expanded" | "minimized"; draft: string; scroll: number }
interface HistoryPolicy { savedSessions?: boolean; turns: number; bytes: number; messageBytes: number }
interface Options { userId?: number; request?: typeof fetch; history: HistoryPolicy }
const encoder = new TextEncoder();

export function readHistoryPolicy(raw: unknown): HistoryPolicy {
  if (!record(raw)) throw new Error("Could not load the assistant history limits.");
  function bound(value: unknown, min: number, max: number): number {
    if (typeof value !== "number" || !Number.isSafeInteger(value) || value < min || value > max) {
      throw new Error("Invalid assistant history limit.");
    }
    return value;
  }
  return {
    ...(raw["saved_sessions"] === true && { savedSessions: true }),
    turns: bound(raw["turns"], 1, 200),
    bytes: bound(raw["bytes"], 4096, 2097152),
    messageBytes: bound(raw["message_bytes"], 1, 32768),
  };
}
const sessions = new WeakMap<object, Map<string, AssistantSession>>();
const emptyDock = (): DockState => ({ mode: "closed", draft: "", scroll: 0 });
function record(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}
function message(value: unknown): value is AssistantMessage {
  return record(value) && (value["role"] === "user" || value["role"] === "assistant") && typeof value["text"] === "string"
    && ["ready", "pending", "error"].includes(String(value["state"]));
}
function dockState(value: unknown): value is DockState {
  return record(value) && ["closed", "expanded", "minimized"].includes(String(value["mode"])) && typeof value["draft"] === "string"
    && typeof value["scroll"] === "number" && Number.isFinite(value["scroll"]) && value["scroll"] >= 0;
}

/** One live HTTP turn per authenticated shell and company, shared by its dock and workspace. */
export function assistantSession(rpcKey: object, company: number, token: string, options: Options): AssistantSession {
  const cache = sessions.get(rpcKey) ?? new Map<string, AssistantSession>();
  sessions.set(rpcKey, cache);
  const id = (options.userId ?? "") + "." + company;
  const previous = cache.get(id);
  if (previous) return previous;
  const prefix = Number.isSafeInteger(options.userId) && Number(options.userId) > 0 ? "erp.session.ai." + id + "." : null;
  const policy = options.history;
  let warning = "";
  let ended = false;
  function read(suffix: string): unknown {
    if (!prefix) return null;
    try {
      const value = window.sessionStorage.getItem(prefix + suffix);
      if (value === null) return null;
      // A prior release or a lowered setting may have left a larger tab value.
      if (value.length > 2097152) {
        warning = "The previous assistant history exceeds the browser storage limit.";
        return null;
      }
      return JSON.parse(value);
    } catch {
      warning = "This tab could not restore its previous chat.";
      return null;
    }
  }
  function save(suffix: string, value: unknown): void {
    if (!prefix || ended) return;
    try { window.sessionStorage.setItem(prefix + suffix, JSON.stringify(value)); }
    catch { warning = "This browser cannot save the chat for a page reload."; }
  }

  const savedId = read("conversation");
  let conversationId = typeof savedId === "number" && Number.isSafeInteger(savedId) && savedId > 0 ? savedId : undefined;
  const storedSubmission = read("submission");
  let submission = typeof storedSubmission === "string" && /^[a-f0-9-]{36}$/.test(storedSubmission) ? storedSubmission : undefined;
  let retryText: string | undefined;
  let loading = false;
  const rpc = "call" in rpcKey && typeof rpcKey.call === "function" ? rpcKey as RpcLike : undefined;
  const rawMessages = conversationId || policy.savedSessions ? null : read("messages");
  let entries: AssistantMessage[] = [];
  if (Array.isArray(rawMessages) && rawMessages.every(message)) {
    entries = rawMessages.map(item => item.state === "pending"
      ? { role: "assistant", state: "error", text: "This tab reloaded before the reply arrived. The provider may have processed the question." } : item);
  } else if (rawMessages !== null) { warning = "This tab could not restore its previous chat."; }
  const rawDock = read("dock");
  let dock = dockState(rawDock) ? rawDock : emptyDock();
  const subscribers = new Set<() => void>();
  const draftSubscribers = new Set<(text: string) => void>();
  let pending: AbortController | undefined;
  let requestedCanvas: unknown;
  // A staged action can mount before the shell disposes the previous workspace.
  const workspaces: {focusChat: () => void; showArtifact: (artifact:unknown) => void}[] = [];
  let trimmed = read("truncated") === true;
  let currentExchangeTooLarge = false;
  function retainRecent(): void {
    currentExchangeTooLarge = false;
    let start = entries.length;
    let bytes = 2; // brackets around the serialized array
    while (start >= 2 && entries.length - start < policy.turns * 2) {
      const pair = entries.slice(start - 2, start);
      const encoded = JSON.stringify(pair);
      const pairBytes = encoder.encode(encoded).length - 2 + (start < entries.length ? 1 : 0);
      if (bytes + pairBytes > policy.bytes) break;
      bytes += pairBytes;
      start -= 2;
    }
    if (start) {
      trimmed = true;
      save("truncated", true);
      if (start === entries.length) {
        // Keep the current question and reply on screen even if that pair alone exceeds
        // the persistence budget. Replacing the pending reply must still work.
        entries = entries.slice(-2);
        save("messages", []);
        currentExchangeTooLarge = true;
        return;
      }
      entries = entries.slice(start);
    }
    save("messages", conversationId || policy.savedSessions ? [] : entries);
  }
  retainRecent();
  const publish = () => {
    retainRecent();
    for (const notify of subscribers) notify();
  };
  const session: AssistantSession = {
    get messages() { return entries; },
    get workspaceActive() { return workspaces.length > 0; },
    attachWorkspace(focusChat, showArtifact) {
      const view = {focusChat, showArtifact};
      workspaces.push(view);
      session.updateDock({mode:"closed"});
      for (const notify of subscribers) notify();
      if (requestedCanvas !== undefined) {
        showArtifact(requestedCanvas);
        requestedCanvas = undefined;
      }
      return () => {
        const index = workspaces.indexOf(view);
        if (index < 0) return;
        workspaces.splice(index, 1);
        for (const notify of subscribers) notify();
      };
    },
    focusWorkspaceChat() {
      const workspace = workspaces.at(-1);
      if (!workspace) return false;
      workspace.focusChat();
      return true;
    },
    showCanvas(artifact) {
      if (ended) return;
      const workspace = workspaces.at(-1);
      if (workspace) workspace.showArtifact(artifact);
      else requestedCanvas = artifact;
    },
    get busy() { return Boolean(pending) || loading; },
    get conversationId() { return conversationId; },
    startNew() {
      if (pending || loading || ended) return;
      submission = retryText = undefined; save("submission", null);
      requestedCanvas = undefined; conversationId = undefined; save("conversation", null);
      trimmed = false; save("truncated", false); warning = ""; entries = [];
      session.updateDock({draft:""}); publish();
    },
    async resume(id) {
      if (pending || loading || ended) throw new Error("Wait for the current request to finish.");
      if (!rpc || !Number.isSafeInteger(id) || id <= 0) throw new Error("Saved conversation unavailable.");
      loading = true; publish();
      try {
        const rows = await conversationHistory(rpc,id,company);
        const restored = rows.map((row): AssistantMessage => {
          if (!record(row) || (row["role"] !== "user" && row["role"] !== "assistant") || typeof row["text"] !== "string")
            throw new Error("Invalid saved message.");
          return {role:row["role"],text:row["text"],state:"ready", ...(row["operations"] !== undefined && {operations:operations(row["operations"])})};
        });
        if (!ended) {
          const switching = conversationId !== id;
          entries = restored; conversationId = id; save("conversation",id); warning = "";
          trimmed = false; save("truncated",false); if (switching) session.updateDock({draft:""});
        }
      } finally { loading = false; if (!ended) publish(); }
    },
    async resumeOperation(approval) {
      if (ended) throw new Error("This session has ended.");
      const active = conversationId;
      const response = await (options.request ?? globalThis.fetch)("/erp/ai/assistant/resume", {
        method:"POST", credentials:"same-origin", redirect:"error", cache:"no-store",
        headers:{"Content-Type":"application/json","X-CSRF-Token":token},
        body:JSON.stringify({company_id:company,approval_request_id:approval}),
      });
      const value:unknown = response.headers.get("content-type")?.includes("application/json") ? await response.json() : null;
      if (!response.ok) throw new Error(record(value) && typeof value["message"] === "string" ? value["message"] : "Could not check approval.");
      const updated = { ...operations([value])[0]!, approval_request_id: approval };
      if (!ended && active === conversationId) {
        entries = entries.map(message => message.operations?.some(item => item.approval_request_id === approval)
          ? {...message, operations:message.operations.map(item => item.approval_request_id === approval ? updated : item)}
          : message);
        publish();
      }
      return updated;
    },
    get persistenceWarning() {
      if (warning) return warning;
      if (currentExchangeTooLarge) return "This exchange is too large to restore after a reload.";
      return trimmed ? "This tab keeps only the most recent exchanges." : "";
    },
    subscribe(changed) { subscribers.add(changed); return () => { subscribers.delete(changed); }; },
    subscribeDraft(changed) {
      draftSubscribers.add(changed);
      changed(dock.draft);
      return () => { draftSubscribers.delete(changed); };
    },
    readDock() { return { ...dock }; },
    updateDock(patch) {
      const draftChanged = patch.draft !== undefined && patch.draft !== dock.draft;
      dock = { ...dock, ...patch };
      if (dock.draft.length > policy.messageBytes || encoder.encode(dock.draft).length > policy.messageBytes) {
        save("dock", { ...dock, draft: "" });
        warning = "Shorten this draft to keep it after a page reload.";
      } else {
        save("dock", dock);
        if (warning === "Shorten this draft to keep it after a page reload.") warning = "";
      }
      if (draftChanged) for (const notify of draftSubscribers) notify(dock.draft);
    },
    async send(value, providerText) {
      const prompt = value.trim();
      if (!prompt || pending || loading || ended || !token || !Number.isSafeInteger(company) || company <= 0) return;
      pending = new AbortController();
      // The transcript the tab already trimmed to the server policy, without pending or failed
      // exchanges: a completed question is resent only together with its completed, resendable
      // reply, so the history stays strictly alternating (the Anthropic family refuses adjacent
      // same-role messages) and never leaks a question the provider never answered.
      const history: { role: "user" | "assistant"; text: string }[] = [];
      for (let i = 0; !policy.savedSessions && !conversationId && i < entries.length; i++) {
        const item = entries[i]!;
        if (item.state !== "ready" || encoder.encode(item.text).length > policy.messageBytes) continue;
        if (item.role === "user") {
          const reply = entries[i + 1];
          if (!reply || reply.role !== "assistant" || reply.state !== "ready"
              || encoder.encode(reply.text).length > policy.messageBytes) continue;
        }
        history.push({ role: item.role, text: item.text });
      }
      entries.push({ role: "user", text: prompt, state: "ready" }, { role: "assistant", text: "Waiting for reply…", state: "pending" });
      publish();
      let reply: AssistantMessage;
      const text = providerText ?? prompt;
      const durable = Boolean(policy.savedSessions && options.userId);
      let requestId = (conversationId || ((policy.savedSessions || !history.length) && options.userId)) ? crypto.randomUUID().toString() : undefined;
      if (durable && requestId) {
        if (retryText === text && submission) requestId = submission;
        retryText = text;
        submission = requestId;
        save("submission", requestId);
      }
      try {
        const response = await (options.request ?? globalThis.fetch)("/erp/ai/assistant/turn", {
          method: "POST", credentials: "same-origin", redirect: "error", cache: "no-store",
          headers: { "Content-Type": "application/json", "X-CSRF-Token": token },
          body: JSON.stringify({ message: text, company_id: company,
            ...(conversationId ? {session_id:conversationId} : {history:policy.savedSessions ? [] : history}),
            ...(requestId && {request_id:requestId}) }), signal: pending.signal,
        });
        const data: unknown = response.headers.get("content-type")?.includes("application/json") ? await response.json() : null;
        if (!response.ok) {
          const reason = record(data) && typeof data["message"] === "string" ? data["message"].replace(/^erp\.ai\.[a-z_]+:\s*/, "").trim() : "";
          throw new Error(reason ? reason[0]!.toUpperCase() + reason.slice(1) : "The assistant could not return a reply.");
        }
        if (!record(data) || typeof data["text"] !== "string" || !data["text"].trim()) throw new Error("The provider returned an empty or invalid reply.");
        const canvases = data["canvases"];
        if (canvases !== undefined && (!Array.isArray(canvases) || !canvases.length || canvases.length > 12 ||
            !canvases.every(value => record(value) && typeof value["canvas_type"] === "string"))) {
          throw new Error("Invalid Canvas outputs.");
        }
        if (data["session_id"] !== undefined) {
          const id = data["session_id"];
          if (typeof id !== "number" || !Number.isSafeInteger(id) || id <= 0 || (conversationId && conversationId !== id))
            throw new Error("Invalid conversation receipt.");
          if (!ended) { conversationId = id; save("conversation",id); }
        }
        if (durable && !ended) { submission = retryText = undefined; save("submission", null); }
        reply = { role: "assistant", text: data["text"], state: "ready", ...(Array.isArray(canvases) ? {canvases} : {canvas:data["canvas"]}),
          ...(data["operations"] !== undefined && {operations:operations(data["operations"])}) };
      } catch (error) {
        reply = { role: "assistant", state: "error", text: error instanceof TypeError ? "Connection lost. The provider may already have processed this question."
          : error instanceof Error ? error.message : "The assistant could not return a reply." };
      } finally { pending = undefined; }
      if (!ended) { entries[entries.length - 1] = reply; publish(); }
    },
  };
  cache.set(id, session);
  if ((conversationId || submission) && rpc) {
    const restore = async () => {
      let id = conversationId;
      if (submission) {
        loading = true;
        try { id = await locateSubmission(rpc,submission,company) ?? conversationId; }
        finally { loading = false; }
      }
      if (ended) return;
      if (id) {
        await session.resume(id);
        submission = retryText = undefined; save("submission", null);
      } else {
        warning = "No saved receipt was found for the interrupted request. Check History before repeating it.";
        publish();
      }
    };
    void restore().catch(() => {
      if (!ended) { warning = "This conversation could not be restored. Open History to retry."; publish(); }
    });
  }
  window.addEventListener("erp:session-ended", () => {
    ended = true; workspaces.length = 0; requestedCanvas = undefined; pending?.abort(); pending = undefined; entries = []; dock = emptyDock();
    cache.delete(id);
    for (const notify of draftSubscribers) notify("");
    draftSubscribers.clear();
    for (const notify of subscribers) notify(); subscribers.clear();
  }, { once: true });
  return session;
}
