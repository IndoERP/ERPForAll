/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
import { button, el, errorMessage } from "./dom.js";
import { DialogService, messageOptions, messageSignal, plainMessageObject, type DialogHandle } from "./dialog-service.js";
import { checkedRelationChoice, searchRelation, type RelationChoice } from "./relation-picker.js";
import { ClientError, RpcClient } from "./transport.js";

export interface SelectCreateDialogOptions {
  readonly dialogs: DialogService; readonly rpc: RpcClient; readonly model: string; readonly title?: string;
  readonly domain: readonly unknown[]; readonly context: Record<string, unknown>;
  readonly select: (choice: RelationChoice) => void | Promise<void>;
  /** Capability supplied by trusted code; resolves only after the embedded editor saves successfully. */
  readonly createEdit?: (input: { domain: readonly unknown[]; context: Record<string, unknown> }) => Promise<RelationChoice | undefined>;
  readonly report?: (error: unknown) => void; readonly signal?: AbortSignal;
}
/** Readonly relational selection. Search semantics stay with the existing name-search contract. */
export function selectCreateDialog(options: SelectCreateDialogOptions): DialogHandle {
  messageOptions(options, ["dialogs", "rpc", "model", "title", "domain", "context", "select", "createEdit", "report", "signal"], "Selection dialog");
  if (!(options.dialogs instanceof DialogService)) throw new ClientError("validation", "Selection dialogs must be a DialogService.");
  if (!(options.rpc instanceof RpcClient)) throw new ClientError("validation", "Selection rpc must be an RpcClient.");
  if (typeof options.model !== "string" || options.model.length > 256 || !/^[a-z][a-z0-9_]*(?:\.[a-z][a-z0-9_]*)*$/.test(options.model)) throw new ClientError("validation", "Selection model must be a valid name of at most 256 bytes.");
  if (!plainMessageObject(options.context)) throw new ClientError("validation", "Selection context must be a plain object.");
  if (typeof options.select !== "function") throw new ClientError("validation", "A relation selection callback is required.");
  if (!Array.isArray(options.domain)) throw new ClientError("validation", "Relation domain must be an array.");
  for (const key of ["createEdit", "report"] as const) if (options[key] !== undefined && typeof options[key] !== "function") throw new ClientError("validation", `Selection ${key} must be a function.`);
  messageSignal(options.signal);
  const scope = new AbortController(); let dead = false, pending = false, generation = 0;
  const domain = structuredClone(options.domain), context = structuredClone(options.context);
  const body = el("div", undefined, "select-create-body");
  // One search box, named for assistive technology; the dialog title already says what is being chosen. `autofocus`
  // makes the dialog service give it the first focus instead of the close button.
  const search = el("input", undefined, "select-create-search"); search.type = "search"; search.placeholder = "Search…";
  search.setAttribute("aria-label", "Find a record"); search.autofocus = true;
  const results = el("div", undefined, "select-create-results");
  const status = el("p", undefined, "select-create-status"); status.setAttribute("role", "status");
  body.append(search, status, results);
  const footer = el("div", undefined, "actions");
  let rows: readonly RelationChoice[] = [];
  let handle: DialogHandle;
  const controls = (): HTMLButtonElement[] => [...body.querySelectorAll<HTMLButtonElement>("button"), ...footer.querySelectorAll<HTMLButtonElement>("button")];
  const disable = (): void => { search.disabled = pending; controls().forEach(control => { control.disabled = pending; }); handle.dialog.setAttribute("aria-busy", String(pending)); };
  const showError = (error: unknown): void => { if (dead) return; status.setAttribute("role", "alert"); status.textContent = errorMessage(error); options.report?.(error); };
  const select = (row: RelationChoice): void => {
    if (dead || pending) return;
    pending = true; disable();
    void (async () => { await options.select(row); if (!dead) handle.close(); })().catch(showError).finally(() => { pending = false; if (!dead) disable(); });
  };
  const small = (): boolean => typeof matchMedia === "function" ? matchMedia("(max-width: 639px)").matches : window.innerWidth < 640;
  const render = (): void => {
    if (dead) return;
    results.replaceChildren();
    // A shown error stays until the next search replaces it.
    const counted = status.getAttribute("role") === "status";
    if (!rows.length) { if (counted) status.textContent = ""; results.append(el("p", "No matching records.", "muted")); return; }
    if (small()) {
      const cards = el("div", undefined, "select-create-cards");
      rows.forEach(row => { const card = el("article", undefined, "select-create-card"); const choice = button(row.display_name, () => select(row)); choice.dataset["recordId"] = String(row.id); card.append(choice); cards.append(card); });
      results.append(cards);
    } else {
      const table = el("table", undefined, "select-create-list"); table.setAttribute("aria-label", "Available records");
      const head = el("thead"), header = el("tr"); header.append(el("th", "Name")); head.append(header);
      const tbody = el("tbody");
      rows.forEach(row => { const tr = el("tr"), cell = el("td"); const choice = button(row.display_name, () => select(row)); choice.dataset["recordId"] = String(row.id); cell.append(choice); tr.append(cell); tbody.append(tr); });
      table.append(head, tbody); results.append(table);
    }
    // The search returns at most 80 names (relation-picker), so a full page says the list is cut.
    if (counted) status.textContent = rows.length >= 80 ? "First 80 records. Type to narrow the search." : rows.length === 1 ? "1 record" : `${rows.length} records`;
    disable();
  };
  /** Arrow keys move between the search box and the result rows, as in a list. */
  const choices = (): HTMLButtonElement[] => [...results.querySelectorAll<HTMLButtonElement>("button[data-record-id]")];
  body.addEventListener("keydown", event => {
    if (event.key !== "ArrowDown" && event.key !== "ArrowUp") return;
    const list = choices(); if (!list.length) return;
    const at = list.indexOf(document.activeElement as HTMLButtonElement);
    if (event.target === search && event.key === "ArrowDown") { event.preventDefault(); list[0]!.focus(); return; }
    if (at < 0) return;
    event.preventDefault();
    if (event.key === "ArrowUp" && at === 0) search.focus();
    else list[Math.max(0, Math.min(list.length - 1, at + (event.key === "ArrowDown" ? 1 : -1)))]!.focus();
  });
  const load = async (): Promise<void> => {
    const request = ++generation; status.setAttribute("role", "status"); status.textContent = "Loading records…";
    try {
      const found = await searchRelation({ rpc: options.rpc, model: options.model, name: search.value, domain, context, signal: scope.signal });
      if (dead || request !== generation) return;
      rows = found; render();
    } catch (error) { if (!dead && request === generation) showError(error); }
  };
  if (options.createEdit) footer.append(button("Create and edit", () => {
    if (dead || pending) return;
    pending = true; disable();
    void Promise.resolve().then(() => {
      if (dead) return undefined;
      return options.createEdit!({ domain: structuredClone(domain), context: structuredClone(context) });
    }).then(async raw => {
      if (dead || raw === undefined) return;
      await options.select(checkedRelationChoice(raw));
      if (!dead) handle.close();
    }).catch(showError).finally(() => { pending = false; if (!dead) disable(); });
  }, "primary"));
  footer.append(button("Cancel", () => { if (!pending) handle.close(); }));
  const externalAbort = (): void => handle.dispose();
  const resize = (): void => { if (!dead && !pending) render(); };
  handle = options.dialogs.open({ title: options.title ?? "Choose a record", body, footer, size: "lg", dismiss: () => !pending,
    onClose: () => { dead = true; generation++; scope.abort(); window.removeEventListener("resize", resize); options.signal?.removeEventListener("abort", externalAbort); } });
  options.signal?.addEventListener("abort", externalAbort, { once: true });
  search.addEventListener("input", () => { if (!pending) void load(); }); window.addEventListener("resize", resize);
  if (options.signal?.aborted) handle.dispose(); else void load();
  return handle;
}
