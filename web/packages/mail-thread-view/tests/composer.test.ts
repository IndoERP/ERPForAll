/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
// The composer: draft, kind, send chords, the post DTO and the placement of its answer, failure with Retry, refusals.
import { flushSync } from "@erpforall/elang";
import { expect, test } from "vitest";
import { MAX_PENDING, ThreadViewError } from "../src/index";
import { Rig, message, key, settle, type } from "./support";

const ann = { "res.partner": [{ id: 1, name: "Ann" }, { id: 2, name: "Bob" }] };
const field = (c: { field: HTMLTextAreaElement }) => c.field;
const polite = (root: HTMLElement): string => root.querySelector(".erp-mail-sr-only [role='status']")?.textContent ?? "";
const assertive = (root: HTMLElement): string => root.querySelector(".erp-mail-sr-only [aria-live='assertive']")?.textContent ?? "";

/** An answer for a successful post: the new message and its id. */
function answerWith(rig: Rig, id: number, replayed = false) {
  return () => Promise.resolve({ message_id: id, replayed, store: rig.payload({ "mail.message": [message(id, { author_id: 2, body: `<p>Posted ${id}</p>` })] }) });
}

test("the composer is a labelled form with a kind switch, a placeholder and a Send button that follow the kind and the draft", () => {
  const rig = new Rig();
  rig.records(ann);
  const composer = rig.composer();
  const form = composer.element;
  expect(form.tagName).toBe("FORM");
  expect(form.getAttribute("aria-label")).toBe("Message composer");
  const label = form.querySelector(`label[for="${field(composer).id}"]`);
  expect(label?.textContent).toBe("Message");
  expect(document.getElementById(field(composer).getAttribute("aria-describedby")!)?.textContent).toBe("Press Control+Enter to send");
  const kinds = [...form.querySelectorAll<HTMLButtonElement>(".erp-mail-kind")];
  expect(kinds.map((k) => `${k.textContent}:${k.getAttribute("aria-pressed")}`)).toEqual(["Send message:true", "Log note:false"]);
  expect(form.querySelector(".erp-mail-kinds")?.getAttribute("aria-label")).toBe("Post type");
  const send = form.querySelector(".erp-mail-send") as HTMLButtonElement;
  expect([send.textContent, send.getAttribute("aria-disabled"), field(composer).placeholder]).toEqual(["Send", "true", "Send a message to followers..."]);
  type(field(composer), "Hello");
  expect(send.getAttribute("aria-disabled")).toBe("false");
  type(field(composer), "   ");
  expect(send.getAttribute("aria-disabled")).toBe("true");
  type(field(composer), "Draft kept");
  kinds[1]!.click();
  flushSync();
  expect(kinds.map((k) => k.getAttribute("aria-pressed"))).toEqual(["false", "true"]);
  expect([send.textContent, field(composer).placeholder]).toEqual(["Log", "Log an internal note..."]);
  expect(field(composer).value).toBe("Draft kept");

  // The draft and the kind are local records: a rebuilt composer starts where the last one stopped.
  composer.dispose();
  const again = rig.composer();
  expect([field(again).value, again.element.querySelector(".erp-mail-kind[aria-pressed='true']")?.textContent]).toEqual(["Draft kept", "Log note"]);
  expect(rig.store.local("mail_ui.thread").get("mail.thread:7", "text")).toBe("Draft kept");
  // On a Mac the chord is named for Command.
  const mac = rig.composer({ platform: "mac" });
  expect(document.getElementById(field(mac).getAttribute("aria-describedby")!)?.textContent).toBe("Press Command+Enter to send");
  expect(document.getElementById(field(rig.composer({ sendOnEnter: true })).getAttribute("aria-describedby")!)?.textContent).toBe("Press Enter to send");
});

