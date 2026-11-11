/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
// The four client registries that act on the shared components, their
// value types and runtime guards. An addon adds a message action, a composer action, a message renderer or a
// suggestion provider with `ctx.registry.category("mail.messageActions").add(id, definition, { sequence })`.
import type { Registry, RegistryHub, Translate } from "@erpforall/elang";
import type { MessagingStore, ThreadRef } from "@erpforall/mail-store";
import type { Capabilities, MessageData } from "./contract";
import { ThreadViewError } from "./errors";
import type { MentionRange } from "./mentions";
import type { ThreadPager, ThreadTransport } from "./transport";

/** A partner offered by the `@` provider or returned by the host's search. */
export interface PartnerSuggestion {
  readonly id: number;
  readonly name: string;
  readonly detail?: string;
}

/** An uploaded attachment item for the composer or thread. */
export interface AttachmentUploadResult {
  readonly id: number;
  readonly name: string;
  readonly size?: number;
  readonly mimetype?: string;
}

/** Host services the components use; each is optional and the features that need one are hidden without it. */
export interface ThreadViewServices {
  /** Open the emoji picker at `anchor`; resolves to the chosen emoji or `null` when dismissed. */
  pickEmoji?(anchor: HTMLElement): Promise<string | null>;
  /** Ask for confirmation; resolves true when accepted. Required by destructive actions. */
  confirm?(options: { readonly title: string; readonly message: string; readonly accept: string; readonly cancel: string }): Promise<boolean>;
  /** Copy text to the clipboard. Defaults to `navigator.clipboard.writeText`; a missing clipboard is an error. */
  copyText?(text: string): Promise<void>;
  /** Search partners beyond those the store holds (the host's partner topic or `res.partner` search). */
  searchPartners?(term: string, signal: AbortSignal): Promise<readonly PartnerSuggestion[]>;
  /** The label of a field of the thread's model, for tracking lines. Defaults to a readable form of the field name. */
  fieldLabel?(model: string, field: string): string;
  /** The current time (tests pass a fixed clock). */
  now?(): Date;
  /** A new `client_uid` (a UUID). Defaults to `crypto.randomUUID`. */
  newUid?(): string;
  /** The locale for dates; defaults to the browser's. */
  readonly locale?: string;
  /** The time zone for dates; defaults to the browser's. */
  readonly timeZone?: string;
  /** Upload an attachment through the host's attachment service. */
  uploadAttachment?(file: File): Promise<AttachmentUploadResult>;
}

/** What every component of one mounted thread shares. */
export interface ThreadViewContext {
  readonly store: MessagingStore;
  readonly ref: ThreadRef;
  readonly transport: ThreadTransport;
  readonly pager: ThreadPager;
  readonly self: { readonly partnerId: number | null };
  readonly translate: Translate;
  readonly services: ThreadViewServices;
  readonly registries: ThreadViewRegistries;
  readonly document: Document;
  /** The capabilities of the store's current schema. */
  capabilities(): Capabilities;
  /** Report an error (the browser error report) after it was shown. */
  reportError(error: unknown): void;
  /** Announce text to assistive technology (polite) or as an alert (assertive). */
  announce(text: string, urgency?: "polite" | "assertive"): void;
  /** Whether the host allows replying to a specific message (Discuss: true; the chatter's flat thread: false). */
  readonly allowReply: boolean;
  /** Set (or clear with `null`) the message the composer replies to. */
  setReplyTo(messageId: number | null): void;
}

/** What a message action sees. */
export interface MessageActionContext {
  readonly view: ThreadViewContext;
  readonly message: MessageData;
  /** The toolbar button of the action (anchor for pickers and dialogs). */
  readonly anchor: HTMLElement;
}

/** A message action: a toolbar button on every message where `condition` holds. */
export interface MessageAction {
  /** The visible, translated name; also the accessible name. */
  label(ctx: MessageActionContext): string;
  /** A short decorative glyph shown before the label (hidden from assistive technology). */
  readonly glyph?: string;
  /** Whether the action applies to this message; evaluated on every render of the message. Default: applies. */
  condition?(ctx: MessageActionContext): boolean;
  /** A reason the action cannot work in this deployment (a capability the server lacks); checked only where `condition` holds, the action is then hidden and listed. */
  unavailable?(ctx: MessageActionContext): string | null;
  /** For toggles: whether the action is currently on (`aria-pressed`). */
  pressed?(ctx: MessageActionContext): boolean;
  /** Run the action. A throw or a rejection is reported and shown, never swallowed. */
  run(ctx: MessageActionContext): void | Promise<void>;
}

/** The composer's state as an action sees it. */
export interface ComposerState {
  readonly text: string;
  readonly kind: "message" | "note";
  readonly mentions: readonly MentionRange[];
  readonly replyTo: number | null;
}

/** What a composer action sees. */
export interface ComposerActionContext {
  readonly view: ThreadViewContext;
  readonly state: ComposerState;
  readonly anchor: HTMLElement;
  /** Insert text at the caret of the draft field. */
  insertText(text: string): void;
  /** Send the draft. */
  send(): Promise<void>;
  /** Trigger the file chooser to attach files. */
  chooseFiles?(): void;
}

