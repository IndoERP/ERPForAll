/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
// The built-in registry entries: message actions, composer actions, message renderers and the `@` suggestion
// provider. Addons add theirs next to these.
import { untrack } from "@erpforall/elang";
import { PARTNER_MODEL, readMessage } from "./contract";
import { h, humanize, normalize } from "./dom";
import { OWNING_TASK, ThreadViewError, unavailable } from "./errors";
import { performOperation } from "./operations";
import type { ComposerAction, MessageAction, MessageRenderer, PartnerSuggestion, Suggestion, SuggestionProvider, ThreadViewRegistries } from "./registries";
import { bodyTextOf, renderBody } from "./sanitize";
import { t } from "./context";
import { threadState } from "./state";

/** The most suggestions one search returns. */
export const MAX_SUGGESTIONS = 8;

const reactionAction: MessageAction = {
  label: (ctx) => t(ctx.view, "Add a reaction"),
  glyph: "+",
  unavailable: (ctx) => {
    if (!ctx.view.capabilities().reactions) return `Reactions need ${OWNING_TASK.reactions} (the schema has no mail.message.reactions)`;
    if (ctx.view.services.pickEmoji === undefined) return "Reactions need the host's emoji picker (services.pickEmoji)";
    return null;
  },
  async run(ctx) {
    const pick = ctx.view.services.pickEmoji;
    if (pick === undefined) throw new ThreadViewError("unavailable", "Reacting to a message is not available: the host gave no services.pickEmoji");
    const emoji = await pick(ctx.anchor);
    if (emoji === null) return;
    const mine = ctx.message.reactions.find((r) => r.content === emoji && r.own);
    if (mine !== undefined) return;
    await performOperation(ctx.view, "setReaction", ctx.message.id, emoji, true);
  },
};

const replyAction: MessageAction = {
  label: (ctx) => t(ctx.view, "Reply"),
  glyph: "<",
  condition: (ctx) => ctx.view.allowReply && ctx.message.messageType === "comment",
  pressed: (ctx) => {
    const { model, id } = threadState(ctx.view.store, ctx.view.ref);
    return model.get(id, "reply_to") === ctx.message.id;
  },
  run(ctx) {
    const { model, id } = threadState(ctx.view.store, ctx.view.ref);
    ctx.view.setReplyTo(model.get(id, "reply_to") === ctx.message.id ? null : ctx.message.id);
  },
};

const bookmarkAction: MessageAction = {
  label: (ctx) => (ctx.message.starred ? t(ctx.view, "Remove bookmark") : t(ctx.view, "Bookmark")),
  glyph: "*",
  unavailable: (ctx) => (ctx.view.capabilities().bookmark ? null : `Bookmarks need ${OWNING_TASK.bookmark} (the schema has no mail.message.starred)`),
  pressed: (ctx) => ctx.message.starred,
  run: (ctx) => performOperation(ctx.view, "setBookmark", ctx.message.id, !ctx.message.starred),
};

const pinAction: MessageAction = {
  label: (ctx) => (ctx.message.pinnedAt === null ? t(ctx.view, "Pin") : t(ctx.view, "Unpin")),
  glyph: "^",
  unavailable: (ctx) => (ctx.view.capabilities().pin ? null : `Pinning needs ${OWNING_TASK.pin} (the schema has no mail.message.pinned_at)`),
  pressed: (ctx) => ctx.message.pinnedAt !== null,
  run: (ctx) => performOperation(ctx.view, "setPinned", ctx.message.id, ctx.message.pinnedAt === null),
};

const copyAction: MessageAction = {
  label: (ctx) => t(ctx.view, "Copy text"),
  glyph: "=",
  condition: (ctx) => bodyTextOf(ctx.message, ctx.message.body, ctx.view.document) !== "",
  async run(ctx) {
    const text = bodyTextOf(ctx.message, ctx.message.body, ctx.view.document);
    const copy = ctx.view.services.copyText ?? (typeof navigator !== "undefined" && navigator.clipboard?.writeText !== undefined
      ? (value: string) => navigator.clipboard.writeText(value)
      : undefined);
    if (copy === undefined) {
      throw new ThreadViewError("unavailable", "Copying text is not available: this browser context has no clipboard (it needs a secure context) and the host gave no services.copyText");
    }
    await copy(text);
    ctx.view.announce(t(ctx.view, "Copied"));
  },
};

