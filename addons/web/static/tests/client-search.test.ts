/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
import { afterEach, beforeEach, describe, expect, test, vi } from "vitest";
import { SearchModel } from "../src/client/search/search-model.js";
import { createSearchBar } from "../src/client/search/search-bar.js";
import { ClientError } from "../src/client/transport.js";
import { createSearchMenuPanel } from "../src/client/search/search-menu-panel.js";
import type { Field } from "../src/client/metadata.js";
import { parseRoute, routeUrl } from "../src/client/route.js";

function setupModel(): SearchModel {
  const fields = new Map<string, Field>([
    ["name", { name: "name", kind: "char", label: "Title", required: true, readonly: false, relation: null, relationField: null, selection: null }],
    ["body", { name: "body", kind: "text", label: "Body", required: false, readonly: false, relation: null, relationField: null, selection: null }],
    ["owner_id", { name: "owner_id", kind: "many2one", label: "Owner", required: false, readonly: false, relation: "res.users", relationField: null, selection: null }],
    ["company_id", { name: "company_id", kind: "many2one", label: "Company", required: false, readonly: false, relation: "res.company", relationField: null, selection: null }],
    ["priority", { name: "priority", kind: "selection", label: "Priority", required: false, readonly: false, relation: null, relationField: null, selection: [["0", "Low"], ["1", "High"]] }],
    ["create_date", { name: "create_date", kind: "datetime", label: "Created On", required: false, readonly: true, relation: null, relationField: null, selection: null }],
    ["active", { name: "active", kind: "boolean", label: "Active", required: false, readonly: false, relation: null, relationField: null, selection: null }]
  ]);

  return new SearchModel({
    model: "test.record",
    fields,
    userId: 2,
    companyId: 1
  });
}

/// (partial: actual SearchModel facets and filters survive shell route serialization)
test("SearchModel state composes with canonical shell transport and restores its domain", () => {
  const source = setupModel();
  source.addFieldFacet("name", "書 & Budget + 20%");
  source.addFieldFacet("priority", "1", "=", "High");
  source.toggleFilter("filter_active");
  source.addCustomFilter("body", "ilike", "Notes");
  const params = new URLSearchParams();
  source.serializeToUrlParams(params);
  const route = parseRoute(new URL(`/web?action=42&${params}`, "https://erp.example.test"));
  const canonical = routeUrl(route);
  const reloaded = parseRoute(new URL(canonical, "https://erp.example.test"));
  expect(reloaded.search_facets).toBe(params.get("search_facets"));
  expect(reloaded.search_filters).toBe(params.get("search_filters"));
  const restored = setupModel();
  expect(restored.loadFromUrlParams(new URL(canonical, "https://erp.example.test").searchParams)).toBe(true);
  expect(restored.getDomain()).toEqual(source.getDomain());
  expect(restored.getGroupBy()).toEqual([]);
});

/// (partial: shell transport preserves SearchModel's explicit unsupported grouping)
test("transport admission does not silently restore a grouping from the address", () => {
  const canonical = routeUrl(parseRoute(new URL('/erp/action-42?search_groupby=%5B%22priority%22%5D', "https://erp.example.test")));
  const restored = setupModel();
  restored.addFieldFacet("name", "Retained");
  const before = restored.getDomain();
  expect(() => restored.loadFromUrlParams(new URL(canonical, "https://erp.example.test").searchParams)).toThrowError(expect.objectContaining({
    kind: "unsupported", message: "Restoring a grouping from the address is not available yet."
  }));
  expect(restored.getDomain()).toEqual(before);
});