test("the send chord is Ctrl+Enter (Cmd+Enter on a Mac), or Enter alone for Discuss, and never during IME composition or on an empty draft", async () => {
  const rig = new Rig();
  rig.records(ann);
  rig.pager.latestHandler = () => Promise.resolve();
  rig.load([message(1)]);
  let n = 100;
  rig.transport.postHandler = () => answerWith(rig, ++n)();
  const chatter = rig.composer();
  const f = field(chatter);
  const sent = () => rig.transport.posts.length;
  type(f, "one");
  expect(key(f, "Enter").defaultPrevented).toBe(false);
  expect(key(f, "Enter", { shiftKey: true }).defaultPrevented).toBe(false);
  expect(key(f, "Enter", { metaKey: true }).defaultPrevented).toBe(false);
  expect(key(f, "Enter", { ctrlKey: true, isComposing: true }).defaultPrevented).toBe(false);
  expect(key(f, "Enter", { ctrlKey: true, keyCode: 229 }).defaultPrevented).toBe(false);
  expect(sent()).toBe(0);
  expect(key(f, "Enter", { ctrlKey: true }).defaultPrevented).toBe(true);
  await settle();
  expect(sent()).toBe(1);
  expect(f.value).toBe("");
  // An empty draft sends nothing and says so.
  key(f, "Enter", { ctrlKey: true });
  await settle();
  expect(sent()).toBe(1);
  expect(polite(chatter.element)).toBe("Nothing to send");

  const mac = rig.composer({ platform: "mac" });
  type(field(mac), "two");
  expect(key(field(mac), "Enter", { ctrlKey: true }).defaultPrevented).toBe(false);
  expect(key(field(mac), "Enter", { metaKey: true }).defaultPrevented).toBe(true);
  await settle();
  expect(sent()).toBe(2);

  const discuss = rig.composer({ sendOnEnter: true });
  type(field(discuss), "three");
  expect(key(field(discuss), "Enter", { shiftKey: true }).defaultPrevented).toBe(false);
  expect(key(field(discuss), "Enter", { ctrlKey: true }).defaultPrevented).toBe(false);
  expect(key(field(discuss), "Enter", { isComposing: true }).defaultPrevented).toBe(false);
  expect(sent()).toBe(2);
  expect(key(field(discuss), "Enter").defaultPrevented).toBe(true);
  await settle();
  expect(sent()).toBe(3);
  expect(chatter.isSendChord(new KeyboardEvent("keydown", { key: "a", ctrlKey: true }))).toBe(false);
});

test("a post sends exactly the closed thread_post object and places the answer in the window before the request settles", async () => {
  const rig = new Rig({ config: { collectOnSettle: true } });
  rig.records(ann);
  rig.load([message(1)]);
  rig.transport.postHandler = answerWith(rig, 9);
  const thread = rig.thread({ order: "asc" });
  const composer = rig.composer();
  type(field(composer), "Hello @Ann", 10);
  rig.store.local("mail_ui.thread").set("mail.thread:7", { mentions: [{ start: 6, end: 10, partnerId: 1, text: "@Ann" }] });
  await composer.send();
  await settle();
  expect(rig.transport.posts).toHaveLength(1);
  expect(rig.transport.posts[0]?.ref).toEqual(rig.ref);
  expect(rig.transport.posts[0]?.dto).toEqual({
    body: "Hello @Ann",
    message_type: "comment",
    subtype: "comment",
    mentions: [{ start: 6, end: 10, partner_id: 1 }],
    client_uid: "uid-1",
  });
  // The message was kept by the collect that runs when the request settles, because the window named it first.
  expect(rig.store.record("mail.message", 9).exists).toBe(true);
  expect(thread.displayedIds()).toEqual([1, 9]);
  expect(thread.element.querySelector(".erp-mail-pending")).toBeNull();
  expect(field(composer).value).toBe("");
  expect(polite(composer.element)).toBe("Message sent");
  expect(rig.errors).toEqual([]);

  // A note, a reply and a silent post change only their own keys.
  const note = rig.composer({ silent: true });
  note.setKind("note");
  rig.store.local("mail_ui.thread").set("mail.thread:7", { reply_to: 1 });
  type(field(note), "Internal");
  rig.transport.postHandler = answerWith(rig, 10);
  await note.send();
  await settle();
  expect(rig.transport.posts[1]?.dto).toEqual({ body: "Internal", message_type: "comment", subtype: "note", parent_id: 1, silent: true, client_uid: "uid-2" });
  expect(rig.store.local("mail_ui.thread").get("mail.thread:7", "reply_to")).toBeNull();
  expect(Object.keys(rig.transport.posts[0]!.dto).sort()).toEqual(["body", "client_uid", "mentions", "message_type", "subtype"]);
});

