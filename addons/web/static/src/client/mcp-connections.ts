/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
import { button, el } from "./dom.js";
import { registerFieldWidget, type WidgetEnvironment, type WidgetHandle } from "./registry.js";

interface Connection {
  id: number; name: string; read_only: boolean; models: string[] | null; methods: string[] | null;
  expires_at: string | null; last_used_at: string | null; expired: boolean;
}
interface Page { db: string; login: string; items: Connection[]; next: number | null }

function decode(value: unknown): Page {
  const p = value as Page;
  const names = (v: unknown) => v === null || (Array.isArray(v) && v.every(s => typeof s === "string"));
  if (!p || typeof p.db !== "string" || typeof p.login !== "string" || !Array.isArray(p.items) || p.items.length > 50 ||
    !(p.next === null || Number.isSafeInteger(p.next)) || p.items.some(c => !c || !Number.isSafeInteger(c.id) || c.id < 1 ||
      typeof c.name !== "string" || typeof c.read_only !== "boolean" || typeof c.expired !== "boolean" ||
      !names(c.models) || !names(c.methods) || [c.last_used_at,c.expires_at].some(t => t !== null && typeof t !== "string"))) {
    throw new Error("Invalid MCP connection list.");
  }
  return p;
}
function labeled(caption: string, field: HTMLElement) {
  const label = el("label", caption); label.append(field); return label;
}
function input(type: string, placeholder?: string) {
  const field = el("input"); field.type = type; if (placeholder) field.placeholder = placeholder; return field;
}
function localDate(stamp: string | null, empty: string) {
  return stamp ? new Date(stamp).toLocaleString() : empty;
}

