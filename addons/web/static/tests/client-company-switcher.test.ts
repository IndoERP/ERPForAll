/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
import { expect, test, vi } from "vitest";
import { CompanySelector, type CompanyItem } from "../src/client/company-selector.js";
import { HumanApp } from "../src/client/app.js";

const companies: CompanyItem[] = [
  { id: 1, name: "Main Corp", parentId: null },
  { id: 2, name: "Branch North", parentId: 1 },
  { id: 3, name: "Branch South", parentId: 1 },
  { id: 4, name: "Sub-branch North 1", parentId: 2 },
  { id: 10, name: "Second Corp", parentId: null },
  { id: 11, name: "Second Branch", parentId: 10 },
];

test("computes hierarchical visible tree with correct indentation levels", () => {
  const selector = new CompanySelector(companies, [1], [1, 2, 3, 4, 10, 11]);
  const visible = selector.computeVisibleCompanies();
  expect(visible).toEqual([
    { company: companies[0], level: 0 },
    { company: companies[1], level: 1 },
    { company: companies[3], level: 2 },
    { company: companies[2], level: 1 },
    { company: companies[4], level: 0 },
    { company: companies[5], level: 1 },
  ]);
});

test("filters visible companies by search term ignoring case and whitespace", () => {
  const selector = new CompanySelector(companies, [1], [1, 2, 3, 4, 10, 11]);
  const filtered = selector.computeVisibleCompanies("north");
  expect(filtered.map(e => e.company.id)).toEqual([2, 4]);
});

test("selecting a parent selects all its branches recursively", () => {
  const selector = new CompanySelector(companies, [10], [1, 2, 3, 4, 10, 11]);
  expect(selector.selectedCompanyIds).toEqual([10]);

  selector.switchCompany("toggle", 1);
  // 1 selected, plus branches 2, 4, 3
  expect(selector.selectedCompanyIds).toEqual([10, 1, 2, 4, 3]);
});

test("deselecting a parent cascades deselect to all its branches recursively", () => {
  const selector = new CompanySelector(companies, [1, 2, 3, 4, 10], [1, 2, 3, 4, 10, 11]);

  selector.switchCompany("toggle", 1);
  // 1 and its branches 2, 4, 3 should be removed; 10 remains
  expect(selector.selectedCompanyIds).toEqual([10]);
});

test("deselecting a child branch leaves parent and sibling branches selected", () => {
  const selector = new CompanySelector(companies, [1, 2, 3, 4], [1, 2, 3, 4, 10, 11]);

  selector.switchCompany("toggle", 2);
  // 2 and 4 (its child) are removed; 1 (parent) and 3 (sibling) remain
  expect(selector.selectedCompanyIds).toEqual([1, 3]);
});

test("isSingleCompanyMode: true when only 1 company is active", () => {
  const selector = new CompanySelector(companies, [1], [1, 2, 3, 4, 10, 11]);
  expect(selector.isSingleCompanyMode()).toBe(true);
});

test("isSingleCompanyMode: true when a root and ALL its branches are active", () => {
  const selector = new CompanySelector(companies, [1, 2, 3, 4], [1, 2, 3, 4, 10, 11]);
  expect(selector.isSingleCompanyMode()).toBe(true);
});

test("isSingleCompanyMode: false when a root and only SOME branches are active", () => {
  const selector = new CompanySelector(companies, [1, 2], [1, 2, 3, 4, 10, 11]);
  expect(selector.isSingleCompanyMode()).toBe(false);
});

test("isSingleCompanyMode: false when companies from different roots are active", () => {
  const selector = new CompanySelector(companies, [1, 10], [1, 2, 3, 4, 10, 11]);
  expect(selector.isSingleCompanyMode()).toBe(false);
});

test("loginto in single-company mode clears previous selection and selects target plus all its branches", () => {
  // Starting in single-company mode: [1, 2, 3, 4]
  const selector = new CompanySelector(companies, [1, 2, 3, 4], [1, 2, 3, 4, 10, 11]);
  expect(selector.isSingleCompanyMode()).toBe(true);

  selector.switchCompany("loginto", 10);
  // Previous selection cleared; 10 and branch 11 selected with 10 at front
  expect(selector.selectedCompanyIds).toEqual([10, 11]);
});

