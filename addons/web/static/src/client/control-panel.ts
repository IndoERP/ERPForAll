/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
import { Component, mount, unmount } from "@erpforall/elang";
import { clientTemplates, controlPanelExtensions } from "./registry.js";
import { Pager, type PagerProps } from "./pager.js";
import { ViewSwitcher, type ViewSwitcherProps } from "./view-switcher.js";
import { ActionMenus, type ActionMenuItem, type ActionMenuEnvironment } from "./action-menus.js";

export interface ControlPanelDisplayFlags {
  showSearch?: boolean;
  showButtons?: boolean;
  showPager?: boolean;
  showViewSwitcher?: boolean;
  adaptToScroll?: boolean;
  layout?: "none" | "compact" | "normal";
}

export interface ControlPanelAdapter {
  title?: string;
  model?: string;
  breadcrumbs?: HTMLElement;
  searchArea?: HTMLElement;
  actionsArea?: HTMLElement;
  pagerProps?: PagerProps | (() => PagerProps | undefined);
  viewSwitcherProps?: ViewSwitcherProps | (() => ViewSwitcherProps | undefined);
  display?: ControlPanelDisplayFlags;
  actionMenuItems?: readonly ActionMenuItem[];
  actionMenuEnv?: ActionMenuEnvironment;
}

export interface ControlPanelProps {
  title: string;
  model: string;
}

export class ControlPanel extends Component<ControlPanelProps> {
  static override template = "web.HumanControlPanel";
}

export interface ControlPanelMountOptions {
  scrollingElement?: HTMLElement;
  isSmall?: boolean;
}

export interface ControlPanelHandle {
  panel: HTMLElement;
  actions: HTMLElement;
  pager?: Pager;
  viewSwitcher?: ViewSwitcher;
  onScroll?: () => void;
  dispose(): void;
  update(adapter: ControlPanelAdapter): void;
}

