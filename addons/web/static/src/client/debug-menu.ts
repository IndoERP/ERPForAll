/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
import type { ComponentClass } from "@erpforall/elang";
import type { RpcClient } from "./transport.js";
import type { Command, CommandProvider } from "./command-palette.js";
import { openProviderUrl } from "./button-url.js";
import { launch } from "./failure-owner.js";
import type { Route } from "./route.js";
import { menuEntryIcons } from "./icons.js";

export interface DebugEnvironment {
  rpc?: RpcClient;
  context?: Record<string, unknown>;
  model?: string;
  activeId?: number;
  activeIds?: readonly number[];
  run?: (job: () => Promise<void>) => void;
  report?: (error: unknown) => void;
  /** Same-origin item links go through the host router (and its unsaved-changes guard). */
  navigate?: (route: Route) => Promise<void>;
  /** Loads a same-origin page that is not a client route, after the host allowed leaving the form. */
  loadPath?: (url: string) => Promise<void>;
}

export interface DebugSection {
  label: string;
  sequence: number;
}

export type DebugItemType = "item" | "separator" | "link" | "component";

export interface DebugItem {
  id?: string;
  type?: DebugItemType;
  section?: string;
  description?: string;
  /** A shipped icon name; a menu where any entry names one shows an icon on every entry. */
  icon?: string;
  callback?(): void | Promise<void>;
  href?: string;
  sequence?: number;
  Component?: ComponentClass;
  render?(): { node: HTMLElement; dispose(): void };
}

export type DebugItemProvider = (env: DebugEnvironment) => Promise<readonly DebugItem[]> | readonly DebugItem[];

export const DEFAULT_DEBUG_SECTIONS: Record<string, DebugSection> = {
  record: { label: "Record", sequence: 10 },
  records: { label: "Records", sequence: 10 },
  ui: { label: "User Interface", sequence: 20 },
  security: { label: "Security", sequence: 30 },
  testing: { label: "Tours & Testing", sequence: 40 },
  tools: { label: "Tools", sequence: 50 },
};

export interface GroupedDebugSection {
  section: string;
  label: string;
  sequence: number;
  items: DebugItem[];
}

export async function loadGroupedDebugItems(
  providers: readonly DebugItemProvider[],
  sectionsRegistry: { get(key: string): DebugSection; has(key: string): boolean },
  env: DebugEnvironment
): Promise<GroupedDebugSection[]> {
  const itemLists = await Promise.all(
    providers.map(async p => {
      try {
        return await p(env);
      } catch (error) {
        if (env.report) env.report(error);
        return [];
      }
    })
  );

  const allItems = itemLists.flat();
  const bySection = new Map<string, DebugItem[]>();

  for (const item of allItems) {
    const sectionKey = item.section || "tools";
    if (!bySection.has(sectionKey)) {
      bySection.set(sectionKey, []);
    }
    bySection.get(sectionKey)!.push(item);
  }

  const sections: GroupedDebugSection[] = [];
  for (const [sectionKey, items] of bySection.entries()) {
    let sectionInfo: DebugSection;
    if (sectionsRegistry.has(sectionKey)) {
      sectionInfo = sectionsRegistry.get(sectionKey);
    } else if (DEFAULT_DEBUG_SECTIONS[sectionKey]) {
      sectionInfo = DEFAULT_DEBUG_SECTIONS[sectionKey]!;
    } else {
      sectionInfo = { label: sectionKey, sequence: 50 };
    }

    sections.push({
      section: sectionKey,
      label: sectionInfo.label,
      sequence: sectionInfo.sequence,
      items
    });
  }

  return sections.sort((a, b) => a.sequence - b.sequence);
}

export function createDebugCommandProvider(
  providers: readonly DebugItemProvider[],
  sectionsRegistry: { get(key: string): DebugSection; has(key: string): boolean },
  env: DebugEnvironment
): CommandProvider {
  return {
    namespace: "debug",
    provide: async ({ searchValue }: { searchValue: string }) => {
      const grouped = await loadGroupedDebugItems(providers, sectionsRegistry, env);
      const commands: Command[] = [];
      const lower = searchValue.trim().toLowerCase();

      for (const group of grouped) {
        for (const item of group.items) {
          const type = item.type || "item";
          if (type === "separator" || type === "component") continue;

          const desc = item.description || "";
          if (lower && !desc.toLowerCase().includes(lower)) continue;

          commands.push({
            id: item.id,
            name: desc,
            category: group.label,
            action: item.callback,
            href: item.href
          });
        }
      }

      return commands;
    }
  };
}

export interface DebugMenuHandle {
  dispose(): void;
}

export async function renderDebugMenu(
  container: HTMLElement,
  providers: readonly DebugItemProvider[],
  sectionsRegistry: { get(key: string): DebugSection; has(key: string): boolean },
  env: DebugEnvironment,
  onItemActivated?: () => void
): Promise<DebugMenuHandle> {
  const disposables: (() => void)[] = [];
  container.replaceChildren();

  const grouped = await loadGroupedDebugItems(providers, sectionsRegistry, env);

  // One icon decision for the whole menu, across its sections.
  const clickable = grouped.flatMap(section => section.items.filter(item => (item.type || "item") !== "separator" && (item.type || "item") !== "component"));
  const icons = menuEntryIcons(clickable.map(entry => entry.icon));
  const entryIcons = new Map(clickable.map((item, position) => [item, icons[position]] as const));

  if (grouped.length === 0) {
    const empty = document.createElement("p");
    empty.className = "debug-menu-empty"; empty.textContent = "No developer tools for this screen.";
    container.append(empty);
  }
  let isFirstSection = true;
  for (const section of grouped) {
    if (!isFirstSection) {
      const divider = document.createElement("div");
      divider.className = "dropdown-divider";
      divider.setAttribute("role", "separator");
      container.append(divider);
    }
    isFirstSection = false;

    const header = document.createElement("div");
    header.className = "debug-section-header";
    header.textContent = section.label;
    container.append(header);

    for (const item of section.items) {
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
          const handle = item.render();
          disposables.push(handle.dispose);
          container.append(handle.node);
        }
        continue;
      }

      // A button, never an anchor: the link is validated and routed by openProviderUrl when it is chosen.
      const el = document.createElement("button");
      el.type = "button";
      el.className = "debug-menu-item";
      el.setAttribute("role", "menuitem");
      el.textContent = item.description || "";
      const entryIcon = entryIcons.get(item);
      if (entryIcon) el.prepend(entryIcon);

      if (typeof item.callback === "function" || item.href) {
        el.addEventListener("click", e => {
          e.preventDefault();
          if (typeof onItemActivated === "function") onItemActivated();
          if (typeof item.callback === "function") launch(env, async () => { await item.callback!(); });
          else launch(env, () => openProviderUrl(item.href!, { navigate: env.navigate, loadPath: env.loadPath }));
        });
      }

      container.append(el);
    }
  }

  return {
    dispose: () => {
      for (const d of disposables) d();
      disposables.length = 0;
      container.replaceChildren();
    }
  };
}