/** The bearer is displayed in this widget exactly once. It never enters record state or a replayable RPC. */
export function mcpConnections(env: Pick<WidgetEnvironment, "rpc" | "context">): WidgetHandle {
  const root = el("section", undefined, "mcp-connections");
  root.setAttribute("aria-label", "MCP connections");
  const status = el("p", undefined, "mcp-status");
  status.setAttribute("role", "status");
  const panel = el("div", undefined, "mcp-panel");
  panel.hidden = true;
  const toggle = button("Manage MCP connections", () => {
    if (panel.hidden) { panel.hidden = false; toggle.setAttribute("aria-expanded", "true"); void load(null); }
    else { panel.hidden = true; toggle.setAttribute("aria-expanded", "false"); hideEditor(); clearSecret(); }
  });
  toggle.setAttribute("aria-expanded", "false");
  const list = el("ul", undefined, "mcp-list");
  const pagination = el("div", undefined, "mcp-actions");
  const editor = el("div", undefined, "mcp-editor"); editor.hidden = true;
  const receipt = el("div", undefined, "mcp-receipt"); receipt.hidden = true;
  const endpoint = new URL("/erp/mcp", location.origin).href;
  const example = el("details");
  example.append(el("summary", "Client configuration"), el("p", "Use the streamable HTTP transport and supply the bearer from your client’s secret store."));
  const config = JSON.stringify({ mcpServers: { erpforall: { url: endpoint, headers: { Authorization: "Bearer <MCP_TOKEN>" } } } }, null, 2);
  example.append(el("pre", config));
  const name = input("text", "e.g. Finance assistant"); name.maxLength = 128;
  const mode = el("select"); mode.append(new Option("Read only", "read"), new Option("Read and write", "write"));
  const permissionNote = el("small", "Your ERP permissions and each tool’s approval policy apply.");
  const expiry = input("datetime-local"); expiry.step = "1";
  const models = input("text", "e.g. sale.order, res.partner");
  const methods = input("text", "e.g. search_read");
  const filter = el("details"); filter.append(el("summary", "Restrict models and methods"), el("p", "Enter exact names separated by commas. Leave a list empty to allow all names within your ERP permissions."),
    labeled("Models", models), labeled("Methods", methods));
  const password = input("password"); password.autocomplete = "current-password";
  const issue = button("Create connection", () => void create(), "primary");
  const controls = el("div", undefined, "mcp-actions");
  const cancelEdit = button("Cancel", () => { hideEditor(); add.focus(); });
  controls.append(issue, cancelEdit);
  editor.addEventListener("keydown", event => {
    if (event.key === "Enter" && !event.isComposing && event.target instanceof HTMLInputElement) {
      event.preventDefault(); event.stopPropagation(); void create();
    }
  });
  editor.append(labeled("Connection name", name), labeled("Access", mode), permissionNote, labeled("Expires at (local time)", expiry),
    el("small", "Leave blank to use your account’s maximum lifetime."), filter, labeled("Confirm your ERP password", password), controls);
  const add = button("New MCP connection", () => {
    clearSecret(); status.textContent = ""; editor.hidden = false; add.hidden = true; name.focus();
  });
  add.disabled = true;
  panel.append(list, pagination, add, editor, receipt, example);
  root.append(toggle, panel, status);
  let active = true, minting = false, sequence = 0, account: Page | undefined;
  let secretField: HTMLInputElement | undefined;
  const cursors: (number | null)[] = [null];
  const controller = new AbortController();
  const report = (e: unknown) => { if (active) status.textContent = e instanceof Error ? e.message : "MCP connections could not be loaded."; };
  function hideEditor() {
    editor.hidden = true; add.hidden = false;
    name.value = ""; expiry.value = ""; password.value = ""; models.value = ""; methods.value = ""; mode.value = "read"; filter.open = false;
  }
  function clearSecret() {
    if (secretField) secretField.value = "";
    secretField = undefined;
    receipt.replaceChildren(); receipt.hidden = true;
  }
  function reveal(secret: string, expiration: string) {
    clearSecret();
    const field = input("password"); field.readOnly = true; field.autocomplete = "off"; field.value = secret; secretField = field;
    const copy = button("Copy token", () => {
      if (!navigator.clipboard?.writeText) {
        field.type = "text"; field.focus(); field.select();
        status.textContent = "Token selected. Copy it now."; return;
      }
      void navigator.clipboard.writeText(field.value).then(() => {
        if (active && secretField === field) status.textContent = "Token copied.";
      }).catch(() => {
        if (active && secretField === field) { field.type = "text"; field.focus(); field.select(); status.textContent = "Clipboard access is unavailable. Copy the selected token."; }
      });
    });
    const done = button("Done", () => { clearSecret(); add.focus(); });
    receipt.append(el("strong", "Copy your token now"), el("p", `This is its only display. Expires ${localDate(expiration, "")}.`),
      labeled("One-time token", field), copy, done);
    receipt.hidden = false; copy.focus();
  }
  async function load(before: number | null) {
    const current = ++sequence;
    try {
      const raw = await env.rpc.call("res.users", "erp_mcp_connections", { ...(before ? { before } : {}), context: env.context },
        { background: true, signal: controller.signal });
      if (!active || current !== sequence) return;
      const data = decode(raw); account = data; add.disabled = false;
      list.replaceChildren(); pagination.replaceChildren();
      for (const c of data.items) {
        const item = el("li");
        const desc = el("div"); desc.append(el("strong", c.name), el("span",
          [c.read_only ? "Read only" : "Read and write", c.expired ? "Expired" : `Expires ${localDate(c.expires_at, "never")}`,
            `Last used ${localDate(c.last_used_at, "—")}`].join(" · ")));
        if (c.models || c.methods) desc.append(el("small", [c.models && `Models: ${c.models.join(", ")}`, c.methods && `Methods: ${c.methods.join(", ")}`].filter(Boolean).join(" · ")));
        const revoke = button("Revoke", () => {
          revoke.hidden = true; confirm.hidden = false; cancel.hidden = false; confirm.focus();
        });
        revoke.setAttribute("aria-label", `Revoke ${c.name}`);
        const cancel = button("Cancel", () => { confirm.hidden = true; cancel.hidden = true; revoke.hidden = false; revoke.focus(); });
        const confirm = button("Confirm revocation", async () => {
          confirm.disabled = true; cancel.disabled = true;
          try {
            await env.rpc.call("res.users", "erp_revoke_mcp_connection", { connection_id: c.id, context: env.context }, { write: true });
            if (!active) return;
            clearSecret(); status.textContent = `Revoked ${c.name}.`; await load(before);
          } catch (e) { report(e); confirm.disabled = false; cancel.disabled = false; }
        });
        cancel.hidden = true; confirm.hidden = true;
        item.append(desc, revoke, confirm, cancel); list.append(item);
      }
      if (!data.items.length) list.append(el("li", "No MCP connections."));
      if (cursors.length > 1) pagination.append(button("Previous", () => { cursors.pop(); void load(cursors.at(-1)!); }));
      if (data.next !== null) pagination.append(button("Next", () => { cursors.push(data.next); void load(data.next); }));
    } catch (error) { if (current === sequence) report(error); }
  }
  async function create() {
    if (!account || minting) return;
    status.textContent = "";
    if (!name.value.trim() || !password.value) { status.textContent = "Enter a connection name and confirm your password."; return; }
    if (!expiry.validity.valid) { status.textContent = "Enter a valid expiration time."; return; }
    minting = true; issue.disabled = true; cancelEdit.disabled = true; toggle.disabled = true;
    try {
      const split = (s: string) => s.split(",").map(part => part.trim());
      const body = JSON.stringify({ db: account.db, login: account.login, password: password.value, device_name: name.value.trim(), audience: "mcp",
        mcp_access: { read_only: mode.value === "read",
          ...(models.value.trim() ? { models: split(models.value) } : {}),
          ...(methods.value.trim() ? { methods: split(methods.value) } : {}),
          ...(expiry.value ? { expires_at: new Date(expiry.value).toISOString().replace(/\.\d{3}Z$/, "Z") } : {}) } });
      password.value = "";
      const response = await fetch("/erp/session/api-key", { method: "POST", credentials: "omit", redirect: "error", cache: "no-store",
        headers: { "Content-Type": "application/json", Accept: "application/json" }, body, signal: controller.signal });
      if (!active) return;
      if (!response.ok) {
        const detail = await response.json().catch(() => null) as { error?: unknown } | null;
        throw new Error(typeof detail?.error === "string" && response.status < 500 ? detail.error : "Unable to create the connection.");
      }
      const data = await response.json() as { api_key?: unknown; expires_at?: unknown };
      if (!active) return;
      if (typeof data.api_key !== "string" || typeof data.expires_at !== "string") throw new Error("Invalid connection response. Check your connections before retrying.");
      hideEditor();
      if (!panel.hidden) reveal(data.api_key, data.expires_at);
      cursors.splice(0, cursors.length, null); await load(null);
    } catch (error) {
      report(error instanceof TypeError ? new Error("Connection result is unknown. Check your connection list before retrying.") : error);
      if (error instanceof TypeError && active) await load(null);
    } finally { minting = false; issue.disabled = false; cancelEdit.disabled = false; toggle.disabled = false; }
  }
  return { node: root, dispose() { active = false; controller.abort(); password.value = ""; clearSecret(); root.remove(); } };
}
registerFieldWidget("web.mcp_connections", { kinds: ["char"], render: mcpConnections });
