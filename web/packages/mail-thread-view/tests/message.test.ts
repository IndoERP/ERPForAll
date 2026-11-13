/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
// The message: renderers, the safe body renderer, attachments, reactions, missing data and failures that stay visible.
import { flushSync } from "@erpforall/elang";
import { expect, test } from "vitest";
import { BODY_MAX_DEPTH, BODY_MAX_LENGTH, BODY_MAX_NODES, ThreadViewError, bodyText, initialsAvatar, renderBody, safeHref } from "../src/index";
import { Rig, article, failsWith, message } from "./support";

const ann = { "res.partner": [{ id: 1, name: "Ann" }, { id: 2, name: "Bob" }] };
const html = (fragment: DocumentFragment): string => {
  const holder = document.createElement("div");
  holder.append(fragment);
  return holder.innerHTML;
};
const tracking = JSON.stringify([
  { f: 3, n: "stage_id", t: "many2one", o: 1, v: 2, od: "New", vd: "Won" },
  { f: 4, n: "expected_revenue", t: "monetary", o: null, v: 1200.5, vd: "1,200.50" },
]);

test("tracking, notification and comment messages are drawn by the renderer registry in sequence order", () => {
  const rig = new Rig();
  rig.records(ann);
  rig.load([
    message(1, { message_type: "notification", body: "<p>Ann joined</p>", author_id: 1 }),
    message(2, { message_type: "notification", body: "", tracking }),
    message(3, { body: "<p>Hello <b>team</b></p>" }),
    message(4, { message_type: "email", body: "<p>From mail</p>" }),
  ]);
  const thread = rig.thread({ order: "asc" });
  expect(article(thread, 1).dataset["layout"]).toBe("row");
  expect(article(thread, 1).textContent).toContain("Ann joined");
  expect(article(thread, 3).dataset["layout"]).toBe("bubble");
  expect(article(thread, 3).querySelector(".erp-mail-body b")?.textContent).toBe("team");

  const lines = [...article(thread, 2).querySelectorAll(".erp-mail-tracking-change")];
  expect(article(thread, 2).dataset["layout"]).toBe("row");
  expect(lines.map((l) => l.getAttribute("data-field"))).toEqual(["stage_id", "expected_revenue"]);
  // Display strings captured at write time are shown; an empty old value reads "None"; the arrow is decorative.
  expect(lines[0]?.textContent).toBe("Stage New changed to Won");
  expect(lines[1]?.textContent).toBe("Expected revenue None changed to 1,200.50");
  expect(lines[0]?.querySelector(".erp-mail-tracking-arrow")?.getAttribute("aria-hidden")).toBe("true");
  expect(lines[0]?.querySelector(".erp-mail-tracking-arrow svg")).not.toBeNull();
  expect(rig.errors).toEqual([]);

  // A renderer with a lower sequence wins; removing it falls back to the default.
  rig.registries.messageRenderers.add("acme.email", {
    match: (m) => m.messageType === "email",
    layout: "row",
    render: ({ view, message: m }) => {
      const node = view.document.createElement("p");
      node.textContent = `email #${m.id}`;
      return node;
    },
  }, { sequence: 5 });
  flushSync();
  expect(article(thread, 4).textContent).toContain("email #4");
  expect(article(thread, 4).dataset["layout"]).toBe("row");
  rig.registries.messageRenderers.remove("acme.email");
  flushSync();
  expect(article(thread, 4).textContent).toContain("From mail");
  // The field label can come from the host's metadata.
  rig.services = { ...rig.services, fieldLabel: (model, field) => `${model}:${field}` };
  const labelled = rig.thread({ order: "asc" });
  expect(article(labelled, 2).querySelector(".erp-mail-tracking-field")?.textContent).toBe("mail.thread:stage_id");
});

