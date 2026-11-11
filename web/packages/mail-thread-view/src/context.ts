/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
// Options shared by the components and the context they build from them.
import type { Translate } from "@erpforall/elang";
import type { MessagingStore, ThreadRef } from "@erpforall/mail-store";
import { assertContract, capabilitiesOf } from "./contract";
import { ThreadViewError } from "./errors";
import type { ThreadViewContext, ThreadViewRegistries, ThreadViewServices } from "./registries";
import { threadState } from "./state";
import type { ThreadPager, ThreadTransport } from "./transport";

/** The options every component takes. */
export interface ThreadViewBaseOptions {
  /** The tab's messaging store. */
  readonly store: MessagingStore;
  /** The thread: the record (model, id) the messages belong to. */
  readonly ref: ThreadRef;
  /** The registries (from `defineThreadViewRegistries` or `threadViewRegistries`). */
  readonly registries: ThreadViewRegistries;
  /** The host's server calls. */
  readonly transport: ThreadTransport;
  /** The host's page loading. */
  readonly pager: ThreadPager;
  /** The viewer: the partner id (or `null` for a viewer without one). */
  readonly self: { readonly partnerId: number | null };
  /** The session's translator; source strings are English. */
  readonly translate: Translate;
  /** The browser error report; every failure is shown and then reported here. */
  readonly reportError: (error: unknown) => void;
  /** Host services (each optional). */
  readonly services?: ThreadViewServices;
  /** Allow replying to a specific message (Discuss); default false. */
  readonly allowReply?: boolean;
  /** The document to create nodes in; defaults to the global document. */
  readonly document?: Document;
}

const BASE_KEYS = ["store", "ref", "registries", "transport", "pager", "self", "translate", "reportError", "services", "allowReply", "document"];

function bad(name: string, why: string): ThreadViewError {
  return new ThreadViewError("bad_option", `options.${name} ${why}`);
}

/**
 * Check the shared options.
 *
 * @param options - what a component was constructed with
 * @param extra - the component's own option names
 * @param component - the component's name, for messages
 * @throws {@link ThreadViewError} `bad_option` for a missing or invalid option or an unknown name; `schema` when the
 *   store's schema lacks a required message field
 */
export function checkBaseOptions(options: ThreadViewBaseOptions, extra: readonly string[], component: string): void {
  if (options === null || typeof options !== "object") throw new ThreadViewError("bad_option", `${component} options must be an object`);
  for (const key of Object.keys(options)) {
    if (!BASE_KEYS.includes(key) && !extra.includes(key)) throw bad(key, `is not an option of ${component}`);
  }
  if (typeof options.store !== "object" || options.store === null || typeof options.store.record !== "function") throw bad("store", "must be a MessagingStore");
  const ref = options.ref as unknown;
  if (ref === null || typeof ref !== "object" || typeof (ref as ThreadRef).model !== "string" || (ref as ThreadRef).model === "" ||
    !Number.isSafeInteger((ref as ThreadRef).id) || (ref as ThreadRef).id < 1) {
    throw bad("ref", "must be { model, id } with a model name and a positive integer id");
  }
  if (typeof options.registries !== "object" || options.registries === null || typeof options.registries.messageActions?.getAll !== "function") {
    throw bad("registries", "must be the registries of defineThreadViewRegistries(hub)");
  }
  if (typeof options.transport !== "object" || options.transport === null || typeof options.transport.post !== "function") throw bad("transport", "must be a ThreadTransport with a post method");
  if (typeof options.pager !== "object" || options.pager === null || typeof options.pager.loadLatest !== "function" || typeof options.pager.loadOlder !== "function") {
    throw bad("pager", "must be a ThreadPager with loadLatest and loadOlder");
  }
  const self = options.self as unknown;
  if (self === null || typeof self !== "object" || !("partnerId" in (self as object))) throw bad("self", "must be { partnerId: number | null }");
  const partner = (self as { partnerId: unknown }).partnerId;
  if (partner !== null && (typeof partner !== "number" || !Number.isSafeInteger(partner))) throw bad("self", "partnerId must be a partner id or null");
  if (typeof options.translate !== "function") throw bad("translate", "is required: pass the session translator (identity for English source text)");
  if (typeof options.reportError !== "function") throw bad("reportError", "is required: the components never swallow an error");
  if (options.document !== undefined && typeof options.document.createElement !== "function") throw bad("document", "must be a Document");
  assertContract(options.store);
}

/** Build the context the actions, renderers and providers receive. */
export function createContext(
  options: ThreadViewBaseOptions,
  announce: (text: string, urgency?: "polite" | "assertive") => void,
): ThreadViewContext {
  const { store, ref } = options;
  threadState(store, ref);
  return {
    store,
    ref,
    transport: options.transport,
    pager: options.pager,
    self: options.self,
    translate: options.translate,
    services: options.services ?? {},
    registries: options.registries,
    document: options.document ?? document,
    capabilities: () => capabilitiesOf(store),
    reportError: options.reportError,
    announce,
    allowReply: options.allowReply === true,
    setReplyTo: (messageId) => {
      const { model, id } = threadState(store, ref);
      model.set(id, { reply_to: messageId });
    },
  };
}

/**
 * Translate and substitute.
 *
 * @param ctx - the context (its translator)
 * @param source - the English source string
 * @param values - values for `%s` placeholders
 */
export function t(ctx: Pick<ThreadViewContext, "translate">, source: string, ...values: unknown[]): string {
  return String(ctx.translate(source, ...values));
}