test("when the answer cannot be placed in the window the latest page is loaded instead", async () => {
  const rig = new Rig({ config: { pageSize: 2, maxWindowPages: 1 } });
  rig.records(ann);
  rig.load([message(5), message(6)], true);
  rig.records({ "mail.message": [message(3), message(4)] });
  rig.store.windows.prependOlder(rig.ref, [3, 4], false);
  expect(rig.store.windows.get(rig.ref)).toEqual({ ids: [3, 4], hasOlder: false, hasNewer: true });
  rig.pager.latestHandler = () => {
    rig.store.windows.replace(rig.ref, [9, 10], true);
    return Promise.resolve();
  };
  rig.transport.postHandler = answerWith(rig, 10);
  const composer = rig.composer();
  type(field(composer), "gap");
  expect(await composer.send()).toBe("sent");
  expect(rig.pager.latest).toEqual([rig.ref]);
  expect(rig.store.windows.get(rig.ref)?.ids).toEqual([9, 10]);
  expect(rig.store.local("mail_ui.thread").get("mail.thread:7", "pending")).toEqual([]);
  // A thread without a window is loaded the same way.
  rig.store.windows.drop(rig.ref);
  type(field(composer), "no window");
  rig.transport.postHandler = answerWith(rig, 11);
  expect(await composer.send()).toBe("sent");
  expect(rig.pager.latest).toHaveLength(2);
});

test("a failed post stays as an unsent row that is retried with the same client_uid or discarded", async () => {
  const rig = new Rig();
  rig.records(ann);
  rig.load([message(1)]);
  const thread = rig.thread({ order: "asc" });
  const composer = rig.composer();
  const offline = new Error("offline");
  rig.transport.postHandler = () => Promise.reject(offline);
  type(field(composer), "Please send");
  expect(await composer.send()).toBe("failed");
  await settle();
  const row = thread.element.querySelector(".erp-mail-pending") as HTMLElement;
  expect(row.classList.contains("is-failed")).toBe(true);
  expect(row.getAttribute("aria-label")).toBe("Unsent message");
  expect(row.querySelector(".erp-mail-pending-text")?.textContent).toBe("Please send");
  expect(row.querySelector(".erp-mail-pending-error")?.textContent).toBe("The message was not sent. offline");
  expect(row.querySelector(".erp-mail-pending-error")?.getAttribute("role")).toBe("alert");
  expect(field(composer).value).toBe("");
  expect(rig.errors).toEqual([offline]);
  expect(assertive(composer.element)).toBe("The message was not sent");
  // The row is drawn at the newest end.
  expect(thread.element.querySelector('[role="feed"]')?.lastElementChild).toBe(row);

  rig.transport.postHandler = answerWith(rig, 20, true);
  const retry = row.querySelector(".erp-mail-retry") as HTMLButtonElement;
  retry.click();
  retry.click();
  await settle();
  // A double click joins the send already running.
  expect(rig.transport.posts.map((p) => p.dto.client_uid)).toEqual(["uid-1", "uid-1"]);
  expect(rig.transport.posts[1]?.dto).toEqual(rig.transport.posts[0]?.dto);
  expect(thread.element.querySelector(".erp-mail-pending")).toBeNull();
  expect(thread.displayedIds()).toEqual([1, 20]);

  // Discard removes the text for good.
  rig.transport.postHandler = () => Promise.reject(offline);
  type(field(composer), "Second");
  await composer.send();
  await settle();
  const second = thread.element.querySelector(".erp-mail-pending") as HTMLElement;
  (second.querySelector(".erp-mail-discard") as HTMLButtonElement).click();
  await settle();
  expect(thread.element.querySelector(".erp-mail-pending")).toBeNull();
  expect(rig.store.local("mail_ui.pending").ids()).toEqual([]);
  expect(polite(thread.element)).toBe("Unsent message discarded");
});

