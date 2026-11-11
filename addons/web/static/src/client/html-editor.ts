/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
import { el, errorMessage } from "./dom.js";
import { editorEnvironment, editorOptions, editorText, type EditorEnvironment } from "./editor-types.js";
import { admitHtml, assertStableHtml, exportHtml as serializeHtml, htmlEditorLimits, markHtmlEditorTransient, plainHtml, type HtmlEditorLimits } from "./html-editor-document.js";
import { HtmlEditorHistory } from "./html-editor-history.js";
import { captureSelection, editableRange, restoreSelection, toggleBold, type SelectionBookmark } from "./html-editor-selection.js";
import { ClientError } from "./transport.js";

export interface HtmlEditorOptions {
  environment: EditorEnvironment; label: string; value: string;
  readonly?: boolean; disabled?: boolean; required?: boolean; invalid?: string;
  clipboardMode?: "rich" | "plain-text"; limits?: Partial<HtmlEditorLimits>;
  change(html: string): void | Promise<void>;
}
export interface HtmlEditorHandle {
  /** Owner must not mutate either element directly; the editor refuses edits it did not make. */
  readonly node: HTMLElement; readonly editable: HTMLElement;
  exportHtml(): string; setValue(value: string): void; focus(): void; undo(): void; redo(): void;
  state(): {canUndo: boolean; canRedo: boolean; pending: boolean; dirty: boolean}; dispose(): void;
}
interface Snapshot { html: string; selection: SelectionBookmark | null; protectedPaths: number[][] }
interface ProtectedContent { node: Element; html: string }
interface NativeEdit { before: Snapshot; protectedContent: ProtectedContent[] }
interface Notification { generation: number; html: string }
const PRIVATE_MIME = "application/vnd.erpforall.html-editor+json";
const utf8 = (value: string): number => new TextEncoder().encode(value).length;
const BLOCKS = "p,div,blockquote,pre,ul,ol,li,h1,h2,h3,h4,h5,h6";
const PARAGRAPHS = new Set(["p", "pre", "h1", "h2", "h3", "h4", "h5", "h6"]);
const CONTAINERS = new Set(["div", "blockquote", "li", "ul", "ol"]);

