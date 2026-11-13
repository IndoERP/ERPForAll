/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
import { effect, signal, type Signal } from "@erpforall/elang";
import { partnerName, readMessage, type MessageData } from "./contract";
import { t } from "./context";
import { formatBytes, glyphNode, h, normalize, SR_ONLY, uniqueId } from "./dom";
import { ThreadViewError } from "./errors";
import { performOperation } from "./operations";
import { safeHref } from "./sanitize";
import type { MessageAction, MessageActionContext, MessageLayout, MessageRenderer, ThreadViewContext } from "./registries";

/** Options of a {@link Message}. */
export interface MessageOptions {
  readonly view: ThreadViewContext;
  /** The message id. */
  readonly id: number;
  /** The thread's name, for the subject rule (a subject that repeats it is not shown). */
  readonly threadName?: () => string | undefined;
  /** Called once per author whose name the store does not hold. */
  readonly unknownAuthor?: (partnerId: number) => void;
}

/** An action hidden because its capability is missing, with the reason. */
export interface HiddenAction {
  readonly id: string;
  readonly reason: string;
}

const formatters = new Map<string, Intl.DateTimeFormat>();

/** A cached Intl formatter (a handful of entries: locale, zone and style). */
export function formatter(locale: string | undefined, timeZone: string | undefined, style: "time" | "date" | "datetime"): Intl.DateTimeFormat {
  const key = `${locale ?? ""}|${timeZone ?? ""}|${style}`;
  let found = formatters.get(key);
  if (found === undefined) {
    const options: Intl.DateTimeFormatOptions =
      style === "time" ? { hour: "2-digit", minute: "2-digit" }
      : style === "date" ? { dateStyle: "medium" }
      : { dateStyle: "medium", timeStyle: "short" };
    found = new Intl.DateTimeFormat(locale, { ...options, ...(timeZone !== undefined ? { timeZone } : {}) });
    formatters.set(key, found);
  }
  return found;
}

const REPLY_PREFIX = /^\s*((re|fw|fwd)\s*:\s*)+/i;


/** Number of stable avatar colours the stylesheet defines (`data-tone` 0..N-1). */
const AVATAR_TONES = 6;

const segmenter = typeof Intl !== "undefined" && "Segmenter" in Intl ? new Intl.Segmenter(undefined, { granularity: "grapheme" }) : null;

/** The graphemes of a word (a letter with its accents, an emoji with its modifiers); a code point each where the engine has no segmenter. */
function graphemes(word: string): string[] {
  return segmenter === null ? [...word] : Array.from(segmenter.segment(word), (part) => part.segment);
}
const firstGrapheme = (word: string): string => graphemes(word)[0] ?? "";
const firstTwoGraphemes = (word: string): string => graphemes(word).slice(0, 2).join("");

/** A decorative initials badge; the colour is derived from the name, so one author always looks the same. */
export function initialsAvatar(doc: Document, name: string): HTMLElement {
  const words = name.trim().split(/\s+/).filter((w) => w !== "");
  const initials = (words.length > 1 ? firstGrapheme(words[0]!) + firstGrapheme(words[words.length - 1]!) : firstTwoGraphemes(words[0] ?? "?")).toUpperCase();
  let hash = 0;
  for (const ch of name) hash = (hash * 31 + ch.codePointAt(0)!) >>> 0;
  return h(doc, "span", { class: "erp-mail-avatar", "data-tone": hash % AVATAR_TONES, "aria-hidden": "true" }, initials);
}

/**
 * One message of a thread: an `article` with author, time, subject, the body its renderer draws, attachments,
 * reactions and the action toolbar. It follows its own store fields, so a reaction or an edit redraws this article only.
 *
 * Patch points (typed `patch(Message, …)`): {@link Message.showSubject}, {@link Message.authorName},
 * {@link Message.timeLabel}, {@link Message.isOwn}, {@link Message.rendererFor}.
 */
export class Message {
  /** The `article` element. */
  readonly element: HTMLElement;
  readonly id: number;
  readonly #options: MessageOptions;
  readonly #view: ThreadViewContext;
  readonly #inner: HTMLElement;
  readonly #errorLine: HTMLElement;
  readonly #squashed: Signal<boolean>;
  readonly #dayLabel: Signal<string | null>;
  readonly #stop: () => void;
  readonly #authorId: string;
  readonly #timeId: string;
  readonly #bodyId: string;
  #hidden: HiddenAction[] = [];
  #active = false;
  #toolbarIndex = 0;
  #controls: HTMLButtonElement[] = [];
  #disposed = false;
  #data: MessageData | undefined;
  readonly #busy = new Set<string>();