test("loginto in multi-company mode moves target to front and selects its branches without clearing other roots", () => {
  // Starting in multi-company mode: [1, 10]
  const selector = new CompanySelector(companies, [1, 10], [1, 2, 3, 4, 10, 11]);
  expect(selector.isSingleCompanyMode()).toBe(false);

  selector.switchCompany("loginto", 10);
  // 10 moved to front, its branch 11 added; 1 remains selected
  expect(selector.selectedCompanyIds).toEqual([10, 1, 11]);
});

test("hasSelectionChanged tracks staged changes and reset restores original active companies", () => {
  const selector = new CompanySelector(companies, [1], [1, 2, 3, 4, 10, 11]);
  expect(selector.hasSelectionChanged).toBe(false);

  selector.switchCompany("toggle", 2);
  expect(selector.hasSelectionChanged).toBe(true);

  selector.reset();
  expect(selector.hasSelectionChanged).toBe(false);
  expect(selector.selectedCompanyIds).toEqual([1]);
});

test("selectAll toggles between deselecting all visible and selecting all visible", () => {
  const selector = new CompanySelector(companies, [1], [1, 2, 3, 4, 10, 11]);
  const visibleIds = [1, 2, 3, 4, 10, 11];

  // Since company 1 is selected, selectAll should deselect all
  selector.selectAll(visibleIds);
  expect(selector.selectedCompanyIds).toEqual([]);

  // Now with none selected, selectAll selects all visible
  selector.selectAll(visibleIds);
  expect(selector.selectedCompanyIds).toEqual([1, 2, 4, 3, 10, 11]);
});

function createHost(): HTMLElement {
  const hostEl = document.createElement("div");
  for (const region of ["header", "navigation", "notice", "content"]) {
    const node = document.createElement("div");
    node.setAttribute(`data-client-${region}`, "");
    hostEl.append(node);
  }
  document.body.append(hostEl);
  return hostEl;
}

const multiCompanyBootstrap = {
  wire_major: 1,
  registry_version: "r1",
  asset_version: "a1",
  identity: { uid: 2, name: "Operator", internal: true },
  companies: [
    { id: 1, name: "Main Corp", parent_id: null },
    { id: 2, name: "Branch North", parent_id: 1 },
    { id: 3, name: "Branch South", parent_id: 1 },
    { id: 10, name: "Second Corp", parent_id: null },
    { id: 11, name: "Second Branch", parent_id: 10 },
  ],
  current_company_id: 1,
  allowed_company_ids: [1, 2, 3, 10, 11],
  active_company_ids: [1, 2, 3],
  locale: { lang: "en_US", tz: "UTC", date_format: "%m/%d/%Y", time_format: "%H:%M:%S" },
  csrf: { logout: "test-token" }
};

function setupMenuFetch(): void {
  vi.stubGlobal("fetch", vi.fn(async (input: RequestInfo | URL) => {
    const url = String(input);
    if (url.includes("erp_load_web_menus")) {
      return new Response(JSON.stringify({
        wire_major: 1,
        registry_version: "r1",
        roots: [],
        nodes: []
      }), { headers: { "Content-Type": "application/json" } });
    }
    return new Response("[]", { headers: { "Content-Type": "application/json" } });
  }));
}

test("renders company switcher hierarchy with checkboxes, role treeitem and switch buttons", async () => {
  history.replaceState(null, "", "/erp");
  const root = createHost();
  setupMenuFetch();
  const app = new HumanApp(root, multiCompanyBootstrap);
  await app.start();

  const switcher = root.querySelector<HTMLElement>(".company-switcher")!;
  expect(switcher).not.toBeNull();

  const rows = [...switcher.querySelectorAll<HTMLElement>(".company-row")];
  expect(rows).toHaveLength(5);
  // The section is a labelled group and its trigger promises a dialog, not a menu.
  const section = switcher.closest<HTMLElement>(".company-dropdown")!;
  expect(section.getAttribute("role")).toBe("group");
  expect(section.getAttribute("aria-label")).toBe("Companies");
  expect(root.querySelector(".company-menu-button")!.getAttribute("aria-haspopup")).toBe("dialog");

  // Role treeitem and accessibility
  for (const row of rows) {
    expect(row.getAttribute("role")).toBe("treeitem");
    expect(row.tabIndex).toBe(0);
    expect(row.getAttribute("aria-selected")).toMatch(/true|false/);
  }

  // Indentation verification
  const branchRow = rows.find(r => r.getAttribute("data-company-id") === "2")!;
  expect(branchRow.style.paddingLeft).toBe("16px");

  // Active companies 1, 2, 3 checked; 10, 11 unchecked
  const chk1 = rows.find(r => r.getAttribute("data-company-id") === "1")!.querySelector<HTMLInputElement>("input[type=checkbox]")!;
  const chk10 = rows.find(r => r.getAttribute("data-company-id") === "10")!.querySelector<HTMLInputElement>("input[type=checkbox]")!;
  expect(chk1.checked).toBe(true);
  expect(chk10.checked).toBe(false);

  // Confirm/Discard not shown when selection hasn't changed
  expect(switcher.querySelector(".company-btn-confirm")).toBeNull();
  app.dispose();
  root.remove();
});