describe("SearchModel State and Domain Construction", () => {
  beforeEach(() => {
    localStorage.clear();
  });

  test("adds field facet with single value and produces correct domain", () => {
    const model = setupModel();
    model.addFieldFacet("name", "Meeting");

    const facets = model.getFacets();
    expect(facets).toHaveLength(1);
    expect(facets[0]!.title).toBe("Title");
    expect(facets[0]!.values).toEqual(["Meeting"]);
    expect(model.getDomain()).toEqual([["name", "ilike", "Meeting"]]);
  });

  test("combines multiple values for the same field with Polish OR notation", () => {
    const model = setupModel();
    model.addFieldFacet("name", "Meeting");
    model.addFieldFacet("name", "Sprint");

    const facets = model.getFacets();
    expect(facets).toHaveLength(1);
    expect(facets[0]!.values).toEqual(["Meeting", "Sprint"]);
    expect(model.getDomain()).toEqual(["|", ["name", "ilike", "Meeting"], ["name", "ilike", "Sprint"]]);
  });

  test("combines different field facets with AND logic", () => {
    const model = setupModel();
    model.addFieldFacet("name", "Meeting");
    model.addFieldFacet("body", "Urgent");

    const facets = model.getFacets();
    expect(facets).toHaveLength(2);
    expect(model.getDomain()).toEqual([
      ["name", "ilike", "Meeting"],
      ["body", "ilike", "Urgent"]
    ]);
  });

  test("removeLastFacet removes trailing facet for backspace on empty input", () => {
    const model = setupModel();
    model.addFieldFacet("name", "Meeting");
    model.addFieldFacet("body", "Urgent");

    const removed = model.removeLastFacet();
    expect(removed).not.toBeNull();
    expect(removed!.fieldName).toBe("body");
    expect(model.getFacets()).toHaveLength(1);
    expect(model.getDomain()).toEqual([["name", "ilike", "Meeting"]]);
  });

  test("removeFacet by id removes specific facet", () => {
    const model = setupModel();
    model.addFieldFacet("name", "Meeting");
    const facets = model.getFacets();
    const id = facets[0]!.id;

    model.removeFacet(id);
    expect(model.getFacets()).toHaveLength(0);
    expect(model.getDomain()).toEqual([]);
  });

  test("editFieldFacet updates value and reconstructs domain", () => {
    const model = setupModel();
    model.addFieldFacet("name", "Meeting");
    const id = model.getFacets()[0]!.id;

    model.editFieldFacet(id, "Updated Meeting");
    expect(model.getFacets()[0]!.values).toEqual(["Updated Meeting"]);
    expect(model.getDomain()).toEqual([["name", "ilike", "Updated Meeting"]]);
  });

  test("toggles active and archived boolean filters", () => {
    const model = setupModel();
    model.toggleFilter("filter_active");

    expect(model.getFacets()).toHaveLength(1);
    expect(model.getFacets()[0]!.title).toBe("Active");
    expect(model.getDomain()).toEqual([["active", "=", true]]);

    model.toggleFilter("filter_active");
    expect(model.getFacets()).toHaveLength(0);
    expect(model.getDomain()).toEqual([]);
  });

  test("date filter options toggle date ranges", () => {
    const model = setupModel();
    model.toggleDateOption("filter_date_create_date", "date_create_date_month");

    expect(model.getFacets()).toHaveLength(1);
    expect(model.getFacets()[0]!.title).toContain("Created On: This Month");
    const domain = model.getDomain() as [string, string, unknown][];
    expect(domain).toHaveLength(2);
    expect(domain[0]![0]).toBe("create_date");
    expect(domain[0]![1]).toBe(">=");
    expect(domain[1]![0]).toBe("create_date");
    expect(domain[1]![1]).toBe("<");
  });

  test("adds custom filter and validates operator", () => {
    const model = setupModel();
    model.addCustomFilter("body", "ilike", "Important");

    expect(model.getFacets()).toHaveLength(1);
    expect(model.getDomain()).toEqual([["body", "ilike", "Important"]]);

    expect(() => model.addCustomFilter("non_existent", "ilike", "x")).toThrowError(expect.objectContaining({ kind: "validation", message: "Unknown field 'non_existent'." }));
    expect(() => model.addCustomFilter("body", "invalid_op", "x")).toThrowError(expect.objectContaining({ kind: "validation", message: "Unsupported operator 'invalid_op'." }));
  });

  test("unavailable grouping and server favorites fail explicitly", () => {
    const model = setupModel();
    expect(() => model.toggleGroupBy("priority")).toThrowError(expect.objectContaining({ kind: "unsupported", message: "Grouped lists are not available yet." }));
    expect(() => model.saveFavorite("Saved", true, true)).toThrowError(expect.objectContaining({ kind: "unsupported", message: "Saved favorites are not available yet." }));
    expect(model.getFacets()).toEqual([]);
  });

  test("offers one search row per text and relational field, and matching selection options", () => {
    const model = setupModel();
    const items = model.getAutocompleteItems("Alice");

    expect(items.map(i => [i.fieldName, i.operator, i.searchTerm])).toEqual([
      ["name", "ilike", "Alice"], ["body", "ilike", "Alice"], ["owner_id", "ilike", "Alice"], ["company_id", "ilike", "Alice"]
    ]);
    expect(items.find(i => i.fieldName === "name")!.label).toContain('Search Title for: "Alice"');
    // Without a name search there is nothing to expand into.
    expect(items.some(i => i.isParent)).toBe(false);

    const high = model.getAutocompleteItems("hi").filter(i => i.fieldName === "priority");
    expect(high).toEqual([expect.objectContaining({ searchTerm: "1", operator: "=", display: "High" })]);
    model.addFieldFacet("priority", "1", "=", "High");
    expect(model.getDomain()).toEqual([["priority", "=", "1"]]);
    expect(model.valueLabel("priority", "=", "1")).toBe("High");
  });

  test("expands a many2one row into the matching records, each searching that record", async () => {
    const fields = new Map([...setupModel().fields]);
    const nameSearch = vi.fn(async () => [{ id: 7, display_name: "Alice Doe" }]);
    const model = new SearchModel({ model: "test.record", fields, nameSearch });
    const parent = model.getAutocompleteItems("ali").find(i => i.fieldName === "owner_id")!;
    expect(parent.isParent).toBe(true);
    const children = await model.loadChildren(parent);
    expect(nameSearch).toHaveBeenCalledWith("res.users", "ali");
    expect(children).toEqual([expect.objectContaining({ isChild: true, parentId: parent.id, searchTerm: "7", operator: "=", display: "Alice Doe" })]);
    model.addFieldFacet("owner_id", children[0]!.searchTerm, "=", children[0]!.display);
    expect(model.getDomain()).toEqual([["owner_id", "=", 7]]);
    expect(model.valueLabel("owner_id", "=", "7")).toBe("Alice Doe");
    model.addFieldFacet("owner_id", "ali", "ilike");
    // Facets of one field with different operators combine with AND, like any two facets.
    expect(model.getDomain()).toEqual([["owner_id", "=", 7], ["owner_id", "ilike", "ali"]]);
  });

  test("serializes and deserializes search state to/from URL search params", () => {
    const model = setupModel();
    model.addFieldFacet("name", "Budget");
    model.toggleFilter("filter_active");

    const params = new URLSearchParams();
    model.serializeToUrlParams(params);

    expect(params.has("search_facets")).toBe(true);
    expect(params.has("search_filters")).toBe(true);
    expect(params.has("search_groupby")).toBe(false);

    const restoredModel = setupModel();
    const changed = restoredModel.loadFromUrlParams(params);
    expect(changed).toBe(true);
    expect(restoredModel.getFacets()).toHaveLength(2);
    expect(restoredModel.getDomain()).toEqual(model.getDomain());
    expect(restoredModel.getGroupBy()).toEqual([]);
  });
});