const deleteAction: MessageAction = {
  label: (ctx) => t(ctx.view, "Delete"),
  glyph: "x",
  condition: (ctx) => ctx.view.self.partnerId !== null && ctx.message.authorId === ctx.view.self.partnerId && ctx.message.messageType === "comment",
  unavailable: (ctx) => {
    if (typeof ctx.view.transport.deleteMessage !== "function") return `Deleting a message needs ${OWNING_TASK.delete} (the transport has no deleteMessage operation)`;
    if (ctx.view.services.confirm === undefined) return "Deleting a message needs the host's confirmation dialog (services.confirm)";
    return null;
  },
  async run(ctx) {
    const confirm = ctx.view.services.confirm;
    if (confirm === undefined) throw new ThreadViewError("unavailable", "Deleting a message is not available: the host gave no services.confirm");
    const accepted = await confirm({
      title: t(ctx.view, "Delete message"),
      message: t(ctx.view, "Do you really want to delete this message?"),
      accept: t(ctx.view, "Delete"),
      cancel: t(ctx.view, "Cancel"),
    });
    if (!accepted) return;
    await performOperation(ctx.view, "deleteMessage", ctx.message.id);
  },
};

const emojiAction: ComposerAction = {
  label: (ctx) => t(ctx.view, "Add emoji"),
  glyph: ":)",
  unavailable: (ctx) => (ctx.view.services.pickEmoji === undefined ? "Emoji need the host's emoji picker (services.pickEmoji)" : null),
  async run(ctx) {
    const pick = ctx.view.services.pickEmoji;
    if (pick === undefined) throw new ThreadViewError("unavailable", "Adding an emoji is not available: the host gave no services.pickEmoji");
    const emoji = await pick(ctx.anchor);
    if (emoji !== null) ctx.insertText(emoji);
  },
};

const attachAction: ComposerAction = {
  label: (ctx) => t(ctx.view, "Attach files"),
  glyph: "📎",
  unavailable: (ctx) => (ctx.view.services.uploadAttachment === undefined ? `Attaching files needs ${OWNING_TASK.upload} (ir.attachment)` : null),
  run: (ctx) => {
    if (ctx.view.services.uploadAttachment === undefined) {
      throw unavailable("Attaching files", `${OWNING_TASK.upload} (ir.attachment)`);
    }
    ctx.chooseFiles?.();
  },
};

const fullComposerAction: ComposerAction = {
  label: (ctx) => t(ctx.view, "Open full composer"),
  glyph: "[]",
  unavailable: () => `The full composer needs ${OWNING_TASK.fullComposer} (templates and the mail.compose.message form)`,
  run: () => {
    throw unavailable("The full composer", OWNING_TASK.fullComposer);
  },
};

const trackingRenderer: MessageRenderer = {
  match: (message) => message.tracking.length > 0,
  layout: "row",
  render({ view, message }) {
    const doc = view.document;
    const none = t(view, "None");
    const list = h(doc, "ul", { class: "erp-mail-tracking" });
    for (const change of message.tracking) {
      const label = view.services.fieldLabel?.(view.ref.model, change.field) ?? humanize(change.field);
      const shown = (display: string | null, raw: unknown): string => (display !== null && display !== "" ? display : raw === null || raw === "" || raw === false ? none : String(raw));
      list.append(
        h(doc, "li", { class: "erp-mail-tracking-change", "data-field": change.field },
          h(doc, "span", { class: "erp-mail-tracking-field" }, label),
          " ",
          h(doc, "span", { class: "erp-mail-tracking-old" }, shown(change.oldDisplay, change.old)),
          h(doc, "span", { class: "erp-mail-tracking-arrow", "aria-hidden": "true" }, " → "),
          h(doc, "span", { class: "erp-mail-sr-only" }, ` ${t(view, "changed to")} `),
          h(doc, "span", { class: "erp-mail-tracking-new" }, shown(change.newDisplay, change.new))),
      );
    }
    const wrapper = h(doc, "div", { class: "erp-mail-body erp-mail-body-tracking" }, list);
    if (bodyTextOf(message, message.body, doc) !== "") wrapper.append(h(doc, "div", { class: "erp-mail-body-text" }, renderBody(message.body, doc)));
    return wrapper;
  },
};