test("a renderer that throws is replaced by a visible line and reported, and no renderer at all is a visible failure", () => {
  const rig = new Rig();
  rig.records(ann);
  rig.load([message(1), message(2, { message_type: "email" })]);
  const boom = new Error("renderer broke");
  rig.registries.messageRenderers.add("acme.email", {
    match: (m) => m.messageType === "email",
    layout: "bubble",
    render: () => {
      throw boom;
    },
  }, { sequence: 5 });
  const thread = rig.thread({ order: "asc" });
  const body = article(thread, 2).querySelector(".erp-mail-body-failed") as HTMLElement;
  expect(body.getAttribute("role")).toBe("alert");
  expect(body.textContent).toBe("This message could not be displayed.");
  expect(article(thread, 1).textContent).toContain("Message 1");
  expect(rig.errors).toEqual([boom]);

  rig.errors.length = 0;
  rig.registries.messageRenderers.remove("acme.email");
  rig.registries.messageRenderers.remove("comment");
  flushSync();
  expect(article(thread, 1).dataset["failed"]).toBe("true");
  expect(article(thread, 1).textContent).toBe("This message could not be displayed.");
  expect(rig.errors.map((e) => (e as ThreadViewError).message)).toEqual([
    "no mail.messageRenderers entry matches message 1 of type comment: register a default renderer",
    "no mail.messageRenderers entry matches message 2 of type email: register a default renderer",
  ]);
  expect(rig.errors.every((e) => (e as ThreadViewError).code === "registry")).toBe(true);
});

test("the body renderer copies only known elements and safe links and drops script, handlers, images and unknown markup", () => {
  const doc = document;
  expect(html(renderBody(
    '<p>ok <b>bold</b></p><script>alert(1)</script><img src="x" onerror="alert(1)"><a href="javascript:alert(1)">bad</a>' +
    '<a href="https://example.com/a?b=1" onclick="x()">good</a><iframe src="https://evil.example"></iframe><style>p{}</style>',
    doc,
  ))).toBe('<p>ok <b>bold</b></p><a>bad</a><a href="https://example.com/a?b=1" rel="noopener noreferrer" target="_blank">good</a>');
  expect(html(renderBody('<font color="red">plain</font><span style="x" onclick="y()" class="o-mail-mention other">@Ann</span><span class="other">kept text</span>', doc)))
    .toBe('plain<span>@Ann</span><span>kept text</span>');
  expect(html(renderBody('<a href="/relative">r</a><a href="data:text/html,x">d</a><a href="mailto:a@b.example">m</a><a href="tel:+62123">t</a><a href="HTTP://EXAMPLE.com">u</a>', doc)))
    .toBe('<a>r</a><a>d</a><a href="mailto:a@b.example" rel="noopener noreferrer" target="_blank">m</a><a href="tel:+62123" rel="noopener noreferrer" target="_blank">t</a><a href="http://example.com/" rel="noopener noreferrer" target="_blank">u</a>');
  expect(safeHref("vbscript:x")).toBeNull();
  expect(safeHref("not a url")).toBeNull();
  expect(bodyText("<p>One</p><p>Two<br>three</p><script>x</script>", doc)).toBe("One\nTwo\nthree");

  failsWith(() => renderBody("x".repeat(BODY_MAX_LENGTH + 1), doc), "body", `message body is ${BODY_MAX_LENGTH + 1} characters; the display limit is ${BODY_MAX_LENGTH}`);
  expect(html(renderBody("<p>x</p>".repeat(BODY_MAX_NODES / 2), doc))).toHaveLength("<p>x</p>".length * (BODY_MAX_NODES / 2));
  failsWith(() => renderBody("<p>x</p>".repeat(BODY_MAX_NODES / 2 + 1), doc), "body", `message body has more than ${BODY_MAX_NODES} nodes; the display limit is ${BODY_MAX_NODES}`);
  const nest = (n: number): string => "<div>".repeat(n) + "x" + "</div>".repeat(n);
  expect(html(renderBody(nest(BODY_MAX_DEPTH), doc))).toContain("x");
  failsWith(() => renderBody(nest(BODY_MAX_DEPTH + 1), doc), "body", `message body nests deeper than ${BODY_MAX_DEPTH} elements; the display limit is ${BODY_MAX_DEPTH}`);
  failsWith(() => renderBody(5 as unknown as string, doc), "body", "a message body must be text, got number");
});

