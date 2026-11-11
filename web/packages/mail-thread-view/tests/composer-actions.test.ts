/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
// Composer actions: the registry, the built-ins and the reasons for the hidden ones.
import { flushSync } from "@erpforall/elang";
import { expect, test } from "vitest";
import { ThreadViewError, type ComposerAction } from "../src/index";
import { Rig, failsWith, message, settle, type } from "./support";

const actionsOf = (root: HTMLElement): string[] => [...root.querySelectorAll<HTMLElement>(".erp-mail-composer-action")].map((b) => b.dataset["action"] ?? "");

test("attach and the full composer are hidden with the task that owns them, and emoji appears only with the host's picker", async () => {
  const rig = new Rig();
  const bare = rig.composer();
  expect(actionsOf(bare.element)).toEqual([]);
  expect(bare.hiddenActions()).toEqual([
    { id: "emoji", reason: "Emoji need the host's emoji picker (services.pickEmoji)" },
    { id: "attach", reason: "Attaching files needs a later release (ir.attachment)" },
    { id: "full-composer", reason: "The full composer needs a later release (templates and the mail.compose.message form)" },
  ]);
  // Invoked anyway, they fail with the same task named.
  const ctx = { view: bare.context } as never;
  failsWith(() => rig.registries.composerActions.get("attach").run(ctx), "unavailable", "Attaching files is not available until a later release (ir.attachment)");
  failsWith(() => rig.registries.composerActions.get("full-composer").run(ctx), "unavailable", "The full composer is not available until a later release");

  const picks: (string | null)[] = ["\u{1F600}", null];
  rig.services = { ...rig.services, pickEmoji: () => Promise.resolve(picks.shift() ?? null) };
  const composer = rig.composer();
  expect(actionsOf(composer.element)).toEqual(["emoji"]);
  const button = composer.element.querySelector(".erp-mail-composer-action") as HTMLButtonElement;
  expect(button.textContent).toBe(":)Add emoji");
  // The emoji goes in at the caret, and a mention after it moves with the text.
  type(composer.field, "ab @Ann", 1);
  rig.store.local("mail_ui.thread").set("mail.thread:7", { mentions: [{ start: 3, end: 7, partnerId: 1, text: "@Ann" }] });
  button.click();
  await settle();
  expect(composer.field.value).toBe("a\u{1F600}b @Ann");
  expect(composer.field.selectionStart).toBe(3);
  expect(rig.store.local("mail_ui.thread").get("mail.thread:7", "mentions")).toEqual([{ start: 5, end: 9, partnerId: 1, text: "@Ann" }]);
  button.click();
  await settle();
  expect(composer.field.value).toBe("a\u{1F600}b @Ann");
});

test("an addon composer action sees the live draft, can send it, depends on the kind and reports its failures", async () => {
  const rig = new Rig();
  rig.records({ "res.partner": [{ id: 2, name: "Bob" }] });
  rig.load([message(1)]);
  rig.transport.postHandler = (dto) => Promise.resolve({ message_id: 40, replayed: false, store: rig.payload({ "mail.message": [message(40, { body: `<p>${dto.body}</p>` })] }) });
  const seen: string[] = [];
  const quick: ComposerAction = {
    label: () => "Quick send",
    glyph: ">",
    run: async (ctx) => {
      seen.push(`${ctx.state.kind}:${ctx.state.text}`);
      ctx.insertText("!");
      await ctx.send();
    },
  };
  const noteOnly: ComposerAction = { label: () => "Template", condition: (ctx) => ctx.state.kind === "note", run: () => undefined };
  const failing: ComposerAction = { label: () => "Fail", run: () => { throw new Error("action broke"); } };
  rig.registries.composerActions.add("acme.quick", quick, { sequence: 10 });
  rig.registries.composerActions.add("acme.note", noteOnly, { sequence: 20 });
  rig.registries.composerActions.add("acme.fail", failing, { sequence: 30 });
  const composer = rig.composer();
  expect(actionsOf(composer.element)).toEqual(["acme.quick", "acme.fail"]);
  composer.setKind("note");
  flushSync();
  expect(actionsOf(composer.element)).toEqual(["acme.quick", "acme.note", "acme.fail"]);
  type(composer.field, "ship it");
  (composer.element.querySelector('[data-action="acme.quick"]') as HTMLButtonElement).click();
  await settle();
  expect(seen).toEqual(["note:ship it"]);
  expect(rig.transport.posts[0]?.dto).toMatchObject({ body: "ship it!", subtype: "note" });
  (composer.element.querySelector('[data-action="acme.fail"]') as HTMLButtonElement).click();
  await settle();
  expect(composer.element.querySelector(".erp-mail-composer-error")?.textContent).toBe("action broke");
  expect(rig.errors).toHaveLength(1);
  expect((rig.errors[0] as Error).message).toBe("action broke");
  // An action added later appears at once.
  rig.registries.composerActions.add("acme.late", { label: () => "Late", run: () => undefined }, { sequence: 5 });
  flushSync();
  expect(actionsOf(composer.element)[0]).toBe("acme.late");
  expect(rig.errors.every((e) => !(e instanceof ThreadViewError))).toBe(true);
});
