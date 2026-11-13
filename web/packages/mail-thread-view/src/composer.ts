/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
import { effect, untrack } from "@erpforall/elang";
import { checkBaseOptions, createContext, t, type ThreadViewBaseOptions } from "./context";
import { partnerName } from "./contract";
import { Announcer, formatBytes, glyphNode, h, uniqueId, SR_ONLY } from "./dom";
import { ThreadViewError } from "./errors";
import { reconcileMentions, type MentionRange } from "./mentions";
import { submitPost, type PostInput } from "./posting";
import type { ComposerAction, ComposerActionContext, ComposerState, Suggestion, SuggestionProvider, ThreadViewContext } from "./registries";
import { draftMentions, threadState, type ComposerKind } from "./state";
import { findTrigger, type Trigger } from "./suggestions";

/** Options of a {@link Composer}. */
export interface ComposerOptions extends ThreadViewBaseOptions {
  /** Enter sends and Shift+Enter breaks the line (Discuss); default false: Ctrl+Enter (Cmd+Enter on a Mac) sends (chatter). */
  readonly sendOnEnter?: boolean;
  /** `mac` makes Cmd+Enter the chord; default is detected from the browser. */
  readonly platform?: "mac" | "other";
  /** Post without push or out-of-focus alerts (5). */
  readonly silent?: boolean;
}

/** Maximum number of attachments permitted on one message. */
export const MAX_ATTACHMENTS = 100;

/** Maximum bytes per uploaded attachment file (25 MiB). */
export const MAX_FILE_SIZE_BYTES = 25 * 1024 * 1024;

const COMPOSER_OPTION_KEYS = ["sendOnEnter", "platform", "silent"];

/** What `send()` did: `sent`, `failed` (left as an unsent row) or `refused` (the draft is not valid; it is kept). */
export type SendOutcome = "sent" | "failed" | "refused";

/**
 * The composer: one plain-text draft with structured mentions, the kind switch (Send message / Log note), the action
 * row from the `mail.composerActions` registry and the Send button. The draft, kind, mentions and reply target are
 * local records of the store, so a rebuilt composer keeps them.
 *
 * Patch points (typed `patch(Composer, …)`): {@link Composer.canSend}, {@link Composer.isSendChord},
 * {@link Composer.placeholder}, {@link Composer.buildPost}.
 */
export class Composer {
  /** The composer's root element (a form). */
  readonly element: HTMLElement;
  readonly #options: ComposerOptions;
  readonly #view: ThreadViewContext;
  readonly #mac: boolean;
  readonly #announcer: Announcer;
  readonly #field: HTMLTextAreaElement;
  readonly #send: HTMLButtonElement;
  readonly #kindButtons: Record<ComposerKind, HTMLButtonElement>;
  readonly #actionsRow: HTMLElement;
  readonly #replyChip: HTMLElement;
  readonly #attachmentsArea: HTMLElement;
  readonly #fileInput: HTMLInputElement;
  readonly #errorLine: HTMLElement;
  readonly #list: HTMLElement;
  readonly #listId = uniqueId("erp-mail-suggestions");
  readonly #stops: (() => void)[] = [];
  readonly #refusedProviders = new Set<string>();
  #attachments: Array<{ id: number; name: string; size?: number; mimetype?: string }> = [];
  #hidden: { id: string; reason: string }[] = [];
  #items: Suggestion[] = [];
  #trigger: Trigger | null = null;
  #provider: SuggestionProvider | null = null;
  #active = 0;
  #search: AbortController | null = null;
  #searchSeq = 0;
  #disposed = false;