test("a message body from the store never reaches the document as script or an event handler", () => {
  const rig = new Rig();
  rig.records(ann);
  rig.load([message(1, { body: '<p onclick="steal()">hi</p><script>window.__x = 1</script><a href="javascript:steal()">link</a>' })]);
  const thread = rig.thread();
  const node = article(thread, 1);
  expect(node.querySelector("script")).toBeNull();
  expect(node.querySelector("[onclick]")).toBeNull();
  expect(node.querySelector('a[href^="javascript"]')).toBeNull();
  expect(node.querySelector(".erp-mail-body")?.innerHTML).toBe("<p>hi</p><a>link</a>");
  expect((window as unknown as { __x?: number }).__x).toBeUndefined();
});

test("attachments are listed by name and type, and an author whose name is not held shows a placeholder without calling the reporter", () => {
  const rig = new Rig();
  rig.records({
    "res.partner": [{ id: 1, name: "Ann" }],
    "ir.attachment": [{ id: 4, name: "plan.pdf", mimetype: "application/pdf" }, { id: 5 }],
  });
  rig.load([
    message(1, { attachments: [4, 5] }),
    message(2, { author_id: 9 }),
    message(3, { author_id: 9 }),
    message(4, { author_id: null }),
  ]);
  const thread = rig.thread({ order: "asc" });
  const items = [...article(thread, 1).querySelectorAll(".erp-mail-attachment")].map((li) => li.textContent);
  expect(items).toEqual(["plan.pdf (application/pdf)", "Attachment #5"]);
  expect(article(thread, 1).querySelector("ul.erp-mail-attachments")?.getAttribute("aria-label")).toBe("Attachments");
  const author = (id: number) => article(thread, id).querySelector(".erp-mail-author")?.textContent;
  expect([author(1), author(2), author(3), author(4)]).toEqual(["Ann", "Partner #9", "Partner #9", "Unknown author"]);
  expect(rig.errors).toEqual([]);
  // The name arriving later redraws the articles.
  rig.records({ "res.partner": [{ id: 9, name: "Dee" }] });
  expect([author(2), author(3)]).toEqual(["Dee", "Dee"]);
});

test("a message the window lists but the store does not hold is a visible, reported failure", () => {
  const rig = new Rig();
  rig.records(ann);
  rig.load([message(1)]);
  rig.store.windows.appendLive(rig.ref, 50);
  const thread = rig.thread({ order: "asc" });
  const node = article(thread, 50);
  expect(node.dataset["failed"]).toBe("true");
  expect(node.textContent).toBe("This message could not be displayed.");
  expect(rig.errors.map((e) => (e as ThreadViewError).message)).toEqual(["mail.message#50 has no data although the thread's window lists it"]);
  // When the record arrives the article is drawn normally.
  rig.records({ "mail.message": [message(50, { body: "<p>Late</p>" })] });
  expect(article(thread, 50).dataset["failed"]).toBeUndefined();
  expect(article(thread, 50).textContent).toContain("Late");
});

/// (partial: message attachments list renders download link, formatted size, and image thumbnail)
test("message attachments render download link, formatted size, and image thumbnail preview", () => {
  const rig = new Rig();
  rig.records({
    "res.partner": [{ id: 1, name: "Ann" }],
    "ir.attachment": [
      { id: 10, name: "photo.png", mimetype: "image/png", file_size: 1024 * 50 },
      { id: 11, name: "data.csv", mimetype: "text/csv", file_size: 2048 },
    ],
  });
  rig.load([message(1, { attachments: [10, 11] })]);
  const thread = rig.thread({ order: "asc" });
  const art = article(thread, 1);
  const items = [...art.querySelectorAll<HTMLElement>(".erp-mail-attachment")];
  expect(items).toHaveLength(2);
  const links = [...art.querySelectorAll<HTMLAnchorElement>(".erp-mail-attachment-link")];
  expect(links).toHaveLength(2);
  expect(links[0]?.getAttribute("href")).toBe("/erp/content/10?download=true");
  expect(links[0]?.getAttribute("download")).toBe("photo.png");
  expect(items[0]?.textContent).toContain("photo.png");
  expect(items[0]?.textContent).toContain("(50 KB)");
  const thumb = items[0]?.querySelector<HTMLImageElement>(".erp-mail-attachment-thumbnail");
  expect(thumb).not.toBeNull();
  expect(thumb?.getAttribute("src")).toBe("/erp/content/10");
  expect(thumb?.getAttribute("alt")).toBe("photo.png");

  expect(links[1]?.getAttribute("href")).toBe("/erp/content/11?download=true");
  expect(items[1]?.textContent).toContain("data.csv");
  expect(items[1]?.textContent).toContain("(2.0 KB)");
  expect(items[1]?.querySelector(".erp-mail-attachment-thumbnail")).toBeNull();
});

