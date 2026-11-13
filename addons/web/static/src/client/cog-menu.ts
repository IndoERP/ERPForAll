/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
import type { ComponentClass } from "@erpforall/elang";
import type { RpcClient } from "./transport.js";
import { openProviderUrl } from "./button-url.js";
import { launch, reportFailure } from "./failure-owner.js";
import type { Route } from "./route.js";
import { attachMenuDropdown } from "./menu-dropdown.js";
import { menuEntryIcons } from "./icons.js";

export interface CogMenuItemEnvironment {
  rpc?: RpcClient;
  context?: Record<string, unknown>;
  model?: string;
  activeId?: number;
  activeIds?: readonly number[];
  viewType?: string;
  run?: (job: () => Promise<void>) => void;
  report?: (error: unknown) => void;
  /** Same-origin item links go through the host router (and its unsaved-changes guard). */
  navigate?: (route: Route) => Promise<void>;
  /** Loads a same-origin page that is not a client route, after the host allowed leaving the form. */
  loadPath?: (url: string) => Promise<void>;
}

export interface CogMenuItem {
  key?: string;
  label?: string;
  description?: string;
  name?: string;
  groupNumber?: number;
  sequence?: number;
  /** A shipped icon name (for example `settings`); a menu where any entry names one shows an icon on every entry. */
  icon?: string;
  href?: string;
  isDisplayed?(env: CogMenuItemEnvironment): boolean | Promise<boolean>;
  callback?(env: CogMenuItemEnvironment): void | Promise<void>;
  Component?: ComponentClass;
  render?(env: CogMenuItemEnvironment): { node: HTMLElement; dispose(): void };
}

export function getPrintItemAriaLabel(item: { description?: string; label?: string; name?: string }): string {
  const desc = item.description || item.label || item.name || "";
  return `Print report: ${desc}`;
}

export async function prepareCogMenu(
  registryItems: readonly CogMenuItem[],
  env: CogMenuItemEnvironment,
  actionItems: readonly CogMenuItem[] = []
): Promise<CogMenuItem[]> {
  const allCandidates = [...registryItems, ...actionItems];
  const admitted: CogMenuItem[] = [];

  for (const item of allCandidates) {
    if (typeof item.isDisplayed === "function") {
      const display = await item.isDisplayed(env);
      if (!display) continue;
    }
    admitted.push(item);
  }

  return admitted.sort((a, b) => {
    const groupA = a.groupNumber ?? 0;
    const groupB = b.groupNumber ?? 0;
    if (groupA !== groupB) return groupA - groupB;
    const seqA = a.sequence ?? 100;
    const seqB = b.sequence ?? 100;
    return seqA - seqB;
  });
}

export interface CogMenuHandle {
  node: HTMLElement;
  dispose(): void;
  update(items: readonly CogMenuItem[]): Promise<void>;
}

export function renderCogMenu(
  initialItems: readonly CogMenuItem[],
  env: CogMenuItemEnvironment,
  actionItems: readonly CogMenuItem[] = []
): CogMenuHandle {
  const container = document.createElement("div");
  container.className = "cog-menu-container";

  const trigger = document.createElement("button");
  trigger.type = "button";
  trigger.className = "cog-menu-trigger";
  trigger.setAttribute("aria-label", "Actions and settings");
  trigger.innerHTML = `<i class="oi oi-cog" aria-hidden="true"></i>`;

  const dropdown = document.createElement("div");
  dropdown.className = "cog-menu-dropdown";

  const dropdownHelper = attachMenuDropdown({
    trigger,
    menu: dropdown,
    mode: "display"
  });

  let activeItems: CogMenuItem[] = [];
  const childDisposables: (() => void)[] = [];
  let populateSeq = 0;
  let isDisposed = false;

  async function populate(items: readonly CogMenuItem[], actions: readonly CogMenuItem[]): Promise<void> {
    const seq = ++populateSeq;
    for (const d of childDisposables) d();
    childDisposables.length = 0;
    dropdown.replaceChildren();

    const prepared = await prepareCogMenu(items, env, actions);
    if (isDisposed || seq !== populateSeq) {
      return;
    }
    activeItems = prepared;

    let lastGroup: number | undefined;
    const entryIcons = menuEntryIcons(activeItems.map(item => typeof item.render === "function" ? undefined : item.icon));
    for (const [position, item] of activeItems.entries()) {
      const currentGroup = item.groupNumber ?? 0;
      if (lastGroup !== undefined && currentGroup !== lastGroup) {
        const divider = document.createElement("div");
        divider.className = "cog-menu-divider";
        divider.setAttribute("role", "separator");
        dropdown.append(divider);
      }
      lastGroup = currentGroup;

      if (typeof item.render === "function") {
        const handle = item.render(env);
        childDisposables.push(handle.dispose);
        dropdown.append(handle.node);
      } else {
        // A button, never an anchor: the link is validated and routed by openProviderUrl when it is chosen.
        const el = document.createElement("button");
        el.type = "button";
        el.className = "cog-menu-item";
        el.setAttribute("role", "menuitem");
        const label = item.description || item.label || item.name || "";
        el.textContent = label;
        const entryIcon = entryIcons[position];
        if (entryIcon) el.prepend(entryIcon);

        if (item.label?.toLowerCase().includes("print") || item.name?.toLowerCase().includes("print")) {
          el.setAttribute("aria-label", getPrintItemAriaLabel(item));
        }

        if (typeof item.callback === "function" || item.href) {
          el.addEventListener("click", e => {
            e.preventDefault();
            dropdownHelper.close(false);
            if (typeof item.callback === "function") launch(env, async () => { await item.callback!(env); });
            else launch(env, () => openProviderUrl(item.href!, { navigate: env.navigate, loadPath: env.loadPath }));
          });
        }
        dropdown.append(el);
      }
    }
    dropdownHelper.refresh();
  }

  reportFailure(populate(initialItems, actionItems), env);
  container.append(trigger, dropdown);

  return {
    node: container,
    dispose: () => {
      isDisposed = true;
      populateSeq++;
      dropdownHelper.dispose();
      for (const d of childDisposables) d();
      childDisposables.length = 0;
      container.remove();
    },
    update: async (items: readonly CogMenuItem[]) => {
      if (isDisposed) return;
      await populate(items, actionItems);
    }
  };
}
