/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
import { getActiveBottomSheetCount } from "./bottom-sheet.js";

export interface HomeMenuApp {
  id: number;
  label: string;
  icon?: string | null;
  actionId?: number | null;
}

export interface HomeMenuOptions {
  container: HTMLElement;
  apps: HomeMenuApp[];
  hasBackgroundAction: () => boolean;
  onOpenApp: (app: HomeMenuApp) => void;
  onClose: () => void;
  onAllAppsCloseSidebar?: () => void;
}

export class HomeMenu {
  private _isOpen = false;
  /** What had focus when the menu opened; it gets focus back on close. */
  private opener: HTMLElement | null = null;
  private focusedIndex: number | null = null;
  private displayedApps: HomeMenuApp[] = [];
  private readonly appTiles: HTMLElement[] = [];
  private searchInput: HTMLInputElement;
  private grid: HTMLElement;
  private overlay: HTMLElement;

  constructor(private readonly options: HomeMenuOptions) {
    this.displayedApps = [...options.apps];

    this.overlay = document.createElement("div");
    this.overlay.className = "home-menu-overlay hidden";

    const content = document.createElement("div");
    content.className = "home-menu-container";

    this.searchInput = document.createElement("input");
    this.searchInput.type = "search";
    this.searchInput.className = "home-menu-search";
    this.searchInput.placeholder = "Search apps…";
    this.searchInput.setAttribute("aria-label", "Search apps");
    this.searchInput.setAttribute("role", "combobox");
    this.searchInput.setAttribute("aria-autocomplete", "list");
    this.searchInput.setAttribute("aria-controls", "home-menu-grid");
    this.searchInput.setAttribute("aria-expanded", "false");

    this.grid = document.createElement("div");
    this.grid.id = "home-menu-grid";
    this.grid.className = "home-grid";
    this.grid.setAttribute("role", "listbox");

    content.append(this.searchInput, this.grid);
    this.overlay.append(content);
    this.options.container.append(this.overlay);

    this.renderTiles();
    this.bindEvents();
  }

  get isOpen(): boolean {
    return this._isOpen;
  }

  get isInApp(): boolean {
    return !this._isOpen;
  }

  get currentFocusedIndex(): number | null {
    return this.focusedIndex;
  }

  get currentDisplayedApps(): HomeMenuApp[] {
    return [...this.displayedApps];
  }

  getSearchInput(): HTMLInputElement {
    return this.searchInput;
  }

  getGrid(): HTMLElement {
    return this.grid;
  }

  getOverlay(): HTMLElement {
    return this.overlay;
  }

  setApps(apps: HomeMenuApp[]): void {
    this.options.apps = apps;
    this.filterApps(this.searchInput.value);
  }

  private renderTiles(): void {
    this.grid.replaceChildren();
    this.appTiles.length = 0;

    for (let i = 0; i < this.displayedApps.length; i++) {
      const app = this.displayedApps[i]!;
      const tile = document.createElement("button");
      tile.type = "button";
      tile.className = "app-tile";
      tile.id = `home-app-tile-${app.id}`;
      tile.dataset["menuId"] = String(app.id);
      tile.tabIndex = -1;
      tile.setAttribute("role", "option");

      const iconBox = document.createElement("div");
      iconBox.className = "app-icon";
      iconBox.setAttribute("aria-hidden", "true");

      const caption = document.createElement("div");
      caption.className = "app-caption";
      caption.textContent = app.label;

      tile.append(iconBox, caption);

      tile.addEventListener("click", () => {
        this.options.onOpenApp(app);
      });

      this.grid.append(tile);
      this.appTiles.push(tile);
    }

    if (this.focusedIndex !== null && this.focusedIndex >= this.displayedApps.length) {
      this.focusedIndex = this.displayedApps.length > 0 ? 0 : null;
    }
    this.applyFocus();
  }

  private filterApps(term: string): void {
    const query = term.toLowerCase().trim();
    if (!query) {
      this.displayedApps = [...this.options.apps];
    } else {
      this.displayedApps = this.options.apps.filter(app =>
        app.label.toLowerCase().includes(query)
      );
    }
    this.renderTiles();
  }

