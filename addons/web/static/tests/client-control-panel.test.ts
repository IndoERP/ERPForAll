/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
import { describe, expect, test, vi, beforeEach } from "vitest";
import {
  Pager,
  parsePagerRange,
  type PagerProps
} from "../src/client/pager.js";
import {
  ViewSwitcher,
  type ViewSwitcherEntry
} from "../src/client/view-switcher.js";
import {
  ActionMenus,
  executeActionMenuItem,
  loadAvailablePrintItems,
  type ActionMenuItem,
  type ActionMenuEnvironment
} from "../src/client/action-menus.js";
import {
  mountControlPanel,
  type ControlPanelAdapter
} from "../src/client/control-panel.js";
import { configureClientTemplates } from "../src/client/registry.js";

describe("M4 Shell Base Control Panel", () => {
  beforeEach(() => {
    configureClientTemplates([
      {
        name: "web.HumanControlPanel",
        render: () => {
          const root = document.createElement("section");
          root.className = "control-panel";
          root.setAttribute("data-control-panel", "");

          const heading = document.createElement("div");
          heading.className = "control-panel-heading";

          const breadcrumbs = document.createElement("nav");
          breadcrumbs.className = "breadcrumbs";
          breadcrumbs.setAttribute("data-control-breadcrumbs", "");

          const searchArea = document.createElement("div");
          searchArea.className = "control-panel-search-area";
          searchArea.setAttribute("data-control-search", "");

          const toolsArea = document.createElement("div");
          toolsArea.className = "control-panel-tools";
          toolsArea.setAttribute("data-control-tools", "");

          heading.append(breadcrumbs, searchArea, toolsArea);

          const actionsRow = document.createElement("div");
          actionsRow.className = "control-panel-actions-row";

          const actions = document.createElement("div");
          actions.className = "actions";
          actions.setAttribute("data-control-actions", "");

          const extension = document.createElement("div");
          extension.className = "actions";
          extension.setAttribute("data-control-extension", "");

          actionsRow.append(actions, extension);
          root.append(heading, actionsRow);
          return root;
        }
      }
    ]);
  });

  test(": control-panel composition, display flags, and omitting pager on non-positive total", async () => {
    const host = document.createElement("div");
    document.body.append(host);

    const breadcrumbsNode = document.createElement("h1");
    breadcrumbsNode.textContent = "Partners";

    const actionButton = document.createElement("button");
    actionButton.textContent = "New";

    const searchInput = document.createElement("input");

    // Case 1: Total > 0 and showButtons = true
    const adapter1: ControlPanelAdapter = {
      title: "Partners",
      model: "res.partner",
      breadcrumbs: breadcrumbsNode,
      searchArea: searchInput,
      actionsArea: actionButton,
      pagerProps: {
        offset: 0,
        limit: 80,
        total: 150
      },
      viewSwitcherProps: {
        entries: [
          { type: "list", label: "List" },
          { type: "form", label: "Form" }
        ],
        activeType: "list",
        onSwitchView: () => {}
      },
      display: {
        showButtons: true,
        showSearch: true,
        showPager: true,
        showViewSwitcher: true
      }
    };

    const handle1 = await mountControlPanel(host, adapter1, err => { throw err; });

    const panelEl1 = handle1.panel;
    expect(panelEl1.querySelector("[data-control-breadcrumbs]")?.textContent).toContain("Partners");
    expect(panelEl1.querySelector("[data-control-search]")?.contains(searchInput)).toBe(true);
    expect(panelEl1.querySelector("[data-control-actions]")?.contains(actionButton)).toBe(true);

    const tools1 = panelEl1.querySelector("[data-control-tools]");
    expect(tools1?.querySelector(".pager")).not.toBeNull();
    expect(tools1?.querySelector(".view-switcher")).not.toBeNull();
    handle1.dispose();

    // Case 2: Display flags disable buttons and search; total <= 0 omits pager
    const host2 = document.createElement("div");
    document.body.append(host2);

    const adapter2: ControlPanelAdapter = {
      title: "Partners Empty",
      model: "res.partner",
      breadcrumbs: breadcrumbsNode,
      searchArea: searchInput,
      actionsArea: actionButton,
      pagerProps: {
        offset: 0,
        limit: 80,
        total: 0
      },
      viewSwitcherProps: {
        entries: [{ type: "list", label: "List" }],
        activeType: "list",
        onSwitchView: () => {}
      },
      display: {
        showButtons: false,
        showSearch: false,
        showPager: true,
        showViewSwitcher: true
      }
    };

    const handle2 = await mountControlPanel(host2, adapter2, err => { throw err; });
    const panelEl2 = handle2.panel;

    // Actions and search slots retain structure but are empty or hidden per display flags
    const actionsSlot = panelEl2.querySelector("[data-control-actions]");
    expect(actionsSlot?.children.length).toBe(0);

    const searchSlot = panelEl2.querySelector("[data-control-search]");
    expect(searchSlot?.children.length).toBe(0);

    // Pager omitted because total is 0 (not positive)
    const tools2 = panelEl2.querySelector("[data-control-tools]");
    expect(tools2?.querySelector(".pager")).toBeNull();

    handle2.dispose();
    host.remove();
    host2.remove();
  });

  test(": pager display, 1-based intervals, single vs multi record, and edge looping", async () => {
    // Multi-record list pager: offset=0, limit=80, total=150
    let currentOffset = 0;
    let currentLimit = 80;
    const updates: { offset: number; limit: number; navigated: boolean }[] = [];

    const pagerProps: PagerProps = {
      offset: currentOffset,
      limit: currentLimit,
      total: 150,
      onUpdate: (bounds, navigated) => {
        currentOffset = bounds.offset;
        currentLimit = bounds.limit;
        updates.push({ ...bounds, navigated });
      }
    };

    const pager = new Pager(pagerProps);
    const node = pager.render();
    expect(node).not.toBeNull();

    expect(pager.minimum).toBe(1);
    expect(pager.maximum).toBe(80);
    expect(pager.value).toBe("1-80 / 150");

    // Click Next: moves from 0 to 80
    await pager.navigate(1);
    expect(updates[updates.length - 1]).toEqual({ offset: 80, limit: 80, navigated: true });
    expect(pager.minimum).toBe(81);
    expect(pager.maximum).toBe(150);
    expect(pager.value).toBe("81-150 / 150");

    // Click Next again: at total edge (offset + limit >= total), loops forward to 0
    await pager.navigate(1);
    expect(updates[updates.length - 1]).toEqual({ offset: 0, limit: 80, navigated: true });
    expect(pager.minimum).toBe(1);

    // Click Previous from 0: loops backward to last page: total - (total % limit || limit) = 150 - 70 = 80
    await pager.navigate(-1);
    expect(updates[updates.length - 1]).toEqual({ offset: 80, limit: 80, navigated: true });
    expect(pager.minimum).toBe(81);

    pager.dispose();

    // Single-record form pager: limit=1, total=5
    let singleOffset = 0;
    const singlePager = new Pager({
      offset: singleOffset,
      limit: 1,
      total: 5,
      onUpdate: (bounds) => { singleOffset = bounds.offset; }
    });
    singlePager.render();

    expect(singlePager.value).toBe("1 / 5");

    // Loop backwards from 0 on single record -> loops to total - 1 = 4
    await singlePager.navigate(-1);
    expect(singleOffset).toBe(4);
    expect(singlePager.value).toBe("5 / 5");

    // Loop forwards from 4 on single record -> loops to 0
    await singlePager.navigate(1);
    expect(singleOffset).toBe(0);
    expect(singlePager.value).toBe("1 / 5");

    singlePager.dispose();
  });

  test(": pager range editing, parsing, clamping, Enter/Escape, and pending lock", async () => {
    // Parser unit tests
    expect(parsePagerRange("1-80", 150)).toEqual({ offset: 0, limit: 80 });
    expect(parsePagerRange("10 50", 150)).toEqual({ offset: 9, limit: 41 });
    expect(parsePagerRange("20, 40", 150)).toEqual({ offset: 19, limit: 21 });
    expect(parsePagerRange("200-300", 150)).toEqual({ offset: 149, limit: 1 }); // Clamped to max total 150
    expect(parsePagerRange("abc", 150)).toBeNull(); // Invalid NaN
    expect(parsePagerRange("50-20", 150)).toBeNull(); // min >= max

    // Interactive editing and pending lock
    let updateResolver: () => void = () => {};
    let onUpdateCalled = false;

    const pager = new Pager({
      offset: 0,
      limit: 80,
      total: 150,
      isEditable: true,
      onUpdate: async () => {
        onUpdateCalled = true;
        await new Promise<void>(resolve => { updateResolver = resolve; });
      }
    });

    const el = pager.render();
    const countSpan = el.querySelector<HTMLElement>(".pager-count");
    expect(countSpan).not.toBeNull();

    // Start editing
    pager.startEditing();
    const input = el.querySelector<HTMLInputElement>(".pager-range-input");
    expect(input).not.toBeNull();
    expect(input?.value).toBe("1-80");

    // Set valid range and press Enter
    input!.value = "20-60";
    const commitPromise = pager.commitEditing();
    expect(onUpdateCalled).toBe(true);

    // While update is in flight, controls are locked (disabled)
    expect(pager.isDisabled).toBe(true);
    expect(el.querySelector<HTMLButtonElement>(".pager-prev")?.disabled).toBe(true);
    expect(el.querySelector<HTMLButtonElement>(".pager-next")?.disabled).toBe(true);

    // Resolve update
    updateResolver();
    await commitPromise;

    expect(pager.isDisabled).toBe(false);
    expect(pager.isEditing).toBe(false);
    expect(pager.offset).toBe(19);
    expect(pager.limit).toBe(41);

    // Test Escape cancels editing without updating
    onUpdateCalled = false;
    pager.startEditing();
    const input2 = el.querySelector<HTMLInputElement>(".pager-range-input");
    input2!.value = "50-100";
    pager.cancelEditing();

    expect(onUpdateCalled).toBe(false);
    expect(pager.isEditing).toBe(false);
    expect(pager.value).toBe("20-60 / 150");

    // Finding #23: editable count must be a <button> and not lose focus on commit
    expect(countSpan?.tagName.toLowerCase()).toBe("button");
    expect((countSpan as HTMLButtonElement).type).toBe("button");

    // Finding #23: Enter followed by blur while update awaits must only call onUpdate once
    let updateCallCount = 0;
    let pendingResolver: () => void = () => {};
    const racingPager = new Pager({
      offset: 0,
      limit: 10,
      total: 100,
      isEditable: true,
      onUpdate: async () => {
        updateCallCount++;
        await new Promise<void>(resolve => { pendingResolver = resolve; });
      }
    });
    const racingEl = racingPager.render();
    document.body.append(racingEl);

    racingPager.startEditing();
    const editInput = racingEl.querySelector<HTMLInputElement>(".pager-range-input")!;
    expect(editInput).not.toBeNull();
    editInput.value = "11-20";

    // Press Enter to trigger commitEditing
    editInput.dispatchEvent(new KeyboardEvent("keydown", { key: "Enter", bubbles: true, cancelable: true }));
    expect(updateCallCount).toBe(1);

    // Now blur fires while onUpdate is still pending
    editInput.dispatchEvent(new FocusEvent("blur"));
    expect(updateCallCount).toBe(1);

    // Resolve the update
    pendingResolver();
    await vi.waitFor(() => {
      expect(racingPager.isDisabled).toBe(false);
      expect(racingEl.querySelector(".pager-count")?.textContent).toBe("11-20 / 100");
    });
    expect(updateCallCount).toBe(1);
    // Focus must return to count button
    expect(document.activeElement).toBe(racingEl.querySelector(".pager-count"));

    racingPager.dispose();
    racingEl.remove();
    pager.dispose();
  });

  /// (partial: pager input is not nested in button, visible range is accessible name, aria-live is present)
  test(": pager replaces button with input on edit, announces page changes, and drops button aria-label", async () => {
    const pager = new Pager({
      offset: 0,
      limit: 80,
      total: 200,
      isEditable: true,
      onUpdate: async () => {},
    });
    const el = pager.render();
    document.body.append(el);

    const countButton = el.querySelector<HTMLButtonElement>("button.pager-count");
    expect(countButton).not.toBeNull();
    // Accessible name is visible range, not replaced by aria-label
    expect(countButton?.getAttribute("aria-label")).toBeNull();
    expect(countButton?.title).toBe("Click to edit range");
    expect(countButton?.getAttribute("aria-live")).toBe("polite");

    // Start editing: button is replaced by input, not nested
    pager.startEditing();
    const input = el.querySelector<HTMLInputElement>("input.pager-range-input");
    expect(input).not.toBeNull();
    expect(input?.closest("button")).toBeNull();
    expect(countButton?.parentElement).toBeNull();

    // Cancel: button is swapped back into place
    pager.cancelEditing();
    expect(el.querySelector("input.pager-range-input")).toBeNull();
    expect(el.querySelector("button.pager-count")).toBe(countButton);
    expect(countButton?.parentElement).toBe(el);

    // Start editing again and commit
    pager.startEditing();
    const input2 = el.querySelector<HTMLInputElement>("input.pager-range-input");
    expect(input2).not.toBeNull();
    expect(input2?.closest("button")).toBeNull();
    input2!.value = "81-160";
    await pager.commitEditing();
    expect(el.querySelector("input.pager-range-input")).toBeNull();
    expect(el.querySelector("button.pager-count")).toBe(countButton);
    expect(countButton?.textContent).toBe("81-160 / 200");

    pager.dispose();
    el.remove();
  });

  test(": action-menu selection scope, guards, callbacks, and active context", async () => {
    const callbackInvoked: string[] = [];
    const executionLogs: any[] = [];

    const env: ActionMenuEnvironment = {
      resModel: "res.partner",
      getActiveIds: () => [101, 102],
      isDomainSelected: false,
      domain: [["is_company", "=", true]],
      context: { default_type: "contact" },
      rpc: {
        call: async (model: string, method: string, args: unknown) => {
          executionLogs.push({ model, method, args });
          if (method === "search") return [101, 102, 103, 104];
          return true;
        }
      } as any,
      doAction: async (action, options) => {
        executionLogs.push({ action, options });
      }
    };

    // 1. Pre-execution guard stops execution when returning false
    const guardedItem: ActionMenuItem = {
      key: "dangerous_action",
      label: "Dangerous Action",
      callback: () => { callbackInvoked.push("dangerous"); }
    };

    const guard = vi.fn().mockResolvedValue(false);
    await executeActionMenuItem(guardedItem, env, { shouldExecuteAction: guard });
    expect(guard).toHaveBeenCalledWith(guardedItem);
    expect(callbackInvoked).toHaveLength(0);

    // 2. Callback execution when guard allows
    const allowedCallbackItem: ActionMenuItem = {
      key: "safe_callback",
      label: "Safe Callback",
      callback: (items) => { callbackInvoked.push(items[0]!.key!); }
    };
    await executeActionMenuItem(allowedCallbackItem, env, { shouldExecuteAction: async () => true });
    expect(callbackInvoked).toContain("safe_callback");

    // 3. Action ID execution injects active context
    const actionItem: ActionMenuItem = {
      key: "archive_action",
      label: "Archive",
      action: 456
    };
    await executeActionMenuItem(actionItem, env);
    const actionCall = executionLogs.find(l => l.action === 456);
    expect(actionCall).toBeDefined();
    expect(actionCall!.options.additionalContext).toEqual({
      default_type: "contact",
      active_id: 101,
      active_ids: [101, 102],
      active_model: "res.partner",
      active_domain: [["is_company", "=", true]]
    });

    // 4. Domain-wide selection resolution
    executionLogs.length = 0;
    const domainWideEnv: ActionMenuEnvironment = {
      ...env,
      isDomainSelected: true
    };
    await executeActionMenuItem(actionItem, domainWideEnv);
    const searchCall = executionLogs.find(l => l.method === "search");
    expect(searchCall).toBeDefined();
    expect(searchCall!.args).toEqual({
      domain: [["is_company", "=", true]],
      limit: 5000,
      context: { default_type: "contact" }
    });
  });

  test(": print menu availability, domain report filtering, and disabled empty fallback", async () => {
    const rpcCalls: any[] = [];
    const reportItems: ActionMenuItem[] = [
      { id: 1, key: "1", name: "Partner Statement", domain: [["has_invoices", "=", true]] },
      { id: 2, key: "2", name: "Partner Badges", domain: [["is_vip", "=", true]] },
      { id: 3, key: "3", name: "Generic Label" } // No domain: always available
    ];

    // Load available print items
    const available = await loadAvailablePrintItems({
      items: reportItems,
      resModel: "res.partner",
      activeIds: [10, 20],
      fetchValidReportIds: async (actionIds, model, recordIds) => {
        rpcCalls.push({ actionIds, model, recordIds });
        return [1];
      },
      loadExtraPrintItems: async () => [
        { id: "extra_print", key: "extra_print", description: "Custom Sheet" }
      ]
    });

    // Report 2 was excluded because get_valid_action_reports returned [1]
    const ids = available.map(i => i.id ?? i.key);
    expect(ids).toContain(1);
    expect(ids).not.toContain(2);
    expect(ids).toContain(3);
    expect(ids).toContain("extra_print");

    // Case 2: When no reports available, renders disabled item "No report available."
    const emptyResult = await loadAvailablePrintItems({
      items: [{ id: 2, key: "2", domain: [["is_vip", "=", true]] }],
      resModel: "res.partner",
      activeIds: [10, 20],
      fetchValidReportIds: async () => []
    });

    expect(emptyResult).toHaveLength(1);
    expect(emptyResult[0]?.description).toBe("No report available.");
    expect(emptyResult[0]?.disabled).toBe(true);
    expect(emptyResult[0]?.class).toContain("disabled");
  });

  /// (partial: Alt+Shift+V uses the physical key and ignores editable targets)
  test("view switcher shortcut matches KeyV when Option+Shift+V yields another character and skips inputs", () => {
    const switches: string[] = [];
    const switcher = new ViewSwitcher({
      entries: [{ type: "list" }, { type: "form" }], activeType: "list",
      onSwitchView: type => { switches.push(type); },
    });
    document.body.append(switcher.render());
    const input = document.createElement("input");
    document.body.append(input);
    input.dispatchEvent(new KeyboardEvent("keydown", { key: "V", code: "KeyV", altKey: true, shiftKey: true, bubbles: true }));
    expect(switches).toEqual([]);
    document.body.dispatchEvent(new KeyboardEvent("keydown", { key: "\u25CA", code: "KeyV", altKey: true, shiftKey: true, bubbles: true }));
    expect(switches).toEqual(["form"]);
    switcher.dispose();
    input.remove();
  });

  /// (partial: an update without actions disposes the previous action menus)
  test("control panel update to an adapter without actions disposes the old action menus", async () => {
    const host = document.createElement("div");
    document.body.append(host);
    const withMenus: ControlPanelAdapter = {
      title: "A", model: "res.partner", actionsArea: document.createElement("span"),
      actionMenuItems: [{ name: "x", label: "X" }] as ActionMenuItem[],
      actionMenuEnv: { resModel: "res.partner", getActiveIds: () => [] },
      display: { showButtons: true },
    };
    const handle = await mountControlPanel(host, withMenus, err => { throw err; });
    const removed = vi.spyOn(document, "removeEventListener");
    handle.update({ title: "B", model: "res.partner", display: { showButtons: true } });
    expect(removed.mock.calls.some(call => call[0] === "pointerdown")).toBe(true);
    removed.mockRestore();
    handle.dispose();
    host.remove();
  });

  test(": view switcher buttons, active view, Alt+Shift+V cycling, and middle-click", async () => {
    const switches: { type: string; newWindow?: boolean }[] = [];
    const entries: ViewSwitcherEntry[] = [
      { type: "list", label: "List" },
      { type: "form", label: "Form" },
      { type: "kanban", label: "Kanban" }
    ];

    const switcher = new ViewSwitcher({
      entries,
      activeType: "list",
      onSwitchView: (type, options) => {
        switches.push({ type, newWindow: options?.newWindow });
      }
    });

    const container = switcher.render();
    document.body.append(container);

    const buttons = container.querySelectorAll<HTMLButtonElement>(".view-btn");
    expect(buttons).toHaveLength(3);

    // Active button has .active and aria-current
    expect(buttons[0]!.classList.contains("active")).toBe(true);
    expect(buttons[0]!.getAttribute("aria-label")).toBe("List view");
    expect(buttons[1]!.classList.contains("active")).toBe(false);

    // Left click switches view
    buttons[1]!.click();
    expect(switches[switches.length - 1]).toEqual({ type: "form", newWindow: false });

    // Middle click (button 1 on auxclick) requests new window
    const auxEvent = new MouseEvent("auxclick", { button: 1, bubbles: true });
    buttons[2]!.dispatchEvent(auxEvent);
    expect(switches[switches.length - 1]).toEqual({ type: "kanban", newWindow: true });

    // Alt+Shift+V cycles to next view: currently "list" -> advances to "form"
    switcher.cycleThroughViews();
    expect(switches[switches.length - 1]).toEqual({ type: "form", newWindow: false });

    switcher.dispose();
    container.remove();
  });

  test(": mobile control-panel compact adaptation and sticky scroll listener cleanup", async () => {
    const scrollContainer = document.createElement("div");
    Object.defineProperty(scrollContainer, "scrollTop", { value: 0, writable: true });
    Object.defineProperty(scrollContainer, "scrollHeight", { value: 1000, writable: true });
    document.body.append(scrollContainer);

    const panelEl = document.createElement("section");
    panelEl.className = "control-panel";
    scrollContainer.append(panelEl);

    const handle = await mountControlPanel(
      panelEl,
      {
        title: "Partners",
        model: "res.partner",
        display: {
          adaptToScroll: true,
          layout: "compact"
        }
      },
      err => { throw err; },
      { scrollingElement: scrollContainer, isSmall: true }
    );

    expect(handle.panel.classList.contains("control-panel-compact")).toBe(true);

    // Scroll down > initial: triggers sticky header
    scrollContainer.scrollTop = 150;
    handle.onScroll?.();
    expect(handle.panel.classList.contains("mobile-sticky")).toBe(true);
    expect(handle.panel.classList.contains("o_mobile_sticky")).toBe(false);

    // Scroll back to top: removes sticky header
    await new Promise(resolve => setTimeout(resolve, 20));
    scrollContainer.scrollTop = 0;
    handle.onScroll?.();
    expect(handle.panel.classList.contains("mobile-sticky")).toBe(false);

    // Disposal removes event listeners and observers cleanly
    handle.dispose();
    scrollContainer.remove();
  });
});