describe("SearchBar UI Component and User Interactions", () => {
  test("renders composite input container, facets, and input", () => {
    const model = setupModel();
    const onChange = vi.fn();
    const searchBar = createSearchBar(model, onChange);
    document.body.append(searchBar.element);

    expect(searchBar.element.querySelector(".search-chips")).not.toBeNull();
    expect(searchBar.element.querySelector(".search-input")).not.toBeNull();
    expect(searchBar.element.querySelector(".search-menu-toggle")).not.toBeNull();

    searchBar.dispose();
  });

  test("submitting form adds facet for default/first field and triggers search", () => {
    const model = setupModel();
    const onChange = vi.fn();
    const searchBar = createSearchBar(model, onChange);
    document.body.append(searchBar.element);

    searchBar.input.value = "Sprint Planning";
    searchBar.form.dispatchEvent(new Event("submit"));

    expect(onChange).toHaveBeenCalled();
    const chip = searchBar.element.querySelector(".search-facet-chip");
    expect(chip).not.toBeNull();
    expect(chip!.textContent).toContain('Title: "Sprint Planning"');

    searchBar.dispose();
  });

  test("clicking chip remove button removes facet and triggers search", () => {
    const model = setupModel();
    const onChange = vi.fn();
    const searchBar = createSearchBar(model, onChange);
    document.body.append(searchBar.element);

    model.addFieldFacet("name", "To Remove");
    searchBar.render();

    const chip = searchBar.element.querySelector(".search-facet-chip")!;
    const removeBtn = chip.querySelector<HTMLButtonElement>(".chip-remove-btn")!;
    removeBtn.click();

    expect(onChange).toHaveBeenCalled();
    expect(searchBar.element.querySelector(".search-facet-chip")).toBeNull();
    expect(model.getFacets()).toHaveLength(0);

    searchBar.dispose();
  });

  test("clicking field facet chip enables editing by restoring value to input", () => {
    const model = setupModel();
    const onChange = vi.fn();
    const searchBar = createSearchBar(model, onChange);
    document.body.append(searchBar.element);

    model.addFieldFacet("name", "Original Term");
    searchBar.render();

    const chip = searchBar.element.querySelector<HTMLElement>(".search-facet-chip")!;
    chip.click();

    expect(searchBar.input.value).toBe("Original Term");
    expect(model.getFacets()).toHaveLength(1);
    searchBar.input.value = "Edited Term";
    searchBar.form.dispatchEvent(new Event("submit"));
    expect(model.getDomain()).toEqual([["name", "ilike", "Edited Term"]]);

    searchBar.dispose();
  });

  test("backspace on empty input removes the trailing facet chip", () => {
    const model = setupModel();
    const onChange = vi.fn();
    const searchBar = createSearchBar(model, onChange);
    document.body.append(searchBar.element);

    model.addFieldFacet("name", "First");
    model.addFieldFacet("body", "Second");
    searchBar.render();

    expect(searchBar.element.querySelectorAll(".search-facet-chip")).toHaveLength(2);

    searchBar.input.value = "";
    searchBar.input.dispatchEvent(new KeyboardEvent("keydown", { key: "Backspace" }));

    expect(onChange).toHaveBeenCalled();
    expect(searchBar.element.querySelectorAll(".search-facet-chip")).toHaveLength(1);
    expect(model.getFacets()[0]!.fieldName).toBe("name");

    searchBar.dispose();
  });

  test("typing in input shows autocomplete dropdown and Arrow navigation works", () => {
    const model = setupModel();
    const onChange = vi.fn();
    const searchBar = createSearchBar(model, onChange);
    document.body.append(searchBar.element);

    searchBar.input.value = "Test";
    searchBar.input.dispatchEvent(new Event("input"));

    const autocomplete = searchBar.element.querySelector<HTMLElement>(".erp-search-autocomplete")!;
    expect(autocomplete.style.display).not.toBe("none");

    const items = autocomplete.querySelectorAll(".erp-autocomplete-item");
    expect(items.length).toBeGreaterThan(0);
    expect(items[0]!.classList.contains("active")).toBe(true);

    // Arrow down moves to next item
    searchBar.input.dispatchEvent(new KeyboardEvent("keydown", { key: "ArrowDown" }));
    const updatedItems = autocomplete.querySelectorAll(".erp-autocomplete-item");
    expect(updatedItems[1]!.classList.contains("active")).toBe(true);

    // Enter selects the active item
    searchBar.input.dispatchEvent(new KeyboardEvent("keydown", { key: "Enter" }));
    expect(onChange).toHaveBeenCalled();
    expect(autocomplete.style.display).toBe("none");
    expect(model.getFacets()).toHaveLength(1);

    searchBar.dispose();
  });

  test("Escape key closes autocomplete dropdown", () => {
    const model = setupModel();
    const onChange = vi.fn();
    const searchBar = createSearchBar(model, onChange);
    document.body.append(searchBar.element);

    searchBar.input.value = "Test";
    searchBar.input.dispatchEvent(new Event("input"));

    const autocomplete = searchBar.element.querySelector<HTMLElement>(".erp-search-autocomplete")!;
    expect(autocomplete.style.display).not.toBe("none");

    searchBar.input.dispatchEvent(new KeyboardEvent("keydown", { key: "Escape" }));
    expect(autocomplete.style.display).toBe("none");

    searchBar.dispose();
  });

  test("toggling the search menu button opens the filters panel", () => {
    const model = setupModel();
    const onChange = vi.fn();
    const searchBar = createSearchBar(model, onChange);
    document.body.append(searchBar.element);

    const toggleBtn = searchBar.element.querySelector<HTMLButtonElement>(".search-menu-toggle")!;
    toggleBtn.click();

    const panel = searchBar.element.querySelector<HTMLElement>(".erp-search-menu-panel")!;
    expect(panel.style.display).toBe("block");
    expect(panel.textContent).toContain("Filters");
    expect(panel.textContent).not.toContain("Group By");
    expect(panel.textContent).not.toContain("Favorites");

    expect(toggleBtn.getAttribute("aria-expanded")).toBe("true");
    expect(toggleBtn.getAttribute("aria-controls")).toBe(panel.id);
    expect(document.activeElement).toBe(panel);
    panel.dispatchEvent(new KeyboardEvent("keydown", { key: "Escape", bubbles: true }));
    expect(panel.style.display).toBe("none");
    expect(toggleBtn.getAttribute("aria-expanded")).toBe("false");
    expect(document.activeElement).toBe(toggleBtn);
    toggleBtn.click();
    expect(panel.style.display).toBe("block");
    toggleBtn.click();
    expect(panel.style.display).toBe("none");
    searchBar.dispose();
  });
});