/** A composer action: a button in the composer's action row. */
export interface ComposerAction {
  label(ctx: ComposerActionContext): string;
  readonly glyph?: string;
  condition?(ctx: ComposerActionContext): boolean;
  unavailable?(ctx: ComposerActionContext): string | null;
  run(ctx: ComposerActionContext): void | Promise<void>;
}

/** What a message renderer sees. */
export interface RendererContext {
  readonly view: ThreadViewContext;
  readonly message: MessageData;
}

/** The look of a message the renderer produces. */
export type MessageLayout = "bubble" | "row";

/** A message renderer: the first (lowest sequence) whose `match` holds draws the message body. */
export interface MessageRenderer {
  match(message: MessageData): boolean;
  /** `bubble` for a comment, `row` for a notification or tracking line. */
  readonly layout: MessageLayout;
  /** The body content. An error thrown is reported and replaced by a visible "could not be displayed" line. */
  render(ctx: RendererContext): Node;
}

/** One suggestion of a provider. */
export interface Suggestion {
  /** A stable id within the list (used as the option element id suffix). */
  readonly id: string;
  readonly label: string;
  readonly detail?: string;
  /** Makes the suggestion a structured mention of this partner. */
  readonly mention?: { readonly partnerId: number };
  /** Plain text inserted for a non-mention suggestion; defaults to the label. */
  readonly insert?: string;
}

/** What a suggestion provider sees. */
export interface SuggestionContext {
  readonly view: ThreadViewContext;
  readonly kind: "message" | "note";
  readonly signal: AbortSignal;
}

/** A suggestion provider for one trigger delimiter (`@`, `:`, `::`, `/`, …). */
export interface SuggestionProvider {
  readonly delimiter: string;
  /** The listbox's accessible name. */
  label(ctx: { readonly view: ThreadViewContext }): string;
  /** The suggestions for the text typed after the delimiter. */
  search(term: string, ctx: SuggestionContext): readonly Suggestion[] | Promise<readonly Suggestion[]>;
}

/** Declares the registry categories to the Elang `Registries` type. */
declare module "@erpforall/elang" {
  interface Registries {
    "mail.messageActions": MessageAction;
    "mail.composerActions": ComposerAction;
    "mail.messageRenderers": MessageRenderer;
    "mail.suggestionProviders": SuggestionProvider;
  }
}

/** The four registries of one hub. */
export interface ThreadViewRegistries {
  readonly messageActions: Registry<MessageAction>;
  readonly composerActions: Registry<ComposerAction>;
  readonly messageRenderers: Registry<MessageRenderer>;
  readonly suggestionProviders: Registry<SuggestionProvider>;
}

function isObject(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null;
}
const fn = (value: unknown): boolean => typeof value === "function";
const optionalFn = (value: unknown): boolean => value === undefined || typeof value === "function";
const optionalText = (value: unknown): boolean => value === undefined || typeof value === "string";

/** Guard of `mail.messageActions` values. */
export function isMessageAction(value: unknown): value is MessageAction {
  return isObject(value) && fn(value["label"]) && fn(value["run"]) && optionalFn(value["condition"]) && optionalFn(value["unavailable"]) &&
    optionalFn(value["pressed"]) && optionalText(value["glyph"]);
}
/** Guard of `mail.composerActions` values. */
export function isComposerAction(value: unknown): value is ComposerAction {
  return isObject(value) && fn(value["label"]) && fn(value["run"]) && optionalFn(value["condition"]) && optionalFn(value["unavailable"]) &&
    optionalText(value["glyph"]);
}
/** Guard of `mail.messageRenderers` values. */
export function isMessageRenderer(value: unknown): value is MessageRenderer {
  return isObject(value) && fn(value["match"]) && fn(value["render"]) && (value["layout"] === "bubble" || value["layout"] === "row");
}
/** Guard of `mail.suggestionProviders` values. */
export function isSuggestionProvider(value: unknown): value is SuggestionProvider {
  return isObject(value) && typeof value["delimiter"] === "string" && value["delimiter"] !== "" && value["delimiter"].length <= 4 &&
    !/\s/.test(value["delimiter"]) && fn(value["label"]) && fn(value["search"]);
}

/**
 * Define the four categories on a registry hub (once per hub; a second call is the registry's own duplicate error).
 *
 * @param hub - the application's registry hub
 */
export function defineThreadViewRegistries(hub: RegistryHub): ThreadViewRegistries {
  return Object.freeze({
    messageActions: hub.define("mail.messageActions", isMessageAction),
    composerActions: hub.define("mail.composerActions", isComposerAction),
    messageRenderers: hub.define("mail.messageRenderers", isMessageRenderer),
    suggestionProviders: hub.define("mail.suggestionProviders", isSuggestionProvider),
  });
}

/**
 * The registries a hub already defines.
 *
 * @param hub - the application's registry hub
 * @throws {@link ThreadViewError} `registry` when {@link defineThreadViewRegistries} was not called on it
 */
export function threadViewRegistries(hub: RegistryHub): ThreadViewRegistries {
  try {
    return Object.freeze({
      messageActions: hub.category("mail.messageActions"),
      composerActions: hub.category("mail.composerActions"),
      messageRenderers: hub.category("mail.messageRenderers"),
      suggestionProviders: hub.category("mail.suggestionProviders"),
    });
  } catch (cause) {
    throw new ThreadViewError("registry", `the thread view registries are not defined on this hub: call defineThreadViewRegistries(hub) first (${(cause as Error).message})`);
  }
}
