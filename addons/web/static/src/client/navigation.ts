/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
import { button, el } from "./dom.js";
import { icon } from "./icons.js";
import { reportFailure } from "./failure-owner.js";
import type { Route } from "./route.js";
import { attachMenuDropdown } from "./menu-dropdown.js";

/** A single descriptor in the breadcrumb trail. */
export interface BreadcrumbItem {
  readonly id: string;
  readonly title: string;
  readonly route: Route;
  readonly url: string;
  readonly isFormView?: boolean;
  onSelected(): Promise<void>;
}

export type NavigationTarget = "current" | "main" | "new";
export type TransitionPhase = "parse" | "guard" | "resolve" | "ready" | "commit";

export type TransitionResult =
  | { readonly kind: "accepted"; readonly route: Route }
  | { readonly kind: "cancelled"; readonly reason?: string }
  | { readonly kind: "failed"; readonly error: unknown };

/** Bounded stack of breadcrumb entries for the active window action hierarchy. */
export class BreadcrumbTrail {
  private stack: BreadcrumbItem[] = [];
  private readonly maxDepth: number;

  constructor(maxDepth = 32) {
    this.maxDepth = maxDepth;
  }

  get items(): readonly BreadcrumbItem[] {
    return this.stack;
  }

  get current(): BreadcrumbItem | undefined {
    return this.stack.at(-1);
  }

  get previous(): BreadcrumbItem | undefined {
    return this.stack.at(-2);
  }

  get collapsed(): readonly BreadcrumbItem[] {
    if (this.stack.length <= 2) return [];
    return this.stack.slice(0, -2).reverse();
  }

  push(item: BreadcrumbItem): void {
    this.stack.push(item);
    if (this.stack.length > this.maxDepth) {
      this.stack.shift();
    }
  }

  clear(): void {
    this.stack = [];
  }

  truncateTo(id: string): void {
    const idx = this.stack.findIndex(item => item.id === id);
    if (idx >= 0) {
      this.stack = this.stack.slice(0, idx + 1);
    }
  }

  replaceCurrent(item: BreadcrumbItem): void {
    if (this.stack.length > 0) {
      this.stack[this.stack.length - 1] = item;
    } else {
      this.push(item);
    }
  }
}

/** Renders breadcrumbs into container. */
export function renderBreadcrumbs(
  container: HTMLElement,
  trail: BreadcrumbTrail,
  options?: {
    isSmall?: boolean;
    onNavigate?(item: BreadcrumbItem): Promise<void>;
    /** Receives a failed navigation; without it the failure surfaces as a global error. */
    report?(error: unknown): void;
  }
): void {
  container.replaceChildren();
  const go = (item: BreadcrumbItem): void => reportFailure(
    (async () => { await (options?.onNavigate ? options.onNavigate(item) : item.onSelected()); })(),
    { report: options?.report }
  );
  const items = trail.items;
  if (!items.length) return;

  const current = trail.current!;
  const previous = trail.previous;
  const collapsed = trail.collapsed;
  const isSmall = options?.isSmall ?? (document.documentElement.dataset["mobile"] === "true");

  if (isSmall) {
    // Mobile view: single back button if previous exists, plus current title
    if (previous) {
      const backBtn = button("", () => {
        go(previous);
      }, "breadcrumb-back o_back_button link");
      backBtn.setAttribute("aria-label", `Back to ${previous.title}`);
      backBtn.append(icon("arrow-left"));
      container.append(backBtn);
    }
    const currentSpan = el("span", current.title, "breadcrumb-current form-breadcrumb-record");
    currentSpan.setAttribute("aria-current", "page");
    container.append(currentSpan);
    return;
  }

  // Desktop view
  if (!previous && !collapsed.length) {
    // Single item in trail (e.g. list view)
    const title = el("h1", current.title, "breadcrumb-current");
    title.setAttribute("aria-current", "page");
    container.append(title);
    return;
  }

  const trailContainer = el("div", undefined, "form-breadcrumb-path");

  // Collapsed history dropdown for entries older than penultimate
  if (collapsed.length > 0) {
    const dropdownWrap = el("div", undefined, "breadcrumb-collapsed-dropdown");
    const toggle = button("…", () => {}, "breadcrumb-collapsed-toggle btn btn-sm");
    toggle.setAttribute("aria-label", "Older breadcrumbs");

    const menu = el("div", undefined, "breadcrumb-dropdown-menu");

    const dropdown = attachMenuDropdown({
      trigger: toggle,
      menu,
      mode: "class-open"
    });

    for (const crumb of collapsed) {
      const itemBtn = button(crumb.title, () => {
        dropdown.close(false);
        go(crumb);
      }, "breadcrumb-dropdown-item dropdown-item");
      itemBtn.setAttribute("role", "menuitem");
      itemBtn.setAttribute("data-tooltip", `Back to "${crumb.title}"`);
      menu.append(itemBtn);
    }
    dropdownWrap.append(toggle, menu);
    trailContainer.append(dropdownWrap, el("span", "›", "form-breadcrumb-divider"));
  }

  // Penultimate entry: previous breadcrumb link
  if (previous) {
    const prevWrap = el("span", undefined, "breadcrumb-previous o_back_button");
    prevWrap.setAttribute("data-hotkey", "b");
    const link = button(previous.title, () => {
      go(previous);
    }, "breadcrumb-back link");
    link.prepend(icon("arrow-left"));
    link.setAttribute("data-tooltip", `Back to "${previous.title}"`);
    prevWrap.append(link);
    trailContainer.append(prevWrap, el("span", "›", "form-breadcrumb-divider"));
  }

  // Current entry
  const currentCrumb = el("span", current.title, "breadcrumb-current form-breadcrumb-record");
  currentCrumb.setAttribute("aria-current", "page");
  trailContainer.append(currentCrumb);
  container.append(trailContainer);
}
