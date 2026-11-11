/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
// Construction options are checked with exact messages, and disposal is complete and idempotent.
import { RegistryHub } from "@erpforall/elang";
import { expect, test } from "vitest";
import { Composer, Thread, ThreadViewError, defineThreadViewRegistries, threadViewRegistries, type ComposerOptions } from "../src/index";
import { Rig, failsWith, message, settle, type } from "./support";

test("every construction option is checked and a refusal names the option", () => {
  const rig = new Rig();
  const base = rig.threadOptions();
  const bad = (override: Record<string, unknown>, text: string, make: "thread" | "composer" = "thread") =>
    failsWith(() => (make === "thread" ? new Thread({ ...base, ...override } as never) : new Composer({ ...base, ...override } as ComposerOptions)), "bad_option", text);
  bad({ store: null }, "options.store must be a MessagingStore");
  bad({ ref: { model: "", id: 1 } }, "options.ref must be { model, id } with a model name and a positive integer id");
  bad({ ref: { model: "mail.thread", id: 0 } }, "options.ref must be { model, id } with a model name and a positive integer id");
  bad({ registries: {} }, "options.registries must be the registries of defineThreadViewRegistries(hub)");
  bad({ transport: {} }, "options.transport must be a ThreadTransport with a post method");
  bad({ pager: { loadLatest: () => undefined } }, "options.pager must be a ThreadPager with loadLatest and loadOlder");
  bad({ self: {} }, "options.self must be { partnerId: number | null }");
  bad({ self: { partnerId: "2" } }, "options.self partnerId must be a partner id or null");
  bad({ translate: undefined }, "options.translate is required: pass the session translator (identity for English source text)");
  bad({ reportError: undefined }, "options.reportError is required: the components never swallow an error");
  bad({ document: {} }, "options.document must be a Document");
  bad({ foo: 1 }, "options.foo is not an option of Thread");
  bad({ order: "up" }, 'options.order must be "asc" or "desc", got "up"');
  bad({ order: "asc" }, "options.order is not an option of Composer", "composer");
  bad({ sendOnEnter: "yes" }, "options.sendOnEnter must be a boolean", "composer");
  bad({ platform: "linux" }, 'options.platform must be "mac" or "other", got "linux"', "composer");
  failsWith(() => new Thread(null as never), "bad_option", "Thread options must be an object");
  // A thread of a model the schema does not hold cannot be held against eviction.
  expect(() => new Thread({ ...base, ref: { model: "no.such", id: 1 } })).toThrowError("hold: no.such is neither a model of schema t1 nor a local model");
  // The registries of a hub are found again, and a hub without them is refused with the fix.
  expect(threadViewRegistries(rig.hub)).toEqual(rig.registries);
  const empty = new RegistryHub();
  expect(() => threadViewRegistries(empty)).toThrowError(ThreadViewError);
  failsWith(() => threadViewRegistries(empty), "registry", 'the thread view registries are not defined on this hub: call defineThreadViewRegistries(hub) first (Registry category "mail.messageActions" is not defined)');
  defineThreadViewRegistries(empty);
  expect(threadViewRegistries(empty).messageActions.name).toBe("mail.messageActions");
});

test("a client_uid the host cannot make is refused before anything is queued", async () => {
  const rig = new Rig();
  rig.services = { ...rig.services, newUid: () => "" };
  const composer = rig.composer();
  type(composer.field, "hello");
  expect(await composer.send()).toBe("refused");
  expect(composer.element.querySelector(".erp-mail-composer-error")?.textContent).toBe('services.newUid must return a non-empty string, got ""');
  expect(rig.store.local("mail_ui.pending").ids()).toEqual([]);
  expect(composer.field.value).toBe("hello");
});

test("a disposed composer refuses use, is idempotent and stops following the draft", async () => {
  const rig = new Rig();
  rig.load([message(1)]);
  const composer = rig.composer();
  composer.dispose();
  composer.dispose();
  expect(composer.element.isConnected).toBe(false);
  failsWith(() => composer.setKind("note"), "disposed", "this Composer was disposed");
  await expect(composer.send()).rejects.toMatchObject({ code: "disposed", message: "this Composer was disposed" });
  rig.store.local("mail_ui.thread").set("mail.thread:7", { text: "changed elsewhere", kind: "note" });
  await settle();
  expect(composer.field.value).toBe("");
  expect(composer.element.querySelector(".erp-mail-kind[aria-pressed='true']")?.textContent).toBe("Send message");
});
