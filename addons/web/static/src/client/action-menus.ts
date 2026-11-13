/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
import { launch } from "./failure-owner.js";
import { openProviderUrl } from "./button-url.js";
import type { Route } from "./route.js";
import { ClientError, type RpcClient } from "./transport.js";
import { attachMenuDropdown } from "./menu-dropdown.js";
import { menuEntryIcons } from "./icons.js";

/** Largest "select all" set an action receives; more fails loudly instead of running on a silent subset. */
export const SELECT_ALL_LIMIT = 5000;

export interface ActionMenuItem {
  id?: number | string;
  key?: string;
  label?: string;
  description?: string;
  name?: string;
  action?: number | string | Record<string, unknown>;
  callback?: (items: readonly ActionMenuItem[]) => void | Promise<void>;
  url?: string;
  domain?: unknown[];
  groupNumber?: number;
  sequence?: number;
  disabled?: boolean;
  class?: string;
  /** A shipped icon name; a menu where any entry names one shows an icon on every entry. */
  icon?: string;
}

export interface ActionMenuEnvironment {
  resModel: string;
  getActiveIds: () => readonly number[];
  isDomainSelected?: boolean;
  domain?: unknown[];
  context?: Record<string, unknown>;
  rpc?: RpcClient;
  doAction?: (action: unknown, options?: { additionalContext?: Record<string, unknown>; onClose?: () => void }) => Promise<void> | void;
  run?: (job: () => Promise<void>) => void;
  report?: (error: unknown) => void;
  /** Same-origin item URLs go through the host router (and its unsaved-changes guard). */
  navigate?: (route: Route) => Promise<void>;
}

export interface ActionMenuOptions {
  shouldExecuteAction?: (item: ActionMenuItem) => boolean | Promise<boolean>;
  onActionExecuted?: () => void;
}

/** Executes an action menu item taking into account pre-execution guards, callbacks, URLs, or action IDs. */
export async function executeActionMenuItem(
  item: ActionMenuItem,
  env: ActionMenuEnvironment,
  options?: ActionMenuOptions
): Promise<void> {
  if (options?.shouldExecuteAction) {
    const allowed = await options.shouldExecuteAction(item);
    if (!allowed) return;
  }

  if (typeof item.callback === "function") {
    if (env.run) {
      env.run(async () => { await item.callback!([item]); });
    } else {
      await item.callback([item]);
    }
    return;
  }

  if (item.url) {
    await openProviderUrl(item.url, { navigate: env.navigate });
    return;
  }

  if (item.action !== undefined && env.doAction) {
    let activeIds = env.getActiveIds();
    if (env.isDomainSelected && env.rpc) {
      const searchRes = await env.rpc.call(env.resModel, "search", {
        domain: env.domain ?? [],
        limit: SELECT_ALL_LIMIT,
        context: env.context ?? {}
      });
      if (!Array.isArray(searchRes)) {
        throw new ClientError("server", "The server did not answer the select-all search with a list of ids.");
      }
      if (searchRes.length >= SELECT_ALL_LIMIT) {
        // The server stopped at the cap: only the count says whether anything was cut off.
        const matched = Number(await env.rpc.call(env.resModel, "search_count", { domain: env.domain ?? [], context: env.context ?? {} }));
        if (!Number.isFinite(matched)) throw new ClientError("server", "The server did not answer the select-all count with a number.");
        if (matched > SELECT_ALL_LIMIT) {
          throw new ClientError("validation", `${matched} records match, more than the ${SELECT_ALL_LIMIT} an action can take at once. Narrow the filter or select fewer records.`);
        }
      }
      activeIds = searchRes.map(Number);
    }

    const activeContext: Record<string, unknown> = {
      ...(env.context ?? {}),
      active_id: activeIds[0],
      active_ids: activeIds,
      active_model: env.resModel
    };
    if (env.domain) {
      activeContext["active_domain"] = env.domain;
    }

    await env.doAction(item.action, {
      additionalContext: activeContext,
      onClose: options?.onActionExecuted
    });
  }
}

export interface PrintMenuLoadOptions {
  items: readonly ActionMenuItem[];
  resModel: string;
  activeIds: readonly number[];
  rpc?: RpcClient;
  fetchValidReportIds?: (actionIds: (number | string)[], resModel: string, activeIds: readonly number[]) => Promise<unknown[]>;
  loadExtraPrintItems?: () => Promise<readonly ActionMenuItem[]> | readonly ActionMenuItem[];
}

export async function loadAvailablePrintItems(options: PrintMenuLoadOptions): Promise<ActionMenuItem[]> {
  const { items, resModel, activeIds, fetchValidReportIds, loadExtraPrintItems } = options;
  const printActions = [...items];
  const domainActionIds: (number | string)[] = [];
  const validActionIds: (number | string)[] = [];

  for (const action of printActions) {
    const actionId = action.id ?? action.key;
    if (actionId !== undefined) {
      if (action.domain !== undefined) {
        domainActionIds.push(actionId);
      } else {
        validActionIds.push(actionId);
      }
    }
  }

  if (domainActionIds.length > 0 && fetchValidReportIds) {
    // A failed availability call propagates: it must not read as "No report available."
    const valid = await fetchValidReportIds(domainActionIds, resModel, activeIds);
    if (Array.isArray(valid)) {
      for (const v of valid) validActionIds.push(v as number | string);
    }
  }

  const validItems = printActions.filter(action => {
    const id = action.id ?? action.key;
    return id !== undefined && validActionIds.includes(id);
  });

  const extra = loadExtraPrintItems ? await loadExtraPrintItems() : [];
  const combined = [...validItems, ...extra];

  if (combined.length === 0) {
    return [
      {
        description: "No report available.",
        disabled: true,
        class: "o_menu_item disabled"
      }
    ];
  }

  return combined;
}

export class ActionMenus {
  private container: HTMLElement | null = null;
  private disposables: (() => void)[] = [];

  constructor(
    public items: readonly ActionMenuItem[],
    public env: ActionMenuEnvironment,
    public options?: ActionMenuOptions
  ) {}

  render(): HTMLElement {
    const wrapper = document.createElement("div");
    wrapper.className = "action-menus-dropdown-group";

    const btn = document.createElement("button");
    btn.type = "button";
    btn.className = "btn btn-secondary action-menus-trigger";
    btn.innerHTML = `Actions <i class="oi oi-chevron-down" aria-hidden="true"></i>`;

    const menu = document.createElement("div");
    menu.className = "action-menu-dropdown";

    const dropdown = attachMenuDropdown({
      trigger: btn,
      menu,
      mode: "display"
    });
    this.disposables.push(() => dropdown.dispose());

    const entryIcons = menuEntryIcons(this.items.map(item => item.icon));
    for (const [position, item] of this.items.entries()) {
      const itemEl = document.createElement("button");
      itemEl.type = "button";
      itemEl.className = `action-menu-item ${item.class ?? ""}`.trim();
      itemEl.setAttribute("role", "menuitem");
      itemEl.textContent = item.description || item.label || item.name || "";
      const entryIcon = entryIcons[position];
      if (entryIcon) itemEl.prepend(entryIcon);
      if (item.disabled) {
        itemEl.disabled = true;
      } else {
        itemEl.addEventListener("click", () => {
          dropdown.close(false);
          launch(this.env, () => executeActionMenuItem(item, this.env, this.options));
        });
      }
      menu.append(itemEl);
    }

    wrapper.append(btn, menu);
    this.container = wrapper;
    return wrapper;
  }

  dispose(): void {
    for (const d of this.disposables) d();
    this.disposables.length = 0;
    this.container?.remove();
    this.container = null;
  }
}
