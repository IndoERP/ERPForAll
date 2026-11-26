/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
import { expect, test } from "vitest";
import { prepareRestrictedCard } from "../src/index.js";

const identity = { arch: "contacts.card", language: "id_ID", sourceSha256: "a".repeat(64) };
const plan = (templates: unknown) => JSON.stringify({
  abi: "erpforall.card.v1", policy: "1", arch: identity.arch,
  language: identity.language, source_sha256: identity.sourceSha256, fields: ["name", "shown"], templates,
});

test("one plan renders isolated records through calls, conditions and escaped text", () => {
  const render = prepareRestrictedCard(plan({
    card: { tag: "t", children: [{ tag: "t", when: ["shown","raw_value"], call: "title" }] },
    title: { tag: "strong", attrs: { class: "name" }, field: ["name","value"] },
  }), identity);
  const record = (shown: boolean) => JSON.stringify({ name: { value: "<Company>" }, shown: { raw_value: shown } });
  const fragment = render("card", record(true), document);
  expect(fragment.firstElementChild?.outerHTML).toBe('<strong class="name">&lt;Company&gt;</strong>');
  expect(render("card", record(false), document).textContent).toBe("");
  expect(() => render("card", "{}", document)).toThrow("Missing card property");
});

test("arch generation, element vocabulary, node shape and recursive expansion are checked", () => {
  const plans = {
    missing: { tag: "unavailable", children: [] },
    body: { tag: "t", call: "missing", children: [] },
    again: { tag: "t", call: "again" },
  };
  expect(() => prepareRestrictedCard(plan(plans), { ...identity, language: "en_US" })).toThrow("identity");
  const render = prepareRestrictedCard(plan(plans), identity);
  for (const name of Object.keys(plans)) expect(() => render(name, "{}", document)).toThrow();
});
