/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
import type { ComponentClass } from "@erpforall/elang";
import type { RpcClient } from "./transport.js";
import type { MessageServices } from "./messages.js";
import type { ActivityWindowAction } from "./activity-menu.js";

export interface SystrayEnvironment {
  rpc?: RpcClient;
  context?: Record<string, unknown>;
  run?: (job: () => Promise<void>) => void;
  report?: (error: unknown) => void;
  messages?: MessageServices;
  /** Opens a checked activity window action as a list; `null` opens the user's own "My Activities" list. */
  openActivities?: (action: ActivityWindowAction | null) => Promise<void>;
}

export interface SystrayItem {
  key?: string;
  sequence?: number;
  isDisplayed?(env: SystrayEnvironment): boolean | Promise<boolean>;
  Component?: ComponentClass;
  render?(env: SystrayEnvironment): { node: HTMLElement; dispose(): void };
}

export async function prepareSystrayItems(
  entries: readonly (readonly [string, SystrayItem])[],
  env: SystrayEnvironment
): Promise<SystrayItem[]> {
  const admitted: SystrayItem[] = [];

  for (const [key, item] of entries) {
    if (typeof item.isDisplayed === "function") {
      const display = await item.isDisplayed(env);
      if (!display) continue;
    }
    admitted.push({ key, ...item });
  }

  return admitted.reverse();
}

export interface SystrayHandle {
  dispose(): void;
  update(entries: readonly (readonly [string, SystrayItem])[]): Promise<void>;
}

export async function renderSystray(
  container: HTMLElement,
  entries: readonly (readonly [string, SystrayItem])[],
  env: SystrayEnvironment
): Promise<SystrayHandle> {
  const disposables: (() => void)[] = [];

  async function populate(currentEntries: readonly (readonly [string, SystrayItem])[]): Promise<void> {
    for (const d of disposables) d();
    disposables.length = 0;
    container.replaceChildren();

    const items = await prepareSystrayItems(currentEntries, env);
    for (const item of items) {
      if (typeof item.render === "function") {
        const handle = item.render(env);
        disposables.push(handle.dispose);
        container.append(handle.node);
      }
    }
  }

  await populate(entries);

  return {
    dispose: () => {
      for (const d of disposables) d();
      disposables.length = 0;
      container.replaceChildren();
    },
    update: async (newEntries: readonly (readonly [string, SystrayItem])[]) => {
      await populate(newEntries);
    }
  };
}
