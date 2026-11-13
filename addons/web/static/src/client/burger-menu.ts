/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
import { MobileDrawer } from "./mobile-drawer.js";

export interface BurgerCompany {
  id: number;
  name: string;
}

export interface BurgerUser {
  userId: number;
  name: string;
  allowedCompanies: BurgerCompany[];
}

export interface BurgerUserMenuItem {
  id: string;
  label: string;
  action: () => void;
}

export interface MobileBurgerMenuOptions {
  panel: HTMLElement;
  backdrop: HTMLElement;
  toggle: HTMLButtonElement;
  user?: BurgerUser;
  isStandalone?: boolean;
  onShareUrl?: () => void;
  userMenuItems?: BurgerUserMenuItem[];
  onSelectCompany?: (companyId: number) => void;
  onAllApps?: () => void;
  focusDesktop?: () => void;
}

export function isDisplayStandalone(): boolean {
  if (typeof window === "undefined") return false;
  const nav = window.navigator as { standalone?: boolean };
  if (nav?.standalone === true) return true;
  return window.matchMedia?.("(display-mode: standalone)")?.matches === true;
}

export class MobileBurgerMenu {
  readonly drawer: MobileDrawer;
  private readonly panel: HTMLElement;
  private readonly backdrop: HTMLElement;
  private readonly toggleBtn: HTMLButtonElement;
  private readonly options: MobileBurgerMenuOptions;
  private isHomeActive = false;

  constructor(options: MobileBurgerMenuOptions) {
    this.options = options;
    this.panel = options.panel;
    this.backdrop = options.backdrop;
    this.toggleBtn = options.toggle;

    this.drawer = new MobileDrawer(this.panel, this.backdrop, this.toggleBtn, options.focusDesktop);

    this.renderHeader();
    this.renderUserAndCompanySections();
  }

  get isOpen(): boolean {
    return this.drawer.isOpen;
  }

  get currentApp(): string | null {
    if (this.isHomeActive) return null;
    return this.panel.querySelector(".mobile-drawer-header strong")?.textContent ?? null;
  }

  open(): void {
    if (!this.isOpen) {
      this.drawer.toggleOpen();
    }
  }

  close(): void {
    if (this.isOpen) {
      this.drawer.close();
    }
  }

  toggle(): void {
    this.drawer.toggleOpen();
  }

  setHomeActive(active: boolean): void {
    this.isHomeActive = active;
    if (active && this.isOpen) {
      this.close();
    }
    const appSection = this.panel.querySelector<HTMLElement>(".mobile-drawer-sections");
    if (appSection) {
      appSection.style.display = active ? "none" : "";
    }
  }

  onActionUpdate(): void {
    if (this.isOpen) {
      this.close();
    }
  }

  private renderHeader(): void {
    const existingTopbar = this.panel.querySelector(".sidebar-topbar");
    if (existingTopbar) return;

    const topbar = document.createElement("div");
    topbar.className = "sidebar-topbar";

    const userGroup = document.createElement("div");
    userGroup.className = "burger-user-badge";

    // Initials like the top-bar user menu: the server has no avatar image field or route.
    const avatar = document.createElement("div");
    avatar.className = "user-avatar burger-menu-avatar";
    avatar.setAttribute("aria-hidden", "true");
    avatar.setAttribute("data-letter", (this.options.user?.name ?? "").trim().charAt(0).toUpperCase() || "U");

    const username = document.createElement("span");
    username.className = "burger-menu-username";
    username.textContent = this.options.user?.name ?? "User";

    userGroup.append(avatar, username);

    const controls = document.createElement("div");
    controls.className = "sidebar-controls";

    const standalone = this.options.isStandalone ?? isDisplayStandalone();
    if (standalone && this.options.onShareUrl) {
      const shareBtn = document.createElement("button");
      shareBtn.type = "button";
      shareBtn.className = "sidebar-share btn border-0";
      shareBtn.setAttribute("data-icon", "family_history");
      shareBtn.setAttribute("aria-label", "Share URL");
      shareBtn.title = "Share URL";
      shareBtn.addEventListener("click", (e) => {
        e.stopPropagation();
        this.options.onShareUrl?.();
      });
      controls.append(shareBtn);
    }

    const closeBtn = this.panel.querySelector<HTMLButtonElement>(".drawer-close-btn");
    if (closeBtn) {
      closeBtn.classList.add("sidebar-close");
      closeBtn.setAttribute("data-icon", "close_small");
      controls.append(closeBtn);
    }

    topbar.append(userGroup, controls);
    this.panel.prepend(topbar);
  }

  private renderUserAndCompanySections(): void {
    let content = this.panel.querySelector<HTMLElement>(".mobile-drawer-content");
    if (!content) {
      content = document.createElement("div");
      content.className = "mobile-drawer-content";
      this.panel.append(content);
    }

    // Company Switcher Menu when multiple companies are available
    const allowed = this.options.user?.allowedCompanies ?? [];
    if (allowed.length > 1) {
      const existingCompanyMenu = content.querySelector(".burger-company-menu");
      if (!existingCompanyMenu) {
        const companySection = document.createElement("div");
        companySection.className = "burger-company-menu mobile-switch-company-menu";

        const title = document.createElement("h3");
        title.className = "drawer-section-title";
        title.textContent = "Companies";
        companySection.append(title);

        for (const company of allowed) {
          const btn = document.createElement("button");
          btn.type = "button";
          btn.className = "drawer-item company-item";
          btn.textContent = company.name;
          btn.dataset["companyId"] = String(company.id);
          btn.addEventListener("click", () => {
            this.close();
            this.options.onSelectCompany?.(company.id);
          });
          companySection.append(btn);
        }
        content.append(companySection);
      }
    }

    // Burger User Menu entries
    const items = this.options.userMenuItems ?? [];
    if (items.length > 0) {
      const existingUserMenu = content.querySelector(".burger-user-menu");
      if (!existingUserMenu) {
        const userSection = document.createElement("div");
        userSection.className = "burger-user-menu";

        const title = document.createElement("h3");
        title.className = "drawer-section-title";
        title.textContent = "Account";
        userSection.append(title);

        for (const item of items) {
          const btn = document.createElement("button");
          btn.type = "button";
          btn.className = "drawer-item user-menu-item";
          btn.textContent = item.label;
          btn.dataset["itemId"] = item.id;
          btn.addEventListener("click", () => {
            this.close();
            item.action();
          });
          userSection.append(btn);
        }
        content.append(userSection);
      }
    }
  }

  dispose(): void {
    this.drawer.dispose();
  }
}