export async function mountControlPanel(
  host: HTMLElement,
  adapter: ControlPanelAdapter,
  report: (error: unknown) => void,
  options?: ControlPanelMountOptions
): Promise<ControlPanelHandle> {
  const root = document.createElement("div");
  // the mount root is exactly as tall as the panel: it, not the panel inside it, is what stays in view (client.css)
  root.className = "control-panel-host";
  host.append(root);
  const mounted: HTMLElement[] = [root];
  let disposed = false;

  const props: ControlPanelProps = {
    title: adapter.title || "",
    model: adapter.model || ""
  };

  try {
    await mount(ControlPanel, root, { templates: clientTemplates(), props, dev: false, onError: report });
    const panel = root.querySelector<HTMLElement>("[data-control-panel]");
    const actions = root.querySelector<HTMLElement>("[data-control-actions]");
    const anchor = root.querySelector<HTMLElement>("[data-control-extension]");
    const breadcrumbsSlot = root.querySelector<HTMLElement>("[data-control-breadcrumbs]");
    const searchSlot = root.querySelector<HTMLElement>("[data-control-search]");
    const toolsSlot = root.querySelector<HTMLElement>("[data-control-tools]");

    if (!panel || !actions || !anchor) {
      throw new Error("Compiled control panel is missing a public extension anchor.");
    }

    // The list header sticks under the menu bar and this panel, so it needs the panel's height (client.css).
    const publishHeight = (): void => document.documentElement.style.setProperty("--control-panel-height", `${root.offsetHeight}px`);
    const sizeWatch = typeof ResizeObserver === "function" ? new ResizeObserver(publishHeight) : undefined;
    sizeWatch?.observe(root);
    publishHeight();

    if (adapter.display?.layout === "compact" || options?.isSmall) {
      panel.classList.add("control-panel-compact");
    }

    let activePager: Pager | undefined;
    let activeSwitcher: ViewSwitcher | undefined;
    let activeActionMenus: ActionMenus | undefined;

    function applyAdapter(curr: ControlPanelAdapter): void {
      const display = curr.display ?? {};

      // 1. Breadcrumbs slot
      if (breadcrumbsSlot) {
        breadcrumbsSlot.replaceChildren();
        if (curr.breadcrumbs) {
          breadcrumbsSlot.append(curr.breadcrumbs);
        } else if (curr.title) {
          const h1 = document.createElement("h1");
          h1.textContent = curr.title;
          breadcrumbsSlot.append(h1);
        }
      }

      // 2. Search slot
      if (searchSlot) {
        searchSlot.replaceChildren();
        if (display.showSearch !== false && curr.searchArea) {
          searchSlot.append(curr.searchArea);
        }
      }

      // 3. Actions slot
      if (actions) {
        actions.replaceChildren();
        activeActionMenus?.dispose();
        activeActionMenus = undefined;
        if (display.showButtons !== false) {
          if (curr.actionsArea) actions.append(curr.actionsArea);
          if (curr.actionMenuItems && curr.actionMenuItems.length > 0 && curr.actionMenuEnv) {
            activeActionMenus = new ActionMenus(curr.actionMenuItems, curr.actionMenuEnv);
            actions.append(activeActionMenus.render());
          }
        }
      }

      // 4. Tools slot: Pager + ViewSwitcher
      if (toolsSlot) {
        toolsSlot.replaceChildren();

        // Pager
        const pProps = typeof curr.pagerProps === "function" ? curr.pagerProps() : curr.pagerProps;
        activePager?.dispose();
        activePager = undefined;
        if (display.showPager !== false && pProps && pProps.total > 0) {
          activePager = new Pager(pProps);
          toolsSlot.append(activePager.render());
        }

        // View Switcher
        const vsProps = typeof curr.viewSwitcherProps === "function" ? curr.viewSwitcherProps() : curr.viewSwitcherProps;
        activeSwitcher?.dispose();
        activeSwitcher = undefined;
        if (display.showViewSwitcher !== false && vsProps && vsProps.entries.length > 1) {
          activeSwitcher = new ViewSwitcher(vsProps);
          toolsSlot.append(activeSwitcher.render());
        }
      }
    }

    applyAdapter(adapter);

    // Mount registered extensions
    if (props.model) {
      for (const [, extension] of controlPanelExtensions.entries()) {
        if (extension.model !== props.model) continue;
        const target = document.createElement("div");
        anchor.append(target);
        mounted.push(target);
        await mount(extension.component, target, { templates: clientTemplates(), props: { ...props }, dev: false, onError: report });
      }
    }

    // Mobile sticky scroll adaptation
    let stopWatchingScroll: (() => void) | undefined;
    let onScrollThrottled: (() => void) | undefined;

    const scrollingEl = options?.scrollingElement || (typeof document !== "undefined" ? document.documentElement : undefined);
    if (adapter.display?.adaptToScroll && scrollingEl) {
      let isScrolling = false;
      let lastScrollTop = 0;
      let oldScrollTop = scrollingEl.scrollTop;
      const initialScrollTop = scrollingEl.scrollTop;

      onScrollThrottled = () => {
        if (isScrolling) return;
        isScrolling = true;
        if (typeof requestAnimationFrame !== "undefined") {
          requestAnimationFrame(() => { isScrolling = false; });
        } else {
          isScrolling = false;
        }

        const scrollTop = scrollingEl.scrollTop;
        const delta = Math.round(scrollTop - oldScrollTop);

        if (scrollTop > initialScrollTop) {
          panel.classList.add("mobile-sticky");
          if (delta <= 0) {
            lastScrollTop = Math.min(0, lastScrollTop - delta);
          } else {
            lastScrollTop = Math.max(-panel.offsetHeight, -panel.offsetTop - delta);
          }
          panel.style.top = `${lastScrollTop}px`;
        } else {
          panel.classList.remove("mobile-sticky");
          lastScrollTop = 0;
          panel.style.top = "0px";
        }
        oldScrollTop = scrollTop;
      };

      scrollingEl.addEventListener("scroll", onScrollThrottled);
      stopWatchingScroll = () => {
        scrollingEl.removeEventListener("scroll", onScrollThrottled!);
      };
    }

    return {
      panel,
      actions,
      get pager() { return activePager; },
      get viewSwitcher() { return activeSwitcher; },
      onScroll: onScrollThrottled,
      update: (newAdapter: ControlPanelAdapter) => {
        if (!disposed) applyAdapter(newAdapter);
      },
      dispose: () => {
        if (disposed) return;
        disposed = true;
        stopWatchingScroll?.();
        sizeWatch?.disconnect();
        activePager?.dispose();
        activeSwitcher?.dispose();
        activeActionMenus?.dispose();
        for (const target of mounted.reverse()) unmount(target);
      }
    };
  } catch (error) {
    if (!disposed) {
      disposed = true;
      for (const target of mounted.reverse()) unmount(target);
    }
    throw error;
  }
}