  private bindEvents(): void {
    this.searchInput.addEventListener("input", () => {
      this.filterApps(this.searchInput.value);
    });

    window.addEventListener("keydown", this.onKeyDown);
  }

  open(): void {
    if (this._isOpen) return;
    this._isOpen = true;
    this.searchInput.setAttribute("aria-expanded", "true");
    this.opener = document.activeElement instanceof HTMLElement ? document.activeElement : null;
    this.overlay.classList.remove("hidden");
    this.focusedIndex = this.displayedApps.length > 0 ? 0 : null;
    this.applyFocus();
  }

  close(): void {
    if (!this._isOpen) return;
    this._isOpen = false;
    this.searchInput.setAttribute("aria-expanded", "false");
    this.overlay.classList.add("hidden");
    this.focusedIndex = null;
    this.applyFocus();
    const opener = this.opener;
    this.opener = null;
    if (opener?.isConnected) opener.focus();
    this.options.onClose?.();
  }

  toggle(targetState?: boolean): void {
    const next = targetState !== undefined ? targetState : !this._isOpen;
    if (next) this.open();
    else this.close();
  }

  onAllAppsBtnClick(): void {
    this.open();
    this.options.onAllAppsCloseSidebar?.();
  }

  updateAppToggle(toggleBtn: HTMLButtonElement, navbarEl?: HTMLElement): void {
    const hasBg = this.options.hasBackgroundAction();
    if (this._isOpen) {
      if (hasBg) {
        toggleBtn.classList.remove("hidden");
        toggleBtn.classList.add("menu-toggle-back");
        toggleBtn.title = "Previous view";
        toggleBtn.setAttribute("aria-label", "Previous view");
      } else {
        toggleBtn.classList.add("hidden");
        toggleBtn.classList.remove("menu-toggle-back");
        toggleBtn.title = "Home menu";
        toggleBtn.setAttribute("aria-label", "Home menu");
      }
    } else {
      toggleBtn.classList.remove("hidden", "menu-toggle-back");
      toggleBtn.title = "Home menu";
      toggleBtn.setAttribute("aria-label", "Home menu");
    }

    if (navbarEl) {
      const brand = navbarEl.querySelector(".brand");
      const brandIcon = navbarEl.querySelector(".brand-icon");
      const subMenus = navbarEl.querySelector(".app-sub-menus, .nav-sections");
      const breadcrumb = navbarEl.querySelector(".breadcrumbs");

      brand?.classList.toggle("hidden", this._isOpen);
      brandIcon?.classList.toggle("hidden", this._isOpen);
      subMenus?.classList.toggle("hidden", this._isOpen);
      breadcrumb?.classList.toggle("hidden", this._isOpen);
    }
  }

  getColumns(): number {
    const gridWidth = this.grid.getBoundingClientRect().width;
    const tileWidth = this.appTiles[0]?.getBoundingClientRect().width || 120;
    if (gridWidth > 0 && tileWidth > 0) {
      return Math.max(1, Math.floor(gridWidth / tileWidth));
    }
    return 6;
  }

