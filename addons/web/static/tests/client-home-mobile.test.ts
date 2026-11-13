/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
import { describe, expect, test, vi, beforeEach, afterEach } from "vitest";
import { BottomSheet, getActiveBottomSheetCount } from "../src/client/bottom-sheet.js";
import { HomeMenu, type HomeMenuApp } from "../src/client/home-menu.js";
import { NavbarOverflow } from "../src/client/navbar-overflow.js";
import { MobileBurgerMenu, isDisplayStandalone } from "../src/client/burger-menu.js";

describe("M4 Shell Base: Home and Mobile", () => {
  let host: HTMLElement;

  beforeEach(() => {
    delete (window as { matchMedia?: unknown }).matchMedia;
    document.body.className = "";
    document.body.replaceChildren();
    host = document.createElement("div");
    document.body.append(host);
  });

  afterEach(() => {
    delete (window as { matchMedia?: unknown }).matchMedia;
    document.body.replaceChildren();
    document.body.className = "";
    vi.restoreAllMocks();
  });

  test(": Home and app navigation toggle coordinates with background action and closes sidebar", async () => {
    let backgroundActionActive = true;
    let sidebarClosed = false;
    let selectedAppId: number | null = null;
    let closedCalled = false;

    const apps: HomeMenuApp[] = [
      { id: 1, label: "Sales", actionId: 101 },
      { id: 2, label: "Accounting", actionId: 102 }
    ];

    const navbar = document.createElement("nav");
    navbar.className = "main-navbar";

    const brand = document.createElement("span");
    brand.className = "brand";
    brand.textContent = "Sales";

    const submenus = document.createElement("div");
    submenus.className = "app-sub-menus";

    const breadcrumbs = document.createElement("div");
    breadcrumbs.className = "breadcrumbs";

    const appToggleBtn = document.createElement("button");
    appToggleBtn.className = "app-switcher-btn";

    navbar.append(appToggleBtn, brand, submenus, breadcrumbs);
    host.append(navbar);

    const home = new HomeMenu({
      container: host,
      apps,
      hasBackgroundAction: () => backgroundActionActive,
      onOpenApp: (app) => { selectedAppId = app.id; },
      onClose: () => { closedCalled = true; },
      onAllAppsCloseSidebar: () => { sidebarClosed = true; }
    });

    // Initial state: not in home, in app
    expect(home.isOpen).toBe(false);
    expect(home.isInApp).toBe(true);

    home.updateAppToggle(appToggleBtn, navbar);
    expect(appToggleBtn.title).toBe("Home menu");
    expect(appToggleBtn.getAttribute("aria-label")).toBe("Home menu");
    expect(appToggleBtn.classList.contains("menu-toggle-back")).toBe(false);
    expect(appToggleBtn.classList.contains("o_menu_toggle_back")).toBe(false);
    expect(brand.classList.contains("hidden")).toBe(false);
    expect(brand.classList.contains("o_menu_brand")).toBe(false);
    expect(submenus.classList.contains("hidden")).toBe(false);
    expect(breadcrumbs.classList.contains("o_breadcrumb")).toBe(false);

    // Open Home menu with active background action
    home.open();
    expect(home.isOpen).toBe(true);
    expect(home.isInApp).toBe(false);

    home.updateAppToggle(appToggleBtn, navbar);
    expect(appToggleBtn.title).toBe("Previous view");
    expect(appToggleBtn.getAttribute("aria-label")).toBe("Previous view");
    expect(appToggleBtn.classList.contains("menu-toggle-back")).toBe(true);
    expect(appToggleBtn.classList.contains("o_menu_toggle_back")).toBe(false);
    expect(brand.classList.contains("hidden")).toBe(true);
    expect(submenus.classList.contains("hidden")).toBe(true);
    expect(breadcrumbs.classList.contains("hidden")).toBe(true);

    // If no background action exists
    backgroundActionActive = false;
    home.updateAppToggle(appToggleBtn, navbar);
    expect(appToggleBtn.classList.contains("hidden")).toBe(true);

    // All-apps button from sidebar/drawer opens home and closes sidebar
    home.close();
    expect(home.isOpen).toBe(false);
    expect(closedCalled).toBe(true);
    home.onAllAppsBtnClick();
    expect(home.isOpen).toBe(true);
    expect(sidebarClosed).toBe(true);

    const firstTile = home.getGrid().querySelector<HTMLButtonElement>(".app-tile");
    firstTile?.click();
    expect(selectedAppId).toBe(1);

    home.dispose();
  });

  test(": Home keyboard grid navigates across columns and lines with Tab/Shift+Tab, Enter, Escape and typing protection", async () => {
    let openedAppId: number | null = null;
    let closeTriggered = false;

    const apps: HomeMenuApp[] = [
      { id: 1, label: "App 1" },
      { id: 2, label: "App 2" },
      { id: 3, label: "App 3" },
      { id: 4, label: "App 4" },
      { id: 5, label: "App 5" },
      { id: 6, label: "App 6" },
      { id: 7, label: "App 7" },
      { id: 8, label: "App 8" }
    ];

    const home = new HomeMenu({
      container: host,
      apps,
      hasBackgroundAction: () => true,
      onOpenApp: (app) => { openedAppId = app.id; },
      onClose: () => { closeTriggered = true; }
    });

    // Mock columns to 4 per row
    vi.spyOn(home, "getColumns").mockReturnValue(4);

    home.open();
    // Default focus starts on first element (index 0)
    expect(home.currentFocusedIndex).toBe(0);

    // Navigate next column
    window.dispatchEvent(new KeyboardEvent("keydown", { key: "ArrowRight" }));
    expect(home.currentFocusedIndex).toBe(1);

    window.dispatchEvent(new KeyboardEvent("keydown", { key: "ArrowRight" }));
    expect(home.currentFocusedIndex).toBe(2);

    // Navigate next line (index 2 + 4 = index 6)
    window.dispatchEvent(new KeyboardEvent("keydown", { key: "ArrowDown" }));
    expect(home.currentFocusedIndex).toBe(6);

    // Navigate previous line (index 6 - 4 = index 2)
    window.dispatchEvent(new KeyboardEvent("keydown", { key: "ArrowUp" }));
    expect(home.currentFocusedIndex).toBe(2);

    // Navigate previous column
    window.dispatchEvent(new KeyboardEvent("keydown", { key: "ArrowLeft" }));
    expect(home.currentFocusedIndex).toBe(1);

    // Tab moves to next element
    window.dispatchEvent(new KeyboardEvent("keydown", { key: "Tab" }));
    expect(home.currentFocusedIndex).toBe(2);

    // Shift+Tab moves to previous element
    window.dispatchEvent(new KeyboardEvent("keydown", { key: "Tab", shiftKey: true }));
    expect(home.currentFocusedIndex).toBe(1);

    // Enter activates focused app
    window.dispatchEvent(new KeyboardEvent("keydown", { key: "Enter" }));
    expect(openedAppId).toBe(2);

    // Typing when active element is not an input shifts focus to search input
    const searchInput = home.getSearchInput();
    document.body.focus();
    expect(document.activeElement).not.toBe(searchInput);
    window.dispatchEvent(new KeyboardEvent("keydown", { key: "s" }));
    expect(document.activeElement).toBe(searchInput);

    // Typing inside another input does NOT steal focus
    const otherInput = document.createElement("input");
    host.append(otherInput);
    otherInput.focus();
    expect(document.activeElement).toBe(otherInput);
    window.dispatchEvent(new KeyboardEvent("keydown", { key: "a" }));
    expect(document.activeElement).toBe(otherInput);

    // Escape closes home menu
    window.dispatchEvent(new KeyboardEvent("keydown", { key: "Escape" }));
    expect(home.isOpen).toBe(false);
    expect(closeTriggered).toBe(true);

    home.dispose();
  });

  test(": Home search input retains DOM focus while typing and Backspacing, and highlights active tile with aria-activedescendant", () => {
    let openedApp: HomeMenuApp | null = null;
    const apps: HomeMenuApp[] = [
      { id: 10, label: "Contacts" },
      { id: 20, label: "CRM" },
      { id: 30, label: "Calendar" },
      { id: 40, label: "Inventory" },
    ];

    const home = new HomeMenu({
      container: host,
      apps,
      hasBackgroundAction: () => true,
      onOpenApp: (app) => { openedApp = app; },
      onClose: () => {}
    });

    home.open();
    const searchInput = home.getSearchInput();
    searchInput.focus();
    expect(document.activeElement).toBe(searchInput);

    // Initial search input ARIA attributes
    expect(searchInput.getAttribute("role")).toBe("combobox");
    expect(searchInput.getAttribute("aria-autocomplete")).toBe("list");
    expect(searchInput.getAttribute("aria-controls")).toBe("home-menu-grid");
    expect(home.getGrid().id).toBe("home-menu-grid");
    expect(home.getGrid().getAttribute("role")).toBe("listbox");

    // Initially first app (Contacts, id 10) is active
    expect(searchInput.getAttribute("aria-activedescendant")).toBe("home-app-tile-10");
    const initialTiles = home.getGrid().querySelectorAll<HTMLButtonElement>(".app-tile");
    expect(initialTiles[0]?.id).toBe("home-app-tile-10");
    expect(initialTiles[0]?.classList.contains("is-focused")).toBe(true);
    expect(initialTiles[0]?.getAttribute("role")).toBe("option");

    // Typing in search input: input event must NOT steal DOM focus to tile
    searchInput.value = "c";
    searchInput.dispatchEvent(new Event("input"));

    expect(document.activeElement).toBe(searchInput);
    expect(home.currentDisplayedApps.map(a => a.label)).toEqual(["Contacts", "CRM", "Calendar"]);
    expect(searchInput.getAttribute("aria-activedescendant")).toBe("home-app-tile-10");

    const filteredTiles = home.getGrid().querySelectorAll<HTMLButtonElement>(".app-tile");
    expect(filteredTiles[0]?.classList.contains("is-focused")).toBe(true);
    expect(document.activeElement).toBe(searchInput);

    // Further typing to narrow down
    searchInput.value = "cr";
    searchInput.dispatchEvent(new Event("input"));
    expect(document.activeElement).toBe(searchInput);
    expect(home.currentDisplayedApps.map(a => a.label)).toEqual(["CRM"]);
    expect(searchInput.getAttribute("aria-activedescendant")).toBe("home-app-tile-20");

    // Backspace: editing the search query in the input
    searchInput.value = "c";
    searchInput.dispatchEvent(new Event("input"));
    expect(document.activeElement).toBe(searchInput);
    expect(home.currentDisplayedApps.map(a => a.label)).toEqual(["Contacts", "CRM", "Calendar"]);

    // Arrow navigation while in search input updates active descendant without blurring search
    window.dispatchEvent(new KeyboardEvent("keydown", { key: "ArrowRight", bubbles: true }));
    expect(document.activeElement).toBe(searchInput);
    expect(home.currentFocusedIndex).toBe(1);
    expect(searchInput.getAttribute("aria-activedescendant")).toBe("home-app-tile-20");

    const reTiles = home.getGrid().querySelectorAll<HTMLButtonElement>(".app-tile");
    expect(reTiles[1]?.classList.contains("is-focused")).toBe(true);
    expect(reTiles[0]?.classList.contains("is-focused")).toBe(false);

    // Enter while in search input activates the active descendant app
    window.dispatchEvent(new KeyboardEvent("keydown", { key: "Enter", bubbles: true }));
    expect(openedApp).toEqual({ id: 20, label: "CRM" });

    home.dispose();
  });

  /// (partial: only an open popup around the focus takes Escape before the sheet)
  test("a bottom sheet with a closed menu or an expanded disclosure still closes on Escape; an open menu takes it first", () => {
    window.matchMedia = vi.fn().mockReturnValue({ matches: true, media: "", onchange: null, addListener: vi.fn(), removeListener: vi.fn(), addEventListener: vi.fn(), removeEventListener: vi.fn(), dispatchEvent: vi.fn() } as unknown as MediaQueryList);
    const content = document.createElement("div");
    content.innerHTML = `<button id="disclosure" aria-expanded="true">Details</button><div class="nav-dropdown-menu" role="menu"><button class="dropdown-item" id="item">Item</button></div>`;
    const sheet = new BottomSheet({ title: "Sheet", content });
    sheet.openSheet(host);
    content.querySelector<HTMLElement>("#disclosure")!.focus();
    window.dispatchEvent(new KeyboardEvent("keydown", { key: "Escape" }));
    expect(sheet.isOpen).toBe(false);
    sheet.dispose();

    const again = new BottomSheet({ title: "Sheet", content });
    again.openSheet(host);
    content.querySelector(".nav-dropdown-menu")!.classList.add("open");
    content.querySelector<HTMLElement>("#item")!.focus();
    window.dispatchEvent(new KeyboardEvent("keydown", { key: "Escape" }));
    expect(again.isOpen).toBe(true);
    again.dispose();
  });

  test(": Mobile bottom-sheet lifetime adapts to viewport, handles Escape, reduced-motion dismissal, and body state tracking", async () => {
    let closed = false;
    expect(getActiveBottomSheetCount()).toBe(0);
    expect(document.body.classList.contains("bottom-sheet-open")).toBe(false);

    const content = document.createElement("div");
    content.textContent = "Sheet Content";

    const sheet = new BottomSheet({
      title: "Filter Options",
      content,
      onClose: () => { closed = true; }
    });

    sheet.openSheet(host);
    expect(sheet.isOpen).toBe(true);
    expect(getActiveBottomSheetCount()).toBe(1);
    expect(document.body.classList.contains("bottom-sheet-open")).toBe(true);

    // Sheet dimensions adaptation
    sheet.updateDimensions();
    expect(sheet.sheet.style.maxHeight).toBeTruthy();

    // Escape triggers dismissal
    // Mock reduced motion for synchronous clean dismissal test
    window.matchMedia = vi.fn().mockReturnValue({
      matches: true,
      media: "(prefers-reduced-motion: reduce)",
      onchange: null,
      addListener: vi.fn(),
      removeListener: vi.fn(),
      addEventListener: vi.fn(),
      removeEventListener: vi.fn(),
      dispatchEvent: vi.fn()
    } as unknown as MediaQueryList);

    window.dispatchEvent(new KeyboardEvent("keydown", { key: "Escape" }));
    expect(sheet.isOpen).toBe(false);
    expect(closed).toBe(true);
    expect(getActiveBottomSheetCount()).toBe(0);
    expect(document.body.classList.contains("bottom-sheet-open")).toBe(false);

    sheet.dispose();
  });

  test(": Stacking two bottom sheets lets only the topmost sheet handle Escape and keeps background sheets open", () => {
    window.matchMedia = vi.fn().mockReturnValue({
      matches: true,
      media: "(prefers-reduced-motion: reduce)",
      onchange: null,
      addListener: vi.fn(),
      removeListener: vi.fn(),
      addEventListener: vi.fn(),
      removeEventListener: vi.fn(),
      dispatchEvent: vi.fn()
    } as unknown as MediaQueryList);

    let sheet1Closed = false;
    let sheet2Closed = false;

    const content1 = document.createElement("div");
    content1.textContent = "Sheet 1 content";
    const sheet1 = new BottomSheet({
      title: "Sheet 1",
      content: content1,
      onClose: () => { sheet1Closed = true; }
    });

    const content2 = document.createElement("div");
    content2.textContent = "Sheet 2 content";
    const sheet2 = new BottomSheet({
      title: "Sheet 2",
      content: content2,
      onClose: () => { sheet2Closed = true; }
    });

    sheet1.openSheet(host);
    expect(sheet1.isOpen).toBe(true);
    expect(getActiveBottomSheetCount()).toBe(1);

    sheet2.openSheet(host);
    expect(sheet2.isOpen).toBe(true);
    expect(getActiveBottomSheetCount()).toBe(2);

    // Escape must close ONLY the topmost sheet (sheet2)
    window.dispatchEvent(new KeyboardEvent("keydown", { key: "Escape" }));
    expect(sheet2.isOpen).toBe(false);
    expect(sheet2Closed).toBe(true);
    expect(sheet1.isOpen).toBe(true);
    expect(sheet1Closed).toBe(false);
    expect(getActiveBottomSheetCount()).toBe(1);

    // Second Escape closes the remaining sheet (sheet1)
    window.dispatchEvent(new KeyboardEvent("keydown", { key: "Escape" }));
    expect(sheet1.isOpen).toBe(false);
    expect(sheet1Closed).toBe(true);
    expect(getActiveBottomSheetCount()).toBe(0);

    sheet1.dispose();
    sheet2.dispose();
  });

  /// (partial: bottom sheet structural DOM classes use our own class naming without o_ prefix)
  test(": BottomSheet DOM elements use bottom-sheet* naming without o_ prefixes", () => {
    const content = document.createElement("div");
    content.textContent = "Sheet Body Content";
    const sheet = new BottomSheet({
      title: "Options",
      className: "custom-sheet-test",
      content
    });
    sheet.openSheet(host);

    expect(sheet.container.className).toBe("bottom-sheet-container");
    expect(sheet.backdrop.className).toBe("bottom-sheet-backdrop");
    expect(sheet.sheet.classList.contains("bottom-sheet")).toBe(true);
    expect(sheet.sheet.classList.contains("custom-sheet-test")).toBe(true);
    expect(sheet.sheet.classList.contains("o_bottom_sheet")).toBe(false);

    const header = sheet.sheet.querySelector(".bottom-sheet-header");
    expect(header).not.toBeNull();
    expect(header?.querySelector(".bottom-sheet-handle")).not.toBeNull();
    expect(header?.querySelector(".bottom-sheet-title")?.textContent).toBe("Options");
    expect(header?.querySelector(".bottom-sheet-close")).toBe(sheet.closeBtn);
    expect(sheet.closeBtn.classList.contains("bottom-sheet-close")).toBe(true);
    expect(sheet.closeBtn.classList.contains("o_bottom_sheet_close")).toBe(false);

    const body = sheet.sheet.querySelector(".bottom-sheet-body");
    expect(body).not.toBeNull();
    expect(body?.textContent).toBe("Sheet Body Content");

    vi.spyOn(window, "getComputedStyle").mockReturnValue({
      animationName: "bottom-sheet-slide-out"
    } as unknown as CSSStyleDeclaration);
    sheet.slideOut();
    expect(sheet.sheet.classList.contains("bottom-sheet-dismissing")).toBe(true);
    expect(sheet.sheet.classList.contains("o_bottom_sheet_dismissing")).toBe(false);

    sheet.dispose();
  });

  /// (partial: Tab focus trapping operates only on topmost bottom sheet when stacked)
  test(": Tab focus trapping operates only on topmost sheet when two sheets are stacked", () => {
    const content1 = document.createElement("div");
    const b1 = document.createElement("button");
    b1.textContent = "B1";
    content1.append(b1);
    const sheet1 = new BottomSheet({ title: "Sheet 1", content: content1 });

    const content2 = document.createElement("div");
    const b2a = document.createElement("button");
    b2a.textContent = "B2A";
    const b2b = document.createElement("button");
    b2b.textContent = "B2B";
    content2.append(b2a, b2b);
    const sheet2 = new BottomSheet({ title: "Sheet 2", content: content2 });

    sheet1.openSheet(host);
    sheet2.openSheet(host);

    b2a.focus();
    expect(document.activeElement).toBe(b2a);

    // Tab keydown inside sheet2 should NOT be captured or prevented by sheet1
    const tabEvent = new KeyboardEvent("keydown", { key: "Tab", bubbles: true, cancelable: true });
    b2a.dispatchEvent(tabEvent);

    // sheet1 must not call preventDefault on ordinary Tab inside sheet2
    expect(tabEvent.defaultPrevented).toBe(false);

    sheet2.dispose();
    sheet1.dispose();
  });

  /// (partial: nested popup inside bottom sheet takes Escape and does not close sheet)
  test(": nested popup inside bottom sheet takes Escape and does not close sheet", () => {
    window.matchMedia = vi.fn().mockReturnValue({
      matches: true,
      media: "(prefers-reduced-motion: reduce)",
      onchange: null,
      addListener: vi.fn(),
      removeListener: vi.fn(),
      addEventListener: vi.fn(),
      removeEventListener: vi.fn(),
      dispatchEvent: vi.fn()
    } as unknown as MediaQueryList);

    const content = document.createElement("div");
    const input = document.createElement("input");
    input.setAttribute("role", "combobox");
    input.setAttribute("aria-expanded", "true");
    const popup = document.createElement("div");
    popup.className = "autocomplete-dropdown";
    content.append(input, popup);

    let sheetClosed = false;
    const sheet = new BottomSheet({
      title: "Sheet with popup",
      content,
      onClose: () => { sheetClosed = true; }
    });
    sheet.openSheet(host);
    input.focus();

    // Escape with nested popup open should NOT close the bottom sheet
    const escEvent = new KeyboardEvent("keydown", { key: "Escape", bubbles: true, cancelable: true });
    input.dispatchEvent(escEvent);

    expect(sheet.isOpen).toBe(true);
    expect(sheetClosed).toBe(false);

    // Close the nested popup
    input.setAttribute("aria-expanded", "false");
    popup.remove();

    // Next Escape closes the sheet
    const escEvent2 = new KeyboardEvent("keydown", { key: "Escape", bubbles: true, cancelable: true });
    input.dispatchEvent(escEvent2);

    expect(sheet.isOpen).toBe(false);
    expect(sheetClosed).toBe(true);

    sheet.dispose();
  });

  test(": Bottom sheet over mobile drawer or home menu handles Escape in capture phase without closing the underlying drawer or home menu", () => {
    window.matchMedia = vi.fn().mockReturnValue({
      matches: true,
      media: "(prefers-reduced-motion: reduce)",
      onchange: null,
      addListener: vi.fn(),
      removeListener: vi.fn(),
      addEventListener: vi.fn(),
      removeEventListener: vi.fn(),
      dispatchEvent: vi.fn()
    } as unknown as MediaQueryList);

    // 1. BottomSheet over a drawer-like listener on window (bubbling)
    let drawerClosed = false;
    const drawerEscapeListener = (e: KeyboardEvent) => {
      if (e.key === "Escape") drawerClosed = true;
    };
    window.addEventListener("keydown", drawerEscapeListener);

    let sheetClosed = false;
    const sheet = new BottomSheet({
      title: "Action Sheet",
      content: document.createElement("div"),
      onClose: () => { sheetClosed = true; }
    });
    sheet.openSheet(host);

    window.dispatchEvent(new KeyboardEvent("keydown", { key: "Escape" }));
    expect(sheet.isOpen).toBe(false);
    expect(sheetClosed).toBe(true);
    // Underlying drawer must NOT have closed
    expect(drawerClosed).toBe(false);

    window.removeEventListener("keydown", drawerEscapeListener);
    sheet.dispose();

    // 2. BottomSheet over HomeMenu
    let homeClosed = false;
    const home = new HomeMenu({
      container: host,
      apps: [{ id: 1, label: "Contacts" }],
      hasBackgroundAction: () => true,
      onOpenApp: () => {},
      onClose: () => { homeClosed = true; }
    });
    home.open();
    expect(home.isOpen).toBe(true);

    let overlaySheetClosed = false;
    const overlaySheet = new BottomSheet({
      title: "Nested Sheet",
      content: document.createElement("div"),
      onClose: () => { overlaySheetClosed = true; }
    });
    overlaySheet.openSheet(host);
    expect(overlaySheet.isOpen).toBe(true);

    // Escape should close the overlay sheet, leaving HomeMenu open
    window.dispatchEvent(new KeyboardEvent("keydown", { key: "Escape" }));
    expect(overlaySheet.isOpen).toBe(false);
    expect(overlaySheetClosed).toBe(true);
    expect(home.isOpen).toBe(true);
    expect(homeClosed).toBe(false);

    // Next Escape closes HomeMenu
    window.dispatchEvent(new KeyboardEvent("keydown", { key: "Escape" }));
    expect(home.isOpen).toBe(false);
    expect(homeClosed).toBe(true);

    overlaySheet.dispose();
    home.dispose();
  });

  /// (partial: the standalone NavbarOverflow component, which the app shell does not use; the shell's row is fitted by fitNavSections)
  test(": Navbar overflow menu adapts to available width, moves overflowing entries to More dropdown, and preserves labels as data", async () => {
    const container = document.createElement("div");
    container.className = "app-sections";
    host.append(container);

    const item1 = document.createElement("button");
    item1.className = "nav-item";
    item1.textContent = "Orders";
    let item1Clicked = false;
    item1.addEventListener("click", () => { item1Clicked = true; });

    const item2 = document.createElement("button");
    item2.className = "nav-item";
    item2.textContent = "Invoices";

    const item3 = document.createElement("button");
    item3.className = "nav-item";
    item3.textContent = "Reporting";

    const item4 = document.createElement("button");
    item4.className = "nav-item";
    item4.textContent = "Configuration";

    container.append(item1, item2, item3, item4);

    const overflow = new NavbarOverflow(container, { moreMenuWidth: 40 });

    // Mock container width 170px, each item 60px -> items 1 and 2 fit (40 + 60 + 60 = 160 <= 170), items 3 and 4 overflow
    vi.spyOn(container, "getBoundingClientRect").mockReturnValue({
      width: 170,
      height: 40,
      top: 0,
      left: 0,
      bottom: 40,
      right: 170,
      x: 0,
      y: 0,
      toJSON: () => {}
    });

    for (const item of [item1, item2, item3, item4]) {
      vi.spyOn(item, "getBoundingClientRect").mockReturnValue({
        width: 60,
        height: 40,
        top: 0,
        left: 0,
        bottom: 40,
        right: 60,
        x: 0,
        y: 0,
        toJSON: () => {}
      });
    }

    overflow.adapt();

    expect(overflow.isOverflowing).toBe(true);
    expect(overflow.getMoreDropdown().classList.contains("nav-more-menu")).toBe(true);
    expect(overflow.getMoreDropdown().classList.contains("o_menu_sections_more")).toBe(false);
    const moreBtn = overflow.getMoreDropdown().querySelector("button");
    expect(moreBtn?.classList.contains("nav-dropdown-toggle")).toBe(true);
    expect(moreBtn?.classList.contains("o_nav_entry")).toBe(false);
    expect(overflow.getMoreDropdown().classList.contains("d-none")).toBe(false);
    // Trailing sections are moved to more dropdown and hidden from navbar
    expect(item3.classList.contains("d-none")).toBe(true);
    expect(item4.classList.contains("d-none")).toBe(true);

    const moreItems = overflow.getMoreList().querySelectorAll(".nav-more-item");
    expect(moreItems.length).toBeGreaterThan(0);
    // Labels remain data strings
    expect(moreItems[0]?.textContent).toBe("Reporting");

    // When space expands to 300px
    vi.spyOn(container, "getBoundingClientRect").mockReturnValue({
      width: 300,
      height: 40,
      top: 0,
      left: 0,
      bottom: 40,
      right: 300,
      x: 0,
      y: 0,
      toJSON: () => {}
    });

    overflow.adapt();
    expect(overflow.isOverflowing).toBe(false);
    expect(overflow.getMoreDropdown().classList.contains("d-none")).toBe(true);
    expect(item3.classList.contains("d-none")).toBe(false);
    expect(item4.classList.contains("d-none")).toBe(false);

    item1.click();
    expect(item1Clicked).toBe(true);

    overflow.dispose();
  });

  test("NavbarOverflow strips id attributes from cloned nodes and supports keyboard navigation and Escape", () => {
    const container = document.createElement("div");
    container.className = "navbar-sections";
    host.append(container);

    const item1 = document.createElement("button");
    item1.id = "section-item-1";
    item1.className = "nav-item";
    item1.textContent = "Home";

    const item2 = document.createElement("div");
    item2.id = "section-item-2";
    item2.className = "nav-item";
    const item2Child = document.createElement("span");
    item2Child.id = "section-item-2-child";
    item2Child.textContent = "Contacts";
    item2.append(item2Child);

    container.append(item1, item2);

    const overflow = new NavbarOverflow(container, { moreMenuWidth: 40 });

    vi.spyOn(container, "getBoundingClientRect").mockReturnValue({
      width: 80,
      height: 40,
      top: 0,
      left: 0,
      bottom: 40,
      right: 80,
      x: 0,
      y: 0,
      toJSON: () => {}
    });

    vi.spyOn(item1, "getBoundingClientRect").mockReturnValue({
      width: 50,
      height: 40,
      top: 0,
      left: 0,
      bottom: 40,
      right: 50,
      x: 0,
      y: 0,
      toJSON: () => {}
    });

    vi.spyOn(item2, "getBoundingClientRect").mockReturnValue({
      width: 50,
      height: 40,
      top: 0,
      left: 0,
      bottom: 40,
      right: 50,
      x: 0,
      y: 0,
      toJSON: () => {}
    });

    overflow.adapt();

    expect(overflow.isOverflowing).toBe(true);
    const moreList = overflow.getMoreList();
    const clonedItems = moreList.querySelectorAll<HTMLElement>(".nav-more-item");
    expect(clonedItems.length).toBe(2);

    // Assert id attributes were stripped from clone and its children
    expect(clonedItems[0]?.hasAttribute("id")).toBe(false);
    expect(clonedItems[1]?.hasAttribute("id")).toBe(false);
    expect(clonedItems[1]?.querySelector("#section-item-2-child")).toBeNull();
    // But original retains its id
    expect(item1.id).toBe("section-item-1");
    expect(item2.id).toBe("section-item-2");
    expect(item2Child.id).toBe("section-item-2-child");

    // Keyboard navigation & Escape
    const toggle = overflow.getMoreDropdown().querySelector<HTMLButtonElement>(".nav-dropdown-toggle")!;
    expect(toggle.getAttribute("aria-expanded")).toBe("false");
    expect(moreList.classList.contains("open")).toBe(false);

    toggle.focus();
    toggle.dispatchEvent(new KeyboardEvent("keydown", { key: "ArrowDown", bubbles: true }));

    expect(toggle.getAttribute("aria-expanded")).toBe("true");
    expect(moreList.classList.contains("open")).toBe(true);
    expect(document.activeElement).toBe(clonedItems[0]);

    moreList.dispatchEvent(new KeyboardEvent("keydown", { key: "Escape", bubbles: true }));
    expect(toggle.getAttribute("aria-expanded")).toBe("false");
    expect(moreList.classList.contains("open")).toBe(false);
    expect(document.activeElement).toBe(toggle);

    overflow.dispose();
  });

  test(": Mobile burger-menu lifetime, user profile, multi-company switcher, user menu entries, standalone share, and transition closure", async () => {
    let companySelected: number | null = null;
    let shareUrlInvoked = false;
    let profileActionInvoked = false;

    const panel = document.createElement("div");
    panel.id = "mobile-navigation-drawer";
    panel.className = "mobile-drawer hidden";

    const backdrop = document.createElement("div");
    backdrop.className = "mobile-drawer-backdrop hidden";

    const toggle = document.createElement("button");
    toggle.className = "burger-btn";

    const header = document.createElement("div");
    header.className = "mobile-drawer-header";
    const headerTitle = document.createElement("strong");
    headerTitle.textContent = "Inventory";
    const closeBtn = document.createElement("button");
    closeBtn.className = "drawer-close-btn";
    header.append(headerTitle, closeBtn);

    const sections = document.createElement("div");
    sections.className = "mobile-drawer-sections";
    const content = document.createElement("div");
    content.className = "mobile-drawer-content";
    content.append(sections);

    panel.append(header, content);
    host.append(toggle, backdrop, panel);

    const burger = new MobileBurgerMenu({
      panel,
      backdrop,
      toggle,
      isStandalone: true,
      onShareUrl: () => { shareUrlInvoked = true; },
      user: {
        userId: 42,
        name: "Admin User",
        allowedCompanies: [
          { id: 1, name: "San Francisco" },
          { id: 2, name: "Chicago" }
        ]
      },
      userMenuItems: [
        { id: "profile", label: "My Profile", action: () => { profileActionInvoked = true; } }
      ],
      onSelectCompany: (id) => { companySelected = id; }
    });

    // Initial state
    expect(isDisplayStandalone()).toBe(false);
    expect(burger.isOpen).toBe(false);
    expect(burger.currentApp).toBe("Inventory");

    // User profile rendered
    expect(panel.querySelector(".sidebar-topbar")).not.toBeNull();
    expect(panel.querySelector(".burger-user-badge")).not.toBeNull();
    expect(panel.querySelector(".burger-menu-username")?.textContent).toBe("Admin User");
    expect(panel.querySelector(".burger-menu-avatar")).not.toBeNull();
    // No avatar image exists on the server: initials like the top-bar user menu, and no image request.
    expect(panel.querySelector("img")).toBeNull();
    expect(panel.querySelector(".burger-menu-avatar")?.getAttribute("data-letter")).toBe("A");

    // Standalone share button rendered
    const shareBtn = panel.querySelector<HTMLButtonElement>(".sidebar-share");
    expect(shareBtn).not.toBeNull();
    shareBtn?.click();
    expect(shareUrlInvoked).toBe(true);

    // Multi-company menu rendered
    expect(panel.querySelector(".burger-company-menu")).not.toBeNull();
    const companyBtns = panel.querySelectorAll<HTMLButtonElement>(".company-item");
    expect(companyBtns.length).toBe(2);
    companyBtns[1]?.click();
    expect(companySelected).toBe(2);

    // User menu entries rendered
    expect(panel.querySelector(".burger-user-menu")).not.toBeNull();
    const userItemBtn = panel.querySelector<HTMLButtonElement>(".user-menu-item");
    expect(userItemBtn?.textContent).toBe("My Profile");
    userItemBtn?.click();
    expect(profileActionInvoked).toBe(true);

    // Ensure no o_ classes exist
    expect(panel.querySelector(".o_sidebar_topbar")).toBeNull();
    expect(panel.querySelector(".o_burger_user_badge")).toBeNull();
    expect(panel.querySelector(".o_burger_menu_username")).toBeNull();
    expect(panel.querySelector(".o_burger_menu_avatar")).toBeNull();
    expect(panel.querySelector(".o_sidebar_share")).toBeNull();
    expect(panel.querySelector(".o_burger_company_menu")).toBeNull();
    expect(panel.querySelector(".o_burger_user_menu")).toBeNull();

    // Open burger menu
    burger.open();
    expect(burger.isOpen).toBe(true);

    // Home transition closure: when home becomes active, burger closes and hides current app section
    burger.setHomeActive(true);
    expect(burger.isOpen).toBe(false);
    expect(burger.currentApp).toBeNull();

    // Action navigation update closes burger
    burger.open();
    expect(burger.isOpen).toBe(true);
    burger.onActionUpdate();
    expect(burger.isOpen).toBe(false);

    burger.dispose();
  });
});
