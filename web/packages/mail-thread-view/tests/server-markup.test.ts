/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
// The markup the server's thread_post renders (addons/mail/src/post.rs on wip/): mention anchors,
// escaped text and line breaks, and the hardening of the body renderer and the composer around them.
import { readFileSync } from "node:fs";
import { resolve } from "node:path";
import { withTestScope } from "@erpforall/elang-test";
import { expect, test } from "vitest";
import { BODY_MAX_DEPTH, BODY_MAX_WALK_DEPTH, Thread, ThreadViewError, buildPostDto, mentionPartnerId, renderBody } from "../src/index";
import { Rig, article, failsWith, key, message, settle, type } from "./support";

const html = (fragment: DocumentFragment): string => {
  const holder = document.createElement("div");
  holder.append(fragment);
  return holder.innerHTML;
};
const anchor = (id: string, extra = ""): string => `<a class="o_mail_redirect" data-oe-model="res.partner" data-oe-id="${id}"${extra}>@Ann</a>`;

test("the server's mention anchors become the renderer's own mention element and every forged anchor stays inert", () => {
  // The exact output of the server's render_text for "Hi @Ann <b>&'\"" with a mention of partner 9 on "@Ann".
  const server = `Hi ${anchor("9")} &lt;b&gt;&amp;&#39;&#34;`;
  expect(html(renderBody(server, document))).toBe(`Hi <span class="o-mail-mention" data-partner-id="9">@Ann</span> &lt;b&gt;&amp;'"`);
  // Escaped markup in the text, also inside the mention, stays text.
  expect(html(renderBody(`${anchor("9").replace("@Ann", "@&lt;b&gt;Otto")} &lt;b&gt;Otto`, document))).toBe(
    `<span class="o-mail-mention" data-partner-id="9">@&lt;b&gt;Otto</span> &lt;b&gt;Otto`,
  );
  expect(renderBody("&lt;b&gt;Otto", document).querySelector("b")).toBeNull();
  const forged = [
    anchor("9").replace("res.partner", "res.users"),
    anchor("9x"), anchor("0"), anchor("-3"), anchor("09"), anchor("9.5"), anchor(""), anchor("9007199254740993"),
    anchor("9", ' href="https://evil.example"'), anchor("9", ' onclick="x()"'), anchor("9", ' title="t"'), anchor("9", ' data-oe-x="1"'),
    anchor("9").replace('class="o_mail_redirect"', 'class="o_mail_redirect extra"'),
    anchor("9").replace('class="o_mail_redirect"', 'class="o-mail-mention"'),
    anchor("9").replace("<a ", "<span ").replace("</a>", "</span>"),
    '<a data-oe-model="res.partner" data-oe-id="9">@Ann</a>',
  ];
  for (const markup of forged) {
    const out = renderBody(markup, document);
    const holder = document.createElement("div");
    holder.append(out);
    expect(holder.querySelector(".o-mail-mention, [data-partner-id]"), markup).toBeNull();
    expect(holder.querySelector("[data-oe-id], [data-oe-model], [onclick]"), markup).toBeNull();
    expect(holder.textContent, markup).toBe("@Ann");
  }
  // A forged anchor with an https href is an ordinary safe link, never a mention.
  expect(html(renderBody(anchor("9", ' href="https://example.com/x"'), document))).toBe('<a href="https://example.com/x" rel="noopener noreferrer" target="_blank">@Ann</a>');
  const probe = (m: string) => mentionPartnerId(new DOMParser().parseFromString(m, "text/html").body.firstElementChild!);
  expect(probe(anchor("42"))).toBe(42);
  expect(probe(anchor("9007199254740991"))).toBe(9007199254740991);
  expect(probe(anchor("9007199254740992"))).toBeNull();
  expect(probe(anchor("5", " href=\"x\""))).toBeNull();

  // In a thread: a message with the server's markup draws the mention element, without a link.
  const rig = new Rig();
  rig.records({ "res.partner": [{ id: 1, name: "Ann" }] });
  rig.load([message(1, { body: server })]);
  const node = article(rig.thread(), 1);
  expect(node.querySelector('.erp-mail-body .o-mail-mention[data-partner-id="9"]')?.textContent).toBe("@Ann");
  expect(node.querySelector(".erp-mail-body a")).toBeNull();
});

test("a text with line breaks is drawn as separate lines, and the unsent row keeps them until the stored message replaces it", async () => {
  const rig = new Rig();
  rig.records({ "res.partner": [{ id: 2, name: "Bob" }] });
  rig.load([message(1, { body: "line one<br>line two<br><br>line four", author_id: 2 })]);
  const thread = rig.thread({ order: "asc" });
  const body = article(thread, 1).querySelector(".erp-mail-body") as HTMLElement;
  expect(body.innerHTML).toBe("line one<br>line two<br><br>line four");
  expect(body.querySelectorAll("br")).toHaveLength(3);

  // The server converts "\n" to <br> in a text body; the composer sends the text as typed.
  let answer!: () => void;
  rig.transport.postHandler = (dto) => new Promise((resolve) => {
    answer = () => resolve({ message_id: 9, replayed: false, store: rig.payload({ "mail.message": [message(9, { author_id: 2, body: dto.body.split("\n").join("<br>") })] }) });
  });
  const composer = rig.composer();
  type(composer.field, "first\nsecond");
  const sending = composer.send();
  await settle();
  expect(rig.transport.posts[0]?.dto.body).toBe("first\nsecond");
  expect(thread.element.querySelector(".erp-mail-pending-text")?.textContent).toBe("first\nsecond");
  answer();
  await sending;
  await settle();
  expect(thread.element.querySelector(".erp-mail-pending")).toBeNull();
  expect(article(thread, 9).querySelector(".erp-mail-body")?.innerHTML).toBe("first<br>second");
  // Only the unsent row keeps line breaks by style; a stored body has its <br>.
  const css = readFileSync(resolve(process.cwd(), "packages/mail-thread-view/src/thread-view.css"), "utf8");
  const preWrap = css.split("}").filter((rule) => rule.includes("pre-wrap")).map((rule) => rule.split("{")[0]?.trim());
  expect(preWrap).toEqual([".erp-mail-pending-text"]);
});