/// (partial: Retry and Discard on a failed pending post move focus before replacing children)
test("Retry and Discard on failed pending message move focus without dropping to body", async () => {
  const rig = new Rig();
  rig.records(ann);
  rig.load([message(1)]);
  const thread = rig.thread({ order: "asc" });
  const composer = rig.composer();
  const offline = new Error("offline");
  rig.transport.postHandler = () => Promise.reject(offline);
  type(field(composer), "Failing msg");
  expect(await composer.send()).toBe("failed");
  await settle();

  const row = thread.element.querySelector(".erp-mail-pending") as HTMLElement;
  expect(row).not.toBeNull();
  const retry = row.querySelector(".erp-mail-retry") as HTMLButtonElement;
  retry.focus();
  expect(thread.element.ownerDocument.activeElement).toBe(retry);

  rig.transport.postHandler = answerWith(rig, 20, true);
  retry.click();
  expect(thread.element.ownerDocument.activeElement).not.toBe(thread.element.ownerDocument.body);
  await settle();

  rig.transport.postHandler = () => Promise.reject(offline);
  type(field(composer), "Discard me");
  await composer.send();
  await settle();

  const secondRow = thread.element.querySelector(".erp-mail-pending") as HTMLElement;
  const discard = secondRow.querySelector(".erp-mail-discard") as HTMLButtonElement;
  discard.focus();
  expect(thread.element.ownerDocument.activeElement).toBe(discard);

  discard.click();
  expect(thread.element.ownerDocument.activeElement).not.toBe(thread.element.ownerDocument.body);
  await settle();
  expect(thread.element.ownerDocument.activeElement).toBe(field(composer));
});

/// (partial: focus queries its own root and does not jump to a sibling chatter)
test("pending post focus query is scoped to its own container and does not jump to sibling composer", async () => {
  const rig1 = new Rig();
  rig1.records(ann);
  rig1.load([message(1)]);
  const container1 = document.createElement("div");
  container1.className = "erp-mail-chatter";
  const thread1 = rig1.thread();
  const composer1 = rig1.composer();
  container1.append(composer1.element, thread1.element);
  document.body.append(container1);

  const rig2 = new Rig();
  rig2.records(ann);
  rig2.load([message(1)]);
  const container2 = document.createElement("div");
  container2.className = "erp-mail-chatter";
  const thread2 = rig2.thread();
  const composer2 = rig2.composer();
  container2.append(composer2.element, thread2.element);
  document.body.append(container2);

  const offline = new Error("offline");
  rig2.transport.postHandler = () => Promise.reject(offline);
  type(field(composer2), "Fail in second");
  await composer2.send();
  await settle();

  const row = thread2.element.querySelector(".erp-mail-pending") as HTMLElement;
  const discard = row.querySelector(".erp-mail-discard") as HTMLButtonElement;
  discard.click();
  await settle();

  expect(document.activeElement).toBe(field(composer2));
  expect(document.activeElement).not.toBe(field(composer1));
});

test("an answer that breaks the contract or cannot be applied leaves a failed row naming why", async () => {
  const rig = new Rig();
  rig.records(ann);
  rig.load([message(1)]);
  const thread = rig.thread();
  const composer = rig.composer();
  const failed = () => (thread.element.querySelector(".erp-mail-pending-error") as HTMLElement).textContent;
  const discard = async () => {
    (thread.element.querySelector(".erp-mail-discard") as HTMLButtonElement).click();
    await settle();
  };
  rig.transport.postHandler = () => Promise.resolve({ replayed: false, store: "" } as never);
  type(field(composer), "a");
  await composer.send();
  await settle();
  expect(failed()).toBe("The message was not sent. thread_post answer.message_id must be a positive integer, got undefined");
  await discard();
  rig.transport.postHandler = () => Promise.resolve({ message_id: 3, replayed: "no", store: "" } as never);
  type(field(composer), "b");
  await composer.send();
  await settle();
  expect(failed()).toBe('The message was not sent. thread_post answer.replayed must be a boolean, got "no"');
  await discard();
  rig.transport.postHandler = () => Promise.resolve({ message_id: 3, replayed: false } as never);
  type(field(composer), "c");
  await composer.send();
  await settle();
  expect(failed()).toBe("The message was not sent. thread_post answer.store must be the store payload text");
  await discard();
  rig.transport.postHandler = () => Promise.resolve({ message_id: 3, replayed: false, store: rig.payload({ "mail.message": [message(3)] }, "other") });
  type(field(composer), "d");
  await composer.send();
  await settle();
  expect(failed()).toBe(
    "The message was not sent. the server accepted the post but answered with schema other, which this tab does not hold: retry after the schema is fetched",
  );
  expect(rig.errors.every((e) => e instanceof ThreadViewError)).toBe(true);
  expect(rig.errors.map((e) => (e as ThreadViewError).code)).toEqual(["transport", "transport", "transport", "transport"]);
});