describe("search revision regressions", () => {
  beforeEach(() => { localStorage.clear(); vi.useFakeTimers(); vi.setSystemTime(new Date(2026, 9, 25, 12)); });
  afterEach(() => { vi.useRealTimers(); vi.restoreAllMocks(); document.body.replaceChildren(); });

  test.each(["remove", "backspace"])("date option identity survives URL and %s clears only that option", mode => {
    const model = setupModel();
    model.toggleDateOption("filter_date_create_date", "date_create_date_month");
    model.toggleDateOption("filter_date_create_date", "date_create_date_year");
    const params = new URLSearchParams();
    model.serializeToUrlParams(params);
    expect(JSON.parse(params.get("search_filters")!)).toEqual([
      "filter_date_create_date:date_create_date_month", "filter_date_create_date:date_create_date_year"
    ]);
    const restored = setupModel(); restored.loadFromUrlParams(params);
    expect(restored.getDomain()).toEqual(model.getDomain());
    if (mode === "remove") restored.removeFacet(restored.getFacets()[1]!.id);
    else restored.removeLastFacet();
    expect(restored.getFilters().find(f => f.type === "dateFilter")!.dateOptions!.map(o => o.isActive)).toEqual([true, false, false, false]);
    restored.removeLastFacet();
    expect(restored.getFilters().some(f => f.isActive)).toBe(false);
    restored.serializeToUrlParams(params);
    expect(params.has("search_filters")).toBe(false);
    expect(restored.getDomain()).toEqual([]);
  });

  /// (partial: generated boolean filters; search arch parsing remains deferred)
  test("same-group filters OR while different groups AND", () => {
    const model = setupModel();
    model.addFieldFacet("name", "Needle");
    model.toggleFilter("filter_active"); model.toggleFilter("filter_archived");
    expect(model.getDomain()).toEqual([["name", "ilike", "Needle"], "|", ["active", "=", true], ["active", "=", false]]);
  });

  test("date option OR preserves each two-term range as a conjunction", () => {
    const model = setupModel();
    model.toggleDateOption("filter_date_create_date", "date_create_date_month");
    model.toggleDateOption("filter_date_create_date", "date_create_date_year");
    expect(model.getDomain()).toEqual(["|", "&", ["create_date", ">=", "2026-10-01 00:00:00"], ["create_date", "<", "2026-11-01 00:00:00"], "&", ["create_date", ">=", "2026-01-01 00:00:00"], ["create_date", "<", "2027-01-01 00:00:00"]]);
    vi.setSystemTime(new Date(2026, 10, 1, 12));
    expect(model.getDomain()).toContainEqual(["create_date", ">=", "2026-11-01 00:00:00"]);
  });

  test("different operators retain their own values through editing and URL reload", () => {
    const model = setupModel(); model.addFieldFacet("name", "A", "="); model.addFieldFacet("name", "B", "ilike");
    expect(model.getFacets()).toHaveLength(2);
    model.editFieldFacet(model.getFacets()[0]!.id, "C");
    const params = new URLSearchParams(); model.serializeToUrlParams(params);
    const restored = setupModel(); restored.loadFromUrlParams(params);
    expect(restored.getDomain()).toEqual([["name", "=", "C"], ["name", "ilike", "B"]]);
  });

  test.each(["{", "{}", '[{"field":"missing","values":["x"]}]', '[{"field":"name","values":[7]}]', ' '.repeat(2049)])("malformed URL fails atomically: %s", raw => {
    const model = setupModel(); model.addFieldFacet("body", "Keep");
    const before = model.getDomain();
    expect(() => model.loadFromUrlParams(new URLSearchParams({search_facets: raw}))).toThrowError(expect.objectContaining({kind:"validation", message:"Invalid search_facets."}));
    expect(model.getDomain()).toEqual(before);
  });

  test("late URL error neither applies early values nor notifies observers", () => {
    const model = setupModel(); const listener = vi.fn(); model.subscribe(listener);
    expect(() => model.loadFromUrlParams(new URLSearchParams({search_facets: '[{"field":"name","values":["x"]}]', search_filters:'["unknown"]'}))).toThrowError(expect.objectContaining({kind:"validation", message:"Invalid search_filters."}));
    expect(model.getDomain()).toEqual([]); expect(listener).not.toHaveBeenCalled();
  });

  test("custom filters retain types, unique ids and domains on URL reload", () => {
    const model = setupModel(); model.addCustomFilter("active", "=", "false"); model.addCustomFilter("body", "ilike", "A");
    expect(new Set(model.getFacets().map(f => f.id)).size).toBe(2);
    const params = new URLSearchParams(); model.serializeToUrlParams(params);
    const restored = setupModel(); restored.loadFromUrlParams(params);
    expect(restored.getDomain()).toEqual([["active", "=", false], ["body", "ilike", "A"]]);
  });

  test("menu stays open after rerender and errors are visible", () => {
    const model = setupModel(); const menu = createSearchMenuPanel(model, vi.fn()); document.body.append(menu.element); menu.open();
    menu.element.querySelector<HTMLButtonElement>(".erp-search-menu-item")!.click();
    expect(menu.isOpen()).toBe(true);
    expect(menu.element.querySelector('[aria-checked="true"]')).not.toBeNull();
    expect(menu.element.querySelector(".erp-groupby-section button")).toBeNull();
    expect(menu.element.querySelector(".erp-favorites-section button")).toBeNull();
    menu.element.querySelector<HTMLButtonElement>(".erp-search-toggle-form-btn")!.click();
    const form = menu.element.querySelector<HTMLFormElement>(".erp-custom-filter-form")!;
    const selects = form.querySelectorAll("select"); selects[0]!.value = "active"; selects[1]!.value = "=";
    form.querySelector("input")!.value = "not-a-boolean";
    form.dispatchEvent(new Event("submit", { bubbles: true, cancelable: true }));
    expect(menu.element.querySelector('[role="alert"]')?.textContent).toBe("Invalid boolean value for Active.");
    expect(model.getDomain()).toEqual([["active", "=", true]]);
    document.body.click(); expect(menu.isOpen()).toBe(false); menu.dispose();
  });

  test("unavailable favorites never read or overwrite legacy browser storage", () => {
    localStorage.setItem("erp_favorites_test.record_2_1", "broken");
    const read = vi.spyOn(Storage.prototype, "getItem"), write = vi.spyOn(Storage.prototype, "setItem");
    const model = setupModel(); expect(model.getFavorites()).toEqual([]);
    expect(() => model.saveFavorite("Saved", false, false)).toThrowError(expect.objectContaining({kind:"unsupported", message:"Saved favorites are not available yet."}));
    expect(read).not.toHaveBeenCalled(); expect(write).not.toHaveBeenCalled();
  });

  test("subscriber exceptions propagate", () => {
    const model = setupModel(); model.subscribe(() => {throw new ClientError("server", "Observer failed.");});
    expect(() => model.addFieldFacet("name", "A")).toThrowError(expect.objectContaining({kind:"server", message:"Observer failed."}));
  });
});