test("checking a company reveals Confirm and Discard buttons; Discard restores state", async () => {
  history.replaceState(null, "", "/erp");
  const root = createHost();
  setupMenuFetch();
  const app = new HumanApp(root, multiCompanyBootstrap);
  await app.start();

  const switcher = root.querySelector<HTMLElement>(".company-switcher")!;
  const chk10 = switcher.querySelector<HTMLElement>('.company-row[data-company-id="10"]')!.querySelector<HTMLInputElement>("input[type=checkbox]")!;

  chk10.checked = true;
  chk10.dispatchEvent(new Event("change"));

  // Selecting parent 10 also selects its branch 11
  const chk11 = switcher.querySelector<HTMLElement>('.company-row[data-company-id="11"]')!.querySelector<HTMLInputElement>("input[type=checkbox]")!;
  expect(chk11.checked).toBe(true);

  // Confirm and Discard appear
  const confirmBtn = switcher.querySelector<HTMLButtonElement>(".company-btn-confirm")!;
  const discardBtn = switcher.querySelector<HTMLButtonElement>(".company-btn-discard")!;
  expect(confirmBtn).not.toBeNull();
  expect(discardBtn).not.toBeNull();

  // Clicking Discard reverts selection and hides action buttons
  discardBtn.click();
  const chk10After = switcher.querySelector<HTMLElement>('.company-row[data-company-id="10"]')!.querySelector<HTMLInputElement>("input[type=checkbox]")!;
  expect(chk10After.checked).toBe(false);
  expect(switcher.querySelector(".company-btn-confirm")).toBeNull();

  app.dispose();
  root.remove();
});

test("Space key on focused company row toggles selection and updates cascade", async () => {
  history.replaceState(null, "", "/erp");
  const root = createHost();
  setupMenuFetch();
  const app = new HumanApp(root, multiCompanyBootstrap);
  await app.start();

  const switcher = root.querySelector<HTMLElement>(".company-switcher")!;
  const row10 = switcher.querySelector<HTMLElement>('.company-row[data-company-id="10"]')!;

  // Press Space on row 10
  row10.dispatchEvent(new KeyboardEvent("keydown", { key: " ", bubbles: true, cancelable: true }));

  // Parent 10 and branch 11 are now selected
  const chk10 = switcher.querySelector<HTMLElement>('.company-row[data-company-id="10"]')!.querySelector<HTMLInputElement>("input[type=checkbox]")!;
  const chk11 = switcher.querySelector<HTMLElement>('.company-row[data-company-id="11"]')!.querySelector<HTMLInputElement>("input[type=checkbox]")!;
  expect(chk10.checked).toBe(true);
  expect(chk11.checked).toBe(true);

  app.dispose();
  root.remove();
});

test("Enter key on focused company row switches to company in loginto mode", async () => {
  history.replaceState(null, "", "/erp");
  const root = createHost();
  setupMenuFetch();
  const assignSpy = vi.fn();

  const app = new HumanApp(root, multiCompanyBootstrap);
  (app as any).navigateLocation = assignSpy;
  await app.start();

  const switcher = root.querySelector<HTMLElement>(".company-switcher")!;
  const row10 = switcher.querySelector<HTMLElement>('.company-row[data-company-id="10"]')!;

  // Press Enter on row 10
  row10.dispatchEvent(new KeyboardEvent("keydown", { key: "Enter", bubbles: true, cancelable: true }));

  await vi.waitFor(() => expect(assignSpy).toHaveBeenCalledWith(expect.stringContaining("cids=10%2C11")));

  app.dispose();
  root.remove();
});

