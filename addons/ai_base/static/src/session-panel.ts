/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
/**
 * The assistant session panel (AI1.SESSION-VIEW / AI1.SESSION-LIST.WEB): the list of the user's
 * own sessions, opened from the assistant dock header. It lists sessions in the server's
 * last-activity order, searches by a term the server matches against titles and
 * the owner's own message text, resumes, forks, archives, restores, deletes and exports
 * sessions, and shows the sessions anchored to the open record with the
 * "Anchor not available" state when the anchor is no longer readable.
 *
 * The "About this record" group is the server's own `about={model, res_id}` result (a second
 * `search_sessions` call), never a client-side filter of the main list, so the list limit can
 * never hide an about-session; it is hoisted above the general group, which is the deliberate
 * session never appears in both groups. The open-record anchor comes from the live systray
 * context (`active_model` + `active_id`): the panel re-reads it on every load and whenever the
 * panel is shown again.
 *
 * Every call goes through session-contract.ts; the display path never calls a provider
 * (4, plan §5.2). Rendering the message thread itself is AI1.ASSIST-TURN's pair
 * slice and stays out of scope.
 */
import { icon, type IconName } from "@erpforall/client";
import {
  anchorFromContext, archiveSession, deleteSession, exportSession, forkSession, resumeSession,
  searchSessions, type OpenRecordAnchor, type ResumeSessionResult, type RpcLike, type SessionSummary,
} from "./session-contract.js";

export interface SessionPanelOptions {
  rpc: RpcLike;
  /**
   * The shell's assistant context; `active_model` + `active_id` anchor the "About this record"
   * section. The object is live: the shell updates those keys as the mounted record changes, and
   * the panel re-reads them on every load and whenever it is shown again.
   */
  context?: Record<string, unknown>;
  /** Called after a successful resume, with the session and the sequence the next turn continues. */
  onResume?: (session: SessionSummary, resume: ResumeSessionResult) => void | Promise<void>;
  /** Called when the panel asks to return to the chat. */
  onClose?: () => void;
}

export interface SessionPanelHandle {
  node: HTMLElement;
  refresh(): Promise<void>;
  dispose(): void;
}

const SEARCH_DEBOUNCE_MS = 300;
const UNTITLED = "Untitled session";
const activityFormatter = new Intl.DateTimeFormat("en-US", { month: "short", day: "numeric" });

function control(label: string, glyph: IconName, className = "icon-button"): HTMLButtonElement {
  const button = document.createElement("button");
  button.type = "button";
  button.className = className;
  button.setAttribute("aria-label", label);
  button.title = label;
  button.append(icon(glyph));
  return button;
}

function formatActivity(value: string): string {
  const time = Date.parse(value);
  return Number.isFinite(time) ? activityFormatter.format(time) : value;
}

/** A `erp.ai.<code>` code the server can refuse with, mapped to user copy (house rule: no codes, no jargon). */
const AI_REFUSALS: Record<string, string> = {
  session_delete_pending: "Deleting sessions isn't available yet.",
};

/**
 * The one place a failure becomes user copy: a known `erp.ai.<code>` refusal is mapped to its
 * friendly sentence; any other `erp.ai.<code>` reads as the generic refusal instead of echoing
 * internal codes and ADR names; other errors keep their already user-safe wording.
 */
function describeSessionError(error: unknown): string {
  if (error instanceof Error && error.message) {
    const code = /^erp\.ai\.([a-z0-9_]+)(?::|$)/.exec(error.message)?.[1];
    if (code !== undefined) return AI_REFUSALS[code] ?? "The session store refused the request.";
    return error.message;
  }
  return "The session store refused the request.";
}