  constructor(options: MessageOptions) {
    this.#options = options;
    this.#view = options.view;
    this.id = options.id;
    const doc = options.view.document;
    this.#authorId = uniqueId("erp-mail-author");
    this.#timeId = uniqueId("erp-mail-time");
    this.#bodyId = uniqueId("erp-mail-body");
    this.#squashed = signal(false);
    this.#dayLabel = signal<string | null>(null);
    this.#inner = h(doc, "div", { class: "erp-mail-message-inner" });
    this.#errorLine = h(doc, "div", { class: "erp-mail-message-error", role: "alert", hidden: true });
    this.element = h(doc, "article", {
      class: "erp-mail-message",
      "data-message-id": options.id,
      tabindex: -1,
      "aria-labelledby": `${this.#authorId} ${this.#timeId}`,
      "aria-describedby": this.#bodyId,
    }, this.#inner, this.#errorLine);
    this.element.addEventListener("keydown", (event) => this.#onKeydown(event));
    this.element.addEventListener("focusin", (event) => {
      const at = this.#controls.indexOf(event.target as HTMLButtonElement);
      if (at >= 0) {
        this.#toolbarIndex = at;
        this.#applyTabindex();
      }
    });
    this.#stop = effect(() => this.#render());
  }

  /** Whether this message belongs to the viewer. */
  isOwn(message: MessageData): boolean {
    const self = this.#view.self.partnerId;
    return self !== null && message.authorId === self;
  }

  /** Whether to show the message's subject: it is not empty and does not repeat the thread's name. */
  showSubject(message: MessageData, threadName?: string): boolean {
    const subject = message.subject;
    if (subject === null || subject.trim() === "") return false;
    if (threadName === undefined) return true;
    return normalize(subject.replace(REPLY_PREFIX, "").trim()) !== normalize(threadName.replace(REPLY_PREFIX, "").trim());
  }

  /** The author's displayed name: the partner's name, or `Partner #<id>` while the store holds none. */
  authorName(message: MessageData): string {
    if (message.authorId === null) return t(this.#view, "Unknown author");
    const name = partnerName(this.#view.store, message.authorId);
    if (name !== undefined) return name;
    this.#options.unknownAuthor?.(message.authorId);
    return `Partner #${message.authorId}`;
  }

  /** The time shown beside the author (the day is in the thread's separator). */
  timeLabel(message: MessageData): string {
    const { locale, timeZone } = this.#view.services;
    return formatter(locale, timeZone, "time").format(message.date);
  }

  /** The renderer that draws a message: the first by sequence whose `match` holds. */
  rendererFor(message: MessageData): MessageRenderer {
    for (const renderer of this.#view.registries.messageRenderers.getAll()) {
      if (renderer.match(message)) return renderer;
    }
    throw new ThreadViewError("registry", `no mail.messageRenderers entry matches message ${message.id} of type ${message.messageType}: register a default renderer`);
  }

  /** Squashed under the previous message (author line hidden). Set by the thread. */
  setSquashed(value: boolean): void {
    this.#squashed.set(value);
  }

  /** The day label drawn at the top of this message (the first message shown for a day), or `null`. Set by the thread. */
  setDayLabel(label: string | null): void {
    this.#dayLabel.set(label);
  }

  /** Whether this is the thread's current article: only then are its controls in the tab order. */
  setActive(active: boolean): void {
    this.#active = active;
    this.#applyTabindex();
  }

  /** The last data this message drew, for the thread's squash and separator rules (undefined while unloaded). */
  get data(): MessageData | undefined {
    return this.#data;
  }

  /** The actions hidden by the last render because the deployment lacks what they need, with the reasons. */
  hiddenActions(): readonly HiddenAction[] {
    return [...this.#hidden];
  }

  /** Stop following the store and release the element. */
  dispose(): void {
    if (this.#disposed) return;
    this.#disposed = true;
    this.#stop();
    this.element.remove();
  }

  #render(): void {
    const view = this.#view;
    const doc = view.document;
    let message: MessageData | undefined;
    const squashed = this.#squashed();
    const dayLabel = this.#dayLabel();
    try {
      message = readMessage(view.store, this.id, view.self.partnerId);
    } catch (error) {
      this.#data = undefined;
      this.#showFailure(error);
      return;
    }
    if (message === undefined) {
      this.#data = undefined;
      this.#showFailure(new ThreadViewError("data", `${view.store.config.messageModel}#${this.id} has no data although the thread's window lists it`));
      return;
    }
    this.#data = message;
    this.element.removeAttribute("data-failed");
    try {
      const renderer = this.rendererFor(message);
      this.element.className = ["erp-mail-message", `is-${renderer.layout}`, this.isOwn(message) ? "is-own" : "", message.isInternal ? "is-note" : "", squashed ? "is-squashed" : ""]
        .filter((c) => c !== "").join(" ");
      this.element.dataset["layout"] = renderer.layout satisfies MessageLayout;
      const body = h(doc, "div", { class: "erp-mail-message-content", id: this.#bodyId }, this.#drawBody(renderer, message));
      const head = h(doc, "header", { class: squashed ? `erp-mail-message-head ${SR_ONLY}` : "erp-mail-message-head" },
        h(doc, "span", { class: "erp-mail-author", id: this.#authorId }, this.authorName(message)),
        " ",
        h(doc, "time", {
          class: "erp-mail-time",
          id: this.#timeId,
          datetime: message.date.toISOString(),
          title: formatter(view.services.locale, view.services.timeZone, "datetime").format(message.date),
        }, this.timeLabel(message)));
      if (message.isInternal) head.append(" ", h(doc, "span", { class: "erp-mail-badge" }, t(view, "Note")));
      if (message.pinnedAt !== null) head.append(" ", h(doc, "span", { class: "erp-mail-badge" }, t(view, "Pinned")));
      if (message.editedAt !== null) {
        head.append(" ", h(doc, "span", { class: "erp-mail-edited", title: formatter(view.services.locale, view.services.timeZone, "datetime").format(message.editedAt) }, t(view, "edited")));
      }
      // The row is the avatar beside one column (head, subject, body, attachments, controls); a squashed message keeps the gutter.
      const main: Node[] = [head];
      if (this.showSubject(message, this.#options.threadName?.())) main.push(h(doc, "div", { class: "erp-mail-subject" }, message.subject ?? ""));
      main.push(body);
      if (message.attachments.length > 0) main.push(this.#drawAttachments(message));
      main.push(this.#drawControls(message));
      const row = h(doc, "div", { class: "erp-mail-message-row" }, ...(squashed ? [] : [initialsAvatar(doc, this.authorName(message))]), h(doc, "div", { class: "erp-mail-message-main" }, ...main));
      this.#inner.replaceChildren(...(dayLabel === null ? [row] : [h(doc, "div", { class: "erp-mail-day" }, dayLabel), row]));
      this.#applyTabindex();
    } catch (error) {
      this.#showFailure(error);
    }
  }

  #drawBody(renderer: MessageRenderer, message: MessageData): Node {
    try {
      return renderer.render({ view: this.#view, message });
    } catch (error) {
      this.#view.reportError(error);
      return h(this.#view.document, "div", { class: "erp-mail-body-failed", role: "alert" }, t(this.#view, "This message could not be displayed."));
    }
  }

  #drawAttachments(message: MessageData): Node {
    const doc = this.#view.document;
    const list = h(doc, "ul", { class: "erp-mail-attachments", "aria-label": t(this.#view, "Attachments") });
    for (const attachment of message.attachments) {
      const name = attachment.name ?? `Attachment #${attachment.id}`;
      const isImg = attachment.mimetype?.startsWith("image/");
      // A stored URL is a link-out only when it is a safe http(s)/mailto/tel URL; images load from our own route, never a remote URL.
      const downloadUrl = (attachment.url ? safeHref(attachment.url) : null) ?? `/erp/content/${attachment.id}?download=true`;
      const viewUrl = `/erp/content/${attachment.id}`;

      const item = h(doc, "li", { class: "erp-mail-attachment", "data-attachment-id": attachment.id });

      if (isImg) {
        const thumb = h(doc, "img", {
          class: "erp-mail-attachment-thumbnail",
          src: viewUrl,
          alt: name,
          loading: "lazy",
        });
        item.append(thumb);
      }

      if (attachment.name !== null && attachment.name !== undefined) {
        const link = h(doc, "a", {
          class: "erp-mail-attachment-download erp-mail-attachment-link",
          href: downloadUrl,
          download: name,
          target: "_blank",
          rel: "noopener noreferrer",
        }, h(doc, "span", { class: "erp-mail-attachment-name" }, name));
        item.append(link);
      } else {
        item.append(h(doc, "span", { class: "erp-mail-attachment-name" }, name));
      }

      if (attachment.fileSize !== undefined && attachment.fileSize !== null && attachment.fileSize > 0) {
        item.append(h(doc, "span", { class: "erp-mail-attachment-size" }, ` (${formatBytes(attachment.fileSize)})`));
      } else if (attachment.mimetype !== null && !isImg) {
        item.append(h(doc, "span", { class: "erp-mail-attachment-type" }, ` (${attachment.mimetype})`));
      }

      list.append(item);
    }
    return list;
  }

  #drawControls(message: MessageData): Node {
    const view = this.#view;
    const doc = view.document;
    this.#controls = [];
    this.#hidden = [];
    const reactions = h(doc, "div", { class: "erp-mail-reactions" });
    for (const reaction of message.reactions) {
      const label = reaction.count === 1
        ? t(view, reaction.own ? "%s, 1 reaction, you reacted" : "%s, 1 reaction", reaction.content)
        : t(view, reaction.own ? "%s, %s reactions, you reacted" : "%s, %s reactions", reaction.content, reaction.count);
      const chip = h(doc, "button", {
        type: "button",
        class: "erp-mail-reaction",
        "aria-pressed": reaction.own ? "true" : "false",
        "aria-label": label,
        "data-reaction": reaction.content,
        tabindex: -1,
      }, h(doc, "span", { "aria-hidden": "true" }, `${reaction.content} ${reaction.count}`));
      chip.addEventListener("click", () => void this.#run(`reaction:${reaction.content}`, chip, () => performOperation(view, "setReaction", message.id, reaction.content, !reaction.own)));
      reactions.append(chip);
      this.#controls.push(chip);
    }
    const actions = h(doc, "div", { class: "erp-mail-actions" });
    for (const [id, action] of view.registries.messageActions.getEntries()) {
      const button = h(doc, "button", { type: "button", class: "erp-mail-action", "data-action": id, tabindex: -1 });
      const ctx: MessageActionContext = { view, message, anchor: button };
      try {
        if (action.condition !== undefined && !action.condition(ctx)) continue;
        const reason = action.unavailable?.(ctx) ?? null;
        if (reason !== null) {
          this.#hidden.push({ id, reason });
          continue;
        }
        const label = action.label(ctx);
        button.title = label;
        button.append(...(action.glyph !== undefined ? [glyphNode(doc, action.glyph)] : []), h(doc, "span", { class: "erp-mail-action-label" }, label));
        const pressed = action.pressed?.(ctx);
        if (pressed !== undefined) button.setAttribute("aria-pressed", pressed ? "true" : "false");
      } catch (error) {
        view.reportError(error);
        continue;
      }
      button.addEventListener("click", () => void this.#run(id, button, () => (action as MessageAction).run({ view, message: this.#data ?? message, anchor: button })));
      actions.append(button);
      this.#controls.push(button);
    }
    const toolbar = h(doc, "div", { class: "erp-mail-toolbar", role: "toolbar", "aria-label": t(view, "Message actions") }, reactions, actions);
    toolbar.hidden = this.#controls.length === 0;
    return toolbar;
  }

  async #run(key: string, button: HTMLElement, work: () => void | Promise<void>): Promise<void> {
    if (this.#busy.has(key)) return;
    this.#busy.add(key);
    button.setAttribute("aria-disabled", "true");
    try {
      await work();
      this.#errorLine.hidden = true;
      this.#errorLine.textContent = "";
    } catch (error) {
      const text = error instanceof Error ? error.message : String(error);
      this.#errorLine.textContent = text;
      this.#errorLine.hidden = false;
      this.#view.announce(text, "assertive");
      this.#view.reportError(error);
    } finally {
      this.#busy.delete(key);
      button.removeAttribute("aria-disabled");
    }
  }

  #showFailure(error: unknown): void {
    const doc = this.#view.document;
    this.element.className = "erp-mail-message is-failed";
    this.element.setAttribute("data-failed", "true");
    this.#controls = [];
    this.#inner.replaceChildren(h(doc, "div", { class: "erp-mail-message-failed", id: this.#bodyId }, t(this.#view, "This message could not be displayed.")));
    this.#view.reportError(error);
  }

  #applyTabindex(): void {
    this.#toolbarIndex = Math.min(this.#toolbarIndex, Math.max(0, this.#controls.length - 1));
    this.#controls.forEach((control, index) => {
      control.tabIndex = this.#active && index === this.#toolbarIndex ? 0 : -1;
    });
  }

  #onKeydown(event: KeyboardEvent): void {
    const target = event.target as HTMLElement;
    const index = this.#controls.indexOf(target as HTMLButtonElement);
    if (index < 0 || event.altKey || event.ctrlKey || event.metaKey || event.shiftKey) return;
    let next = index;
    if (event.key === "ArrowRight") next = (index + 1) % this.#controls.length;
    else if (event.key === "ArrowLeft") next = (index - 1 + this.#controls.length) % this.#controls.length;
    else if (event.key === "Home") next = 0;
    else if (event.key === "End") next = this.#controls.length - 1;
    else return;
    event.preventDefault();
    event.stopPropagation();
    this.#toolbarIndex = next;
    this.#applyTabindex();
    this.#controls[next]?.focus();
  }
}