test("unknown wrappers count toward the depth limit, and a draft with half an emoji is refused with its index", () => {
  // Kept elements are limited to 32 levels; wrappers that are unwrapped (tables) have their own, larger cap of 256.
  const nest = (n: number): string => "<div>".repeat(n) + "x" + "</div>".repeat(n);
  expect(html(renderBody(nest(BODY_MAX_DEPTH), document))).toContain("x");
  failsWith(() => renderBody(nest(BODY_MAX_DEPTH + 1), document), "body", `message body nests deeper than ${BODY_MAX_DEPTH} elements; the display limit is ${BODY_MAX_DEPTH}`);
  const wrap = (n: number): string => "<font>".repeat(n) + "x" + "</font>".repeat(n);
  expect(html(renderBody(wrap(BODY_MAX_WALK_DEPTH), document))).toBe("x");
  failsWith(() => renderBody(wrap(BODY_MAX_WALK_DEPTH + 1), document), "body", `message body nests deeper than ${BODY_MAX_WALK_DEPTH} elements in total; the display limit is ${BODY_MAX_WALK_DEPTH}`);
  // An email built from nested tables (table, tbody, tr, td per level): 20 levels keep their text.
  const table = (n: number): string => "<table><tbody><tr><td>".repeat(n) + "<p>deep text</p>" + "</td></tr></tbody></table>".repeat(n);
  expect(html(renderBody(table(20), document))).toBe("<p>deep text</p>");
  failsWith(() => renderBody(table(65), document), "body", `message body nests deeper than ${BODY_MAX_WALK_DEPTH} elements in total; the display limit is ${BODY_MAX_WALK_DEPTH}`);
  const input = (text: string) => ({ text, mentions: [], kind: "message" as const, replyTo: null });
  failsWith(() => buildPostDto(input("a\uD83D"), "u"), "body", "the draft has a lone surrogate (half of an emoji) at index 1: remove it before sending");
  failsWith(() => buildPostDto(input("ok \uDE00b"), "u"), "body", "the draft has a lone surrogate (half of an emoji) at index 3: remove it before sending");
  expect(buildPostDto(input("fine \u{1F600}"), "u").body).toBe("fine \u{1F600}");
});

test("a provider registered under another key than its delimiter is refused once, and a thread whose first layout fails releases its lease", async () => withTestScope(async (scope) => {
  const rig = new Rig();
  rig.registries.suggestionProviders.add("slash", { delimiter: "/", label: () => "Commands", search: () => [{ id: "help", label: "/help" }] });
  const composer = rig.composer();
  type(composer.field, "/he");
  type(composer.field, "/hel");
  await settle();
  expect(composer.element.querySelector(".erp-mail-suggestions")?.hasAttribute("hidden")).toBe(true);
  expect(rig.errors.map((e) => (e as ThreadViewError).message)).toEqual(['mail.suggestionProviders entry "slash" has delimiter "/": the key must equal the delimiter, so the provider is ignored']);
  expect((rig.errors[0] as ThreadViewError).code).toBe("registry");

  // Lease: a failing first layout throws from the constructor and leaves nothing held.
  const held = new Rig({ config: { maxThreads: 1 } });
  held.records({ "res.partner": [{ id: 1, name: "Ann" }] });
  held.load([message(1)]);
  for (const id of [8, 9]) held.store.windows.replace({ model: "mail.thread", id }, [], false);
  const boom = new Error("layout broke");
  const unpatch = scope.patch(Thread, () => ({ dayLabel() { throw boom; } }));
  expect(() => new Thread(held.threadOptions())).toThrowError(boom);
  unpatch();
  for (const id of [10, 11]) held.store.windows.replace({ model: "mail.thread", id }, [], false);
  held.store.collect();
  expect(held.store.windows.get(held.ref)).toBeUndefined();
}));

test("positions follow the order shown with unsent posts included, and unknown authors never reach the reporter", async () => {
  const rig = new Rig();
  rig.records({ "res.partner": [{ id: 1, name: "Ann" }, { id: 2, name: "Bob" }] });
  rig.load([message(1), message(2), message(3)]);
  rig.transport.postHandler = () => Promise.reject(new Error("offline"));
  const thread = rig.thread();
  const composer = rig.composer();
  type(composer.field, "unsent");
  await composer.send();
  await settle();
  const feed = thread.element.querySelector('[role="feed"]') as HTMLElement;
  expect([...feed.children].map((a) => `${(a as HTMLElement).dataset["messageId"] ?? "pending"}:${a.getAttribute("aria-posinset")}:${a.getAttribute("aria-setsize")}`)).toEqual([
    "pending:1:-1", "3:2:-1", "2:3:-1", "1:4:-1",
  ]);
  key(article(thread, 3), "ArrowDown");

  const many = new Rig();
  many.load(Array.from({ length: 70 }, (_, i) => message(i + 1, { author_id: 100 + i })));
  many.thread();
  expect(many.errors).toEqual([]);
});