/// (partial: author avatar shows initials with a stable colour tone and is hidden from assistive technology)
test("a message head shows author initials with a stable tone", () => {
  const rig = new Rig();
  rig.records(ann);
  rig.load([message(1, { body: "<p>One</p>", author_id: 1 }), message(2, { body: "<p>Two</p>", author_id: 1 })]);
  // Not squashed: a squashed message (same author, within minutes) has no avatar, only the gutter.
  const thread = rig.thread({ order: "asc", squash: false });
  const first = article(thread, 1).querySelector<HTMLElement>(".erp-mail-avatar")!;
  const second = article(thread, 2).querySelector<HTMLElement>(".erp-mail-avatar")!;
  expect(first.textContent).toBe("AN");
  expect(first.getAttribute("aria-hidden")).toBe("true");
  expect(first.dataset["tone"]).toBe(second.dataset["tone"]);
});

/// (partial: the row is the avatar beside one column; a squashed message keeps the gutter and shows no avatar)
test("a message row has the avatar beside the head and body, and a squashed one has no avatar", () => {
  const rig = new Rig();
  rig.records(ann);
  rig.load([message(1, { date: "2026-11-08 09:00:00", body: "<p>One</p>", author_id: 1 }), message(2, { date: "2026-11-08 09:01:00", body: "<p>Two</p>", author_id: 1 })]);
  const thread = rig.thread({ order: "asc", squash: true });
  const first = article(thread, 1).querySelector<HTMLElement>(".erp-mail-message-row")!;
  expect([...first.children].map((c) => c.className)).toEqual(["erp-mail-avatar", "erp-mail-message-main"]);
  expect(first.querySelector(".erp-mail-message-head .erp-mail-avatar")).toBeNull();
  expect(first.querySelector(".erp-mail-message-main > .erp-mail-message-head .erp-mail-author")?.textContent).toBe("Ann");
  expect(first.querySelector(".erp-mail-message-main .erp-mail-message-content")?.textContent).toBe("One");
  const second = article(thread, 2).querySelector<HTMLElement>(".erp-mail-message-row")!;
  expect([...second.children].map((c) => c.className)).toEqual(["erp-mail-message-main"]);
  // The day separator is a sibling of the row, so it spans the column.
  expect(first.parentElement?.querySelector(":scope > .erp-mail-day")).not.toBeNull();
});

/// (partial: initials are whole graphemes: an emoji, a decomposed accent, Arabic letters)
test("initials take whole graphemes, never half an emoji or a bare accent", () => {
  const initials = (name: string) => initialsAvatar(document, name).textContent;
  expect(initials("\u{1F600} Ann")).toBe("\u{1F600}A");
  expect(initials("\u{1F468}\u200D\u{1F469}\u200D\u{1F467} Lee")).toBe("\u{1F468}\u200D\u{1F469}\u200D\u{1F467}L");
  expect(initials("E\u0301lodie Martin")).toBe("E\u0301M");
  expect(initials("e\u0301lodie")).toBe("E\u0301L");
  expect(initials("\u0645\u062D\u0645\u062F \u0639\u0644\u064A")).toBe("\u0645\u0639");
  expect(initials("\u0645\u062D\u0645\u062F")).toBe("\u0645\u062D");
  expect(initials("Ann")).toBe("AN");
});