test("a draft that cannot be sent is refused and kept with the reason shown, never sent partly", async () => {
  const rig = new Rig();
  rig.records(ann);
  const composer = rig.composer();
  const line = composer.element.querySelector(".erp-mail-composer-error") as HTMLElement;
  expect(await composer.send()).toBe("refused");
  expect(rig.transport.posts).toEqual([]);

  type(field(composer), "Hi @Ann");
  rig.store.local("mail_ui.thread").set("mail.thread:7", { mentions: [{ start: 2, end: 6, partnerId: 1, text: "@Ann" }] });
  expect(await composer.send()).toBe("refused");
  expect(line.hidden).toBe(false);
  expect(line.textContent).toBe('mention of partner 1 at 2..6 no longer matches its text "@Ann": the draft holds " @An"');
  expect(field(composer).value).toBe("Hi @Ann");
  expect(rig.errors.map((e) => (e as ThreadViewError).code)).toEqual(["mention"]);
  expect(assertive(composer.element)).toBe(line.textContent);

  rig.store.local("mail_ui.thread").set("mail.thread:7", { mentions: [], text: "x".repeat(1_048_577) });
  flushSync();
  expect(await composer.send()).toBe("refused");
  expect(line.textContent).toBe("the draft is 1048577 bytes; a post holds at most 1048576");

  // Too many unsent posts in one thread.
  rig.store.local("mail_ui.thread").set("mail.thread:7", { text: "another" });
  rig.transport.postHandler = () => Promise.reject(new Error("offline"));
  for (let i = 0; i < MAX_PENDING; i += 1) {
    type(field(composer), `p${i}`);
    expect(await composer.send()).toBe("failed");
  }
  type(field(composer), "one too many");
  expect(await composer.send()).toBe("refused");
  expect(line.textContent).toBe(`mail.thread:7 already has ${MAX_PENDING} unsent posts; the limit is ${MAX_PENDING}: retry or discard some first`);
  expect(field(composer).value).toBe("one too many");
  expect(rig.transport.posts).toHaveLength(MAX_PENDING);
});

test("the reply chip names the author, is cancelled by its button or Escape, and its message becomes the post's parent", async () => {
  const rig = new Rig();
  rig.records(ann);
  rig.load([message(1, { author_id: 1 }), message(2, { author_id: 9 })]);
  const composer = rig.composer();
  const chip = composer.element.querySelector(".erp-mail-reply") as HTMLElement;
  expect(chip.hidden).toBe(true);
  const state = rig.store.local("mail_ui.thread");
  state.set("mail.thread:7", { reply_to: 1 });
  flushSync();
  expect(chip.hidden).toBe(false);
  expect(chip.textContent).toBe("Replying to Ann Cancel");
  const cancel = chip.querySelector("button") as HTMLButtonElement;
  expect(cancel.getAttribute("aria-label")).toBe("Cancel reply to Ann");
  cancel.click();
  flushSync();
  expect(state.get("mail.thread:7", "reply_to")).toBeNull();
  expect(document.activeElement).toBe(field(composer));
  state.set("mail.thread:7", { reply_to: 2 });
  flushSync();
  expect(chip.textContent).toBe("Replying to Partner #9 Cancel");
  expect(key(field(composer), "Escape").defaultPrevented).toBe(true);
  expect(state.get("mail.thread:7", "reply_to")).toBeNull();
  state.set("mail.thread:7", { reply_to: 1 });
  rig.transport.postHandler = answerWith(rig, 30);
  type(field(composer), "answer");
  await composer.send();
  await settle();
  expect(rig.transport.posts[0]?.dto.parent_id).toBe(1);
  expect(chip.hidden).toBe(true);
});

