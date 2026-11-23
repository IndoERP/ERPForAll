/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
import { describe, expect, test, vi } from "vitest";
import { SearchModel } from "../src/client/search/search-model.js";
import { createSearchMenuPanel } from "../src/client/search/search-menu-panel.js";
import { decodeFavorite, favoriteStore, namesFavorite } from "../src/client/search/favorites.js";
import type { FavoriteDraft, FavoriteItem, FavoriteStore } from "../src/client/search/types.js";
import type { Field } from "../src/client/metadata.js";

const field = (name: string, kind: Field["kind"], label: string): Field =>
  ({ name, kind, label, required: false, readonly: false, relation: null, relationField: null, selection: null });
const fields = new Map<string, Field>([["name", field("name", "char", "Name")], ["city", field("city", "char", "City")]]);
const item = (id: string, name: string, extra: Partial<FavoriteItem> = {}): FavoriteItem =>
  ({ id, name, domain: [["city", "=", name]], groupBy: [], sort: [], isDefault: false, isShared: false, isOwn: true, isActive: false, ...extra });

/** A store over an in-memory list, recording what it was asked to save and delete. */
function memoryStore(initial: FavoriteItem[]) {
  let rows = structuredClone(initial), next = 100;
  const saved: FavoriteDraft[] = [], removed: string[] = [];
  const store: FavoriteStore = {
    load: vi.fn(async () => structuredClone(rows)),
    save: async draft => {
      saved.push(structuredClone(draft));
      const row = item(String(next++), draft.name, { domain: draft.domain, groupBy: draft.groupBy, isDefault: draft.isDefault, isShared: draft.isShared, isOwn: !draft.isShared });
      rows = [...rows.map(old => draft.isDefault ? { ...old, isDefault: false } : old), row];
      return structuredClone(row);
    },
    remove: async id => { removed.push(id); rows = rows.filter(row => row.id !== id); }
  };
  return { store, saved, removed };
}
function model(store: FavoriteStore, initialDomain: unknown[] = [["active", "=", true]]): SearchModel {
  const search = new SearchModel({ model: "x.contact", fields, initialDomain, groupBy: { load: async () => ({ items: [], fields: [{ name: "city", label: "City", kind: "char", selection: null }], skipped: [] }), multiple: true, persist: true } });
  search.declareFavorites(store);
  return search;
}

