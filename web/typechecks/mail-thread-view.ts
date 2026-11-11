/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
// Checked addon-guide examples for: host-supplied values are declared, never
// executed. The guide embeds the regions below; scripts/ci.sh runs `tsc -p tsconfig.json`, which compiles this file.
import { RegistryHub, createTranslator, patch } from "@erpforall/elang";
import type { AddonRegistration } from "@erpforall/elang";
import type { MessagingStore } from "@erpforall/mail-store";
import {
  Composer,
  Message,
  Thread,
  defineThreadViewRegistries,
  registerBuiltins,
  type ThreadPager,
  type ThreadTransport,
} from "@erpforall/mail-thread-view";

declare const store: MessagingStore;
declare const hub: RegistryHub;
declare const host: HTMLElement;
declare function jsonRpc(path: string, body: unknown): Promise<{ message_id: number; replayed: boolean; store: unknown }>;
declare function fetchLatestPage(model: string, id: number): Promise<void>;
declare function fetchOlderPage(model: string, id: number, before: number): Promise<void>;

// region: mount
const registries = defineThreadViewRegistries(hub);
registerBuiltins(registries);

const transport: ThreadTransport = {
  async post(ref, post) {
    const answer = await jsonRpc(`/json/2/${ref.model}/thread_post`, { ids: [ref.id], post });
    // applyAnswer takes the payload as text, exactly as received.
    return { message_id: answer.message_id, replayed: answer.replayed, store: JSON.stringify(answer.store) };
  },
};
const pager: ThreadPager = {
  loadLatest: (ref) => fetchLatestPage(ref.model, ref.id),
  loadOlder: (ref, before) => fetchOlderPage(ref.model, ref.id, before),
};

const common = {
  store,
  ref: { model: "sale.order", id: 42 },
  registries,
  transport,
  pager,
  self: { partnerId: 7 },
  translate: createTranslator({ language: "en_US" })._t,
  reportError: (error: unknown) => console.error(error),
};
const thread = new Thread({ ...common, order: "desc" });
const composer = new Composer({ ...common });
host.append(thread.element, composer.element);
// endregion

// region: addon
export const acmeCalls: AddonRegistration = {
  id: "acme_calls",
  register(ctx) {
    ctx.registry.category("mail.messageActions").add(
      "acme_calls.flag",
      {
        label: (a) => String(a.view.translate("Flag")),
        glyph: "!",
        // Only offered where the schema has the field your addon adds (see "Capabilities").
        unavailable: (a) => (a.view.capabilities().bookmark ? null : "Flagging needs the starred field"),
        run: async (a) => {
          await fetchFlag(a.message.id);
        },
      },
      { sequence: 15 },
    );
    ctx.registry.category("mail.messageRenderers").add(
      "acme_calls.call",
      {
        match: (message) => message.messageType === "acme_call",
        layout: "row",
        render: ({ view, message }) => {
          const card = view.document.createElement("div");
          card.textContent = `Call log #${message.id}`;
          return card;
        },
      },
      { sequence: 5 },
    );
    ctx.registry.category("mail.suggestionProviders").add("#", {
      delimiter: "#",
      label: () => "Channels",
      search: (term) => ["general", "sales"].filter((c) => c.startsWith(term)).map((c) => ({ id: c, label: `#${c}` })),
    });
    ctx.registry.category("mail.composerActions").add("acme_calls.log", {
      label: () => "Log call",
      run: (a) => a.insertText("[call] "),
    });
  },
};
declare function fetchFlag(messageId: number): Promise<void>;
// endregion

// region: patch
export const acmePatches: AddonRegistration = {
  id: "acme_patches",
  register(ctx) {
    ctx.patch(Message, (sup) => ({
      authorName(message) {
        return `${sup.authorName.call(this, message)} (verified)`;
      },
    }));
    ctx.patch(Composer, (sup) => ({
      // Everything buildPost returns is sent, and kept with an unsent post for Retry.
      buildPost() {
        return { ...sup.buildPost.call(this), subject: "Re: order" };
      },
    }));
    ctx.patch(Thread, (sup) => ({
      emptyLabel() {
        return `${sup.emptyLabel.call(this)} (yet)`;
      },
    }));
  },
};
// The same, outside an addon (a host or a test): `patch` is the module function and returns its undo.
export const undo = patch(Thread, () => ({ emptyLabel: () => "Nothing here" }));
// endregion

export const keep = [thread, composer, acmeCalls, acmePatches, undo];
