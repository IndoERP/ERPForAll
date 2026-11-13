/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
import { editorEnvironment, editorOptions, editorText, EDITOR_OPTIONS_BYTES, EDITOR_OPTIONS_LIMIT, type EditorEnvironment } from "./editor-types.js";
import { openEditorOverlay, type EditorOverlay, type EditorCloseReason } from "./editor-overlay.js";
import { el, errorMessage } from "./dom.js";
import { ClientError } from "./transport.js";
export type AutocompleteOption = { kind: "option"; label: string; select(): void | Promise<void> } | { kind: "status" | "heading"; label: string };
export interface AutocompleteSource { options: readonly AutocompleteOption[] | ((query: string, signal: AbortSignal) => readonly AutocompleteOption[] | Promise<readonly AutocompleteOption[]>); placeholder?: string }
export interface AutocompleteOptions { /** The field box the suggestion list lines up with (left edge, at least its width). */ alignTo?: HTMLElement; input: HTMLInputElement; owner: HTMLElement; environment: EditorEnvironment; sources: readonly AutocompleteSource[]; debounceMs?: number; autoSelect?: boolean; focusFirst?: boolean; representedValue?: () => string; onCancel?: () => void; touch?: boolean }
export interface AutocompleteHandle { dropdown: HTMLElement; search(query: string): Promise<void>; toggle(query: string): void; close(): void; dispose(): void; isOpen(): boolean; isDisposed(): boolean }
let autocompleteIdentity = 0;
/** Value-independent suggestion interactions; adapters own search permissions and selected identities. */
export function createAutocomplete(options: AutocompleteOptions): AutocompleteHandle {
  editorOptions(options, ["input", "owner", "environment", "sources", "debounceMs", "autoSelect", "focusFirst", "representedValue", "onCancel", "touch", "alignTo"], "Autocomplete");
  editorEnvironment(options.environment);
  const { input, owner, environment } = options;
  if (!(input instanceof HTMLInputElement) || !(owner instanceof HTMLElement) || !owner.contains(input)) throw new ClientError("validation", "Autocomplete requires an input inside its owner.");
  const title = input.getAttribute("aria-label") ?? "Choices"; editorText(title, "Autocomplete label");
  if (!Array.isArray(options.sources) || options.sources.length > 32) throw new ClientError("validation", "Autocomplete requires at most 32 sources.");
  for (const source of options.sources) {
    editorOptions(source, ["options", "placeholder"], "Autocomplete source");
    if (!Array.isArray(source.options) && typeof source.options !== "function") throw new ClientError("validation", "Autocomplete source options must be an array or function.");
    if (source.placeholder !== undefined) editorText(source.placeholder, "Autocomplete source placeholder");
  }
  const delay = options.debounceMs === undefined ? 150 : options.debounceMs;
  if (typeof delay !== "number" || !Number.isFinite(delay) || delay < 0 || delay > 10000) throw new ClientError("validation", "Autocomplete debounce must be a finite number from 0 to 10000.");
  if (options.autoSelect !== undefined && typeof options.autoSelect !== "boolean" || options.focusFirst !== undefined && typeof options.focusFirst !== "boolean" || options.touch !== undefined && typeof options.touch !== "boolean") throw new ClientError("validation", "Autocomplete autoSelect, focusFirst and touch must be booleans.");
  if (options.representedValue !== undefined && typeof options.representedValue !== "function" || options.onCancel !== undefined && typeof options.onCancel !== "function") throw new ClientError("validation", "Autocomplete representedValue and onCancel must be functions.");
  const attributes = ["role", "aria-autocomplete", "aria-controls", "aria-expanded", "aria-activedescendant", "aria-busy"] as const;
  const previous = attributes.map(name => input.getAttribute(name));
  const dropdown = el("div", undefined, "editor-autocomplete-list"); dropdown.id = `editor-autocomplete-${++autocompleteIdentity}`;
  dropdown.setAttribute("role", "listbox"); dropdown.setAttribute("aria-label", input.getAttribute("aria-label") ?? "Choices"); dropdown.style.display = "none";
  const status = el("div", undefined, "editor-autocomplete-status"); status.setAttribute("role", "status"); status.setAttribute("aria-live", "polite");
  const content = el("div", undefined, "editor-autocomplete"); content.append(dropdown, status);
  input.setAttribute("role", "combobox"); input.setAttribute("aria-autocomplete", "list"); input.setAttribute("aria-controls", dropdown.id); input.setAttribute("aria-expanded", "false");
  let dead = false, composing = false, generation = 0, active = -1, opened = false, transitioning = false;
  let overlay: EditorOverlay | undefined, timer: ReturnType<typeof setTimeout> | undefined, request: AbortController | undefined;
  let pendingKey: "Enter" | "Tab" | undefined, loading = false;
  let inputSlot: Comment | undefined;
  let items: AutocompleteOption[] = [], rows: HTMLElement[] = [];
  const placeholder = (message: string, failure = false): HTMLElement => {
    const row = el("div", undefined, failure ? "editor-autocomplete-error" : "editor-autocomplete-note");
    row.setAttribute("role", "option"); row.setAttribute("aria-disabled", "true"); row.setAttribute("aria-selected", "false");
    const text = el("span", message); if (failure) text.setAttribute("role", "alert"); row.append(text); return row;
  };
  const alive = (): boolean => !dead && !environment.signal?.aborted && owner.isConnected;
  const clearPending = (): void => {
    if (timer !== undefined) clearTimeout(timer); timer = undefined;
    generation++; request?.abort(); request = undefined; pendingKey = undefined; loading = false; input.removeAttribute("aria-busy");
  };
  const highlight = (index: number): void => {
    active = index;
    rows.forEach((row, rowIndex) => { const selected = rowIndex === index; row.classList.toggle("active", selected); if (row.getAttribute("role") === "option") row.setAttribute("aria-selected", String(selected)); });
    const row = rows[index];
    if (row) { input.setAttribute("aria-activedescendant", row.id); row.scrollIntoView?.({ block: "nearest" }); }
    else input.removeAttribute("aria-activedescendant");
  };
  const closeState = (reason: EditorCloseReason): void => {
    const priorTransition = transitioning; transitioning = true;
    try { if (inputSlot?.parentNode) inputSlot.replaceWith(input); inputSlot = undefined; } finally { transitioning = priorTransition; }
    opened = false; overlay = undefined; clearPending(); highlight(-1); dropdown.style.display = "none"; input.setAttribute("aria-expanded", "false"); status.textContent = "";
    if (reason === "cancel" && alive() && options.autoSelect) {
      try { if (options.representedValue) { const value = options.representedValue(); editorText(value, "Autocomplete represented value"); input.value = value; } options.onCancel?.(); }
      catch (error) { environment.report(error); }
    }
  };
  const close = (reason: EditorCloseReason = "cancel", restore = true): void => {
    if (overlay) { const current = overlay; overlay = undefined; transitioning = true; try { current.close(reason, restore); } finally { transitioning = false; } }
    else closeState(reason);
  };
  const ensureOpen = (): void => {
    if (opened || !alive()) return;
    opened = true; dropdown.style.display = "block"; input.setAttribute("aria-expanded", "true");
    transitioning = true;
    try {
      overlay = openEditorOverlay({ anchor: input, content, environment, title, ...(options.touch === undefined ? {} : { touch: options.touch }), ...(options.alignTo ? { alignTo: options.alignTo } : {}), onClose: closeState });
      if (overlay.mode === "sheet") { inputSlot = document.createComment("editor-input"); input.before(inputSlot); content.prepend(input); input.focus(); }
    }
    catch (error) { opened = false; dropdown.style.display = "none"; input.setAttribute("aria-expanded", "false"); throw error; }
    finally { transitioning = false; }
  };
  const reportFailure = (error: unknown): void => {
    if (!alive()) return;
    try { ensureOpen(); } catch (overlayError) { environment.report(overlayError); return; }
    items = []; rows = []; dropdown.replaceChildren(); highlight(-1);
    const rawMessage = errorMessage(error);
    // Preserve the original error through report; oversized diagnostics get an explicit bounded display summary.
    const message = rawMessage.length > 4096 || new TextEncoder().encode(rawMessage).length > 4096 ? "The choice source failed. Its diagnostic exceeds the editor display limit." : rawMessage;
    dropdown.append(placeholder(message, true));
    environment.report(error); status.textContent = ""; status.setAttribute("role", "status");
  };
  const choose = (item: AutocompleteOption, restore = true): void => {
    if (!alive() || item.kind !== "option") return;
    clearPending(); close("select", restore);
    try { const result = item.select(); void Promise.resolve(result).catch(error => { if (alive()) reportFailure(error); }); }
    catch (error) { reportFailure(error); }
  };
  const render = (next: readonly AutocompleteOption[]): void => {
    let bytes = 0;
    if (!Array.isArray(next) || next.length > EDITOR_OPTIONS_LIMIT) throw new ClientError("validation", "Autocomplete options exceed 1024 entries or 262144 bytes.");
    for (const item of next) {
      editorOptions(item, ["kind", "label", "select"], "Autocomplete option");
      if (item.kind !== "option" && Object.hasOwn(item, "select")) throw new ClientError("validation", "Invalid autocomplete option kind or selection callback.");
      if (!["option", "heading", "status"].includes(item.kind) || item.kind === "option" && typeof item.select !== "function") throw new ClientError("validation", "Invalid autocomplete option kind or selection callback.");
      editorText(item.label, "Autocomplete option label"); bytes += new TextEncoder().encode(item.label).length;
      if (bytes > EDITOR_OPTIONS_BYTES) throw new ClientError("validation", "Autocomplete options exceed 1024 entries or 262144 bytes.");
    }
    items = [...next]; dropdown.replaceChildren(); rows = []; active = -1; input.removeAttribute("aria-activedescendant");
    for (const [index, item] of items.entries()) {
      const row = el("div", item.label, item.kind === "option" ? "editor-autocomplete-option" : "editor-autocomplete-note"); row.id = `${dropdown.id}-${generation}-${index}`;
      if (item.kind === "option") {
        row.setAttribute("role", "option"); row.setAttribute("aria-selected", "false");
        row.addEventListener("pointermove", event => {
          // Layout/keyboard scrolling can emit stationary hover events over another row.
          // Only deliberate mouse/pen movement replaces the keyboard's active choice.
          if ((event.pointerType === "mouse" || event.pointerType === "pen") && Number.isFinite(event.movementX) && Number.isFinite(event.movementY) && (event.movementX !== 0 || event.movementY !== 0)) highlight(index);
        });
        row.addEventListener("click", event => { event.preventDefault(); choose(item); });
      } else if (item.kind === "status") { row.setAttribute("role", "option"); row.setAttribute("aria-disabled", "true"); row.setAttribute("aria-selected", "false"); }
      else { row.setAttribute("role", "presentation"); }
      dropdown.append(row); rows.push(row);
    }
    if (!items.some(item => item.kind !== "heading")) dropdown.append(placeholder("No matches."));
    status.textContent = items.length ? "" : "No matches."; status.setAttribute("role", "status");
    if (options.focusFirst === undefined ? options.autoSelect : options.focusFirst) highlight(items.findIndex(item => item.kind === "option"));
  };
  const search = async (query: string): Promise<void> => {
    if (!alive()) return;
    editorText(query, "Autocomplete query");
    clearPending(); const epoch = generation; request = new AbortController(); const signal = request.signal;
    ensureOpen(); loading = true; input.setAttribute("aria-busy", "true"); status.textContent = "Loading…"; status.setAttribute("role", "status");
    // Preserve existing choices during debounce/in-flight input so a pointer selection can cancel the pending input.
    if (!items.length) { dropdown.replaceChildren(placeholder("Loading…")); rows = []; }
    try {
      const results = await Promise.all(options.sources.map(source => typeof source.options === "function" ? source.options(query, signal) : source.options));
      if (!alive() || epoch !== generation || signal.aborted) return;
      const total = results.reduce((count, result) => count + (Array.isArray(result) ? result.length : EDITOR_OPTIONS_LIMIT + 1), 0);
      if (total > EDITOR_OPTIONS_LIMIT) throw new ClientError("validation", "Autocomplete options exceed 1024 entries or 262144 bytes.");
      const keyboard = pendingKey; loading = false; input.removeAttribute("aria-busy"); render(results.flat());
      if (keyboard) { const selected = items[active >= 0 ? active : items.findIndex(item => item.kind === "option")]; if (selected) choose(selected, keyboard !== "Tab"); }
    } catch (error) {
      if (!alive() || epoch !== generation || signal.aborted) return;
      loading = false; input.removeAttribute("aria-busy"); items = []; rows = []; dropdown.replaceChildren(); highlight(-1); pendingKey = undefined;
      reportFailure(error);
    }
  };
  const schedule = (): void => {
    if (composing || !alive()) return;
    // A new raw query owns pending keyboard selection immediately, before its debounce
    // expires. Retain already-rendered pointer choices, but never let an older read
    // publish choices or consume this query's Enter/Tab request during that window.
    clearPending();
    if (delay === 0) { timer = undefined; void search(input.value.trim()).catch(reportFailure); return; }
    try { ensureOpen(); } catch (error) { environment.report(error); return; }
    if (!items.length) dropdown.replaceChildren(placeholder("Waiting for input…"));
    status.textContent = "Waiting for input…";
    timer = setTimeout(() => { timer = undefined; const keyboard = pendingKey; const pending = search(input.value.trim()); pendingKey = keyboard; void pending.catch(reportFailure); }, delay);
  };
  const navigate = (direction: number): void => {
    const selectable = items.flatMap((item, index) => item.kind === "option" ? [index] : []);
    if (!selectable.length) return;
    const position = selectable.indexOf(active), next = position < 0 ? direction < 0 ? selectable.length - 1 : 0 : Math.max(0, Math.min(selectable.length - 1, position + direction));
    highlight(selectable[next]!);
  };
  const key = (event: KeyboardEvent): void => {
    if (!alive() || event.isComposing || composing || event.altKey || event.ctrlKey || event.metaKey) return;
    if (event.key === "ArrowDown" || event.key === "ArrowUp") {
      event.preventDefault(); event.stopPropagation();
      if (!opened) void search(input.value.trim()).catch(reportFailure); else navigate(event.key === "ArrowDown" ? 1 : -1);
    } else if (event.key === "Enter" || event.key === "Tab") {
      if (!opened) return;
      if (loading || timer !== undefined) { pendingKey = event.key; if (event.key === "Enter") { event.preventDefault(); event.stopPropagation(); } return; }
      const item = items[active];
      if (item?.kind === "option") { if (event.key === "Enter") { event.preventDefault(); event.stopPropagation(); } choose(item, event.key !== "Tab"); }
    } else if (event.key === "Escape") { if (opened) { event.preventDefault(); event.stopPropagation(); } close(); }
  };
  const compositionStart = (): void => { composing = true; clearPending(); };
  const compositionEnd = (): void => { composing = false; schedule(); };
  const blur = (): void => { if (!transitioning && overlay?.mode !== "sheet") close("cancel", false); };
  const preventBlur = (event: Event): void => event.preventDefault();
  const dispose = (): void => {
    if (dead) return; dead = true; close("dispose", false); clearPending(); removed.disconnect();
    input.removeEventListener("input", schedule); input.removeEventListener("keydown", key); input.removeEventListener("compositionstart", compositionStart); input.removeEventListener("compositionend", compositionEnd); input.removeEventListener("blur", blur); content.removeEventListener("mousedown", preventBlur);
    environment.signal?.removeEventListener("abort", dispose);
    attributes.forEach((name, index) => { const prior = previous[index]; if (prior === null || prior === undefined) input.removeAttribute(name); else input.setAttribute(name, prior); });
  };
  let mounted = owner.isConnected;
  const removed = new MutationObserver(() => { if (owner.isConnected) mounted = true; else if (mounted) dispose(); });
  input.addEventListener("input", schedule); input.addEventListener("keydown", key); input.addEventListener("compositionstart", compositionStart); input.addEventListener("compositionend", compositionEnd); input.addEventListener("blur", blur); content.addEventListener("mousedown", preventBlur);
  removed.observe(owner.ownerDocument.documentElement, { childList: true, subtree: true }); environment.signal?.addEventListener("abort", dispose, { once: true });
  if (environment.signal?.aborted) dispose();
  return { dropdown, search, toggle: query => { if (opened) close(); else void search(query).catch(reportFailure); }, close: () => close(), dispose, isOpen: () => opened, isDisposed: () => dead };
}