describe("favorites in the search model", () => {
  test("applying one replaces the search with its facet, and removing it leaves an empty search", async () => {
    const search = model(memoryStore([item("1", "Paris", { groupBy: ["city"] })]).store);
    await search.loadFavorites();
    search.addFieldFacet("name", "Ada");
    search.toggleFavorite("1");
    expect(search.getFacets().map(facet => [facet.type, facet.values])).toEqual([["favorite", ["Paris"]]]);
    expect(search.getDomain()).toEqual([["active", "=", true], ["city", "=", "Paris"]]);
    expect(search.getGroupBy()).toEqual(["city"]);
    expect(search.getFavorites()[0]!.isActive).toBe(true);
    search.toggleFavorite("1");
    expect(search.getFacets()).toEqual([]);
    expect(search.getGroupBy()).toEqual([]);
  });

  test("saving stores the facets' domain and grouping without the action's domain, then shows the saved favorite", async () => {
    const { store, saved } = memoryStore([item("1", "Paris", { isDefault: true })]);
    const search = model(store);
    await search.loadFavorites();
    search.addFieldFacet("name", "Ada");
    await search.loadGroupByCatalogue();
    search.addCustomGroupBy("city");
    const result = await search.saveFavorite("Ada by city", true, false);
    expect(saved).toEqual([{ name: "Ada by city", domain: [["name", "ilike", "Ada"]], groupBy: ["city"], isDefault: true, isShared: false }]);
    expect(search.getFacets().map(facet => [facet.type, facet.favoriteId])).toEqual([["favorite", result.id]]);
    // One default per user and action: the previous default is no longer marked.
    expect(search.getFavorites().map(entry => [entry.name, entry.isDefault])).toEqual([["Paris", false], ["Ada by city", true]]);
  });

  test("the address keeps an applied favorite and restores it next to other facets", async () => {
    const { store } = memoryStore([item("1", "Paris")]);
    const search = model(store);
    await search.loadFavorites();
    search.toggleFavorite("1");
    search.addFieldFacet("name", "Ada");
    const params = new URLSearchParams();
    search.serializeToUrlParams(params);
    expect(params.get("search_filters")).toBe(JSON.stringify(["favorite_1"]));
    expect(namesFavorite(params)).toBe(true);
    const reloaded = model(store);
    await reloaded.loadFavorites();
    expect(reloaded.loadFromUrlParams(params)).toBe(true);
    expect(reloaded.getFacets().map(facet => facet.type).sort()).toEqual(["favorite", "field"]);
    // A favorite that no longer exists is refused by the address, like any unknown filter.
    const gone = model(memoryStore([]).store);
    await gone.loadFavorites();
    expect(() => gone.loadFromUrlParams(params)).toThrowError(expect.objectContaining({ kind: "validation", message: "Invalid search_filters." }));
  });

  test("deleting one of the user's own favorites removes its facet; another user's shared favorite cannot be deleted", async () => {
    const { store, removed } = memoryStore([item("1", "Mine"), item("2", "Team", { isShared: true, isOwn: false })]);
    const search = model(store);
    await search.loadFavorites();
    search.toggleFavorite("1");
    await search.deleteFavorite("1");
    expect(removed).toEqual(["1"]);
    expect(search.getFacets()).toEqual([]);
    await expect(search.deleteFavorite("2")).rejects.toMatchObject({ kind: "validation" });
  });

  test("a favorite the server cannot apply is refused with its reason, and several defaults are refused by name", async () => {
    const search = model(memoryStore([item("1", "Odd", { problem: "its context sets 'lang', which the search panel does not apply" }), item("2", "A", { isDefault: true }), item("3", "B", { isDefault: true })]).store);
    await search.loadFavorites();
    expect(() => search.toggleFavorite("1")).toThrowError(expect.objectContaining({ kind: "unsupported", message: "The favorite “Odd” cannot be applied: its context sets 'lang', which the search panel does not apply." }));
    expect(() => search.defaultFavorite()).toThrowError(expect.objectContaining({ kind: "validation", message: expect.stringContaining("Several of your favorites are marked as the default (A, B)") }));
  });

  test("favorites are read once", async () => {
    const { store } = memoryStore([item("1", "Paris")]);
    const search = model(store);
    await Promise.all([search.loadFavorites(), search.loadFavorites()]);
    await search.loadFavorites();
    expect(store.load).toHaveBeenCalledTimes(1);
  });
});