function protectionPaths(root: HTMLElement): number[][] {
  return [...root.querySelectorAll('[contenteditable="false"]')].map(island => {
    const path: number[] = [];
    for (let child: Element = island; child !== root && child.parentElement; child = child.parentElement) path.unshift(Array.prototype.indexOf.call(child.parentElement.children, child));
    return path;
  });
}
function applyProtections(fragment: DocumentFragment, paths: readonly number[][]): void {
  for (const path of paths) {
    let parent: Element | DocumentFragment = fragment;
    for (const index of path) {
      const next: Element | undefined = parent.children[index];
      if (!next) throw new ClientError("validation", "HTML editor protection bookmark no longer matches the document.");
      parent = next;
    }
    if (parent instanceof Element) { markHtmlEditorTransient(parent); parent.setAttribute("contenteditable", "false"); }
  }
}
function insertFragment(root: HTMLElement, range: Range, fragment: DocumentFragment): void {
  // Admitted vocabulary is narrower than a complete HTML editing grammar. Refuse a block
  // below an inline wrapper before moving any live branch; wrapping it in another paragraph
  // would create a tree whose shape changes when history reparses its serialized HTML.
  for (const inline of fragment.querySelectorAll("strong,b,em,i,u,s,code")) {
    if (inline.querySelector(BLOCKS)) throw new ClientError("unsupported", "HTML editor block content inside inline formatting is unsupported until a later release.");
  }
  let insertion = range;
  if (fragment.querySelector(BLOCKS)) {
    const paragraph = (node: Node): HTMLElement | null => {
      let element = node instanceof HTMLElement ? node : node.parentElement;
      while (element && element !== root) {
        if (PARAGRAPHS.has(element.localName)) return element;
        if (CONTAINERS.has(element.localName)) return null;
        element = element.parentElement;
      }
      return null;
    };
    const first = paragraph(range.startContainer), last = paragraph(range.endContainer);
    const splitParagraphs = first !== null && last !== null && first.parentElement === last.parentElement;
    let container = range.commonAncestorContainer instanceof HTMLElement ? range.commonAncestorContainer : range.commonAncestorContainer.parentElement!;
    while (container !== root && !CONTAINERS.has(container.localName)) container = container.parentElement!;
    const list = container.localName === "ul" || container.localName === "ol";
    const blocks = (content: DocumentFragment): DocumentFragment => {
      const output = root.ownerDocument.createDocumentFragment(); let inline: HTMLElement | null = null, item: HTMLElement | null = null;
      for (const child of [...content.childNodes]) {
        if (list && child instanceof Element && child.localName === "li") { inline = null; item = null; output.appendChild(child); continue; }
        const parent = (): Node => {
          if (!list) return output;
          if (!item) { item = root.ownerDocument.createElement("li"); output.append(item); }
          return item;
        };
        if (child instanceof Element && child.matches(BLOCKS)) { inline = null; parent().appendChild(child); }
        else {
          if (!inline) { inline = root.ownerDocument.createElement("p"); parent().appendChild(inline); }
          inline.appendChild(child);
        }
      }
      return output;
    };
    // Refuse shapes the HTML parser would rewrite on the next read (nested li, heading in heading).
    const host = splitParagraphs ? first!.parentElement! : container;
    if (host.closest("h1,h2,h3,h4,h5,h6") && fragment.querySelector("h1,h2,h3,h4,h5,h6")) throw new ClientError("unsupported", "HTML editor cannot place a heading inside a heading.");
    if (!list && host.closest("li") && [...fragment.children].some(child => child.localName === "li")) throw new ClientError("unsupported", "HTML editor cannot place a list item inside a list item.");
    const inserted = blocks(fragment);
    const prefixRange = root.ownerDocument.createRange(), suffixRange = root.ownerDocument.createRange();
    prefixRange.selectNodeContents(splitParagraphs ? first : container); prefixRange.setEnd(range.startContainer, range.startOffset);
    suffixRange.selectNodeContents(splitParagraphs ? last : container); suffixRange.setStart(range.endContainer, range.endOffset);
    const prefix = prefixRange.extractContents(), suffix = suffixRange.extractContents(), replacement = root.ownerDocument.createDocumentFragment();
    if (prefix.textContent || prefix.querySelector("br")) {
      if (splitParagraphs) { const block = first.cloneNode(false); block.appendChild(prefix); replacement.appendChild(block); }
      else replacement.appendChild(blocks(prefix));
    }
    const insertedEnd = inserted.lastChild; replacement.appendChild(inserted);
    if (suffix.textContent || suffix.querySelector("br")) {
      if (splitParagraphs) { const block = last.cloneNode(false); block.appendChild(suffix); replacement.appendChild(block); }
      else replacement.appendChild(blocks(suffix));
    }
    insertion = root.ownerDocument.createRange();
    if (splitParagraphs) { insertion.setStartBefore(first); insertion.setEndAfter(last); }
    else insertion.selectNodeContents(container);
    insertion.deleteContents(); insertion.insertNode(replacement);
    if (insertedEnd) insertion.setStartAfter(insertedEnd);
  } else {
    const end = fragment.lastChild; insertion.deleteContents(); insertion.insertNode(fragment); if (end) insertion.setStartAfter(end);
  }
  insertion.collapse(true); const selection = root.ownerDocument.getSelection(); selection?.removeAllRanges(); selection?.addRange(insertion);
}

/** Local HTML drafts only. Strings remain untrusted; the owner handles durable validation/storage.
 * Declaration and parent import failures throw ClientError. User-event failures retain the accepted
 * document and are shown/reported through the owner's environment. Dispose with the owning view. */
