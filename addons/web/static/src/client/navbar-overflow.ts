/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
import { attachMenuDropdown, type MenuDropdownController } from "./menu-dropdown.js";

/**
 * How many trailing top-bar entries must move into the "More" dropdown so that the fixed lead (the app name), the
 * entries that stay and the More button fit in `available`. Widths are in pixels; `gap` is the space between two items.
 */
export function navOverflowCount(lead: number, entries: readonly number[], available: number, more: number, gap: number): number {
  const total = entries.reduce((sum, width) => sum + gap + width, lead);
  if (total <= available) return 0;
  let used = lead + gap + more, kept = 0;
  for (const width of entries) {
    if (used + gap + width > available) break;
    used += gap + width; kept += 1;
  }
  return entries.length - kept;
}

export interface NavbarSectionItem {
  id: string | number;
  label: string;
  element: HTMLElement;
  action?: () => void;
  children?: NavbarSectionItem[];
}

export interface NavbarOverflowOptions {
  moreMenuWidth?: number; // default: 46
}

export class NavbarOverflow {
  private disposed = false;
  private readonly moreMenuWidth: number;
  private moreDropdown: HTMLElement;
  private moreToggle: HTMLButtonElement;
  private moreList: HTMLElement;
  private overflowingElements: HTMLElement[] = [];
  private dropdown: MenuDropdownController;

  constructor(
    private readonly container: HTMLElement,
    options?: NavbarOverflowOptions
  ) {
    this.moreMenuWidth = options?.moreMenuWidth ?? 46;

    this.moreDropdown = document.createElement("div");
    this.moreDropdown.className = "nav-more-menu d-none";

    this.moreToggle = document.createElement("button");
    this.moreToggle.type = "button";
    this.moreToggle.className = "nav-dropdown-toggle";
    this.moreToggle.textContent = "More";

    this.moreList = document.createElement("div");
    this.moreList.className = "nav-dropdown-menu";

    this.moreDropdown.append(this.moreToggle, this.moreList);
    this.container.append(this.moreDropdown);

    this.dropdown = attachMenuDropdown({
      trigger: this.moreToggle,
      menu: this.moreList,
      mode: "class-open"
    });

    window.addEventListener("resize", this.onResize);
  }

  get isOverflowing(): boolean {
    return this.overflowingElements.length > 0;
  }

  get overflowCount(): number {
    return this.overflowingElements.length;
  }

  getMoreDropdown(): HTMLElement {
    return this.moreDropdown;
  }

  getMoreList(): HTMLElement {
    return this.moreList;
  }

  closeMore(): void {
    this.dropdown.close(false);
  }

  private readonly onResize = (): void => {
    this.adapt();
  };

  adapt(): void {
    if (this.disposed) return;

    // Get all direct child elements except the moreDropdown itself
    const sections = [...this.container.children].filter(
      el => el instanceof HTMLElement && el !== this.moreDropdown
    ) as HTMLElement[];

    if (sections.length === 0) {
      this.moreDropdown.classList.add("d-none");
      this.moreList.replaceChildren();
      this.overflowingElements = [];
      this.dropdown.refresh();
      return;
    }

    // Unhide everything to measure true dimensions
    for (const section of sections) {
      section.classList.remove("d-none", "hidden");
    }

    const availableWidth = this.container.getBoundingClientRect().width;
    const totalWidth = sections.reduce(
      (sum, s) => sum + s.getBoundingClientRect().width,
      0
    );

    if (availableWidth > 0 && totalWidth > availableWidth) {
      let currentWidth = this.moreMenuWidth;
      const overflowing: HTMLElement[] = [];

      for (let i = 0; i < sections.length; i++) {
        const s = sections[i]!;
        const width = s.getBoundingClientRect().width || s.offsetWidth;
        if (currentWidth + width > availableWidth) {
          overflowing.push(...sections.slice(i));
          break;
        }
        currentWidth += width;
      }

      this.overflowingElements = overflowing;
      this.moreList.replaceChildren();

      for (const section of overflowing) {
        section.classList.add("d-none");

        // Clone item for dropdown display while preserving interaction, stripping all id attributes
        const clone = section.cloneNode(true) as HTMLElement;
        clone.removeAttribute("id");
        for (const el of clone.querySelectorAll("[id]")) {
          el.removeAttribute("id");
        }
        clone.classList.remove("d-none", "hidden");
        clone.classList.add("nav-more-item");
        if (!clone.hasAttribute("role")) {
          clone.setAttribute("role", "menuitem");
        }

        // Link click to original element
        clone.addEventListener("click", (e) => {
          this.closeMore();
          section.dispatchEvent(new MouseEvent("click", e));
        });
        this.moreList.append(clone);
      }

      this.moreDropdown.classList.remove("d-none");
      this.dropdown.refresh();
    } else {
      // Everything fits
      this.overflowingElements = [];
      this.moreDropdown.classList.add("d-none");
      this.moreList.replaceChildren();
      this.closeMore();
      this.dropdown.refresh();
    }
  }

  dispose(): void {
    if (this.disposed) return;
    this.disposed = true;
    window.removeEventListener("resize", this.onResize);
    this.dropdown.dispose();
    this.moreDropdown.remove();
  }
}