/// (partial: composer stages attachments, renders chips, validates size and count limits, and allows empty text post)
test("the composer stages attachments with chips, validates limits, and allows sending with empty text", async () => {
  const rig = new Rig();
  rig.records(ann);
  const composer = rig.composer();
  const send = composer.element.querySelector(".erp-mail-send") as HTMLButtonElement;
  const chipsArea = composer.element.querySelector(".erp-mail-composer-attachments") as HTMLElement;
  const errorLine = composer.element.querySelector(".erp-mail-composer-error") as HTMLElement;

  expect(chipsArea.hidden).toBe(true);
  expect(send.getAttribute("aria-disabled")).toBe("true");

  // Stage an attachment
  composer.attach({ id: 201, name: "spec.pdf", size: 1024 * 200 });
  flushSync();
  expect(composer.attachments).toEqual([{ id: 201, name: "spec.pdf", size: 204800 }]);
  expect(chipsArea.hidden).toBe(false);
  expect(send.getAttribute("aria-disabled")).toBe("false");

  const chips = chipsArea.querySelectorAll<HTMLElement>(".erp-mail-attachment-chip");
  expect(chips).toHaveLength(1);
  expect(chips[0]?.querySelector(".erp-mail-attachment-chip-name")?.textContent).toBe("spec.pdf");
  expect(chips[0]?.querySelector(".erp-mail-attachment-chip-size")?.textContent).toBe(" (200 KB)");
  const removeBtn = chips[0]?.querySelector<HTMLButtonElement>(".erp-mail-attachment-chip-remove");
  expect(removeBtn?.getAttribute("aria-label")).toBe("Remove spec.pdf");

  // Max attachments limit (100)
  for (let i = 1; i < 100; i++) {
    composer.attach({ id: 300 + i, name: `file_${i}.txt`, size: 10 });
  }
  expect(composer.attachments).toHaveLength(100);
  composer.attach({ id: 999, name: "overflow.txt", size: 10 });
  flushSync();
  expect(composer.attachments).toHaveLength(100);
  expect(errorLine.textContent).toContain("Cannot attach more than 100 files");

  // Remove chips back to just 201
  for (let i = 1; i < 100; i++) {
    composer.removeAttachment(300 + i);
  }
  expect(composer.attachments).toHaveLength(1);

  // buildPost includes attachmentIds
  rig.transport.postHandler = answerWith(rig, 55);
  const postInput = composer.buildPost();
  expect(postInput.attachmentIds).toEqual([201]);

  // Send with empty text but with attachment succeeds
  const outcome = await composer.send();
  await settle();
  expect(outcome).toBe("sent");
  expect(rig.transport.posts[0]?.dto.attachment_ids).toEqual([201]);
  expect(composer.attachments).toEqual([]);
  expect(chipsArea.hidden).toBe(true);

  // Clicking remove button
  composer.attach({ id: 205, name: "test.txt", size: 50 });
  flushSync();
  expect(composer.attachments).toHaveLength(1);
  const newRemoveBtn = chipsArea.querySelector<HTMLButtonElement>(".erp-mail-attachment-chip-remove");
  newRemoveBtn?.click();
  flushSync();
  expect(composer.attachments).toHaveLength(0);
  expect(chipsArea.hidden).toBe(true);
  expect(send.getAttribute("aria-disabled")).toBe("true");
});

/// (partial: attach action triggers file upload when uploadAttachment service is provided)
test("attach composer action triggers file input when uploadAttachment service is present", async () => {
  const rig = new Rig();
  rig.records(ann);
  const uploadedFiles: File[] = [];
  rig.services = {
    ...rig.services,
    uploadAttachment: async (file: File) => {
      uploadedFiles.push(file);
      return { id: 777, name: file.name, size: file.size, mimetype: file.type };
    },
  };

  const composer = rig.composer();
  const attachBtn = composer.element.querySelector('.erp-mail-composer-action[data-action="attach"]') as HTMLButtonElement;
  expect(attachBtn).not.toBeNull();
  expect(attachBtn.querySelector("svg")).not.toBeNull();
  expect(attachBtn.textContent).toContain("Attach files");

  // Clicking attach action triggers chooseFiles (which clicks file input)
  let clicked = false;
  const fileInput = composer.element.querySelector<HTMLInputElement>(".erp-mail-file-input")!;
  fileInput.click = () => { clicked = true; };
  attachBtn.click();
  expect(clicked).toBe(true);
});

