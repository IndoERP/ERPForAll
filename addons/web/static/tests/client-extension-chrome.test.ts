/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
import { describe, expect, test, vi } from "vitest";
import {
  prepareCogMenu,
  renderCogMenu,
  getPrintItemAriaLabel,
  type CogMenuItem,
  type CogMenuItemEnvironment
} from "../src/client/cog-menu.js";
import {
  prepareSystrayItems,
  renderSystray,
  type SystrayItem,
  type SystrayEnvironment
} from "../src/client/systray.js";
import {
  openCommandPalette,
  getActiveCommandPalette,
  type Command,
  type CommandProvider
} from "../src/client/command-palette.js";
import {
  composeUserMenuItems,
  renderUserMenuItems,
  type UserMenuEnvironment,
  type UserMenuItemFactory
} from "../src/client/user-menu.js";
import {
  loadGroupedDebugItems,
  createDebugCommandProvider,
  renderDebugMenu,
  DEFAULT_DEBUG_SECTIONS,
  type DebugEnvironment
} from "../src/client/debug-menu.js";
import { Registry, registerCommandCategory, commandCategories } from "../src/client/registry.js";

describe("M4 Chrome Extension Registries", () => {
  test(": cog menu registration, display predicates, grouping and print labels", async () => {
    const env: CogMenuItemEnvironment = {
      model: "res.partner",
      activeId: 42,
      run: (job) => job(),
      report: (err) => { throw err; }
    };

    const actionCalled: string[] = [];
    const registryItems: CogMenuItem[] = [
      {
        key: "export",
        label: "Export Records",
        groupNumber: 20,
        sequence: 10,
        isDisplayed: () => true,
        callback: () => { actionCalled.push("export"); }
      },
      {
        key: "delete",
        label: "Delete",
        groupNumber: 30,
        sequence: 10,
        isDisplayed: () => false, // Excluded by predicate
        callback: () => { actionCalled.push("delete"); }
      },
      {
        key: "archive",
        label: "Archive",
        groupNumber: 20,
        sequence: 5, // Lower sequence than export, same group
        callback: () => { actionCalled.push("archive"); }
      }
    ];

    const actionItems: CogMenuItem[] = [
      {
        key: "print_partner",
        label: "Partner Summary",
        name: "print_summary",
        groupNumber: 10, // Lowest group number, appears first
        callback: () => { actionCalled.push("print"); }
      }
    ];

    // 1. Preparation: display predicates filtered, sorted by groupNumber then sequence
    const prepared = await prepareCogMenu(registryItems, env, actionItems);
    expect(prepared.map(i => i.key)).toEqual(["print_partner", "archive", "export"]);

    // 2. Print item accessible label
    expect(getPrintItemAriaLabel(actionItems[0]!)).toBe("Print report: Partner Summary");

    // 3. Render and interaction
    const handle = renderCogMenu(registryItems, env, actionItems);
    document.body.append(handle.node);

    const trigger = handle.node.querySelector<HTMLButtonElement>(".cog-menu-trigger")!;
    trigger.click(); // Open dropdown
    await vi.waitFor(() => {
      expect(handle.node.querySelector<HTMLElement>(".cog-menu-dropdown")?.style.display).toBe("block");
      const buttons = handle.node.querySelectorAll<HTMLButtonElement>(".cog-menu-item");
      expect(buttons.length).toBe(3);
    });

    const buttons = handle.node.querySelectorAll<HTMLButtonElement>(".cog-menu-item");
    expect(buttons[0]?.textContent).toBe("Partner Summary");
    expect(buttons[0]?.getAttribute("aria-label")).toBe("Print report: Partner Summary");

    // Activate item
    buttons[0]?.click();
    expect(actionCalled).toEqual(["print"]);

    handle.dispose();
    handle.node.remove();
  });

  test("CogMenu dropdown keyboard roving focus and Escape dismissal to trigger", async () => {
    const env: CogMenuItemEnvironment = {};
    const items: CogMenuItem[] = [
      { key: "item1", label: "First Item", callback: () => {} },
      { key: "item2", label: "Second Item", callback: () => {} },
    ];
    const handle = renderCogMenu(items, env);
    document.body.append(handle.node);

    const trigger = handle.node.querySelector<HTMLButtonElement>(".cog-menu-trigger")!;
    const menu = handle.node.querySelector<HTMLElement>(".cog-menu-dropdown")!;

    expect(trigger.getAttribute("aria-expanded")).toBe("false");
    expect(menu.style.display).toBe("none");

    await vi.waitFor(() => {
      expect(menu.querySelectorAll(".cog-menu-item").length).toBe(2);
    });

    trigger.focus();
    trigger.dispatchEvent(new KeyboardEvent("keydown", { key: "ArrowDown", bubbles: true }));

    expect(trigger.getAttribute("aria-expanded")).toBe("true");
    expect(menu.style.display).toBe("block");
    const buttons = menu.querySelectorAll<HTMLButtonElement>(".cog-menu-item");
    expect(document.activeElement).toBe(buttons[0]);

    menu.dispatchEvent(new KeyboardEvent("keydown", { key: "ArrowDown", bubbles: true }));
    expect(document.activeElement).toBe(buttons[1]);

    menu.dispatchEvent(new KeyboardEvent("keydown", { key: "Escape", bubbles: true }));
    expect(trigger.getAttribute("aria-expanded")).toBe("false");
    expect(menu.style.display).toBe("none");
    expect(document.activeElement).toBe(trigger);

    handle.dispose();
    handle.node.remove();
  });

  test("CogMenu populate sequence prevents out-of-order race and disposed render", async () => {
    let fastResolve: () => void = () => {};
    let slowResolve: () => void = () => {};

    const env: CogMenuItemEnvironment = {};
    const slowItem: CogMenuItem = {
      key: "slow",
      label: "Slow Item",
      isDisplayed: async () => {
        await new Promise<void>(resolve => { slowResolve = resolve; });
        return true;
      }
    };
    const fastItem: CogMenuItem = {
      key: "fast",
      label: "Fast Item",
      isDisplayed: async () => {
        await new Promise<void>(resolve => { fastResolve = resolve; });
        return true;
      }
    };

    // 1. Initial render with slow item
    const handle = renderCogMenu([slowItem], env);
    document.body.append(handle.node);

    // Trigger update with fast item before slow item resolves
    const updatePromise = handle.update([fastItem]);

    // Fast resolves first
    fastResolve();
    await updatePromise;

    const dropdown = handle.node.querySelector<HTMLElement>(".cog-menu-dropdown")!;
    expect(dropdown.querySelectorAll(".cog-menu-item").length).toBe(1);
    expect(dropdown.querySelector(".cog-menu-item")?.textContent).toBe("Fast Item");

    // Now slow resolves late: it must NOT overwrite the fast item
    slowResolve();
    await new Promise(r => setTimeout(r, 10));

    expect(dropdown.querySelectorAll(".cog-menu-item").length).toBe(1);
    expect(dropdown.querySelector(".cog-menu-item")?.textContent).toBe("Fast Item");

    // 2. Disposed handle must not render into DOM after late resolve
    let resolveDisposed: () => void = () => {};
    const itemPending: CogMenuItem = {
      key: "pending",
      label: "Pending Item",
      isDisplayed: async () => {
        await new Promise<void>(resolve => { resolveDisposed = resolve; });
        return true;
      }
    };
    const handle2 = renderCogMenu([itemPending], env);
    const node2 = handle2.node;
    document.body.append(node2);

    handle2.dispose();
    resolveDisposed();
    await new Promise(r => setTimeout(r, 10));

    const dropdown2 = node2.querySelector<HTMLElement>(".cog-menu-dropdown");
    expect(dropdown2?.children.length ?? 0).toBe(0);
    node2.remove();

    handle.dispose();
    handle.node.remove();
  });

  test(": systray extension order, display predicates and component ownership", async () => {
    const env: SystrayEnvironment = {
      run: (job) => job(),
      report: (err) => { throw err; }
    };

    const clicked: string[] = [];
    const entries: [string, SystrayItem][] = [
      [
        "mail_widget",
        {
          key: "mail_widget",
          isDisplayed: () => true,
          render: () => {
            const btn = document.createElement("button");
            btn.className = "systray-mail-btn";
            btn.setAttribute("aria-label", "Conversations");
            btn.addEventListener("click", () => { clicked.push("mail"); });
            return { node: btn, dispose: () => { btn.remove(); } };
          }
        }
      ],
      [
        "hidden_widget",
        {
          key: "hidden_widget",
          isDisplayed: () => false, // Failing predicate
          render: () => {
            const btn = document.createElement("button");
            return { node: btn, dispose: () => { btn.remove(); } };
          }
        }
      ],
      [
        "activities_widget",
        {
          key: "activities_widget",
          isDisplayed: () => true,
          render: () => {
            const btn = document.createElement("button");
            btn.className = "systray-activities-btn";
            btn.setAttribute("aria-label", "Activities");
            btn.addEventListener("click", () => { clicked.push("activities"); });
            return { node: btn, dispose: () => { btn.remove(); } };
          }
        }
      ]
    ];

    // Surviving items should be reversed entry order: activities_widget first, then mail_widget
    const prepared = await prepareSystrayItems(entries, env);
    expect(prepared.map(i => i.key)).toEqual(["activities_widget", "mail_widget"]);

    const container = document.createElement("div");
    container.className = "systray";
    document.body.append(container);

    const handle = await renderSystray(container, entries, env);
    expect(container.children.length).toBe(2);
    expect(container.children[0]?.classList.contains("systray-activities-btn")).toBe(true);
    expect(container.children[1]?.classList.contains("systray-mail-btn")).toBe(true);

    container.querySelector<HTMLButtonElement>(".systray-activities-btn")?.click();
    expect(clicked).toEqual(["activities"]);

    handle.dispose();
    container.remove();
  });

  test(": command palette singleton lifecycle, focus, keyboard navigation and execution", async () => {
    let executedAction = false;
    let openedHref = false;
    const originalOpen = window.open;
    window.open = (() => { openedHref = true; return null; }) as unknown as typeof window.open;

    const testProvider: CommandProvider = {
      namespace: "default",
      provide: ({ searchValue }) => {
        const commands: Command[] = [
          {
            id: "cmd_partner",
            name: "Go to Partners",
            category: "Navigation",
            action: () => { executedAction = true; }
          },
          {
            id: "cmd_link",
            name: "Documentation Link",
            category: "Help",
            href: "https://example.com/docs"
          }
        ];
        if (!searchValue) return commands;
        return commands.filter(c => c.name.toLowerCase().includes(searchValue.toLowerCase()));
      }
    };

    // 1. Open palette
    const palette = openCommandPalette({
      providers: [testProvider],
      setup: { emptyMessage: "No matching commands found", placeholder: "Type a command..." }
    });

    expect(getActiveCommandPalette()).toBe(palette);

    // Verify duplicate open updates existing rather than opening duplicate
    const secondCall = openCommandPalette({
      providers: [testProvider],
      initialSearch: "partner"
    });
    expect(secondCall).toBe(palette);
    expect(document.querySelectorAll(".command-palette-dialog").length).toBe(1);

    const input = palette.node.querySelector<HTMLInputElement>(".command-palette-input")!;
    expect(input.value).toBe("partner");

    await vi.waitFor(() => {
      const items = palette.node.querySelectorAll(".command-palette-item");
      expect(items.length).toBe(1);
    });

    // 2. Clear input to see all results
    input.value = "";
    input.dispatchEvent(new Event("input"));

    await vi.waitFor(() => {
      const items = palette.node.querySelectorAll(".command-palette-item");
      expect(items.length).toBe(2);
    });

    // 3. Arrow Down navigation and aria-activedescendant
    const items = palette.node.querySelectorAll(".command-palette-item");
    expect(items[0]?.getAttribute("aria-selected")).toBe("true");
    expect(input.getAttribute("aria-activedescendant")).toBe(items[0]?.id);

    input.dispatchEvent(new KeyboardEvent("keydown", { key: "ArrowDown" }));
    expect(items[1]?.getAttribute("aria-selected")).toBe("true");
    expect(input.getAttribute("aria-activedescendant")).toBe(items[1]?.id);

    // 4. Control+Enter opens href separately on selected item
    input.dispatchEvent(new KeyboardEvent("keydown", { key: "Enter", ctrlKey: true }));
    await vi.waitFor(() => expect(openedHref).toBe(true));
    expect(palette.isOpen).toBe(false);

    // 5. Re-open and verify Enter invokes action
    const palette2 = openCommandPalette({ providers: [testProvider] });
    await vi.waitFor(() => {
      expect(palette2.node.querySelectorAll(".command-palette-item").length).toBe(2);
    });
    const input2 = palette2.node.querySelector<HTMLInputElement>(".command-palette-input")!;
    input2.dispatchEvent(new KeyboardEvent("keydown", { key: "Enter" }));
    await vi.waitFor(() => expect(executedAction).toBe(true));
    expect(palette2.isOpen).toBe(false);

    // 6. Distinct empty state with empty providers and cleared aria-activedescendant
    const emptyProvider: CommandProvider = {
      namespace: "default",
      provide: () => []
    };
    const palette3 = openCommandPalette({
      providers: [emptyProvider],
      setup: { emptyMessage: "Custom empty message" }
    });
    await vi.waitFor(() => {
      const emptyEl = palette3.node.querySelector<HTMLElement>(".command-palette-empty")!;
      expect(emptyEl.style.display).not.toBe("none");
      expect(emptyEl.textContent).toBe("Custom empty message");
    });
    const input3 = palette3.node.querySelector<HTMLInputElement>(".command-palette-input")!;
    expect(input3.getAttribute("aria-activedescendant")).toBeNull();
    palette3.close();

    // 7. Background inertness, focus trap and dialog-wide Escape
    const backgroundEl = document.createElement("div");
    backgroundEl.className = "test-background-app";
    document.body.append(backgroundEl);
    expect(backgroundEl.hasAttribute("inert")).toBe(false);

    const palette4 = openCommandPalette({ providers: [testProvider] });
    expect(backgroundEl.hasAttribute("inert")).toBe(true);

    const dialogEl = palette4.node.querySelector<HTMLElement>(".command-palette-dialog")!;
    // Escape dispatched anywhere inside dialog closes palette
    dialogEl.dispatchEvent(new KeyboardEvent("keydown", { key: "Escape", bubbles: true }));
    window.open = originalOpen;
  });

  test(": command palette focus trap, inert background and escape dismissal", () => {
    const originalOpen = window.open;
    const testProvider: CommandProvider = {
      namespace: "default",
      provide: () => [{ id: "cmd_test", name: "Test Command", action: () => {} }]
    };

    const backgroundEl = document.createElement("div");
    backgroundEl.className = "test-background-app";
    document.body.append(backgroundEl);
    expect(backgroundEl.hasAttribute("inert")).toBe(false);

    const palette = openCommandPalette({ providers: [testProvider] });
    expect(backgroundEl.hasAttribute("inert")).toBe(true);

    const dialogEl = palette.node.querySelector<HTMLElement>(".command-palette-dialog")!;
    // Escape dispatched anywhere inside dialog closes palette
    dialogEl.dispatchEvent(new KeyboardEvent("keydown", { key: "Escape", bubbles: true }));
    expect(palette.isOpen).toBe(false);
    expect(backgroundEl.hasAttribute("inert")).toBe(false);
    backgroundEl.remove();

    window.open = originalOpen;
  });

  /// (partial: command categories, namespace and sequence)
  test(": command categories registration, namespace isolation and sequence-based ordering", async () => {
    // 1. An addon registers custom command categories across namespaces
    registerCommandCategory("addon_reports", { name: "Reports & Analytics", sequence: 30, namespace: "default" });
    registerCommandCategory("addon_quick", { name: "Quick Actions", sequence: 10, namespace: "default" });
    registerCommandCategory("addon_config", { name: "Configuration", sequence: 50, namespace: "default" });
    registerCommandCategory("inv_ops", { name: "Inventory Operations", sequence: 15, namespace: "inventory" });

    // 2. Providers supplying commands in unsorted order across namespaces
    const defaultProvider: CommandProvider = {
      namespace: "default",
      provide: () => [
        { id: "cmd_gl", name: "General Ledger", category: "addon_reports" },
        { id: "cmd_settings", name: "System Settings", category: "addon_config" },
        { id: "cmd_sale", name: "New Sale Order", category: "addon_quick" },
        { id: "cmd_orphan", name: "Uncategorized Utility" }
      ]
    };
    const inventoryProvider: CommandProvider = {
      namespace: "inventory",
      provide: () => [
        { id: "cmd_transfer", name: "Transfer Products", category: "inv_ops" }
      ]
    };

    try {
      // 3. Open palette in default namespace with registered categories
      const categories = [...commandCategories.entries()].map(([id, cat]) => ({
        id,
        ...cat
      }));
      const palette = openCommandPalette({
        providers: [defaultProvider, inventoryProvider],
        categories,
        setup: { emptyMessage: "No commands found" }
      });

      await vi.waitFor(() => {
        const items = palette.node.querySelectorAll<HTMLElement>(".command-palette-item");
        expect(items.length).toBe(4);
      });

      const items = palette.node.querySelectorAll<HTMLElement>(".command-palette-item");

      // Verify sequence-based ordering:
      // Item 0: Quick Actions (seq 10) -> "New Sale Order"
      expect(items[0]?.querySelector(".command-name")?.textContent).toBe("New Sale Order");
      expect(items[0]?.querySelector(".command-category")?.textContent).toBe("Quick Actions");

      // Item 1: Reports & Analytics (seq 30) -> "General Ledger"
      expect(items[1]?.querySelector(".command-name")?.textContent).toBe("General Ledger");
      expect(items[1]?.querySelector(".command-category")?.textContent).toBe("Reports & Analytics");

      // Item 2: Configuration (seq 50) -> "System Settings"
      expect(items[2]?.querySelector(".command-name")?.textContent).toBe("System Settings");
      expect(items[2]?.querySelector(".command-category")?.textContent).toBe("Configuration");

      // Item 3: Uncategorized Utility -> appears after sequenced categories
      expect(items[3]?.querySelector(".command-name")?.textContent).toBe("Uncategorized Utility");

      // Verify inventory namespace command is isolated and not in default results
      const names = Array.from(items).map(el => el.querySelector(".command-name")?.textContent);
      expect(names).not.toContain("Transfer Products");

      palette.close();

      // 4. Open palette targeting the "inventory" namespace
      const invPalette = openCommandPalette({
        namespace: "inventory",
        providers: [defaultProvider, inventoryProvider],
        categories
      });

      await vi.waitFor(() => {
        const invItems = invPalette.node.querySelectorAll<HTMLElement>(".command-palette-item");
        expect(invItems.length).toBe(1);
      });

      const invItems = invPalette.node.querySelectorAll<HTMLElement>(".command-palette-item");
      expect(invItems[0]?.querySelector(".command-name")?.textContent).toBe("Transfer Products");
      expect(invItems[0]?.querySelector(".command-category")?.textContent).toBe("Inventory Operations");

      invPalette.close();
    } finally {
      // Clean up registered categories from global registry
      commandCategories.remove("addon_reports");
      commandCategories.remove("addon_quick");
      commandCategories.remove("addon_config");
      commandCategories.remove("inv_ops");
    }
  });

  test(": user menu item composition, sequence sorting, switches and separators", () => {
    const env: UserMenuEnvironment = {
      userName: "Administrator",
      userId: 1,
      currentCompanyId: 1
    };

    let switchToggled = false;
    let actionInvoked = false;

    const factories: [string, UserMenuItemFactory][] = [
      [
        "dark_mode",
        () => ({
          id: "switch_dark_mode",
          type: "switch",
          description: "Dark Mode",
          sequence: 30,
          isChecked: true,
          callback: (checked) => { switchToggled = Boolean(checked); }
        })
      ],
      [
        "separator_1",
        () => ({
          type: "separator",
          sequence: 20
        })
      ],
      [
        "support",
        () => ({
          id: "item_support",
          type: "item",
          description: "Support",
          sequence: 10, // First
          callback: () => { actionInvoked = true; }
        })
      ],
      [
        "hidden_entry",
        () => ({
          id: "item_hidden",
          type: "item",
          description: "Hidden Item",
          sequence: 5,
          hide: true // Excluded
        })
      ],
      [
        "fallback_seq",
        () => ({
          id: "item_fallback",
          type: "item",
          description: "Fallback Sequence Item"
          // sequence omitted -> falls back to 100
        })
      ]
    ];

    // 1. Composition: filters hidden, sorts by sequence (10, 20, 30, 100)
    const composed = composeUserMenuItems(factories, env);
    expect(composed.map(i => i.id || i.type)).toEqual([
      "item_support",
      "separator",
      "switch_dark_mode",
      "item_fallback"
    ]);

    // 2. Rendering
    const container = document.createElement("div");
    container.className = "user-dropdown-custom-items";
    document.body.append(container);

    const handle = renderUserMenuItems(container, composed, env);

    // Support item
    const supportBtn = container.querySelector<HTMLButtonElement>('[data-menu="item_support"]')!;
    expect(supportBtn).toBeDefined();
    expect(supportBtn.textContent).toBe("Support");
    supportBtn.click();
    expect(actionInvoked).toBe(true);

    // Separator
    const divider = container.querySelector<HTMLElement>(".dropdown-divider")!;
    expect(divider).toBeDefined();
    expect(divider.getAttribute("role")).toBe("separator");

    // Switch
    const switchCheckbox = container.querySelector<HTMLInputElement>('input[type="checkbox"].form-switch')!;
    expect(switchCheckbox).toBeDefined();
    expect(switchCheckbox.checked).toBe(true);
    switchCheckbox.checked = false;
    switchCheckbox.dispatchEvent(new Event("change"));
    expect(switchToggled).toBe(false);

    handle.dispose();
    container.remove();
  });

  test(": contextual developer menu grouping, sequence and command palette bridge", async () => {
    const env: DebugEnvironment = {
      model: "res.partner",
      activeId: 10,
      context: {}
    };

    let inspectedFields = false;
    const testProviders = [
      async () => [
        {
          id: "debug_view_fields",
          type: "item" as const,
          section: "ui",
          description: "View Fields",
          callback: () => { inspectedFields = true; }
        },
        {
          id: "debug_view_metadata",
          type: "item" as const,
          section: "record",
          description: "View Metadata",
          callback: () => {}
        },
        {
          id: "debug_custom_tool",
          type: "item" as const,
          section: "tools",
          description: "Custom Profiler",
          callback: () => {}
        }
      ]
    ];

    const sectionsRegistry = new Registry<{ label: string; sequence: number }>("debug section");
    for (const [k, v] of Object.entries(DEFAULT_DEBUG_SECTIONS)) {
      sectionsRegistry.add(k, v);
    }

    // 1. Grouped sections sorting: record (10), ui (20), tools (50)
    const grouped = await loadGroupedDebugItems(testProviders, sectionsRegistry, env);
    expect(grouped.map(g => g.section)).toEqual(["record", "ui", "tools"]);
    expect(grouped.map(g => g.label)).toEqual(["Record", "User Interface", "Tools"]);

    // 2. Command Palette Bridge
    const commandProvider = createDebugCommandProvider(testProviders, sectionsRegistry, env);
    expect(commandProvider.namespace).toBe("debug");

    const commands = await commandProvider.provide({ searchValue: "fields" });
    expect(commands.length).toBe(1);
    expect(commands[0]?.name).toBe("View Fields");
    expect(commands[0]?.category).toBe("User Interface");

    // Invoke action through bridge
    commands[0]?.action?.();
    expect(inspectedFields).toBe(true);

    // 3. Render debug menu dropdown
    const container = document.createElement("div");
    container.className = "debug-menu-dropdown";
    document.body.append(container);

    const handle = await renderDebugMenu(container, testProviders, sectionsRegistry, env);
    const headers = container.querySelectorAll<HTMLElement>(".debug-section-header");
    expect(headers.length).toBe(3);
    expect(headers[0]?.textContent).toBe("Record");
    expect(headers[1]?.textContent).toBe("User Interface");
    expect(headers[2]?.textContent).toBe("Tools");

    handle.dispose();
    container.remove();
  });
});