test("search URL limits reject duplicates, unknown keys and too many entries without partial changes", () => {
  const model = setupModel();
  for (const raw of [
    JSON.stringify([{field: "name", values: ["A", "A"]}]),
    JSON.stringify([{field: "name", values: ["A"], ignored: true}]),
    JSON.stringify(Array.from({length: 33}, () => ({field: "name", values: ["A"]})))
  ]) expect(() => model.loadFromUrlParams(new URLSearchParams({search_facets: raw}))).toThrowError(expect.objectContaining({kind: "validation", message: "Invalid search_facets."}));
  expect(model.getFacets()).toEqual([]);
  const params = new URLSearchParams({search_facets: "[]"}); params.append("search_facets", "[]");
  expect(() => model.loadFromUrlParams(params)).toThrowError(expect.objectContaining({kind: "validation", message: "Invalid search_facets."}));
  expect(() => model.loadFromUrlParams(new URLSearchParams({search_groupby: '["priority"]'}))).toThrowError(expect.objectContaining({kind: "unsupported", message: "Restoring a grouping from the address is not available yet."}));
});

test.each(["field", "custom"])("oversized %s state rolls back domain, chips, filters and notifications", kind => {
  const model=setupModel(),changed=vi.fn();model.subscribe(changed);
  let refused=false;
  for(let i=0;i<20;i++) {
    const facets=model.getFacets(),domain=model.getDomain(),filters=model.getFilters(),count=changed.mock.calls.length;
    try {
      if(kind==="field") model.addFieldFacet("name",`${i}${"é".repeat(250)}`);
      else model.addCustomFilter("body","ilike",`${i}${"é".repeat(250)}`);
    } catch(error) {
      expect(error).toMatchObject({kind:"validation",message:kind==="field"?"Invalid search_facets: exceeds 2048 characters.":"Invalid search_filters: exceeds 2048 characters."});
      expect(model.getFacets()).toEqual(facets);expect(model.getDomain()).toEqual(domain);expect(model.getFilters()).toEqual(filters);expect(changed).toHaveBeenCalledTimes(count);
      refused=true;break;
    }
  }
  expect(refused).toBe(true);
  const params=new URLSearchParams();model.serializeToUrlParams(params);
  expect((params.get(kind==="field"?"search_facets":"search_filters")??"").length).toBeLessThanOrEqual(2048);
});

