/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
import { button, el, errorMessage } from "./dom.js";
import { EMOJI_DATA, type EmojiEntry } from "./emoji-data.js";
import { editorEnvironment, editorOptions, editorText, EDITOR_OPTIONS_BYTES, EDITOR_OPTIONS_LIMIT, type EditorEnvironment } from "./editor-types.js";
import { openEditorOverlay, type EditorOverlay } from "./editor-overlay.js";
import { ClientError } from "./transport.js";
export interface EmojiPickerOptions {
  environment: EditorEnvironment; label?: string; data?: readonly EmojiEntry[];
  load?: () => Promise<readonly EmojiEntry[]>; allowRemove?: boolean; readonly?: boolean; touch?: boolean;
  select(value: string | false): boolean | void;
}
function catalogue(raw: readonly EmojiEntry[]): readonly EmojiEntry[] {
  if (!Array.isArray(raw) || raw.length > EDITOR_OPTIONS_LIMIT) throw new ClientError("validation", "Emoji data must be an array of at most 1024 entries.");
  const seen = new Set<string>(); let bytes = 0;
  return raw.map(item => {
    editorOptions(item, ["value", "label", "keywords", "category"], "Emoji");
    editorText(item.value, "Emoji value", 64); editorText(item.label, "Emoji label"); editorText(item.category, "Emoji category", 128);
    if (!item.value || !item.label || !item.category || seen.has(item.value) || !Array.isArray(item.keywords) || item.keywords.length > 32) throw new ClientError("validation", "Emoji identities, labels and categories must be nonempty; identities must be unique and keywords bounded.");
    seen.add(item.value); for (const word of item.keywords) editorText(word, "Emoji keyword", 128);
    bytes += new TextEncoder().encode(item.value + item.label + item.category + item.keywords.join("")).length;
    if (bytes > EDITOR_OPTIONS_BYTES) throw new ClientError("validation", "Emoji data exceeds 262144 bytes.");
    return { ...item, keywords: [...item.keywords] };
  });
}
/** Original curated emoji data, safe labels and owner-scoped keyboard/pointer selection. */
export function createEmojiPicker(options: EmojiPickerOptions): { node: HTMLElement; dispose(): void } {
  editorOptions(options, ["environment", "label", "data", "load", "allowRemove", "readonly", "touch", "select"], "Emoji picker"); editorEnvironment(options.environment);
  if (options.label !== undefined) editorText(options.label, "Emoji picker label");
  if (typeof options.select !== "function" || options.load !== undefined && typeof options.load !== "function" || options.data !== undefined && options.load !== undefined || [options.allowRemove, options.readonly, options.touch].some(value => value !== undefined && typeof value !== "boolean")) throw new ClientError("validation", "Invalid emoji picker declaration.");
  let data = catalogue(options.data === undefined ? EMOJI_DATA : options.data), dead = false, generation = 0, overlay: EditorOverlay | undefined;
  const node = el("div", undefined, "choice-emoji");
  const trigger = button(options.label ?? "Choose emoji", () => open(), "emoji-trigger"); trigger.disabled = options.readonly === true;
  trigger.setAttribute("aria-haspopup", "dialog"); trigger.setAttribute("aria-expanded", "false"); node.append(trigger);
  const abort = (): void => dispose(); options.environment.signal?.addEventListener("abort", abort, { once: true });
  const open = (): void => {
    if (dead || options.environment.signal?.aborted || overlay || options.readonly) return;
    const request = ++generation;
    const panel = el("div", undefined, "emoji-picker-panel");
    const search = el("input"); search.type = "search"; search.setAttribute("aria-label", "Search emoji"); search.placeholder = "Search emoji";
    const categories = el("div", undefined, "emoji-categories"); categories.setAttribute("role", "group"); categories.setAttribute("aria-label", "Emoji categories");
    const grid = el("div", undefined, "emoji-grid"); grid.setAttribute("role", "group"); grid.setAttribute("aria-label", "Emoji choices");
    const status = el("p", "", "editor-status"); status.setAttribute("role", "status");
    panel.append(search, categories, grid, status);
    let category = "", active = 0, shown: readonly EmojiEntry[] = [];
    const selected = (value: string | false, keep: boolean): void => {
      if (dead || request !== generation) return;
      try { const accepted = options.select(value); if (accepted !== false && !keep) overlay?.close("select"); }
      catch (error) { status.textContent = errorMessage(error); options.environment.report(error); }
    };
    const highlight = (): void => {
      const controls = [...grid.querySelectorAll<HTMLButtonElement>("button")];
      controls.forEach((control, index) => { control.classList.toggle("active", index === active); control.tabIndex = index === active ? 0 : -1; });
      controls[active]?.scrollIntoView?.({ block: "nearest" });
    };
    const render = (): void => {
      let query: string;
      try {
        // Admit the raw value before normalization allocates additional strings.
        editorText(search.value, "Emoji search");
        query = search.value.trim().normalize("NFD").replace(/\p{M}/gu, "").toLowerCase();
      } catch (error) {
        shown = []; active = -1; grid.replaceChildren(); status.textContent = errorMessage(error);
        options.environment.report(error); return;
      }
      shown = data.filter(item => (!category || item.category === category) && [item.label, ...item.keywords].join(" ").normalize("NFD").replace(/\p{M}/gu, "").toLowerCase().includes(query));
      active = 0; grid.replaceChildren(); status.textContent = shown.length ? "" : "No emoji matches.";
      for (const item of shown) {
        const control = button(item.value, () => {}, "emoji-choice"); control.dataset["emoji"] = item.value; control.setAttribute("aria-label", item.label); control.title = item.label;
        control.addEventListener("click", event => selected(item.value, event.shiftKey)); grid.append(control);
      }
      highlight();
    };
    const renderCategories = (): void => {
      categories.replaceChildren();
      for (const name of ["", ...new Set(data.map(item => item.category))]) {
        const control = button(name || "All", () => { category = name; search.value = ""; renderCategories(); render(); search.focus(); });
        control.setAttribute("aria-pressed", String(name === category)); categories.append(control);
      }
    };
    search.addEventListener("input", render);
    panel.addEventListener("keydown", event => {
      const target = event.target;
      if (event.key === "Enter" && target === search && shown[active]) { event.preventDefault(); selected(shown[active]!.value, event.shiftKey); return; }
      const step = event.key === "ArrowDown" ? 6 : event.key === "ArrowUp" ? -6 : event.key === "ArrowRight" ? 1 : event.key === "ArrowLeft" ? -1 : 0;
      // Horizontal arrows in a search string retain native caret editing.
      if (!step || (target === search && search.value && Math.abs(step) === 1) || !shown.length) return;
      event.preventDefault(); active = Math.max(0, Math.min(shown.length - 1, active + step)); highlight();
      if (target !== search) grid.querySelectorAll<HTMLButtonElement>("button")[active]?.focus();
    });
    if (options.allowRemove) panel.append(button("Remove emoji", () => selected(false, false), "emoji-remove"));
    overlay = openEditorOverlay({ anchor: trigger, content: panel, environment: options.environment, title: options.label ?? "Choose emoji", ...(options.touch === undefined ? {} : { touch: options.touch }), onClose: () => { generation++; overlay = undefined; trigger.setAttribute("aria-expanded", "false"); } });
    trigger.setAttribute("aria-expanded", "true");
    renderCategories(); render(); search.focus();
    if (options.load) {
      status.textContent = "Loading emoji…"; grid.replaceChildren(); categories.replaceChildren(); search.disabled = true;
      void Promise.resolve().then(options.load).then(raw => {
        if (dead || generation !== request) return;
        data = catalogue(raw); search.disabled = false; renderCategories(); render();
      }).catch(error => { if (!dead && generation === request) { status.textContent = `Emoji could not be loaded: ${errorMessage(error)}`; options.environment.report(error); } });
    }
  };
  const dispose = (): void => { if (dead) return; dead = true; generation++; options.environment.signal?.removeEventListener("abort", abort); overlay?.dispose(); overlay = undefined; trigger.disabled = true; };
  if (options.environment.signal?.aborted) dispose();
  return { node, dispose };
}
