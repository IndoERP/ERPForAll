/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
import { afterEach, beforeEach, expect, test, vi } from "vitest";
import { clientLanguage, registerTranslations, resetTranslations, setClientLanguage, t } from "../src/client/translate.js";
import { configureClientTemplates } from "../src/client/registry.js";
import { HumanApp } from "../src/client/app.js";
import * as publicApi from "../src/client/public.js";
import { clientCatalogSkeleton, extractClientStrings } from "../../../../web/scripts/extract-client-strings.mjs";

let app: HumanApp | undefined;

beforeEach(() => configureClientTemplates([{
  name: "web.HumanControlPanel",
  render: () => {
    const root = document.createElement("section");
    root.setAttribute("data-control-panel", "");
    for (const key of ["breadcrumbs", "search", "actions", "extension", "tools"]) {
      const node = document.createElement("div");
      node.setAttribute(`data-control-${key}`, "");
      root.append(node);
    }
    return root;
  }
}]));

afterEach(() => {
  app?.dispose();
  app = undefined;
  resetTranslations();
  vi.unstubAllGlobals();
  vi.restoreAllMocks();
  document.body.replaceChildren();
  history.replaceState(null, "", "/");
});

test("a text without a catalog stays English and takes its placeholder values", () => {
  expect(clientLanguage()).toBe("en_US");
  expect(t("No records yet")).toBe("No records yet");
  expect(t("No {name} records yet", { name: "Notes" })).toBe("No Notes records yet");
  expect(() => t("No {name} records yet")).toThrow("needs a value for {name}");
});

test("the user's language selects its catalog, then its base language, then English", () => {
  registerTranslations("fr_FR", { "No records yet": "Aucun enregistrement", "No {name} records yet": "Aucun enregistrement {name}" });
  registerTranslations("pt", { "Save": "Salvar" });
  setClientLanguage("fr_FR");
  expect(t("No records yet")).toBe("Aucun enregistrement");
  expect(t("No {name} records yet", { name: "Notes" })).toBe("Aucun enregistrement Notes");
  expect(t("Save")).toBe("Save");
  setClientLanguage("pt_BR");
  expect(t("Save")).toBe("Salvar");
  expect(t("No records yet")).toBe("No records yet");
});

test("several catalogs of one language add up, and repeating a translation is accepted", () => {
  registerTranslations("de_DE", { "Save": "Speichern" });
  registerTranslations("de_DE", { "Save": "Speichern", "Discard": "Verwerfen" });
  setClientLanguage("de_DE");
  expect([t("Save"), t("Discard")]).toEqual(["Speichern", "Verwerfen"]);
});

test("a malformed catalog is refused by name and keeps none of its entries", () => {
  expect(() => registerTranslations("fr_FR", [])).toThrow("must be an object");
  expect(() => registerTranslations("fr_FR", null)).toThrow("must be an object");
  expect(() => registerTranslations("fr_FR", new Map())).toThrow("must be an object");
  expect(() => registerTranslations("fr_FR", { "Save": 3 })).toThrow('translation of "Save" must be a non-empty text');
  expect(() => registerTranslations("fr_FR", { "Save": "" })).toThrow('translation of "Save" must be a non-empty text');
  expect(() => registerTranslations("fr_FR", { "": "Rien" })).toThrow("empty or overlong source text");
  expect(() => registerTranslations("fr_FR", { "No {name} records yet": "Aucun enregistrement" })).toThrow("must keep the placeholders {name}");
  expect(() => registerTranslations("fr_FR", { "Save": "Enregistrer {name}" })).toThrow("must keep the placeholders (none)");
  expect(() => registerTranslations("not a language", { "Save": "Enregistrer" })).toThrow("Invalid translation language");
  expect(() => setClientLanguage("")).toThrow("Invalid translation language");
  registerTranslations("fr_FR", { "Save": "Enregistrer" });
  expect(() => registerTranslations("fr_FR", { "Discard": "Annuler", "Save": "Sauver" })).toThrow('"Save" already has another fr_FR translation');
  setClientLanguage("fr_FR");
  // the refused catalog left nothing behind, not even its valid entry
  expect([t("Save"), t("Discard")]).toEqual(["Enregistrer", "Discard"]);
});

test("addons reach the mechanism through the public client API", () => {
  expect(publicApi.registerTranslations).toBe(registerTranslations);
  expect(publicApi.t).toBe(t);
  expect(publicApi.clientLanguage).toBe(clientLanguage);
});

