/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
import { createFocusTrap } from "@erpforall/elang";
import { openProviderUrl } from "./button-url.js";
import { deliverFailure, launch, reportFailure } from "./failure-owner.js";
import type { Route } from "./route.js";

export interface Command {
  id?: string;
  name: string;
  category?: string;
  action?(): void | Promise<void>;
  href?: string;
}

export interface CommandProvider {
  namespace?: string;
  provide(options: { searchValue: string }): Promise<readonly Command[]> | readonly Command[];
}

export interface CommandCategory {
  id?: string;
  key?: string;
  namespace?: string;
  name: string;
  sequence?: number;
}

export interface CommandSetup {
  emptyMessage?: string;
  debounceDelay?: number;
  placeholder?: string;
}

export interface CommandPaletteConfig {
  providers?: readonly CommandProvider[];
  categories?: readonly CommandCategory[];
  setup?: CommandSetup;
  namespace?: string;
  initialSearch?: string;
  /** The host's guarded runner: command actions and links start through it. */
  run?: (job: () => Promise<void>) => void;
  /** Receives every provider failure and every failed command; the other providers' results still render. */
  report?: (error: unknown) => void;
  /** Same-origin links go through the host router (and its unsaved-changes guard). */
  navigate?: (route: Route) => Promise<void>;
  /** Loads a same-origin page that is not a client route, after the host allowed leaving the form. */
  loadPath?: (url: string) => Promise<void>;
}

export interface CommandPaletteHandle {
  node: HTMLElement;
  updateConfig(config?: CommandPaletteConfig): void;
  close(): void;
  readonly isOpen: boolean;
}

let activeInstance: CommandPaletteHandle | null = null;

export function getActiveCommandPalette(): CommandPaletteHandle | null {
  return activeInstance;
}