/** The one place a session row is drawn; a row is rebuilt wholesale on every list render. */
export function sessionPanel(options: SessionPanelOptions): SessionPanelHandle {
  const rpc = options.rpc;
  let sessions: SessionSummary[] = [];
  let aboutSessions: SessionSummary[] = [];
  let anchor: OpenRecordAnchor | null = null;
  let query = "";
  let includeArchived = false;
  let activeId: number | null = null;
  let confirmingDelete: number | null = null;
  let statusText = "";
  let error: string | null = null;
  let loading = false;
  let requestSeq = 0;
  let closed = false;
  let debounce: ReturnType<typeof setTimeout> | undefined;

  const panel = document.createElement("section");
  panel.className = "ai-session-panel";
  panel.id = "ai-session-panel";
  panel.setAttribute("role", "region");
  panel.setAttribute("aria-label", "Sessions");

  const head = document.createElement("header");
  head.className = "ai-session-head";
  const back = control("Back to chat", "chevron-left");
  back.addEventListener("click", () => options.onClose?.());
  const heading = document.createElement("h2");
  heading.textContent = "Sessions";
  head.append(back, heading);

  const toolbar = document.createElement("div");
  toolbar.className = "ai-session-toolbar";
  const search = document.createElement("input");
  search.type = "search";
  search.className = "ai-session-search";
  search.placeholder = "Search by title or your message";
  search.setAttribute("aria-label", "Search sessions");
  search.addEventListener("input", () => {
    query = search.value;
    clearTimeout(debounce);
    debounce = setTimeout(() => { void load(); }, SEARCH_DEBOUNCE_MS);
  });
  const archivedToggle = document.createElement("label");
  archivedToggle.className = "ai-session-archived-toggle";
  const archivedCheck = document.createElement("input");
  archivedCheck.type = "checkbox";
  archivedCheck.addEventListener("change", () => {
    includeArchived = archivedCheck.checked;
    void load();
  });
  archivedToggle.append(archivedCheck, document.createTextNode("Show archived"));
  toolbar.append(search, archivedToggle);

  const status = document.createElement("p");
  status.className = "ai-session-status";
  status.setAttribute("role", "status");
  status.hidden = true;

  const body = document.createElement("div");
  body.className = "ai-session-body";
  body.tabIndex = 0;
  body.setAttribute("role","region");
  body.setAttribute("aria-label","Session list");

  panel.append(head, toolbar, status, body);

  function renderStatus(): void {
    status.textContent = statusText;
    status.hidden = statusText === "";
  }

  /** The row control that held focus, as a marker that survives the rebuild. */
  function focusMarker(): { id: string; action: string } | null {
    const active = document.activeElement;
    if (!(active instanceof HTMLElement)) return null;
    const rowEl = active.closest<HTMLElement>(".ai-session-row");
    if (!rowEl || rowEl.dataset["sessionId"] === undefined) return null;
    return { id: rowEl.dataset["sessionId"], action: active.dataset["sessionAction"] ?? "title" };
  }

  function row(session: SessionSummary): HTMLElement {
    const li = document.createElement("li");
    li.className = "ai-session-row";
    li.dataset["sessionId"] = String(session.id);
    if (session.id === activeId) li.classList.add("is-active");

    const main = document.createElement("div");
    main.className = "ai-session-main";
    const title = document.createElement("button");
    title.type = "button";
    title.className = "ai-session-title";
    title.dataset["sessionAction"] = "title";
    title.textContent = session.title || UNTITLED;
    title.setAttribute("aria-label", `Resume ${session.title || UNTITLED}`);
    if (session.id === activeId) title.setAttribute("aria-current", "true");
    title.addEventListener("click", () => { void resume(session); });

    const meta = document.createElement("p");
    meta.className = "ai-session-meta";
    const time = document.createElement("span");
    time.className = "ai-session-time";
    time.textContent = formatActivity(session.last_event_at);
    meta.append(time);
    if (session.state === "archived") {
      const badge = document.createElement("span");
      badge.className = "ai-session-badge";
      badge.textContent = "Archived";
      meta.append(badge);
    }
    if (session.anchor) {
      const anchorNote = document.createElement("span");
      anchorNote.className = "ai-session-anchor" + (session.anchor.available ? "" : " ai-session-anchor-missing");
      if (session.anchor.available) {
        anchorNote.append(icon("target"));
        anchorNote.append(document.createTextNode(`About ${session.anchor.model} #${session.anchor.res_id}`));
      } else {
        anchorNote.append(icon("lock"));
        anchorNote.append(document.createTextNode("Anchor not available"));
      }
      meta.append(anchorNote);
    }
    main.append(title, meta);

    const actions = document.createElement("div");
    actions.className = "ai-session-actions";
    const fork = control("Fork session", "git-branch");
    fork.dataset["sessionAction"] = "fork";
    fork.addEventListener("click", () => { void forkOne(session); });
    const archive = session.state === "archived" ? control("Restore session", "archive-restore") : control("Archive session", "archive");
    archive.dataset["sessionAction"] = "archive";
    archive.addEventListener("click", () => {
      void (session.state === "archived" ? restoreOne(session) : archiveOne(session));
    });
    const exportBtn = control("Export session", "file-text");
    exportBtn.dataset["sessionAction"] = "export";
    exportBtn.addEventListener("click", () => { void exportOne(session); });
    const del = document.createElement("button");
    del.type = "button";
    del.dataset["sessionAction"] = "delete";
    if (confirmingDelete === session.id) {
      del.className = "icon-button ai-session-delete-confirm";
      del.setAttribute("aria-label", "Confirm delete");
      del.title = "Delete this session permanently";
      del.append(icon("trash"));
      del.append(document.createTextNode("Confirm"));
    } else {
      del.className = "icon-button";
      del.setAttribute("aria-label", "Delete session");
      del.title = "Delete session";
      del.append(icon("trash"));
    }
    del.addEventListener("click", () => {
      if (confirmingDelete === session.id) {
        confirmingDelete = null;
        void deleteOne(session);
      } else {
        confirmingDelete = session.id;
        renderBody(true);
      }
    });
    actions.append(fork, archive, exportBtn, del);
    li.append(main, actions);
    return li;
  }

  function group(label: string, items: readonly SessionSummary[]): HTMLElement {
    const section = document.createElement("section");
    section.className = "ai-session-group";
    const title = document.createElement("h3");
    title.textContent = label;
    const list = document.createElement("ul");
    list.className = "ai-session-list";
    list.setAttribute("role", "list");
    for (const item of items) list.append(row(item));
    section.append(title, list);
    return section;
  }

  function emptyBlock(): HTMLElement {
    const wrap = document.createElement("div");
    wrap.className = "ai-session-empty";
    if (loading) {
      const note = document.createElement("p");
      note.textContent = "Loading sessions…";
      wrap.append(note);
      return wrap;
    }
    const note = document.createElement("p");
    note.textContent = query ? "No sessions match your search." : "No sessions yet.";
    wrap.append(note);
    if (!query) {
      const hint = document.createElement("p");
      hint.className = "ai-session-empty-hint";
      hint.textContent = "Ask the assistant a question to start a session.";
      wrap.append(hint);
    }
    return wrap;
  }

  function errorBlock(): HTMLElement {
    const wrap = document.createElement("div");
    wrap.className = "ai-session-error";
    wrap.setAttribute("role", "alert");
    const note = document.createElement("p");
    note.textContent = error ?? "Could not load sessions.";
    const retry = document.createElement("button");
    retry.type = "button";
    retry.className = "erp-button erp-button-secondary";
    retry.textContent = "Retry";
    retry.addEventListener("click", () => { void load(); });
    wrap.append(note, retry);
    return wrap;
  }

  /** Rebuild the list region; `keepFocus` restores focus to the same row control when it survived. */
  function renderBody(keepFocus = false): void {
    const marker = keepFocus ? focusMarker() : null;
    body.replaceChildren();
    if (error) { body.append(errorBlock()); return; }
    if (aboutSessions.length === 0 && sessions.length === 0) { body.append(emptyBlock()); return; }
    // The about group is the server's own `about`-filtered answer and is hoisted
    // above the general group; a session never appears in both groups.
    const aboutIds = new Set(aboutSessions.map(session => session.id));
    const rest = sessions.filter(session => !aboutIds.has(session.id));
    if (aboutSessions.length) body.append(group("About this record", aboutSessions));
    if (rest.length) body.append(group("Sessions", rest));
    if (marker) {
      const target = body.querySelector<HTMLElement>(`.ai-session-row[data-session-id="${marker.id}"] [data-session-action="${marker.action}"]`);
      (target ?? search).focus();
    }
  }

  async function load(keepFocus = false): Promise<void> {
    const seq = ++requestSeq;
    error = null;
    // The anchor comes from the live systray context: read it fresh on every load. One half of
    // the pair present without the other, or a malformed value, is a wiring bug: fail loudly.
    try {
      anchor = anchorFromContext(options.context);
    } catch (contextError) {
      anchor = null;
      if (closed || seq !== requestSeq) return;
      error = describeSessionError(contextError);
      loading = false;
      renderBody(keepFocus);
      return;
    }
    if (sessions.length === 0 && aboutSessions.length === 0) { loading = true; renderBody(keepFocus); }
    try {
      // list limit never hides an about-session; the general list is the ordinary call.
      const [about, main] = await Promise.all([
        anchor ? searchSessions(rpc, { search: query, include_archived: includeArchived, about: anchor }) : Promise.resolve(null),
        searchSessions(rpc, { search: query, include_archived: includeArchived }),
      ]);
      if (closed || seq !== requestSeq) return;
      aboutSessions = about ?? [];
      sessions = main;
      confirmingDelete = null;
    } catch (loadError) {
      if (closed || seq !== requestSeq) return;
      sessions = [];
      aboutSessions = [];
      confirmingDelete = null;
      error = describeSessionError(loadError);
    } finally {
      if (!closed && seq === requestSeq) { loading = false; renderBody(keepFocus); }
    }
  }

  async function act(job: () => Promise<{ message: string }>): Promise<void> {
    statusText = "";
    renderStatus();
    try {
      const { message } = await job();
      if (closed) return;
      statusText = message;
      renderStatus();
    } catch (actError) {
      if (closed) return;
      statusText = describeSessionError(actError);
      // An action failed while another row's delete confirm was armed: the confirm pose goes away
      // with the failure, next to the status line that explains it. (The delete action's own
      // failure rebuilds the row inside `deleteOne`, because its confirm state is cleared on the
      // confirming click.)
      if (confirmingDelete !== null) {
        confirmingDelete = null;
        renderBody(true);
      }
      renderStatus();
    }
  }

  function resume(session: SessionSummary): void {
    void act(async () => {
      const result = await resumeSession(rpc, session.id);
      activeId = result.id;
      renderBody(true);
      await options.onResume?.(session, result);
      return { message: "" };
    });
  }

  function forkOne(session: SessionSummary): void {
    void act(async () => {
      await forkSession(rpc, session.id);
      await load(true);
      return { message: "Forked as a new session." };
    });
  }

  function archiveOne(session: SessionSummary): void {
    void act(async () => {
      await archiveSession(rpc, session.id, "archived");
      await load(true);
      return { message: "Session archived." };
    });
  }

  function restoreOne(session: SessionSummary): void {
    void act(async () => {
      await archiveSession(rpc, session.id, "open");
      await load(true);
      return { message: "Session restored." };
    });
  }

  function deleteOne(session: SessionSummary): void {
    void act(async () => {
      try {
        await deleteSession(rpc, session.id);
      } catch (deleteError) {
        // The click that armed the delete already cleared the confirm state, so the row would
        // keep its stale "Confirm delete" pose: rebuild it out of that pose here, so the status
        // line that follows sits next to the normal delete control.
        if (!closed) renderBody(true);
        throw deleteError;
      }
      confirmingDelete = null;
      await load(true);
      return { message: "Session deleted." };
    });
  }

  function exportOne(session: SessionSummary): void {
    void act(async () => {
      const result = await exportSession(rpc, session.id);
      if (typeof URL.createObjectURL !== "function") throw new Error("This browser cannot save the export.");
      const blob = new Blob([result.content], { type: "application/json" });
      const url = URL.createObjectURL(blob);
      const link = document.createElement("a");
      link.href = url;
      link.download = result.filename;
      link.hidden = true;
      document.body.append(link);
      link.click();
      link.remove();
      URL.revokeObjectURL(url);
      return { message: "Session exported." };
    });
  }

  panel.addEventListener("keydown", event => {
    if (event.key !== "Escape") return;
    if (confirmingDelete !== null) {
      event.preventDefault();
      event.stopPropagation();
      confirmingDelete = null;
      renderBody(true);
      return;
    }
    if (event.target === search) {
      event.preventDefault();
      event.stopPropagation();
      if (search.value) {
        search.value = "";
        query = "";
        clearTimeout(debounce);
        void load();
      } else {
        options.onClose?.();
      }
      return;
    }
    event.preventDefault();
    event.stopPropagation();
    options.onClose?.();
  });

  // The systray context is live: the record behind the panel can change while the dock stays
  // open. Reload when the panel is shown again, so the About this record section always reads
  // the record that is open now.
  const shown = new MutationObserver(() => {
    if (!closed && !panel.hidden) void load();
  });
  shown.observe(panel, { attributes: true, attributeFilter: ["hidden"] });

  void load();

  return {
    node: panel,
    refresh: () => load(),
    dispose() {
      closed = true;
      clearTimeout(debounce);
      shown.disconnect();
      panel.remove();
    },
  };
}