test("dirty guard rejection on loginto aborts switch and reverts selection", async () => {
  history.replaceState(null, "", "/erp");
  const root = createHost();
  setupMenuFetch();
  const assignSpy = vi.fn();

  const app = new HumanApp(root, multiCompanyBootstrap);
  (app as any).navigateLocation = assignSpy;
  // Make dirty guard reject departure
  (app as any).canLeave = vi.fn(async () => false);
  await app.start();

  const switcher = root.querySelector<HTMLElement>(".company-switcher")!;
  const switchBtn10 = switcher.querySelector<HTMLElement>('.company-row[data-company-id="10"]')!.querySelector<HTMLButtonElement>(".company-switch-btn")!;

  switchBtn10.click();

  // Wait a tick for async canLeave to resolve
  await new Promise(r => setTimeout(r, 10));

  // Navigation must NOT have been called
  expect(assignSpy).not.toHaveBeenCalled();

  // Selection must have reverted to initial active companies [1, 2, 3]
  const chk10 = switcher.querySelector<HTMLElement>('.company-row[data-company-id="10"]')!.querySelector<HTMLInputElement>("input[type=checkbox]")!;
  expect(chk10.checked).toBe(false);

  app.dispose();
  root.remove();
});

test("dirty guard rejection on Confirm aborts switch and reverts selection", async () => {
  history.replaceState(null, "", "/erp");
  const root = createHost();
  setupMenuFetch();
  const assignSpy = vi.fn();

  const app = new HumanApp(root, multiCompanyBootstrap);
  (app as any).navigateLocation = assignSpy;
  (app as any).canLeave = vi.fn(async () => false);
  await app.start();

  const switcher = root.querySelector<HTMLElement>(".company-switcher")!;
  const chk10 = switcher.querySelector<HTMLElement>('.company-row[data-company-id="10"]')!.querySelector<HTMLInputElement>("input[type=checkbox]")!;

  chk10.checked = true;
  chk10.dispatchEvent(new Event("change"));

  const confirmBtn = switcher.querySelector<HTMLButtonElement>(".company-btn-confirm")!;
  expect(confirmBtn).not.toBeNull();

  confirmBtn.click();

  await new Promise(r => setTimeout(r, 10));

  expect(assignSpy).not.toHaveBeenCalled();

  // Selection reverted
  const chk10After = switcher.querySelector<HTMLElement>('.company-row[data-company-id="10"]')!.querySelector<HTMLInputElement>("input[type=checkbox]")!;
  expect(chk10After.checked).toBe(false);
  expect(switcher.querySelector(".company-btn-confirm")).toBeNull();

  app.dispose();
  root.remove();
});

test("clicking switch button in single-company mode switches to target and all its branches", async () => {
  history.replaceState(null, "", "/erp");
  const root = createHost();
  setupMenuFetch();
  const assignSpy = vi.fn();

  const app = new HumanApp(root, multiCompanyBootstrap);
  (app as any).navigateLocation = assignSpy;
  await app.start();

  const switcher = root.querySelector<HTMLElement>(".company-switcher")!;
  const switchBtn10 = switcher.querySelector<HTMLElement>('.company-row[data-company-id="10"]')!.querySelector<HTMLButtonElement>(".company-switch-btn")!;

  switchBtn10.click();

  // In single-company mode, switching to 10 clears 1, 2, 3 and activates [10, 11]
  await vi.waitFor(() => expect(assignSpy).toHaveBeenCalledWith(expect.stringContaining("cids=10%2C11")));

  app.dispose();
  root.remove();
});

test("Control+Enter confirms pending selection changes", async () => {
  history.replaceState(null, "", "/erp");
  const root = createHost();
  setupMenuFetch();
  const assignSpy = vi.fn();

  const app = new HumanApp(root, multiCompanyBootstrap);
  (app as any).navigateLocation = assignSpy;
  await app.start();

  const switcher = root.querySelector<HTMLElement>(".company-switcher")!;
  const chk10 = switcher.querySelector<HTMLElement>('.company-row[data-company-id="10"]')!.querySelector<HTMLInputElement>("input[type=checkbox]")!;

  chk10.checked = true;
  chk10.dispatchEvent(new Event("change"));

  // Press Control+Enter inside company switcher
  switcher.dispatchEvent(new KeyboardEvent("keydown", { key: "Enter", ctrlKey: true, bubbles: true }));

  await vi.waitFor(() => expect(assignSpy).toHaveBeenCalledWith(expect.stringContaining("cids=1%2C2%2C3%2C10%2C11")));

  app.dispose();
  root.remove();
});