const notificationRenderer: MessageRenderer = {
  match: (message) => message.messageType === "notification",
  layout: "row",
  render: ({ view, message }) => h(view.document, "div", { class: "erp-mail-body erp-mail-body-notification" }, renderBody(message.body, view.document)),
};

const commentRenderer: MessageRenderer = {
  match: () => true,
  layout: "bubble",
  render: ({ view, message }) => h(view.document, "div", { class: "erp-mail-body" }, renderBody(message.body, view.document)),
};

/** The authors of the messages a thread's window lists: the partners the store can name without a search. */
function threadAuthors(ctx: Parameters<SuggestionProvider["search"]>[1]): PartnerSuggestion[] {
  const { store, ref } = ctx.view;
  const ids = untrack(() => store.windows.get(ref)?.ids ?? []);
  const seen = new Set<number>();
  const found: PartnerSuggestion[] = [];
  for (let i = ids.length - 1; i >= 0; i -= 1) {
    const message = untrack(() => readMessage(store, ids[i] as number, null));
    const author = message?.authorId ?? null;
    if (author === null || seen.has(author)) continue;
    seen.add(author);
    const name = untrack(() => {
      if (!ctx.view.capabilities().partnerNames) return undefined;
      const value = store.record(PARTNER_MODEL, author).get("name");
      return typeof value === "string" && value !== "" ? value : undefined;
    });
    if (name !== undefined) found.push({ id: author, name });
  }
  return found;
}

const partnerProvider: SuggestionProvider = {
  delimiter: "@",
  label: (ctx) => t(ctx.view, "Mention a partner"),
  async search(term, ctx) {
    const needle = normalize(term);
    const local = threadAuthors(ctx);
    const remote = ctx.view.services.searchPartners === undefined ? [] : await ctx.view.services.searchPartners(term, ctx.signal);
    const merged = new Map<number, PartnerSuggestion>();
    for (const partner of [...local, ...remote]) if (!merged.has(partner.id)) merged.set(partner.id, partner);
    const matching = [...merged.values()].filter((p) => normalize(p.name).includes(needle));
    const rank = (p: PartnerSuggestion): number => (normalize(p.name).startsWith(needle) ? 0 : 1);
    const order = new Map([...merged.keys()].map((id, index) => [id, index] as const));
    matching.sort((a, b) => rank(a) - rank(b) || (order.get(a.id) ?? 0) - (order.get(b.id) ?? 0) || a.name.localeCompare(b.name) || a.id - b.id);
    return matching.slice(0, MAX_SUGGESTIONS).map((p): Suggestion => ({
      id: `partner-${p.id}`,
      label: p.name,
      ...(p.detail !== undefined ? { detail: p.detail } : {}),
      mention: { partnerId: p.id },
    }));
  },
};

/**
 * Add the built-in entries to the registries. Call once per hub, from the host's startup (an addon never calls it).
 *
 * @param registries - the registries of {@link defineThreadViewRegistries}
 */
export function registerBuiltins(registries: ThreadViewRegistries): void {
  const { messageActions: actions, composerActions: composer, messageRenderers: renderers, suggestionProviders: providers } = registries;
  actions.add("reaction", reactionAction, { sequence: 10 });
  actions.add("reply", replyAction, { sequence: 20 });
  actions.add("bookmark", bookmarkAction, { sequence: 80 });
  actions.add("copy-text", copyAction, { sequence: 85 });
  actions.add("pin", pinAction, { sequence: 90 });
  actions.add("delete", deleteAction, { sequence: 120 });
  composer.add("emoji", emojiAction, { sequence: 20 });
  composer.add("attach", attachAction, { sequence: 30 });
  composer.add("full-composer", fullComposerAction, { sequence: 40 });
  renderers.add("tracking", trackingRenderer, { sequence: 10 });
  renderers.add("notification", notificationRenderer, { sequence: 20 });
  renderers.add("comment", commentRenderer, { sequence: 90 });
  providers.add("@", partnerProvider, { sequence: 10 });
}