test("a view label or icon name with markup stays text in the view switcher", () => {
  const switcher = new ViewSwitcher({
    entries: [
      { type: "list", label: '<img src=x onerror="window.__xss = 1">' },
      { type: "form", label: "Form", icon: 'x" onmouseover="window.__xss = 1' },
    ],
    activeType: "list",
    onSwitchView: () => {},
  });
  const container = switcher.render();
  document.body.append(container);
  expect(container.querySelector("img")).toBeNull();
  expect(container.querySelectorAll(".view-btn-label")[0]?.textContent).toBe('<img src=x onerror="window.__xss = 1">');
  const icons = container.querySelectorAll("i.oi");
  expect(icons[1]?.className).toBe("oi oi-view-form");
  expect(icons[1]?.hasAttribute("onmouseover")).toBe(false);
  container.remove();
});

test("ActionMenus dropdown keyboard roving focus and Escape dismissal to trigger", () => {
  const env: ActionMenuEnvironment = {
    resModel: "res.partner",
    getActiveIds: () => [1],
  };
  const items: ActionMenuItem[] = [
    { id: 1, label: "Export" },
    { id: 2, label: "Archive" },
    { id: 3, label: "Delete" },
  ];
  const actionMenus = new ActionMenus(items, env);
  const node = actionMenus.render();
  document.body.append(node);

  const trigger = node.querySelector<HTMLButtonElement>(".action-menus-trigger")!;
  const menu = node.querySelector<HTMLElement>(".action-menu-dropdown")!;
  const buttons = menu.querySelectorAll<HTMLButtonElement>(".action-menu-item");

  expect(trigger.getAttribute("aria-expanded")).toBe("false");
  expect(menu.style.display).toBe("none");

  trigger.focus();
  trigger.dispatchEvent(new KeyboardEvent("keydown", { key: "ArrowDown", bubbles: true }));

  expect(trigger.getAttribute("aria-expanded")).toBe("true");
  expect(menu.style.display).toBe("block");
  expect(document.activeElement).toBe(buttons[0]);

  menu.dispatchEvent(new KeyboardEvent("keydown", { key: "ArrowDown", bubbles: true }));
  expect(document.activeElement).toBe(buttons[1]);

  menu.dispatchEvent(new KeyboardEvent("keydown", { key: "Escape", bubbles: true }));
  expect(trigger.getAttribute("aria-expanded")).toBe("false");
  expect(menu.style.display).toBe("none");
  expect(document.activeElement).toBe(trigger);

  actionMenus.dispose();
  node.remove();
});