/// (partial: one Control+Enter confirms once)
test("Control+Enter inside the switcher clicks the confirm button once", async () => {
  history.replaceState(null, "", "/erp");
  const root = createHost();
  setupMenuFetch();
  const app = new HumanApp(root, multiCompanyBootstrap);
  (app as any).navigateLocation = vi.fn();
  await app.start();
  const switcher = root.querySelector<HTMLElement>(".company-switcher")!;
  const chk = switcher.querySelector<HTMLElement>('.company-row[data-company-id="10"]')!.querySelector<HTMLInputElement>("input[type=checkbox]")!;
  chk.checked = true;
  chk.dispatchEvent(new Event("change"));
  const confirm = switcher.querySelector<HTMLButtonElement>(".company-btn-confirm")!;
  let clicks = 0;
  confirm.addEventListener("click", () => { clicks++; });
  switcher.dispatchEvent(new KeyboardEvent("keydown", { key: "Enter", ctrlKey: true, bubbles: true }));
  expect(clicks).toBe(1);
  app.dispose();
  root.remove();
});

test("search input filters visible company rows in the DOM", async () => {
  history.replaceState(null, "", "/erp");
  const root = createHost();
  setupMenuFetch();
  // The search field appears with more than six companies.
  const many = [...multiCompanyBootstrap.companies, { id: 20, name: "Extra One", parent_id: null }, { id: 21, name: "Extra Two", parent_id: null }];
  const app = new HumanApp(root, { ...multiCompanyBootstrap, companies: many, allowed_company_ids: [...multiCompanyBootstrap.allowed_company_ids, 20, 21] });
  await app.start();

  const switcher = root.querySelector<HTMLElement>(".company-switcher")!;
  const searchInput = switcher.querySelector<HTMLInputElement>(".company-search-input")!;
  expect(searchInput).not.toBeNull();

  searchInput.value = "North";
  searchInput.dispatchEvent(new Event("input"));

  const rows = [...switcher.querySelectorAll<HTMLElement>(".company-row")];
  expect(rows).toHaveLength(1);
  expect(rows[0]!.getAttribute("data-company-id")).toBe("2");

  app.dispose();
  root.remove();
});

/// (partial: company dropdown restores focus to active control after selection change)
test("company dropdown restores focus to the same control (checkbox or switch button) after selection change", async () => {
  history.replaceState(null, "", "/erp");
  const root = createHost();
  setupMenuFetch();
  const app = new HumanApp(root, multiCompanyBootstrap);
  await app.start();

  const switcher = root.querySelector<HTMLElement>(".company-switcher")!;
  const row10 = switcher.querySelector<HTMLElement>('.company-row[data-company-id="10"]')!;
  const chk10 = row10.querySelector<HTMLInputElement>("input[type=checkbox]")!;

  // Focus the checkbox directly
  chk10.focus();
  expect(document.activeElement).toBe(chk10);

  chk10.checked = true;
  chk10.dispatchEvent(new Event("change"));

  // After re-rendering the tree, focus must be restored to the checkbox for company 10
  const updatedRow10 = switcher.querySelector<HTMLElement>('.company-row[data-company-id="10"]')!;
  const updatedChk10 = updatedRow10.querySelector<HTMLInputElement>("input[type=checkbox]")!;
  expect(document.activeElement).toBe(updatedChk10);

  // Now test button focus restoration:
  const btn10 = updatedRow10.querySelector<HTMLButtonElement>(".company-switch-btn")!;
  btn10.focus();
  expect(document.activeElement).toBe(btn10);

  // Toggle company 11 while btn10 was focused
  const chk11 = switcher.querySelector<HTMLElement>('.company-row[data-company-id="11"]')!.querySelector<HTMLInputElement>("input[type=checkbox]")!;
  chk11.checked = false;
  chk11.dispatchEvent(new Event("change"));

  const btn10After = switcher.querySelector<HTMLElement>('.company-row[data-company-id="10"]')!.querySelector<HTMLButtonElement>(".company-switch-btn")!;
  expect(document.activeElement).toBe(btn10After);

  app.dispose();
  root.remove();
});

