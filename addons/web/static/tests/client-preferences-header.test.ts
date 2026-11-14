/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
import { readFileSync } from "node:fs";
import { resolve } from "node:path";
import { afterEach, expect, test } from "vitest";
import { renderFormLayout } from "../src/client/form-layout.js";
import type { Node } from "../src/client/metadata.js";
import { applyColorScheme, loadUserColorScheme } from "../src/client/theme.js";
import { RpcClient } from "../src/client/transport.js";

const css = readFileSync(resolve(process.cwd(), "..", "addons/web/static/src/client/client.css"), "utf8");
afterEach(() => { document.body.replaceChildren(); delete document.documentElement.dataset["theme"]; });

const field = (name: string, widget = "char", label = name) => ({ kind: "field", force_save: false, field: { name }, label, widget });
const titleNodes = [{ kind: "root", children: [{ kind: "sheet", children: [{ kind: "title", children: [
  { kind: "heading", level: 1, children: [field("name")] }, field("email", "email", "Email"), field("phone", "phone", "Phone")
] }] }] }] as unknown as Node[];
/** A caption span and an input, the shape the real field widgets give the title block. */
const renderField = (node: Extract<Node, { kind: "field" }>, parent: HTMLElement): void => {
  const label = document.createElement("label"); label.className = "field-label";
  const caption = document.createElement("span"); caption.textContent = node.label;
  const input = document.createElement("input"); input.name = node.field.name;
  label.append(caption, input); parent.append(label);
};
const render = (profile?: { initial: string }): HTMLFormElement => {
  const form = document.createElement("form"); document.body.append(form);
  renderFormLayout(form, titleNodes, renderField, new Map(), undefined, undefined, profile ? { profile } : undefined);
  return form;
};

/// (partial: the client's presentation of the title block; the view itself is the server half)
test("a profile title block puts the initial circle left of the heading and one icon on each contact row", () => {
  const block = render({ initial: "R" }).querySelector<HTMLElement>(".form-title-block")!;
  expect(block.classList.contains("form-title-profile")).toBe(true);
  const [avatar, body] = [...block.children] as HTMLElement[];
  expect(avatar!.className).toContain("user-avatar");
  expect(avatar!.getAttribute("data-letter")).toBe("R");
  expect(avatar!.getAttribute("aria-hidden")).toBe("true");
  expect(body!.firstElementChild!.tagName).toBe("H1");
  const rows = [...body!.querySelectorAll<HTMLElement>(".form-title-row")];
  expect(rows.map(row => row.querySelector(":scope > svg")?.getAttribute("data-icon"))).toEqual(["mail", "phone"]);
  expect(rows.map(row => row.querySelector("input")!.placeholder)).toEqual(["Email", "Phone"]);
});

/// (partial: other forms keep the plain title block)
test("without the profile rule the title block has no avatar and no row icons", () => {
  const form = render();
  expect(form.querySelector(".user-avatar, .form-title-row, svg")).toBeNull();
  expect(form.querySelector(".form-title-block h1")).not.toBeNull();
});

/// (partial: the client applies the saved scheme to the document)
test("system leaves the document to the device, light and dark set data-theme, anything else is refused", () => {
  applyColorScheme("dark"); expect(document.documentElement.dataset["theme"]).toBe("dark");
  applyColorScheme("light"); expect(document.documentElement.dataset["theme"]).toBe("light");
  applyColorScheme("system"); expect(document.documentElement.dataset["theme"]).toBeUndefined();
  expect(() => applyColorScheme("sepia")).toThrow("Invalid colour scheme.");
  applyColorScheme("dark"); applyColorScheme(false); expect(document.documentElement.dataset["theme"]).toBeUndefined();
  expect(() => applyColorScheme(null)).toThrow("Invalid colour scheme.");
});

/// (partial: reads the signed-in user's own row)
test("loadUserColorScheme reads the user's color_scheme and applies it", async () => {
  const calls: { path: string; body: Record<string, unknown> }[] = [];
  const rpc = new RpcClient(async (path, init) => {
    calls.push({ path: String(path), body: JSON.parse(String(init?.body)) as Record<string, unknown> });
    return new Response(JSON.stringify([{ id: 2, color_scheme: "dark" }]), { headers: { "Content-Type": "application/json" } });
  }, () => {});
  await loadUserColorScheme(rpc, 2, { lang: "en_US" });
  expect(document.documentElement.dataset["theme"]).toBe("dark");
  expect(calls[0]!.path.endsWith("res.users/read")).toBe(true);
  expect(calls[0]!.body).toEqual({ ids: [2], fields: ["color_scheme"], context: { lang: "en_US" } });
});

/// (partial: an unexpected answer fails loudly and leaves the attribute alone)
test("loadUserColorScheme refuses an answer without exactly one row", async () => {
  const rpc = new RpcClient(async () => new Response("[]", { headers: { "Content-Type": "application/json" } }), () => {});
  await expect(loadUserColorScheme(rpc, 2, {})).rejects.toThrow("Invalid colour scheme response.");
  expect(document.documentElement.dataset["theme"]).toBeUndefined();
});

// Stylesheet pins (no spec case): the notice strip, the Settings link buttons and the profile rows' validation message.
test("the notice region has no tint of its own and a Settings link button keeps its content width", () => {
  expect(css).not.toMatch(/\.client-notice:not\(:empty\)/);
  expect(css).toMatch(/\.client-notice \{ position: relative; overflow-wrap: anywhere; \}/);
  expect(css).toMatch(/\.settings-action-link \{[^}]*align-self: flex-start;/);
  expect(css).toMatch(/\.form-title-row \.field-label > span:first-child:not\(\.field-error\):not\(\.field-help\) \{/);
});