export function openCommandPalette(
  config: CommandPaletteConfig = {},
  onClose?: () => void
): CommandPaletteHandle {
  if (activeInstance && activeInstance.isOpen) {
    activeInstance.updateConfig(config);
    return activeInstance;
  }

  const previousFocus = document.activeElement instanceof HTMLElement ? document.activeElement : null;

  const backdrop = document.createElement("div");
  backdrop.className = "command-palette-backdrop";

  const dialog = document.createElement("div");
  dialog.className = "command-palette-dialog";
  dialog.setAttribute("role", "dialog");
  dialog.setAttribute("aria-modal", "true");
  dialog.setAttribute("aria-label", "Command palette");

  const input = document.createElement("input");
  input.type = "text";
  input.className = "command-palette-input";
  input.setAttribute("role", "combobox");
  input.setAttribute("aria-autocomplete", "list");
  input.setAttribute("aria-expanded", "true");
  input.setAttribute("aria-controls", "command-palette-results");

  const loadingEl = document.createElement("div");
  loadingEl.className = "command-palette-loading";
  loadingEl.setAttribute("role", "status");
  loadingEl.setAttribute("aria-live", "polite");
  loadingEl.textContent = "Loading…";
  loadingEl.style.display = "none";

  const emptyEl = document.createElement("div");
  emptyEl.className = "command-palette-empty";
  emptyEl.setAttribute("role", "status");
  emptyEl.style.display = "none";

  const resultsList = document.createElement("div");
  resultsList.className = "command-palette-results";
  resultsList.id = "command-palette-results";
  resultsList.setAttribute("role", "listbox");

  dialog.append(input, loadingEl, emptyEl, resultsList);
  backdrop.append(dialog);

  // Inert background siblings along document.body
  const inertElements = new Map<HTMLElement, boolean>();
  for (const sibling of Array.from(document.body.children)) {
    if (sibling instanceof HTMLElement && sibling !== backdrop) {
      inertElements.set(sibling, sibling.inert === true || sibling.hasAttribute("inert"));
      sibling.inert = true;
      sibling.setAttribute("inert", "");
    }
  }

  document.body.append(backdrop);

  let currentConfig = { ...config };
  let selectedIndex = -1;
  let currentCommands: Command[] = [];
  let querySeq = 0;
  let pendingPromise: Promise<void> | null = null;
  let isClosed = false;
  let trap: ReturnType<typeof createFocusTrap> | undefined;
  try {
    trap = createFocusTrap(dialog, { onEscape: () => handle.close(), initialFocus: input });
  } catch (error) {
    deliverFailure(currentConfig, error);
  }
  /** Providers whose failure was already reported: a search runs on every keystroke, the user is told once per session. */
  const reportedProviders = new Set<number>();

  function applySetup(): void {
    const setup = currentConfig.setup || {};
    const activeNs = currentConfig.namespace || "default";
    input.placeholder = setup.placeholder || (activeNs !== "default" ? `Search ${activeNs}…` : "Search commands…");
    emptyEl.textContent = setup.emptyMessage || "No commands found";
  }
  applySetup();

  function selectIndex(index: number): void {
    const items = resultsList.querySelectorAll<HTMLElement>(".command-palette-item");
    if (!items.length) {
      selectedIndex = -1;
      input.removeAttribute("aria-activedescendant");
      return;
    }
    selectedIndex = Math.max(0, Math.min(index, items.length - 1));
    items.forEach((item, i) => {
      const isSelected = i === selectedIndex;
      item.classList.toggle("selected", isSelected);
      item.setAttribute("aria-selected", String(isSelected));
      if (isSelected) {
        if (item.id) {
          input.setAttribute("aria-activedescendant", item.id);
        }
        item.scrollIntoView?.({ block: "nearest" });
      }
    });
  }

  async function executeSearch(searchValue: string): Promise<void> {
    const seq = ++querySeq;
    loadingEl.style.display = "block";
    emptyEl.style.display = "none";
    resultsList.replaceChildren();

    const allProviders = currentConfig.providers || [];
    const allCategories = currentConfig.categories || [];

    // Collect all registered namespaces
    const knownNamespaces = new Set<string>();
    for (const p of allProviders) {
      if (p.namespace) knownNamespaces.add(p.namespace);
    }
    for (const c of allCategories) {
      if (c.namespace) knownNamespaces.add(c.namespace);
    }

    let activeNamespace = currentConfig.namespace || "default";
    let query = searchValue;

    if (!currentConfig.namespace || currentConfig.namespace === "default") {
      if (searchValue.length > 0 && knownNamespaces.has(searchValue[0]!)) {
        activeNamespace = searchValue[0]!;
        query = searchValue.slice(1).trimStart();
      } else {
        const colonIdx = searchValue.indexOf(":");
        const spaceIdx = searchValue.indexOf(" ");
        let sepIdx = -1;
        if (colonIdx > 0 && spaceIdx > 0) sepIdx = Math.min(colonIdx, spaceIdx);
        else if (colonIdx > 0) sepIdx = colonIdx;
        else if (spaceIdx > 0) sepIdx = spaceIdx;

        if (sepIdx > 0) {
          const prefix = searchValue.slice(0, sepIdx);
          if (knownNamespaces.has(prefix)) {
            activeNamespace = prefix;
            query = searchValue.slice(sepIdx + 1).trimStart();
          }
        }
      }
    }

    if (activeNamespace !== "default") {
      dialog.setAttribute("data-namespace", activeNamespace);
    } else {
      dialog.removeAttribute("data-namespace");
    }

    const providers = allProviders.filter(
      p => (p.namespace || "default") === activeNamespace
    );

    const failures: { index: number; error: unknown }[] = [];
    // Each provider settles on its own: one failure never blanks the others.
    const prom = Promise.all(
      providers.map(async (p, index) => {
        try {
          return [...(await p.provide({ searchValue: query }))];
        } catch (error) {
          failures.push({ index, error });
          return [];
        }
      })
    );

    pendingPromise = prom.then(() => {});

    try {
      const results = await prom;
      if (seq !== querySeq || isClosed) return;
      for (const { index, error } of failures) {
        if (reportedProviders.has(index)) continue;
        reportedProviders.add(index);
        deliverFailure(currentConfig, error);
      }

      loadingEl.style.display = "none";
      const flatCommands = results.flat();

      // Resolve categories for active namespace
      const activeCategories = allCategories.filter(
        c => (c.namespace || "default") === activeNamespace
      );
      const categoryByKey = new Map<string, CommandCategory>();
      const categoryByName = new Map<string, CommandCategory>();
      for (const cat of activeCategories) {
        if (cat.id) categoryByKey.set(cat.id, cat);
        if (cat.key) categoryByKey.set(cat.key, cat);
        categoryByName.set(cat.name.toLowerCase(), cat);
        categoryByName.set(cat.name, cat);
      }
      const defaultCategory = categoryByKey.get("default") ?? categoryByName.get("default");

      const tagged = flatCommands.map((cmd, originalIndex) => {
        let matchedCat: CommandCategory | undefined;
        if (cmd.category) {
          matchedCat = categoryByKey.get(cmd.category)
            ?? categoryByName.get(cmd.category)
            ?? categoryByName.get(cmd.category.toLowerCase());
        }
        if (!matchedCat && defaultCategory) {
          matchedCat = defaultCategory;
        }
        const sequence = typeof matchedCat?.sequence === "number" ? matchedCat.sequence : (matchedCat ? 100 : 1000);
        const displayCategory = matchedCat?.name ?? cmd.category;
        return {
          cmd,
          sequence,
          displayCategory,
          originalIndex
        };
      });

      tagged.sort((a, b) => {
        if (a.sequence !== b.sequence) {
          return a.sequence - b.sequence;
        }
        return a.originalIndex - b.originalIndex;
      });

      currentCommands = tagged.map(t => t.cmd);

      if (!tagged.length) {
        // Nothing typed yet and no custom message: a hint, never "No commands found" before the user searched.
        const setup = currentConfig.setup || {};
        emptyEl.textContent = setup.emptyMessage || (searchValue.trim() ? "No commands found" : "Type to search menus and commands.");
        emptyEl.style.display = "block";
        selectedIndex = -1;
        input.removeAttribute("aria-activedescendant");
      } else {
        emptyEl.style.display = "none";
        for (let i = 0; i < tagged.length; i++) {
          const { cmd, displayCategory } = tagged[i]!;
          const item = document.createElement("div");
          item.className = "command-palette-item";
          item.setAttribute("role", "option");
          item.setAttribute("id", `command-item-${i}`);
          if (displayCategory) {
            item.setAttribute("data-category", displayCategory);
          }

          const nameSpan = document.createElement("span");
          nameSpan.className = "command-name";
          nameSpan.textContent = cmd.name;
          item.append(nameSpan);

          if (displayCategory) {
            const catSpan = document.createElement("span");
            catSpan.className = "command-category";
            catSpan.textContent = displayCategory;
            item.append(catSpan);
          }

          item.addEventListener("click", () => {
            invokeCommand(cmd, false);
          });

          resultsList.append(item);
        }
        selectIndex(0);
      }
    } finally {
      if (seq === querySeq) {
        loadingEl.style.display = "none";
        pendingPromise = null;
      }
    }
  }

  let invoking = false;
  function invokeCommand(cmd: Command, ctrlKey: boolean): void {
    if (invoking) return;
    invoking = true;
    // Close first so a failing or slow command never leaves the palette open.
    handle.close();
    const newTab = ctrlKey && Boolean(cmd.href);
    if (cmd.href && (newTab || !cmd.action)) {
      launch(currentConfig, () => openProviderUrl(cmd.href!, { navigate: currentConfig.navigate, loadPath: currentConfig.loadPath, newTab }));
    } else if (cmd.action) {
      launch(currentConfig, async () => { await cmd.action!(); });
    }
  }

  const search = (value: string): void => reportFailure(executeSearch(value), currentConfig);
  input.addEventListener("input", () => search(input.value));

  input.addEventListener("keydown", async (e: KeyboardEvent) => {
    if (e.key === "ArrowDown") {
      e.preventDefault();
      const count = currentCommands.length;
      if (count > 0) {
        selectIndex((selectedIndex + 1) % count);
      }
    } else if (e.key === "ArrowUp") {
      e.preventDefault();
      const count = currentCommands.length;
      if (count > 0) {
        selectIndex((selectedIndex - 1 + count) % count);
      }
    } else if (e.key === "Enter") {
      e.preventDefault();
      if (invoking) return;
      if (pendingPromise) {
        invoking = true;
        await pendingPromise;
        invoking = false;
      }
      if (selectedIndex >= 0 && selectedIndex < currentCommands.length) {
        const cmd = currentCommands[selectedIndex]!;
        invokeCommand(cmd, e.ctrlKey || e.metaKey);
      }
    } else if (e.key === "Escape") {
      e.preventDefault();
      handle.close();
    }
  });

  dialog.addEventListener("keydown", (e: KeyboardEvent) => {
    if (e.key === "Escape") {
      e.preventDefault();
      handle.close();
    }
  });

  const onPointerDown = (e: MouseEvent): void => {
    if (e.target instanceof Node && !dialog.contains(e.target)) {
      handle.close();
    }
  };
  window.addEventListener("pointerdown", onPointerDown);

  const handle: CommandPaletteHandle = {
    node: backdrop,
    get isOpen() {
      return !isClosed;
    },
    updateConfig: (newConfig?: CommandPaletteConfig) => {
      if (newConfig) {
        currentConfig = { ...currentConfig, ...newConfig };
        applySetup();
        if (newConfig.initialSearch !== undefined) {
          input.value = newConfig.initialSearch;
        }
      }
      input.focus();
      search(input.value);
    },
    close: () => {
      if (isClosed) return;
      isClosed = true;
      trap?.dispose();
      window.removeEventListener("pointerdown", onPointerDown);
      backdrop.remove();
      for (const [element, wasInert] of inertElements) {
        if (element.isConnected) {
          element.inert = wasInert;
          if (wasInert) {
            element.setAttribute("inert", "");
          } else {
            element.removeAttribute("inert");
          }
        }
      }
      inertElements.clear();
      if (activeInstance === handle) {
        activeInstance = null;
      }
      if (previousFocus && typeof previousFocus.focus === "function") {
        previousFocus.focus();
      }
      if (typeof onClose === "function") {
        onClose();
      }
    }
  };

  activeInstance = handle;

  if (currentConfig.initialSearch !== undefined) {
    input.value = currentConfig.initialSearch;
  }
  input.focus();
  search(input.value);

  return handle;
}