describe("the Favorites section of the search menu", () => {
  test("lists the favorites after reading them, applies, saves and deletes", async () => {
    const { store, saved, removed } = memoryStore([item("1", "Paris", { isDefault: true }), item("2", "Team", { isShared: true, isOwn: false, sort: ["name desc"] })]);
    const search = model(store);
    const updates = vi.fn();
    const panel = createSearchMenuPanel(search, updates);
    document.body.append(panel.element);
    panel.open();
    const section = () => panel.element.querySelector<HTMLElement>(".erp-favorites-section")!;
    await vi.waitFor(() => expect([...section().querySelectorAll(".erp-item-label")].map(node => node.textContent)).toEqual(["Paris (default)", "Team (shared)"]));
    expect(section().textContent).toContain("Its list order is not applied.");
    // Only the user's own favorite can be deleted.
    expect([...section().querySelectorAll(".erp-favorite-delete")].map(node => node.getAttribute("aria-label"))).toEqual(["Delete favorite Paris"]);
    section().querySelector<HTMLButtonElement>(".erp-search-menu-item")!.click();
    expect(search.getFacets().map(facet => facet.favoriteId)).toEqual(["1"]);
    expect(updates).toHaveBeenCalledTimes(1);

    [...section().querySelectorAll<HTMLButtonElement>(".erp-search-toggle-form-btn")].at(-1)!.click();
    section().querySelector<HTMLInputElement>(".erp-favorite-name")!.value = "Shared Paris";
    section().querySelector<HTMLInputElement>(".erp-favorite-shared")!.checked = true;
    section().querySelector<HTMLFormElement>(".erp-save-favorite-form")!.dispatchEvent(new Event("submit", { cancelable: true }));
    await vi.waitFor(() => expect(saved).toEqual([{ name: "Shared Paris", domain: [["city", "=", "Paris"]], groupBy: [], isDefault: false, isShared: true }]));
    await vi.waitFor(() => expect(updates).toHaveBeenCalledTimes(2));

    section().querySelector<HTMLButtonElement>(".erp-favorite-delete")!.click();
    await vi.waitFor(() => expect(removed).toEqual(["1"]));
    panel.dispose();
  });

  test("a refused save is shown and changes nothing", async () => {
    const { store } = memoryStore([]);
    store.save = async () => { throw new Error("A shared favorite cannot be a default: save it as your own favorite to make it your default."); };
    const search = model(store);
    const panel = createSearchMenuPanel(search, () => undefined);
    document.body.append(panel.element);
    panel.open();
    await vi.waitFor(() => expect(panel.element.querySelector(".erp-favorites-section .erp-search-toggle-form-btn")).not.toBeNull());
    panel.element.querySelector<HTMLButtonElement>(".erp-favorites-section .erp-search-toggle-form-btn")!.click();
    panel.element.querySelector<HTMLInputElement>(".erp-favorite-name")!.value = "Both";
    panel.element.querySelector<HTMLFormElement>(".erp-save-favorite-form")!.dispatchEvent(new Event("submit", { cancelable: true }));
    await vi.waitFor(() => expect(panel.element.querySelector("[role=alert]")!.textContent).toBe("A shared favorite cannot be a default: save it as your own favorite to make it your default."));
    expect(search.getFacets()).toEqual([]);
    panel.dispose();
  });
});

describe("the ir.filters store", () => {
  test("reads, saves and deletes through the server's favorite calls", async () => {
    const calls: [string, string, Record<string, unknown>][] = [];
    const row = { id: 7, name: "Paris", is_default: false, shared: false, own: true, domain: [["city", "=", "Paris"]], group_by: ["city"], sort: [] };
    const rpc = { call: async (model: string, method: string, args: Record<string, unknown>) => { calls.push([model, method, args]); return method === "erp_get_favorites" ? [row] : method === "unlink" ? true : row; } };
    const store = favoriteStore(rpc, "x.contact", 20, { lang: "en_US" });
    expect(await store.load()).toEqual([{ id: "7", name: "Paris", domain: [["city", "=", "Paris"]], groupBy: ["city"], sort: [], isDefault: false, isShared: false, isOwn: true, isActive: false }]);
    await store.save({ name: "Paris", domain: [["city", "=", "Paris"]], groupBy: ["city"], isDefault: true, isShared: false });
    await store.remove("7");
    expect(calls).toEqual([
      ["ir.filters", "erp_get_favorites", { model: "x.contact", action_id: 20, context: { lang: "en_US" } }],
      ["ir.filters", "create_or_replace", { name: "Paris", model: "x.contact", action_id: 20, domain: [["city", "=", "Paris"]], group_by: ["city"], is_default: true, shared: false, context: { lang: "en_US" } }],
      ["ir.filters", "unlink", { ids: [7], context: { lang: "en_US" } }],
    ]);
  });

  test("a favorite with a problem keeps its reason; a malformed row is refused", () => {
    expect(decodeFavorite({ id: 3, name: "Odd", is_default: false, shared: true, own: false, problem: "its domain is invalid" }).problem).toBe("its domain is invalid");
    expect(() => decodeFavorite({ id: 3, name: "Odd", is_default: false, shared: true, own: false, domain: "[]", group_by: [], sort: [] })).toThrowError(expect.objectContaining({ kind: "server" }));
    expect(() => decodeFavorite({ id: 0, name: "Odd", is_default: false, shared: true, own: false })).toThrowError(expect.objectContaining({ kind: "server" }));
  });
});