export function createHtmlEditor(options: HtmlEditorOptions): HtmlEditorHandle {
  editorOptions(options, ["environment", "label", "value", "readonly", "disabled", "required", "invalid", "clipboardMode", "limits", "change"], "HTML editor");
  options = {...options};
  editorEnvironment(options.environment);
  options.environment = {...options.environment};
  editorText(options.label, "HTML editor label");
  if (!options.label.trim()) throw new ClientError("validation", "HTML editor label must not be blank.");
  for (const key of ["readonly", "disabled", "required"] as const) if (options[key] !== undefined && typeof options[key] !== "boolean") throw new ClientError("validation", `HTML editor ${key} must be a boolean.`);
  if (options.invalid !== undefined) editorText(options.invalid, "HTML editor invalid message");
  if (typeof options.change !== "function") throw new ClientError("validation", "HTML editor change must be a function.");
  if (options.clipboardMode !== undefined && options.clipboardMode !== "rich" && options.clipboardMode !== "plain-text") throw new ClientError("validation", "HTML editor clipboard mode must be rich or plain-text.");
  const limits = htmlEditorLimits(options.limits), admitted = admitHtml(document, options.value, limits, "initial");
  const node = el("section", undefined, "html-editor"), toolbar = el("div", undefined, "html-editor-toolbar");
  toolbar.setAttribute("role", "group"); toolbar.setAttribute("aria-label", "Text formatting and history");
  const editable = el("div", undefined, "html-editor-content");
  editable.setAttribute("contenteditable", String(!options.readonly && !options.disabled));
  editable.setAttribute("role", "textbox"); editable.setAttribute("aria-multiline", "true"); editable.setAttribute("aria-label", options.label);
  editable.setAttribute("aria-readonly", String(Boolean(options.readonly))); editable.setAttribute("aria-disabled", String(Boolean(options.disabled)));
  editable.setAttribute("aria-required", String(Boolean(options.required))); editable.tabIndex = options.disabled ? -1 : 0;
  editable.append(admitted.fragment);
  const status = el("p", "", "html-editor-status"), alert = el("p", options.invalid ?? "", "html-editor-error");
  status.setAttribute("role", "status"); status.setAttribute("aria-live", "polite"); alert.setAttribute("role", "alert"); alert.hidden = !options.invalid;
  const controls = {bold: el("button", "Bold"), undo: el("button", "Undo"), redo: el("button", "Redo")};
  for (const [id, control] of Object.entries(controls)) { control.type = "button"; control.dataset["htmlCommand"] = id; toolbar.append(control); }
  controls.bold.setAttribute("aria-pressed", "false"); node.append(toolbar, editable, status, alert);
  let accepted: Snapshot = {html: admitted.html, selection: null, protectedPaths: []}, baseline = admitted.html;
  const history = new HtmlEditorHistory<Snapshot>(accepted, limits, value => utf8(JSON.stringify(value)));
  let dead = false, generation = 0, composition: {generation: number; edit: NativeEdit} | null = null;
  let beforeInput: NativeEdit | null = null, staged: SelectionBookmark | null = null, notice = "";
  let flight: Notification | null = null, latest: Notification | null = null;
  const stops: (() => void)[] = [];
  const live = (): void => { if (dead) throw new ClientError("validation", "The HTML editor is disposed."); };
  const writable = (): void => { live(); if (options.readonly) throw new ClientError("validation", "The HTML editor is readonly."); if (options.disabled) throw new ClientError("validation", "The HTML editor is disabled."); };
  const pending = (): boolean => flight?.generation === generation || latest?.generation === generation;
  const state = (): ReturnType<HtmlEditorHandle["state"]> => ({canUndo: !dead && history.canUndo, canRedo: !dead && history.canRedo, pending: !dead && pending(), dirty: accepted.html !== baseline});
  const selectedBold = (): boolean => {
    const range = editableRange(editable);
    if (!range || range.collapsed) return false;
    const walker = document.createTreeWalker(editable, NodeFilter.SHOW_TEXT); let text = walker.nextNode(), found = false;
    while (text) {
      const from = text === range.startContainer ? range.startOffset : 0, to = text === range.endContainer ? range.endOffset : (text.textContent ?? "").length;
      if (range.intersectsNode(text) && to > from) {
        let parent = text.parentElement, bold = false;
        while (parent && parent !== editable) { if (parent.localName === "strong" || parent.localName === "b") bold = true; parent = parent.parentElement; }
        if (!bold) return false; found = true;
      }
      text = walker.nextNode();
    }
    return found;
  };
  const update = (): void => {
    const locked = dead || Boolean(options.readonly || options.disabled), range = editableRange(editable);
    controls.bold.disabled = locked || !staged || !range || range.collapsed || composition !== null;
    controls.bold.dataset["selection"] = String(!controls.bold.disabled);
    controls.bold.setAttribute("aria-pressed", String(!controls.bold.disabled && selectedBold()));
    controls.undo.disabled = locked || !history.canUndo || composition !== null; controls.redo.disabled = locked || !history.canRedo || composition !== null;
    editable.setAttribute("aria-busy", String(pending() && !dead)); editable.setAttribute("aria-invalid", String(!alert.hidden));
    status.textContent = notice || (pending() && !dead ? "Notifying owner…" : accepted.html !== baseline ? "Unsaved local draft." : "");
  };
  const fail = (error: unknown): void => { if (dead) return; alert.hidden = false; alert.textContent = errorMessage(error); update(); options.environment.report(error); };
  const present = (snapshot: Snapshot): void => {
    const documentValue = admitHtml(document, snapshot.html, limits, "initial");
    applyProtections(documentValue.fragment, snapshot.protectedPaths);
    editable.replaceChildren(documentValue.fragment); accepted = snapshot; staged = snapshot.selection;
    if (snapshot.selection) restoreSelection(editable, snapshot.selection);
  };
  const finishNotification = (job: Notification, outcome: {ok: true} | {ok: false; error: unknown}): void => {
    if (flight !== job) return;
    flight = null;
    if (!dead && job.generation === generation && !outcome.ok) fail(outcome.error);
    if (!dead) { update(); pump(); }
  };
  const pump = (): void => {
    if (dead || flight || !latest) return;
    const job = latest; latest = null; flight = job; update();
    try {
      const result = options.change(job.html);
      if (result !== undefined) void Promise.resolve(result).then(() => finishNotification(job, {ok: true}), error => finishNotification(job, {ok: false, error}));
      else finishNotification(job, {ok: true});
    } catch (error) { finishNotification(job, {ok: false, error}); }
  };
  const notify = (): void => { latest = {generation, html: accepted.html}; pump(); update(); };
  const currentBefore = (): Snapshot => {
    // Export admission proves these private metadata addresses are owner-marked. Never grant
    // protection capability to an unchecked attribute left by a native/user DOM insertion.
    if (serializeHtml(editable, limits) !== accepted.html) throw new ClientError("validation", "HTML editor document changed outside an accepted edit.");
    return {html: accepted.html, selection: captureSelection(editable) ?? accepted.selection, protectedPaths: protectionPaths(editable)};
  };
  const protectedContent = (): ProtectedContent[] => [...editable.querySelectorAll('[contenteditable="false"]')]
    .filter(island => !island.parentElement?.closest('[contenteditable="false"]'))
    .map(island => ({node: island, html: serializeHtml(island as HTMLElement, limits)}));
  const nativeEdit = (): NativeEdit => ({before: currentBefore(), protectedContent: protectedContent()});
  const accept = (before: Snapshot, mutate?: () => boolean | void, protectedBefore?: readonly ProtectedContent[]): void => {
    writable();
    try {
      // Fail closed: a native input with no recorded protected content cannot be shown safe.
      if (!protectedBefore && !mutate && before.protectedPaths.length) throw new ClientError("validation", "HTML editor edits cannot change protected content.");
      const guards = protectedBefore ?? (mutate ? protectedContent() : []);
      history.replaceCurrent(before);
      if (mutate && mutate() === false) return;
      for (const guard of guards) {
        if (!editable.contains(guard.node) || guard.node.getAttribute("contenteditable") !== "false" || serializeHtml(guard.node as HTMLElement, limits) !== guard.html) throw new ClientError("validation", "HTML editor edits cannot change protected content.");
      }
      const next: Snapshot = {html: serializeHtml(editable, limits), selection: captureSelection(editable), protectedPaths: protectionPaths(editable)};
      if (next.html === before.html) { accepted = next; update(); return; }
      assertStableHtml(document, next.html, limits);
      const result = history.commit(next); accepted = next; staged = next.selection;
      alert.hidden = true; alert.textContent = ""; notice = result.shortened ? "Earlier undo history was shortened to stay within the editor limit." : "";
      update(); notify();
    } catch (error) {
      try { present(before); } catch (recovery) { update(); fail(recovery); }
      update(); fail(error);
    }
  };
  const commandBold = (): void => {
    if (dead || options.readonly || options.disabled || composition || !staged) return;
    restoreSelection(editable, staged);
    if (!editableRange(editable)) return;
    accept(currentBefore(), () => toggleBold(editable));
  };
  const undo = (): void => {
    writable(); if (composition) return;
    history.replaceCurrent(currentBefore()); const previous = history.peekUndo();
    if (previous) { present(previous); history.undo(); notice = ""; update(); notify(); }
  };
  const redo = (): void => {
    writable(); if (composition) return;
    history.replaceCurrent(currentBefore()); const next = history.peekRedo();
    if (next) { present(next); history.redo(); notice = ""; update(); notify(); }
  };
  function listen<K extends keyof HTMLElementEventMap>(target: HTMLElement, name: K, handler: (event: HTMLElementEventMap[K]) => void): void {
    target.addEventListener(name, handler); stops.push(() => target.removeEventListener(name, handler));
  }
  const safe = (job: () => void): void => { try { job(); } catch (error) { fail(error); } };
  const selectionChanged = (): void => {
    if (dead || composition) return;
    staged = captureSelection(editable);
    update();
  };
  document.addEventListener("selectionchange", selectionChanged); stops.push(() => document.removeEventListener("selectionchange", selectionChanged));
  listen(node, "focusout", event => { if (!(event.relatedTarget instanceof Node) || !node.contains(event.relatedTarget)) { staged = null; update(); } });
  listen(editable, "focus", selectionChanged);
  for (const event of ["pointerdown", "mousedown"] as const) listen(controls.bold, event, event => { selectionChanged(); event.preventDefault(); });
  listen(controls.bold, "click", () => safe(commandBold)); listen(controls.undo, "click", () => safe(undo)); listen(controls.redo, "click", () => safe(redo));
  listen(editable, "keydown", event => {
    if (dead || options.readonly || options.disabled || composition || event.altKey || !(event.ctrlKey || event.metaKey)) return;
    const key = event.key.toLowerCase();
    if (key === "b" && !event.shiftKey) { event.preventDefault(); selectionChanged(); safe(commandBold); }
    else if (key === "z") { event.preventDefault(); safe(event.shiftKey ? redo : undo); }
    else if (key === "y" && event.ctrlKey && !event.metaKey && !event.shiftKey) { event.preventDefault(); safe(redo); }
  });
  listen(editable, "beforeinput", event => {
    if (dead || options.readonly || options.disabled) { event.preventDefault(); return; }
    if (["insertFromPaste", "insertFromDrop", "insertFromYank"].includes(event.inputType)) { event.preventDefault(); return; }
    // Bold, undo and redo are the only routed commands; every other native format command
    // would let the engine write tags and attributes (i, u, style, dir) that export refuses.
    if (event.inputType.startsWith("format")) { event.preventDefault(); return; }
    if (event.inputType === "historyUndo" || event.inputType === "historyRedo") { event.preventDefault(); safe(event.inputType === "historyUndo" ? undo : redo); return; }
    if (!editableRange(editable)) { event.preventDefault(); return; }
    if (!composition) {
      try { beforeInput = nativeEdit(); }
      catch (error) { event.preventDefault(); fail(error); }
    }
  });
  listen(editable, "input", () => {
    if (dead || composition) return;
    safe(() => {
      if (options.readonly || options.disabled) { present(accepted); return; }
      const before = beforeInput; beforeInput = null; accept(before?.before ?? accepted, undefined, before?.protectedContent);
    });
  });
  listen(editable, "compositionstart", () => safe(() => { if (!dead && !options.readonly && !options.disabled) { composition = {generation, edit: nativeEdit()}; beforeInput = null; update(); } }));
  listen(editable, "compositionend", () => {
    const current = composition; composition = null;
    safe(() => { if (!dead && current && current.generation === generation) accept(current.edit.before, undefined, current.edit.protectedContent); });
    update();
  });
  const attachmentFailure = (): never => { throw new ClientError("unsupported", "HTML editor file paste and drop require attachment support (a later release)."); };
  listen(editable, "drop", event => { event.preventDefault(); if (!dead && !options.readonly && !options.disabled) safe(attachmentFailure); });
  listen(editable, "dragover", event => event.preventDefault());
  listen(editable, "paste", event => {
    event.preventDefault();
    if (dead || options.readonly || options.disabled || composition || !editableRange(editable)) return;
    safe(() => {
      const data = event.clipboardData; if (!data) throw new ClientError("unsupported", "HTML clipboard data is unavailable.");
      if (data.files.length) attachmentFailure();
      let source = data.getData("text/html"), simplified = false;
      if (options.clipboardMode !== "plain-text" && data.types.includes(PRIVATE_MIME)) {
        const raw = data.getData(PRIVATE_MIME); let payload: unknown;
        if (raw.length > limits.inputBytes || utf8(raw) > limits.inputBytes) throw new ClientError("validation", "Invalid ERPForAll HTML clipboard payload.");
        try { payload = JSON.parse(raw); } catch { throw new ClientError("validation", "Invalid ERPForAll HTML clipboard payload."); }
        if (!payload || typeof payload !== "object" || Array.isArray(payload) || Reflect.ownKeys(payload).length !== 2 || !("version" in payload) || payload.version !== 1 || !("html" in payload) || typeof payload.html !== "string") throw new ClientError("validation", "Invalid ERPForAll HTML clipboard payload.");
        source = payload.html;
      }
      let fragment: DocumentFragment;
      if (options.clipboardMode === "plain-text" || !source) fragment = plainHtml(document, data.getData("text/plain"), limits);
      else { const result = admitHtml(document, source, limits, "paste"); fragment = result.fragment; simplified = result.simplified; }
      if (!fragment.childNodes.length) { if (simplified) { notice = "Pasted formatting was simplified."; update(); } return; }
      accept(currentBefore(), () => {
        const range = editableRange(editable); if (!range) return false;
        insertFragment(editable, range, fragment);
        return true;
      });
      if (simplified) { notice = "Pasted formatting was simplified."; update(); }
    });
  });
  const clipboardOut = (event: ClipboardEvent, cut: boolean): void => {
    if (cut) event.preventDefault();
    if (dead || composition) return;
    const range = editableRange(editable); if (!range || range.collapsed) return;
    event.preventDefault(); if (cut && (options.readonly || options.disabled)) return;
    safe(() => {
      if (!event.clipboardData) throw new ClientError("unsupported", "HTML clipboard data is unavailable.");
      let fragment = range.cloneContents();
      let ancestor = range.commonAncestorContainer.nodeType === Node.ELEMENT_NODE ? range.commonAncestorContainer as Element : range.commonAncestorContainer.parentElement;
      while (ancestor && ancestor !== editable && ["strong", "b", "em", "i", "u", "s", "code"].includes(ancestor.localName)) {
        const wrapper = ancestor.cloneNode(false); wrapper.appendChild(fragment); fragment = document.createDocumentFragment(); fragment.appendChild(wrapper); ancestor = ancestor.parentElement;
      }
      const html = serializeHtml(fragment, limits);
      event.clipboardData.setData("text/plain", range.toString()); event.clipboardData.setData("text/html", html);
      event.clipboardData.setData(PRIVATE_MIME, JSON.stringify({version: 1, html}));
      if (cut) accept(currentBefore(), () => { range.deleteContents(); range.collapse(true); });
    });
  };
  listen(editable, "copy", event => clipboardOut(event, false)); listen(editable, "cut", event => clipboardOut(event, true));
  const dispose = (): void => {
    if (dead) return; dead = true; generation++; flight = null; latest = null; composition = null; beforeInput = null; staged = null;
    for (const stop of stops.splice(0)) stop(); options.environment.signal?.removeEventListener("abort", dispose); history.clear();
    editable.setAttribute("contenteditable", "false"); editable.tabIndex = -1; notice = ""; update();
  };
  options.environment.signal?.addEventListener("abort", dispose, {once: true});
  if (options.environment.signal?.aborted) dispose(); else update();
  return {node, editable, state, dispose, undo, redo,
    focus: () => { live(); editable.focus(); },
    exportHtml: () => { live(); return serializeHtml(editable, limits); },
    setValue: value => {
      live(); const replacement = admitHtml(document, value, limits, "initial"), snapshot: Snapshot = {html: replacement.html, selection: null, protectedPaths: []};
      history.reset(snapshot); generation++; flight = null; latest = null; composition = null; beforeInput = null; staged = null;
      editable.replaceChildren(replacement.fragment); accepted = snapshot; baseline = snapshot.html; notice = "";
      alert.textContent = options.invalid ?? ""; alert.hidden = !options.invalid; update();
    },
  };
}
