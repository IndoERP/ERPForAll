/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
import type { ComponentClass } from "@erpforall/elang";
import type { RpcClient } from "./transport.js";
import { openProviderUrl } from "./button-url.js";
import { launch } from "./failure-owner.js";
import type { Route } from "./route.js";
import { menuEntryIcons } from "./icons.js";

export interface UserMenuEnvironment {
  userName?: string;
  userId?: number;
  currentCompanyId?: number;
  companies?: readonly unknown[];
  rpc?: RpcClient;
  run?: (job: () => Promise<void>) => void;
  report?: (error: unknown) => void;
  /** Same-origin item links go through the host router (and its unsaved-changes guard). */
  navigate?: (route: Route) => Promise<void>;
  /** Loads a same-origin page that is not a client route, after the host allowed leaving the form. */
  loadPath?: (url: string) => Promise<void>;
}

export type UserMenuItemType = "item" | "switch" | "separator" | "component";

export interface UserMenuItem {
  id?: string;
  type?: UserMenuItemType;
  description?: string;
  /** A shipped icon name; the user menu's built-in entries always carry one, so every custom entry does too. */
  icon?: string;
  sequence?: number;
  hide?: boolean;
  href?: string;
  isChecked?: boolean;
  callback?(checkedOrEnv?: unknown, env?: UserMenuEnvironment): void | Promise<void>;
  Component?: ComponentClass;
  render?(env: UserMenuEnvironment): { node: HTMLElement; dispose(): void };
}

export type UserMenuItemFactory = (env: UserMenuEnvironment) => UserMenuItem | readonly UserMenuItem[] | null | undefined;

export function composeUserMenuItems(
  factories: readonly (readonly [string, UserMenuItemFactory])[],
  env: UserMenuEnvironment
): UserMenuItem[] {
  const result: UserMenuItem[] = [];

  for (const [, factory] of factories) {
    try {
      const items = factory(env);
      if (!items) continue;
      const list = Array.isArray(items) ? (items as readonly UserMenuItem[]) : [items as UserMenuItem];
      for (const item of list) {
        if (item && !item.hide) {
          result.push(item);
        }
      }
    } catch (error) {
      if (env.report) env.report(error);
    }
  }

  return result.sort((a, b) => {
    const seqA = a.sequence ?? 100;
    const seqB = b.sequence ?? 100;
    return seqA - seqB;
  });
}

export interface UserMenuRendererHandle {
  dispose(): void;
}

export function renderUserMenuItems(
  container: HTMLElement,
  items: readonly UserMenuItem[],
  env: UserMenuEnvironment,
  onItemActivated?: () => void
): UserMenuRendererHandle {
  const disposables: (() => void)[] = [];
  // The built-in entries (My Preferences, Sign out) carry icons, so every custom entry gets one as well.
  const entryIcons = menuEntryIcons(items.map(item => item.icon), true);

  for (const [position, item] of items.entries()) {
    const type = item.type || "item";

    if (type === "separator") {
      const divider = document.createElement("div");
      divider.className = "dropdown-divider";
      divider.setAttribute("role", "separator");
      container.append(divider);
      continue;
    }

    if (type === "component") {
      if (typeof item.render === "function") {
        const handle = item.render(env);
        disposables.push(handle.dispose);
        container.append(handle.node);
      }
      continue;
    }

    if (type === "switch") {
      const row = document.createElement("div");
      row.className = "user-menu-switch-row";
      row.setAttribute("role", "menuitem");

      const label = document.createElement("span");
      label.className = "user-menu-label";
      label.textContent = item.description || "";
      label.prepend(entryIcons[position]!);

      const checkbox = document.createElement("input");
      checkbox.type = "checkbox";
      checkbox.className = "form-switch";
      checkbox.setAttribute("role", "switch");
      checkbox.checked = Boolean(item.isChecked);
      checkbox.setAttribute("aria-checked", String(checkbox.checked));
      if (item.id) checkbox.setAttribute("data-menu", item.id);

      checkbox.addEventListener("change", () => {
        checkbox.setAttribute("aria-checked", String(checkbox.checked));
        if (typeof item.callback === "function") {
          launch(env, async () => { await item.callback!(checkbox.checked, env); });
        }
      });

      row.append(label, checkbox);
      container.append(row);
      continue;
    }

    // Default: item (callback or href link)
    // A button, never an anchor: the link is validated and routed by openProviderUrl when it is chosen.
    const el = document.createElement("button");
    el.type = "button";
    el.className = "user-menu-item";
    el.setAttribute("role", "menuitem");
    if (item.id) el.setAttribute("data-menu", item.id);
    el.textContent = item.description || "";
    el.prepend(entryIcons[position]!);

    if (typeof item.callback === "function" || item.href) {
      el.addEventListener("click", e => {
        e.preventDefault();
        if (typeof onItemActivated === "function") onItemActivated();
        if (typeof item.callback === "function") launch(env, async () => { await item.callback!(env, env); });
        else launch(env, () => openProviderUrl(item.href!, { navigate: env.navigate, loadPath: env.loadPath }));
      });
    }

    container.append(el);
  }

  return {
    dispose: () => {
      for (const d of disposables) d();
      disposables.length = 0;
    }
  };
}