  constructor(options: ComposerOptions) {
    checkBaseOptions(options, COMPOSER_OPTION_KEYS, "Composer");
    for (const key of ["sendOnEnter", "silent"] as const) {
      if (options[key] !== undefined && typeof options[key] !== "boolean") throw new ThreadViewError("bad_option", `options.${key} must be a boolean`);
    }
    if (options.platform !== undefined && options.platform !== "mac" && options.platform !== "other") {
      throw new ThreadViewError("bad_option", `options.platform must be "mac" or "other", got ${JSON.stringify(options.platform)}`);
    }
    this.#options = options;
    const doc = options.document ?? document;
    this.#announcer = new Announcer(doc);
    this.#view = createContext(options, (text, urgency) => this.#announcer.say(text, urgency));
    const view = this.#view;
    this.#mac = options.platform !== undefined ? options.platform === "mac" : typeof navigator !== "undefined" && /Mac|iPhone|iPad/.test(navigator.platform);
    const fieldId = uniqueId("erp-mail-draft");
    const hintId = uniqueId("erp-mail-hint");
    const chord = options.sendOnEnter === true ? t(view, "Enter") : this.#mac ? t(view, "Command+Enter") : t(view, "Control+Enter");
    const label = h(doc, "label", { class: SR_ONLY, for: fieldId }, t(view, "Message"));
    this.#field = h(doc, "textarea", {
      class: "erp-mail-draft",
      id: fieldId,
      rows: 3,
      "aria-describedby": hintId,
      "aria-autocomplete": "list",
      "aria-controls": this.#listId,
    });
    const hint = h(doc, "span", { class: "erp-mail-hint", id: hintId }, t(view, "Press %s to send", chord));
    this.#list = h(doc, "ul", { class: "erp-mail-suggestions", id: this.#listId, role: "listbox", hidden: true });
    this.#kindButtons = {
      message: h(doc, "button", { type: "button", class: "erp-mail-kind", "data-kind": "message" }, t(view, "Send message")),
      note: h(doc, "button", { type: "button", class: "erp-mail-kind", "data-kind": "note" }, t(view, "Log note")),
    };
    this.#kindButtons.message.addEventListener("click", () => this.setKind("message"));
    this.#kindButtons.note.addEventListener("click", () => this.setKind("note"));
    const kinds = h(doc, "div", { class: "erp-mail-kinds", role: "group", "aria-label": t(view, "Post type") }, this.#kindButtons.message, this.#kindButtons.note);
    this.#replyChip = h(doc, "div", { class: "erp-mail-reply", hidden: true });
    this.#attachmentsArea = h(doc, "div", { class: "erp-mail-composer-attachments", role: "group", "aria-label": t(view, "Attached files"), hidden: true });
    this.#fileInput = h(doc, "input", { type: "file", multiple: true, class: "erp-mail-file-input", hidden: true }) as HTMLInputElement;
    this.#fileInput.addEventListener("change", () => void this.#onFilesSelected());
    this.#errorLine = h(doc, "div", { class: "erp-mail-composer-error", role: "alert", hidden: true });
    this.#actionsRow = h(doc, "div", { class: "erp-mail-composer-actions" });
    this.#send = h(doc, "button", { type: "button", class: "erp-mail-send primary" });
    this.#send.addEventListener("click", () => void this.send());
    this.element = h(doc, "form", { class: "erp-mail-composer", "aria-label": t(view, "Message composer"), novalidate: true },
      this.#announcer.element, kinds, this.#replyChip, label, this.#field, this.#attachmentsArea, this.#list, this.#errorLine,
      // Send, the hint, then the tool icons: the document order is the visual order and the Tab order.
      h(doc, "div", { class: "erp-mail-composer-bar" }, this.#send, hint, this.#actionsRow), this.#fileInput);
    this.element.addEventListener("submit", (event) => {
      event.preventDefault();
      void this.send();
    });
    this.#field.addEventListener("input", () => this.#onInput());
    this.#field.addEventListener("keydown", (event) => this.#onKeydown(event));
    this.#field.addEventListener("click", () => void this.#updateSuggestions());
    this.#field.addEventListener("keyup", (event) => {
      if (event.key === "ArrowLeft" || event.key === "ArrowRight" || event.key === "Home" || event.key === "End") void this.#updateSuggestions();
    });
    this.#field.addEventListener("blur", () => this.#closeSuggestions());
    this.#list.addEventListener("mousedown", (event) => event.preventDefault());
    this.#stops.push(effect(() => this.#syncDraft()));
    this.#stops.push(effect(() => this.#syncKind()));
    this.#stops.push(effect(() => this.#syncReply()));
    this.#stops.push(effect(() => this.#syncActions()));
  }

  // ---- patch points

  /** Whether the draft can be sent: it holds text other than whitespace or staged attachments. */
  canSend(): boolean {
    return untrack(() => this.#text().trim() !== "" || this.#attachments.length > 0);
  }

  /** Whether a key event is the send chord: Ctrl/Cmd+Enter, or Enter alone with `sendOnEnter`. IME composition never sends. */
  isSendChord(event: KeyboardEvent): boolean {
    if (event.key !== "Enter" || event.isComposing || event.keyCode === 229) return false;
    if (this.#options.sendOnEnter === true) return !event.shiftKey && !event.ctrlKey && !event.metaKey && !event.altKey;
    return !event.altKey && !event.shiftKey && (this.#mac ? event.metaKey : event.ctrlKey);
  }

  /** The field's placeholder for the current kind. */
  placeholder(kind: ComposerKind): string {
    return kind === "note" ? t(this.#view, "Log an internal note...") : t(this.#view, "Send a message to followers...");
  }

  /** The draft as the input of a post. Add a subject or recipients by patching this. */
  buildPost(): PostInput {
    const state = this.#state();
    return {
      text: state.text,
      mentions: state.mentions,
      kind: state.kind,
      replyTo: state.replyTo,
      attachmentIds: this.#attachments.map((a) => a.id),
      ...(this.#options.silent === true ? { silent: true } : {}),
    };
  }

  // ---- public

  /** The composer's context. */
  get context(): ThreadViewContext {
    return this.#view;
  }

  /** The draft field (the host focuses it). */
  get field(): HTMLTextAreaElement {
    return this.#field;
  }

  /** The current staged attachments on this composer. */
  get attachments(): readonly { id: number; name: string; size?: number; mimetype?: string }[] {
    return [...this.#attachments];
  }

  /** Stage an attachment onto the draft. */
  attach(attachment: { id: number; name: string; size?: number; mimetype?: string }): void {
    this.#alive();
    if (this.#attachments.length >= MAX_ATTACHMENTS) {
      this.#showError(t(this.#view, "Cannot attach more than %s files", MAX_ATTACHMENTS));
      return;
    }
    if (attachment.size !== undefined && attachment.size > MAX_FILE_SIZE_BYTES) {
      this.#showError(t(this.#view, 'File "%s" exceeds maximum allowed size of 25 MiB', attachment.name));
      return;
    }
    this.#attachments.push(attachment);
    this.#renderAttachmentChips();
    this.#syncDraft();
  }

  /** Remove a staged attachment from the draft. */
  removeAttachment(id: number): void {
    this.#alive();
    const idx = this.#attachments.findIndex((a) => a.id === id);
    if (idx !== -1) {
      this.#attachments.splice(idx, 1);
      this.#renderAttachmentChips();
      this.#syncDraft();
    }
  }

  /** Trigger file selection dialog. */
  chooseFiles(): void {
    this.#alive();
    this.#fileInput.click();
  }

  /** The actions hidden by the last render because the deployment lacks what they need, with the reasons. */
  hiddenActions(): readonly { id: string; reason: string }[] {
    return [...this.#hidden];
  }

  /**
   * Switch between "Send message" and "Log note" (the draft is kept).
   *
   * @param kind - the kind
   * @throws {@link ThreadViewError} `disposed` after `dispose()`
   */
  setKind(kind: ComposerKind): void {
    this.#alive();
    const { model, id } = threadState(this.#view.store, this.#view.ref);
    model.set(id, { kind });
  }

  /**
   * Send the draft. A draft that is not valid (empty, a broken mention, too many unsent posts) is refused and kept,
   * with the reason shown; a failed post stays as an unsent row of the thread.
   *
   * @throws {@link ThreadViewError} `disposed` after `dispose()`
   */
  async send(): Promise<SendOutcome> {
    this.#alive();
    this.#closeSuggestions();
    if (!this.canSend()) {
      this.#view.announce(t(this.#view, "Nothing to send"));
      return "refused";
    }
    try {
      this.#showError(null);
      return await submitPost(this.#view, this.buildPost(), () => this.#clearDraft());
    } catch (error) {
      this.#showError(error instanceof Error ? error.message : String(error));
      this.#view.reportError(error);
      return "refused";
    }
  }

  /** Remove listeners and effects and the element. Idempotent. */
  dispose(): void {
    if (this.#disposed) return;
    this.#disposed = true;
    this.#search?.abort();
    for (const stop of this.#stops) stop();
    this.element.remove();
  }

  // ---- internals

  #alive(): void {
    if (this.#disposed) throw new ThreadViewError("disposed", "this Composer was disposed");
  }

  #text(): string {
    const { model, id } = threadState(this.#view.store, this.#view.ref);
    return String(model.get(id, "text") ?? "");
  }

  #state(): ComposerState {
    const { model, id } = threadState(this.#view.store, this.#view.ref);
    return {
      text: String(untrack(() => model.get(id, "text")) ?? ""),
      kind: untrack(() => model.get(id, "kind")) === "note" ? "note" : "message",
      mentions: draftMentions(untrack(() => model.get(id, "mentions"))),
      replyTo: (untrack(() => model.get(id, "reply_to")) as number | null | undefined) ?? null,
    };
  }

  #write(values: Record<string, unknown>): void {
    const { model, id } = threadState(this.#view.store, this.#view.ref);
    model.set(id, values);
  }

  #clearDraft(): void {
    this.#attachments = [];
    this.#renderAttachmentChips();
    this.#write({ text: "", mentions: [], reply_to: null });
  }

  #syncDraft(): void {
    const { model, id } = threadState(this.#view.store, this.#view.ref);
    const text = String(model.get(id, "text") ?? "");
    if (this.#field.value !== text) this.#field.value = text;
    const sendable = (text.trim() !== "" || this.#attachments.length > 0) && this.canSend();
    this.#send.setAttribute("aria-disabled", sendable ? "false" : "true");
  }

  #syncKind(): void {
    const { model, id } = threadState(this.#view.store, this.#view.ref);
    const kind: ComposerKind = model.get(id, "kind") === "note" ? "note" : "message";
    for (const key of ["message", "note"] as const) this.#kindButtons[key].setAttribute("aria-pressed", key === kind ? "true" : "false");
    this.#send.textContent = kind === "note" ? t(this.#view, "Log") : t(this.#view, "Send");
    this.#field.placeholder = this.placeholder(kind);
  }

  #syncReply(): void {
    const view = this.#view;
    const { model, id } = threadState(view.store, view.ref);
    const target = model.get(id, "reply_to") as number | null | undefined;
    this.#replyChip.replaceChildren();
    if (target === null || target === undefined) {
      this.#replyChip.hidden = true;
      return;
    }
    const author = view.store.record(view.store.config.messageModel, target).get("author_id");
    const name = typeof author === "number" ? partnerName(view.store, author) ?? `Partner #${author}` : t(view, "message");
    const cancel = h(view.document, "button", { type: "button", class: "erp-mail-reply-cancel", "aria-label": t(view, "Cancel reply to %s", name) }, t(view, "Cancel"));
    cancel.addEventListener("click", () => {
      this.#write({ reply_to: null });
      this.#field.focus();
    });
    this.#replyChip.append(h(view.document, "span", {}, t(view, "Replying to %s", name)), " ", cancel);
    this.#replyChip.hidden = false;
  }

  #syncActions(): void {
    const view = this.#view;
    const doc = view.document;
    const { model, id } = threadState(view.store, view.ref);
    model.get(id, "kind");
    this.#hidden = [];
    const buttons: HTMLElement[] = [];
    for (const [actionId, action] of view.registries.composerActions.getEntries()) {
      const button = h(doc, "button", { type: "button", class: "erp-mail-composer-action", "data-action": actionId });
      const ctx = this.#actionContext(button);
      try {
        if (action.condition !== undefined && !untrack(() => (action as ComposerAction).condition!(ctx))) continue;
        const reason = untrack(() => action.unavailable?.(ctx) ?? null);
        if (reason !== null) {
          this.#hidden.push({ id: actionId, reason });
          continue;
        }
        const actionLabel = untrack(() => action.label(ctx));
        button.title = actionLabel;
        button.append(...(action.glyph !== undefined ? [glyphNode(doc, action.glyph)] : []), h(doc, "span", { class: "erp-mail-action-label" }, actionLabel));
      } catch (error) {
        view.reportError(error);
        continue;
      }
      button.addEventListener("click", () => void this.#runAction(action, button));
      buttons.push(button);
    }
    this.#actionsRow.replaceChildren(...buttons);
  }

  #actionContext(anchor: HTMLElement): ComposerActionContext {
    const read = (): ComposerState => this.#state();
    return {
      view: this.#view,
      get state(): ComposerState {
        return read();
      },
      anchor,
      insertText: (text: string) => this.#insert(text),
      send: async () => {
        await this.send();
      },
      chooseFiles: () => this.chooseFiles(),
    };
  }

  async #onFilesSelected(): Promise<void> {
    const files = Array.from(this.#fileInput.files ?? []);
    this.#fileInput.value = "";
    if (files.length === 0) return;

    if (this.#attachments.length + files.length > MAX_ATTACHMENTS) {
      this.#showError(t(this.#view, "Cannot attach more than %s files", MAX_ATTACHMENTS));
      return;
    }

    for (const file of files) {
      if (file.size > MAX_FILE_SIZE_BYTES) {
        this.#showError(t(this.#view, 'File "%s" exceeds maximum allowed size of 25 MiB', file.name));
        return;
      }
    }

    const upload = this.#view.services.uploadAttachment;
    if (upload === undefined) {
      this.#showError(t(this.#view, "Attachment upload service is not available"));
      return;
    }

    try {
      this.#showError(null);
      for (const file of files) {
        const result = await upload(file);
        this.#attachments.push({
          id: result.id,
          name: result.name,
          size: result.size ?? file.size,
          mimetype: result.mimetype ?? file.type,
        });
        // Per file: a later file that fails must not leave an earlier one attached without a chip.
        this.#renderAttachmentChips();
        this.#syncDraft();
      }
    } catch (error) {
      this.#showError(error instanceof Error ? error.message : String(error));
      this.#view.reportError(error);
    }
  }

  #renderAttachmentChips(): void {
    const doc = this.#view.document;
    this.#attachmentsArea.replaceChildren();
    if (this.#attachments.length === 0) {
      this.#attachmentsArea.hidden = true;
      return;
    }
    this.#attachmentsArea.hidden = false;
    for (const att of this.#attachments) {
      const chip = h(doc, "div", { class: "erp-mail-attachment-chip", "data-id": String(att.id) });
      const name = h(doc, "span", { class: "erp-mail-attachment-chip-name" }, att.name);
      chip.append(name);
      if (att.size !== undefined) {
        const size = h(doc, "span", { class: "erp-mail-attachment-chip-size" }, ` (${formatBytes(att.size)})`);
        chip.append(size);
      }
      const removeBtn = h(doc, "button", {
        type: "button",
        class: "erp-mail-attachment-chip-remove",
        "aria-label": t(this.#view, "Remove %s", att.name),
      }, "×");
      removeBtn.addEventListener("click", () => this.removeAttachment(att.id));
      chip.append(removeBtn);
      this.#attachmentsArea.append(chip);
    }
  }

  async #runAction(action: ComposerAction, button: HTMLElement): Promise<void> {
    try {
      this.#showError(null);
      await action.run(this.#actionContext(button));
    } catch (error) {
      this.#showError(error instanceof Error ? error.message : String(error));
      this.#view.reportError(error);
    }
  }

  #showError(text: string | null): void {
    this.#errorLine.textContent = text ?? "";
    this.#errorLine.hidden = text === null;
    if (text !== null) this.#view.announce(text, "assertive");
  }

  /** Replace the draft text, keeping mentions valid, and move the caret. */
  #commit(next: string, caret: number, extra?: MentionRange): void {
    const state = this.#state();
    const kept = reconcileMentions(state.mentions, state.text, next);
    this.#write({ text: next, mentions: extra === undefined ? kept : [...kept, extra] });
    if (this.#field.value !== next) this.#field.value = next;
    this.#field.setSelectionRange(caret, caret);
  }

  #insert(text: string): void {
    const state = this.#state();
    const start = this.#field.selectionStart ?? state.text.length;
    const end = this.#field.selectionEnd ?? start;
    this.#commit(state.text.slice(0, start) + text + state.text.slice(end), start + text.length);
    this.#field.focus();
    void this.#updateSuggestions();
  }

  #onInput(): void {
    const state = this.#state();
    const next = this.#field.value;
    this.#write({ text: next, mentions: reconcileMentions(state.mentions, state.text, next) });
    void this.#updateSuggestions();
  }

  async #updateSuggestions(): Promise<void> {
    if (this.#disposed) return;
    const view = this.#view;
    const caret = this.#field.selectionStart ?? this.#field.value.length;
    const registry = view.registries.suggestionProviders;
    const delimiters: string[] = [];
    for (const [key, entry] of registry.getEntries()) {
      if (key === entry.delimiter) {
        delimiters.push(key);
      } else if (!this.#refusedProviders.has(key)) {
        // A provider registered under another key than its delimiter cannot be found by its trigger: refuse it, once.
        this.#refusedProviders.add(key);
        view.reportError(new ThreadViewError("registry", `mail.suggestionProviders entry "${key}" has delimiter "${entry.delimiter}": the key must equal the delimiter, so the provider is ignored`));
      }
    }
    const trigger = findTrigger(this.#field.value, caret, delimiters);
    if (trigger === null) {
      this.#closeSuggestions();
      return;
    }
    const provider = registry.get(trigger.delimiter);
    this.#search?.abort();
    const controller = new AbortController();
    this.#search = controller;
    const seq = (this.#searchSeq += 1);
    let found: readonly Suggestion[];
    try {
      found = await provider.search(trigger.term, { view, kind: this.#state().kind, signal: controller.signal });
    } catch (error) {
      if (controller.signal.aborted || seq !== this.#searchSeq) return;
      this.#closeSuggestions();
      this.#showError(error instanceof Error ? error.message : String(error));
      view.reportError(error);
      return;
    }
    if (controller.signal.aborted || seq !== this.#searchSeq || this.#disposed) return;
    if (found.length === 0) {
      this.#closeSuggestions();
      view.announce(t(view, "No matching suggestions"));
      return;
    }
    this.#trigger = trigger;
    this.#provider = provider;
    this.#items = [...found];
    this.#active = 0;
    this.#renderSuggestions();
    view.announce(found.length === 1 ? t(view, "1 suggestion") : t(view, "%s suggestions", found.length));
  }

  #renderSuggestions(): void {
    const doc = this.#view.document;
    const provider = this.#provider;
    this.#list.setAttribute("aria-label", provider === null ? "" : provider.label({ view: this.#view }));
    this.#list.replaceChildren(...this.#items.map((item, index) => {
      const option = h(doc, "li", {
        class: "erp-mail-suggestion",
        role: "option",
        id: `${this.#listId}-${item.id}`,
        "aria-selected": index === this.#active ? "true" : "false",
      }, item.label, ...(item.detail !== undefined ? [" ", h(doc, "span", { class: "erp-mail-suggestion-detail" }, item.detail)] : []));
      option.addEventListener("click", () => this.#select(index));
      return option;
    }));
    this.#list.hidden = false;
    this.#field.setAttribute("aria-activedescendant", `${this.#listId}-${this.#items[this.#active]?.id ?? ""}`);
  }

  #closeSuggestions(): void {
    this.#search?.abort();
    this.#searchSeq += 1;
    this.#items = [];
    this.#trigger = null;
    this.#provider = null;
    if (!this.#list.hidden) this.#list.hidden = true;
    this.#list.replaceChildren();
    this.#field.removeAttribute("aria-activedescendant");
  }

  #move(delta: number): void {
    this.#active = (this.#active + delta + this.#items.length) % this.#items.length;
    this.#renderSuggestions();
  }

  #select(index: number): void {
    const item = this.#items[index];
    const trigger = this.#trigger;
    if (item === undefined || trigger === null) return;
    const state = this.#state();
    const caret = this.#field.selectionStart ?? state.text.length;
    const literal = item.mention !== undefined ? `${trigger.delimiter}${item.label}` : item.insert ?? item.label;
    const inserted = `${literal} `;
    const next = state.text.slice(0, trigger.start) + inserted + state.text.slice(caret);
    const position = trigger.start + inserted.length;
    this.#closeSuggestions();
    this.#commit(next, position, item.mention !== undefined
      ? { start: trigger.start, end: trigger.start + literal.length, partnerId: item.mention.partnerId, text: literal }
      : undefined);
    this.#view.announce(t(this.#view, "%s inserted", item.label));
    this.#field.focus();
  }

  #onKeydown(event: KeyboardEvent): void {
    if (this.#items.length > 0 && !this.#list.hidden) {
      if (event.key === "ArrowDown") {
        event.preventDefault();
        this.#move(1);
        return;
      }
      if (event.key === "ArrowUp") {
        event.preventDefault();
        this.#move(-1);
        return;
      }
      if ((event.key === "Enter" || event.key === "Tab") && !event.isComposing && !event.shiftKey && !event.ctrlKey && !event.metaKey && !event.altKey) {
        event.preventDefault();
        this.#select(this.#active);
        return;
      }
      if (event.key === "Escape") {
        event.preventDefault();
        event.stopPropagation();
        this.#closeSuggestions();
        return;
      }
    }
    if (event.key === "Escape" && this.#state().replyTo !== null) {
      event.preventDefault();
      this.#write({ reply_to: null });
      return;
    }
    if (this.isSendChord(event)) {
      event.preventDefault();
      void this.send();
    }
  }
}