test("oversized facet edits preserve the previous value and emit no event",()=>{
  const model=setupModel();
  for(let i=0;i<7;i++)model.addFieldFacet("name",`${i}${"x".repeat(250)}`);
  model.addFieldFacet("body","short");const before=model.getFacets(),changed=vi.fn();model.subscribe(changed);
  const body=before.find(f=>f.fieldName==="body")!;
  expect(()=>model.editFieldFacet(body.id,"x".repeat(256))).toThrowError(expect.objectContaining({kind:"validation",message:"Invalid search_facets: exceeds 2048 characters."}));
  expect(model.getFacets()).toEqual(before);expect(changed).not.toHaveBeenCalled();
});


test("search icons are decorative and preserve keyboard control labels", () => {
  const bar = createSearchBar(setupModel(), () => {});
  const searchIcon = bar.element.querySelector('svg[data-icon="search"]');
  expect(searchIcon?.getAttribute("aria-hidden")).toBe("true");
  expect(searchIcon?.getAttribute("focusable")).toBe("false");
  expect(bar.element.querySelector('input[aria-label="Search records"]')).not.toBeNull();
  const toggle = bar.element.querySelector<HTMLButtonElement>(".search-menu-toggle")!;
  expect(toggle.getAttribute("aria-label")).toBe("Toggle search filters");
  expect(toggle.querySelector('svg[data-icon="chevron-down"]')).not.toBeNull();
  bar.dispose();
});