  updateFocusedIndex(cmd: "previousColumn" | "nextColumn" | "previousLine" | "nextLine" | "previousElem" | "nextElem"): void {
    const nbrApps = this.displayedApps.length;
    if (nbrApps === 0) return;

    if (this.focusedIndex === null) {
      this.focusedIndex = 0;
      this.applyFocus();
      return;
    }

    const maxIconNumber = Math.max(1, this.getColumns());
    const lineNumber = Math.ceil(nbrApps / maxIconNumber);
    const currentLine = Math.ceil((this.focusedIndex + 1) / maxIconNumber);
    let newIndex = this.focusedIndex;

    switch (cmd) {
      case "previousElem":
        newIndex = this.focusedIndex - 1;
        break;
      case "nextElem":
        newIndex = this.focusedIndex + 1;
        break;
      case "previousColumn":
        if (this.focusedIndex % maxIconNumber !== 0) {
          newIndex = this.focusedIndex - 1;
        } else {
          newIndex = this.focusedIndex + Math.min(nbrApps - 1 - this.focusedIndex, maxIconNumber - 1);
        }
        break;
      case "nextColumn":
        if (this.focusedIndex === nbrApps - 1 || (this.focusedIndex + 1) % maxIconNumber === 0) {
          newIndex = (currentLine - 1) * maxIconNumber;
        } else {
          newIndex = this.focusedIndex + 1;
        }
        break;
      case "previousLine":
        if (currentLine === 1) {
          newIndex = this.focusedIndex + (lineNumber - 1) * maxIconNumber;
          if (newIndex > nbrApps - 1) {
            newIndex = nbrApps - 1;
          }
        } else {
          newIndex = this.focusedIndex - maxIconNumber;
        }
        break;
      case "nextLine":
        if (currentLine === lineNumber) {
          newIndex = this.focusedIndex % maxIconNumber;
        } else {
          newIndex = this.focusedIndex + Math.min(maxIconNumber, nbrApps - 1 - this.focusedIndex);
        }
        break;
    }

    if (newIndex < 0) {
      newIndex = nbrApps - 1;
    } else if (newIndex >= nbrApps) {
      newIndex = 0;
    }

    this.focusedIndex = newIndex;
    this.applyFocus();
  }

  private applyFocus(): void {
    const activeApp = this.focusedIndex !== null ? this.displayedApps[this.focusedIndex] : null;
    const activeId = activeApp ? `home-app-tile-${activeApp.id}` : null;
    if (activeId) {
      this.searchInput.setAttribute("aria-activedescendant", activeId);
    } else {
      this.searchInput.removeAttribute("aria-activedescendant");
    }

    const inSearchInput = document.activeElement === this.searchInput;

    for (let i = 0; i < this.appTiles.length; i++) {
      const tile = this.appTiles[i]!;
      const isFocused = i === this.focusedIndex;
      tile.classList.toggle("is-focused", isFocused);
      tile.tabIndex = isFocused ? 0 : -1;
      tile.setAttribute("aria-selected", isFocused ? "true" : "false");
      if (isFocused && this._isOpen && !inSearchInput) {
        tile.focus();
      }
    }
  }

  private readonly onKeyDown = (event: KeyboardEvent): void => {
    if (!this._isOpen) return;

    if (event.key === "Escape") {
      if (getActiveBottomSheetCount() > 0) return;
      event.preventDefault();
      this.close();
      return;
    }

    if (event.key === "Enter") {
      if (this.focusedIndex !== null && this.displayedApps[this.focusedIndex]) {
        event.preventDefault();
        this.options.onOpenApp(this.displayedApps[this.focusedIndex]!);
      }
      return;
    }

    if (event.key === "ArrowDown") {
      event.preventDefault();
      this.updateFocusedIndex("nextLine");
      return;
    }
    if (event.key === "ArrowRight") {
      event.preventDefault();
      this.updateFocusedIndex("nextColumn");
      return;
    }
    if (event.key === "ArrowUp") {
      event.preventDefault();
      this.updateFocusedIndex("previousLine");
      return;
    }
    if (event.key === "ArrowLeft") {
      event.preventDefault();
      this.updateFocusedIndex("previousColumn");
      return;
    }
    if (event.key === "Tab") {
      event.preventDefault();
      this.updateFocusedIndex(event.shiftKey ? "previousElem" : "nextElem");
      return;
    }

    // Key typing outside input or textarea shifts focus to search input
    if (
      event.key.length === 1 &&
      !event.ctrlKey &&
      !event.altKey &&
      !event.metaKey &&
      document.activeElement !== this.searchInput &&
      !["INPUT", "TEXTAREA"].includes(document.activeElement?.tagName ?? "")
    ) {
      this.searchInput.focus();
    }
  };

  dispose(): void {
    window.removeEventListener("keydown", this.onKeyDown);
    this.overlay.remove();
  }
}