test("the core empty state and New follow the user's language", async () => {
  registerTranslations("fr_FR", {
    "New": "Nouveau",
    "No {name} records yet": "Aucun enregistrement {name}",
    "Create your first {name} record with New.": "Créez votre premier enregistrement {name} avec Nouveau.",
  });
  history.replaceState(null, "", "/erp/action-20");
  const view = (type: "list" | "form", id: number): unknown => ({
    identity: { kind: "stored", id }, type, schema_major: 1, renderer: `web.${type}`, root: 0,
    nodes: [type === "list" ? { kind: "list", children: [1] } : { kind: "form", record_mode: "record", children: [1] },
      { kind: "field", force_save: false, field: { model: "test.note", name: "name" }, widget: "char", options: {}, modifiers: { readonly: null, required: null, invisible: null }, label: "Name" }]
  });
  const description = {
    wire_major: 1, registry_version: "r1", description_version: "d1", model: "test.note", notices: [],
    fields: { "test.note": { name: { type: "char", label: "Name", required: true, readonly: false, relation: null, relation_field: null, selection: null } } },
    views: [view("form", 1), view("list", 2)]
  };
  vi.stubGlobal("fetch", vi.fn(async (path: string) => {
    const action = { wire_major: 1, id: 20, name: "Notes", model: "test.note", target: "current", views: [{ id: 2, type: "list" }, { id: 1, type: "form" }], domain: null, context: null };
    const value = path.endsWith("erp_load_web_menus") ? { wire_major: 1, registry_version: "r1", roots: [10], nodes: [{ module: null, id: 10, parent_id: null, label: "Notes", action_id: 20, children: [], icon: null }] }
      : path.endsWith("erp_load_web_action_views") ? { action, views: description }
      : path.endsWith("erp_load_web_action") ? action
      : path.endsWith("get_views") ? description
      : path.endsWith("web_search_read") ? { length: 0, records: [] }
      : [];
    return new Response(JSON.stringify(value), { headers: { "Content-Type": "application/json" } });
  }));
  const root = document.createElement("div");
  for (const region of ["header", "navigation", "notice", "content"]) {
    const node = document.createElement("div");
    node.setAttribute(`data-client-${region}`, "");
    root.append(node);
  }
  document.body.append(root);
  app = new HumanApp(root, {
    wire_major: 1, registry_version: "r1", asset_version: "a1", identity: { uid: 2, name: "Operator", internal: true },
    companies: [{ id: 1, name: "Company" }], current_company_id: 1, allowed_company_ids: [1], active_company_ids: [1],
    locale: { lang: "fr_FR", tz: "UTC", date_format: "%d/%m/%Y", time_format: "%H:%M:%S" }, csrf: { logout: "test-token-not-secret" }
  });
  await app.start();
  expect(clientLanguage()).toBe("fr_FR");
  await vi.waitFor(() => expect(root.querySelector(".empty-state h2")?.textContent).toBe("Aucun enregistrement Notes"));
  expect(root.querySelector(".empty-state p")?.textContent).toBe("Créez votre premier enregistrement Notes avec Nouveau.");
  expect([...root.querySelectorAll("button")].some(button => button.textContent === "Nouveau")).toBe(true);
});

test("the extractor lists t() literals and refuses a text it cannot read", () => {
  expect(extractClientStrings('el("h2", t("No records yet")); x.t("skip"); set("skip"); t("A \\"quoted\\" {name}", { name });')).toEqual(["No records yet", 'A "quoted" {name}']);
  expect(() => extractClientStrings("const a = 1;\nel(t(title));", "a.ts")).toThrow("a.ts:2: t() takes one plain string literal");
  expect(() => extractClientStrings("t(`No ${name}`);")).toThrow("plain string literal");
});

test("the core client texts are extractable from the web addon", () => {
  const skeleton = clientCatalogSkeleton("web");
  for (const text of ["New", "Save", "Discard", "Edit", "Add a line", "No lines yet.", "No matching records", "No records yet",
    "No {name} records yet", "Create your first {name} record with New.", "There is nothing to show here yet.", "Try another search or clear the filter."]) {
    expect(skeleton[text]).toBe("");
  }
  expect(Object.keys(skeleton)).toEqual([...Object.keys(skeleton)].sort());
  expect(() => clientCatalogSkeleton("../web")).toThrow("Invalid addon name");
});