/// (partial: a failed later upload leaves the earlier file visible as a chip)
test("when the second of two files fails to upload, the first still shows as a chip", async () => {
  const rig = new Rig();
  rig.records(ann);
  let calls = 0;
  rig.services = {
    ...rig.services,
    uploadAttachment: async (file: File) => {
      calls += 1;
      if (calls === 2) throw new Error("upload failed");
      return { id: 801, name: file.name, size: file.size, mimetype: file.type };
    },
  };
  const composer = rig.composer();
  const input = composer.element.querySelector<HTMLInputElement>(".erp-mail-file-input")!;
  Object.defineProperty(input, "files", { value: [new File(["a"], "a.txt"), new File(["b"], "b.txt")], configurable: true });
  input.dispatchEvent(new Event("change"));
  await settle();
  expect(composer.attachments.map((a) => a.id)).toEqual([801]);
  const chips = composer.element.querySelectorAll(".erp-mail-attachment-chip");
  expect(chips).toHaveLength(1);
  expect(chips[0]!.getAttribute("data-id")).toBe("801");
});

/// (partial: the size limit comes only from the injected upload, no client constant)
test("a file above the old 25 MiB figure is passed to the upload and the configured refusal is shown", async () => {
  const rig = new Rig();
  rig.records(ann);
  const offered: string[] = [];
  const limit = 64 * 1024 * 1024;
  rig.services = {
    ...rig.services,
    uploadAttachment: async (file: File) => {
      offered.push(file.name);
      if (file.size > limit) throw new Error(`"${file.name}" exceeds the current browser upload limit of 64 MiB.`);
      return { id: 900 + offered.length, name: file.name, size: file.size, mimetype: file.type };
    },
  };
  const composer = rig.composer();
  const input = composer.element.querySelector<HTMLInputElement>(".erp-mail-file-input")!;
  const errorLine = composer.element.querySelector(".erp-mail-composer-error") as HTMLElement;
  const pick = (file: File): void => {
    Object.defineProperty(input, "files", { value: [file], configurable: true });
    input.dispatchEvent(new Event("change"));
  };
  const sized = (name: string, size: number): File => {
    const file = new File(["x"], name);
    Object.defineProperty(file, "size", { value: size });
    return file;
  };
  pick(sized("thirty.bin", 30 * 1024 * 1024));
  await settle();
  expect(offered).toEqual(["thirty.bin"]);
  expect(composer.attachments.map((a) => a.name)).toEqual(["thirty.bin"]);
  pick(sized("huge.bin", 100 * 1024 * 1024));
  await settle();
  expect(offered).toEqual(["thirty.bin", "huge.bin"]);
  expect(errorLine.textContent).toBe('"huge.bin" exceeds the current browser upload limit of 64 MiB.');
  expect(errorLine.textContent).not.toContain("25 MiB");
});

/// (partial: the paperclip carries the host's limit hint for keyboard and screen reader)
test("the composer paperclip describes the upload limit through aria-describedby", () => {
  const rig = new Rig();
  rig.records(ann);
  rig.services = {
    ...rig.services,
    uploadAttachment: async (file: File) => ({ id: 1, name: file.name }),
    uploadLimitHint: () => "Each file can be up to 128 MiB.",
  };
  const composer = rig.composer();
  const attachBtn = composer.element.querySelector<HTMLButtonElement>('.erp-mail-composer-action[data-action="attach"]')!;
  const id = attachBtn.getAttribute("aria-describedby")!;
  expect(composer.element.querySelector(`#${id}`)?.textContent).toBe("Each file can be up to 128 MiB.");
  expect(attachBtn.querySelector(`#${id}`)).toBeNull(); // next to the button: inside it the hint would be part of the button's name
});
