/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
import { array, integer, object, text } from "./metadata.js";
import { actionButton, button, choose, el, labelled } from "./dom.js";
import { createChoiceDropdown, type ChoiceDropdown } from "./choice-dropdown.js";
import { icon, menuIcon } from "./icons.js";
import { readSettingsItems, type SettingsViewOptions } from "./settings-items.js";
import type { SettingsItem } from "../generated/human-ui.js";
import { registerServiceController } from "./registry.js";
import { ClientError, RpcClient } from "./transport.js";

export interface ServiceController { busy(): boolean; dirty(): boolean; save(): Promise<void>; discard(): void; /** Release listeners and ignore late rendering; safe to call repeatedly. */ dispose?(): void }

function renderHighlightedText(container: HTMLElement, textContent: string, query: string): void {
  container.replaceChildren();
  if (!query) {
    container.textContent = textContent;
    return;
  }
  const lowerText = textContent.toLowerCase();
  const lowerQuery = query.toLowerCase();
  let start = 0;
  let index = lowerText.indexOf(lowerQuery, start);
  if (index === -1) {
    container.textContent = textContent;
    return;
  }
  while (index !== -1) {
    if (index > start) {
      container.append(document.createTextNode(textContent.slice(start, index)));
    }
    const mark = document.createElement("mark");
    mark.className = "highlight";
    mark.textContent = textContent.slice(index, index + query.length);
    container.append(mark);
    start = index + query.length;
    index = lowerText.indexOf(lowerQuery, start);
  }
  if (start < textContent.length) {
    container.append(document.createTextNode(textContent.slice(start)));
  }
}

function humanizeAppTitle(appKey: string): string {
  if (appKey === "general" || appKey === "base") return "General Settings";
  return appKey
    .split(/[_-]/)
    .map(part => part.charAt(0).toUpperCase() + part.slice(1))
    .join(" ");
}

interface SettingBlockInfo {
  element: HTMLElement;
  titleEl: HTMLElement;
  helpEl?: HTMLElement;
  labelText: string;
  helpText?: string;
  keyText: string;
  searchText?: string;
}

interface SettingBlockGroupInfo {
  element: HTMLElement;
  titleText: string;
  blocks: SettingBlockInfo[];
}

interface AppGroupInfo {
  appKey: string;
  appTitle: string;
  section: HTMLElement;
  tab: HTMLButtonElement;
  blocks: SettingBlockInfo[];
  blockGroups: SettingBlockGroupInfo[];
}

const UNINSTALL_UNAVAILABLE = "Uninstalling this addon from this page is not available yet.";

function confirmUninstallDialog(
  rpc: RpcClient,
  addon: { name: string; label: string },
  impact: Record<string, unknown>,
  opener: HTMLElement,
  onSuccess: () => Promise<void> | void,
  onError: (error: unknown) => Promise<boolean>
): Promise<void> {
  return new Promise<void>((resolve, reject) => {
    const dialog = el("dialog", undefined, "addon-uninstall-dialog");
    // Decoded before anything is shown: an impact without its fingerprint cannot be confirmed.
    const fingerprint = text(impact["fingerprint"], "uninstall impact fingerprint");
    const heading = el("h2", `Uninstall ${addon.label}?`, "addon-uninstall-title");
    heading.id = "addon-uninstall-heading";
    dialog.setAttribute("aria-labelledby", heading.id);

    const warning = el("div", "Uninstalling modules can be risky. This operation permanently removes the records and schema shown below.", "alert alert-warning addon-uninstall-warning");

    const body = el("div", undefined, "addon-uninstall-body");
    body.append(warning);

    const resolved = array(impact["resolved"], "resolved modules").map(item => text(item, "resolved module"));
    const modulesSection = el("div", undefined, "addon-impact-section");
    modulesSection.append(el("h3", "Affected modules:", "addon-impact-subtitle"));
    const moduleList = el("ul", undefined, "addon-impact-modules");
    for (const mod of resolved) {
      moduleList.append(el("li", mod));
    }
    modulesSection.append(moduleList);
    body.append(modulesSection);

    const droppedModels = array(impact["dropped_models"], "dropped models").map(item => object(item));
    if (droppedModels.length > 0) {
      const modelsSection = el("div", undefined, "addon-impact-section");
      modelsSection.append(el("h3", "Models and records to be deleted:", "addon-impact-subtitle"));
      const table = el("table", undefined, "addon-impact-models-table");
      const thead = el("thead");
      const headerRow = el("tr");
      headerRow.append(el("th", "Model"), el("th", "Table"), el("th", "# Records"));
      thead.append(headerRow);
      const tbody = el("tbody");
      for (const m of droppedModels) {
        const tr = el("tr");
        tr.append(
          el("td", text(m["model"], "dropped model name")),
          el("td", text(m["table"], "dropped model table")),
          el("td", text(m["row_count"], "dropped model record count"), "numeric")
        );
        tbody.append(tr);
      }
      table.append(thead, tbody);
      modelsSection.append(table);
      body.append(modelsSection);
    }

    const metadata = array(impact["metadata"], "uninstall metadata effects").map(item => object(item));
    if (metadata.length > 0) {
      const section = el("div", undefined, "addon-impact-section");
      section.append(el("h3", "Database effects", "addon-impact-subtitle"));
      const effects = el("ul", undefined, "addon-impact-database");
      const labels = new Map([
        ["delete", "Delete records"], ["drop_schema_objects", "Remove schema objects"],
        ["bump_policy", "Refresh access policy"], ["bump_registry", "Reload the model registry"],
      ]);
      for (const item of metadata) {
        const kind = text(item["kind"], "database effect kind");
        const target = text(item["table"], "database effect table");
        const count = text(item["row_count"], "database effect count");
        effects.append(el("li", `${labels.get(kind) ?? kind} · ${target} (${count})`));
      }
      section.append(effects);
      body.append(section);
    }

    const host = array(impact["effects"], "uninstall package and host effects").map(item => object(item));
    if (host.length > 0) {
      const section = el("div", undefined, "addon-impact-section");
      section.append(el("h3", "Package and host effects", "addon-impact-subtitle"));
      const effects = el("dl", undefined, "addon-facts");
      for (const item of host) {
        const pair = el("div");
        pair.append(el("dt", text(item["component"], "effect component")), el("dd", text(item["description"], "effect description")));
        effects.append(pair);
      }
      section.append(effects);
      body.append(section);
    }

    const targetCount = text(impact["target_count"], "uninstall impact target count");
    if (targetCount) {
      body.append(el("p", `Affected records and schema objects: ${targetCount}`, "addon-impact-summary muted"));
    }

    const footer = el("footer", undefined, "addon-uninstall-footer");
    const cancelBtn = button("Cancel", () => dialog.close(), "btn btn-secondary addon-uninstall-cancel");
    let subPending = false;
    let settled = false;
    const confirmBtn = button("Confirm Uninstall", async () => {
      if (subPending) return;
      subPending = true;
      cancelBtn.disabled = true;
      confirmBtn.disabled = true;
      try {
        await rpc.post("/erp/module/uninstall", {
          modules: [addon.name],
          impact: fingerprint,
        }, { write: true });
      } catch (error) {
        settled = true;
        dialog.close();
        try {
          if (await onError(error)) resolve();
          else reject(error);
        } catch (handlingError) {
          reject(handlingError);
        }
        return;
      }
      settled = true;
      dialog.close();
      try {
        await onSuccess();
        resolve();
      } catch (error) {
        reject(error);
      }
    }, "danger addon-uninstall-confirm");

    footer.append(cancelBtn, confirmBtn);
    dialog.append(heading, body, footer);

    dialog.addEventListener("keydown", event => {
      if (event.key === "Escape") {
        event.preventDefault();
        if (!subPending) dialog.close();
      } else if (event.key === "Tab") {
        const focusable = [cancelBtn, confirmBtn].filter(b => !b.disabled);
        if (focusable.length === 0) return;
        const first = focusable[0];
        const last = focusable[focusable.length - 1];
        if (!first || !last) return;
        if (event.shiftKey && document.activeElement === first) {
          event.preventDefault();
          last.focus();
        } else if (!event.shiftKey && document.activeElement === last) {
          event.preventDefault();
          first.focus();
        }
      }
    });

    // The browser's own Escape handling too: the dialog stays open while the uninstall request runs.
    dialog.addEventListener("cancel", event => { if (subPending) event.preventDefault(); });
    dialog.addEventListener("close", () => {
      dialog.remove();
      if (opener.isConnected) opener.focus();
      if (!settled) {
        settled = true;
        resolve();
      }
    });

    document.body.append(dialog);
    dialog.showModal();
    cancelBtn.focus();
  });
}

export async function settingsView(
  host: HTMLElement,
  rpc: RpcClient,
  companies: readonly { id: number; name: string }[],
  run: (job: () => Promise<void>) => void,
  notice: (message: string) => void,
  context: Record<string, unknown>,
  scopeKinds: readonly unknown[],
  expectedRegistry?: string,
  controlsHost?: HTMLElement,
  panelHost?: HTMLElement,
  initialApp: string | null = null,
  options: SettingsViewOptions = {}
): Promise<ServiceController> {
  let scope: Record<string, unknown> = { kind: "global" }, revision = "", baseline = new Map<string, unknown>(), edits = new Map<string, unknown>(), pending = false, ambiguous = false, generation = 0;
  let currentRegistry = expectedRegistry;
  let declarations: Record<string, unknown>[] = [];
  // An app without a root
  // menu takes its addon's manifest icon from the catalogue, read once; when that read fails it keeps its letter.
  let addonIcons: ReadonlyMap<string, string> | null = null;
  const readAddonIcons = async (): Promise<ReadonlyMap<string, string>> => {
    try {
      const catalog = object(await rpc.call("ir.module.module", "erp_available_addons", { context }), "addon catalogue");
      const addons = array(catalog["addons"], "addons").map(raw => object(raw, "addon"));
      return new Map(addons.filter(addon => addon["icon"] !== undefined).map(addon => [text(addon["name"], "addon name"), text(addon["icon"], "addon icon")]));
    } catch (error) {
      console.warn(`Could not load Settings app icons from the addon catalogue: ${(error as Error).message || "the addon catalogue could not be read"}`);
      return new Map();
    }
  };
  let disposed = false;
  const choiceControls: { control: ChoiceDropdown; readonly: boolean }[] = [];
  const disposeChoices = (): void => { for (const { control } of choiceControls.splice(0)) control.dispose(); };
  let navigationObserver: ResizeObserver | undefined;
  const dispose = (): void => { if (disposed) return; disposed = true; generation++; navigationObserver?.disconnect(); disposeChoices(); };

  let items: SettingsItem[] = [], linkPending = false;
  const appGroups = new Map<string, AppGroupInfo>();
  const appsByScope = new Map<string, Set<string>>();
  // An addon's own
  // Configuration > Settings menu opens on its app (`app` widget option); without one the first app opens.
  let selectedApp: string | null = initialApp, firstDraw = true;

  const sheet = el("div", undefined, "content-sheet settings-sheet");
  const form = el("form", undefined, "record-form settings-form");
  const actions = el("div", undefined, "settings-toolbar-actions");

  const scopeChoices: { value: string; label: string }[] = [];
  if (scopeKinds.includes("global")) scopeChoices.push({ value: "global", label: "Global settings" });
  // The shell owns company selection. Several checked companies still have one current company.
  const currentId = options.navigation?.currentCompanyId ?? (companies.length === 1 ? companies[0]?.id : undefined);
  const currentCompany = companies.find(company => company.id === currentId);
  if (scopeKinds.includes("company") && companies.length) {
    if (!currentCompany) throw new ClientError("server", "The current company is unavailable for Settings.");
    scopeChoices.push({ value: String(currentCompany.id), label: "Current company" });
  }
  if (!scopeChoices.length) throw new ClientError("server", "No supported settings scope was declared.");
  // Open a business app in the operator's current company. The general Settings menu starts shared.
  // Remember the scope kind for this tab, never a company id. A shell company switch reloads the page.
  const preferenceKey = options.navigation?.scopePreferenceKey;
  let preferCompany = Boolean(initialApp);
  if (preferenceKey) {
    try {
      const saved = window.sessionStorage.getItem(preferenceKey);
      if (saved === "global" || saved === "company") preferCompany = saved === "company";
    } catch (error) { console.warn("Could not read the Settings scope preference.", error); }
  }
  const companyChoice = preferCompany && scopeChoices.find(choice => choice.value === String(currentId));
  const firstScope = companyChoice ? companyChoice.value : scopeChoices[0]!.value;
  scope = firstScope === "global" ? { kind: "global" } : { kind: "company", company_id: Number(firstScope) };

  const scopeKey = (value: Record<string, unknown>): string => value["kind"] === "global" ? "global" : String(value["company_id"]);
  const checkScope = (value: unknown, requested: Record<string, unknown>): void => {
    const actual = object(value, "settings scope");
    if (actual["kind"] !== requested["kind"] || actual["company_id"] !== requested["company_id"] || Object.keys(actual).length !== Object.keys(requested).length) throw new ClientError("server", "Settings response scope does not match the request.");
  };

  const toolbar = el("div", undefined, "settings-toolbar");
  // Segmented control (our own, a radio group): the scope switch is never a native select. One scope needs no switch.
  const scopeGroup = el("div", undefined, "settings-toolbar-scope settings-scope");
  scopeGroup.setAttribute("role", "radiogroup");
  scopeGroup.setAttribute("aria-label", "Settings scope");
  const scopeButtons = scopeChoices.map(choice => {
    const segment = el("button", choice.label, "settings-scope-option") as HTMLButtonElement;
    segment.type = "button"; segment.setAttribute("role", "radio"); segment.dataset["scope"] = choice.value;
    if (choice.value !== "global" && currentCompany) segment.title = currentCompany.name;
    return segment;
  });
  const showScope = (value: string): void => {
    for (const segment of scopeButtons) {
      const on = segment.dataset["scope"] === value;
      segment.setAttribute("aria-checked", String(on)); segment.tabIndex = on ? 0 : -1;
    }
  };
  showScope(firstScope);
  scopeGroup.append(...scopeButtons);
  scopeGroup.hidden = scopeChoices.length < 2;
  toolbar.append(actions, scopeGroup);

  if (controlsHost) {
    controlsHost.replaceChildren(toolbar);
  } else {
    sheet.append(toolbar);
  }

  // Search box setup
  const searchBox = el("div", undefined, "settings-search-box");
  const searchInput = el("input", undefined, "search-input");
  searchInput.type = "search";
  searchInput.placeholder = "Search settings…";
  searchInput.setAttribute("aria-label", "Search settings");
  searchBox.append(searchInput);

  if (panelHost) {
    const searchArea = panelHost.querySelector("[data-control-search]") || panelHost;
    searchArea.replaceChildren(searchBox);
  }

  const container = el("div", undefined, "settings-container");
  const sidebar = el("aside", undefined, "settings-sidebar");
  sidebar.setAttribute("aria-label", "Settings apps");
  const sidebarList = el("nav", undefined, "settings-sidebar-list");
  sidebarList.setAttribute("role", "tablist");
  sidebar.append(sidebarList);

  // Keep the active choices in view when the desktop rail becomes a horizontal phone strip.
  const revealSelection = (strip: HTMLElement, selector: string): void => {
    const choice = strip.querySelector<HTMLElement>(selector);
    if (!choice || strip.clientWidth === 0 || strip.scrollWidth <= strip.clientWidth) return;
    const frame = strip.getBoundingClientRect(), selected = choice.getBoundingClientRect();
    if (selected.left < frame.left) strip.scrollLeft += selected.left - frame.left;
    else if (selected.right > frame.right) strip.scrollLeft += selected.right - frame.right;
  };
  const revealNavigation = (): void => {
    revealSelection(sidebarList, '[aria-selected="true"]');
    revealSelection(scopeGroup, '[aria-checked="true"]');
  };
  if (typeof ResizeObserver !== "undefined") {
    navigationObserver = new ResizeObserver(revealNavigation);
    navigationObserver.observe(sidebarList);
    navigationObserver.observe(scopeGroup);
  }

  const content = el("div", undefined, "settings-content");
  content.setAttribute("role", "region");
  content.setAttribute("aria-label", "Settings content");
  content.append(el("p", "Loading settings…", "settings-loading"));

  const noResults = el("div", undefined, "settings-no-results");
  noResults.style.display = "none";
  const noResultsText = el("p", "No settings match your search.", "muted");
  noResults.append(noResultsText);
  content.append(noResults);

  container.append(sidebar, content);
  form.append(container);
  sheet.append(form);
  host.append(sheet);

  const disable = (): void => {
    if (disposed) return;
    for (const raw of sheet.querySelectorAll("input,select,button")) {
      const input = raw as HTMLInputElement;
      input.disabled = pending || input.dataset["readonly"] === "true" || linkPending && input.hasAttribute("data-setting-link");
    }
    for (const raw of toolbar.querySelectorAll("input,select,button")) {
      const input = raw as HTMLInputElement;
      input.disabled = pending || input.dataset["readonly"] === "true" || linkPending && input.hasAttribute("data-setting-link");
    }
    for (const entry of choiceControls) entry.control.setDisabled(pending || entry.readonly);
  };

  const operation = async (job: () => Promise<void>): Promise<void> => {
    if (disposed) throw new ClientError("validation", "This settings view is closed.");
    if (pending) throw new ClientError("validation", "Wait for the settings operation to finish.");
    pending = true; content.setAttribute("aria-busy", "true"); disable();
    try { await job(); } finally { pending = false; content.setAttribute("aria-busy", "false"); disable(); }
  };

  const performFilter = (queryText: string): void => {
    const query = queryText.trim().toLowerCase();
    let totalMatches = 0;
    for (const group of appGroups.values()) {
      let appMatches = 0;
      for (const bg of group.blockGroups) {
        let blockMatches = 0;
        const blockTitleMatches = Boolean(query && bg.titleText.toLowerCase().includes(query));
        for (const blockInfo of bg.blocks) {
          const { element, titleEl, helpEl, labelText, helpText, keyText } = blockInfo;
          const matches = !query ||
            blockTitleMatches ||
            Boolean(blockInfo.searchText?.toLowerCase().includes(query)) ||
            labelText.toLowerCase().includes(query) ||
            keyText.toLowerCase().includes(query) ||
            (helpText !== undefined && helpText.toLowerCase().includes(query));
          if (matches) {
            element.style.display = "";
            blockMatches++;
            appMatches++;
            totalMatches++;
            renderHighlightedText(titleEl, labelText, query);
            if (helpEl && helpText) renderHighlightedText(helpEl, helpText, query);
          } else {
            element.style.display = "none";
            titleEl.textContent = labelText;
            if (helpEl && helpText) helpEl.textContent = helpText;
          }
        }
        bg.element.style.display = blockMatches > 0 || !query ? "" : "none";
      }
      group.section.style.display = query ? (appMatches > 0 ? "" : "none") : (group.appKey === selectedApp ? "" : "none");
      group.tab.style.display = appMatches > 0 || !query ? "" : "none";
    }
    noResults.style.display = totalMatches === 0 && query ? "" : "none";
    if (totalMatches === 0 && query) noResultsText.textContent = `No settings match “${queryText.trim()}”. Check the spelling, try a shorter word, or clear the search to see every setting.`;
  };

  const clearFilter = (): void => { searchInput.value = ""; performFilter(""); };
  const selectApp = (appKey: string): void => {
    selectedApp = appKey;
    for (const group of appGroups.values()) {
      group.tab.classList.toggle("active", group.appKey === appKey);
      group.tab.setAttribute("aria-selected", String(group.appKey === appKey));
    }
    clearFilter();
    revealNavigation();
  };

  searchInput.addEventListener("input", () => {
    performFilter(searchInput.value);
  });
  searchInput.addEventListener("keydown", event => {
    if (event.key === "Escape" && searchInput.value) { event.preventDefault(); clearFilter(); }
  });

  const draw = (): void => {
    if (disposed) return;
    disposeChoices();
    sidebarList.replaceChildren();
    content.replaceChildren(noResults);
    appGroups.clear();

    const grouped = new Map<string, Record<string, unknown>[]>();
    for (const choice of scopeChoices) {
      for (const app of appsByScope.get(choice.value) ?? []) grouped.set(app, []);
    }
    const itemByKey = new Map(items.map(item => [item.key, item]));
    for (const declaration of [...declarations, ...items.map(item => ({ ...item }))]) {
      const appKey = text(declaration["app"], "setting app");
      if (!grouped.has(appKey)) grouped.set(appKey, []);
      grouped.get(appKey)!.push(declaration);
    }

    // A tab represents an installed contribution, even when its values use the other scope.
    const alternate = scopeChoices.find(choice => choice.value !== scopeKey(scope));
    if (selectedApp !== null && !grouped.has(selectedApp)) {
      // The addon's own settings menu names an app with no declared settings: a configuration error, said loudly.
      if (firstDraw) throw new ClientError("validation", `The addon '${selectedApp}' declares no settings for this page.`);
      selectedApp = null; // A scope switch can leave the app without settings; its first app opens instead.
    }
    selectedApp ??= grouped.has("base") ? "base" : grouped.keys().next().value ?? null;
    firstDraw = false;
    const orderedApps = [...grouped.entries()].sort(([a], [b]) =>
      a === "base" ? -1 : b === "base" ? 1 : 0);
    for (const [appKey, appDecls] of orderedApps) {
      const rootMenu = options.navigation?.roots.find(root => root.module === appKey);
      const appTitle = rootMenu?.label ?? humanizeAppTitle(appKey);
      const makeIcon = (): HTMLElement | SVGSVGElement => {
        const declared = rootMenu ? rootMenu.icon : addonIcons?.get(appKey);
        const tile = el("span", undefined, "settings-app-icon settings-menu-icon");
        tile.setAttribute("aria-hidden", "true"); tile.append(menuIcon(declared ?? null)); return tile;
      };
      const selected = appKey === selectedApp;

      const tab = el("button", undefined, "settings-app-tab") as HTMLButtonElement;
      tab.type = "button";
      tab.setAttribute("role", "tab");
      tab.setAttribute("data-app", appKey);
      tab.setAttribute("aria-selected", String(selected));
      tab.append(makeIcon(), el("span", appTitle, "settings-app-name"));
      if (selected) tab.classList.add("active");

      const section = el("section", undefined, "settings-section");
      section.setAttribute("data-app-section", appKey);
      if (!selected) section.style.display = "none";
      const heading = el("h2", undefined, "settings-section-title");
      heading.append(makeIcon(), el("span", appKey === "base" ? "General Settings" : appTitle));
      section.append(heading);

      // Group declarations by block with sequence
      interface RawBlockGroup {
        title: string;
        sequence: number;
        decls: Record<string, unknown>[];
      }
      const rawBlocks = new Map<string, RawBlockGroup>();
      for (const declaration of appDecls) {
        const blockTitle = text(declaration["block"], "setting block");
        const seq = integer(declaration["sequence"], "setting sequence");
        if (!rawBlocks.has(blockTitle)) {
          rawBlocks.set(blockTitle, { title: blockTitle, sequence: seq, decls: [] });
        }
        const bGroup = rawBlocks.get(blockTitle)!;
        if (seq < bGroup.sequence) bGroup.sequence = seq;
        bGroup.decls.push(declaration);
      }

      // Sort blocks by minimum sequence, then by title
      const sortedBlocks = [...rawBlocks.values()].sort((a, b) => a.sequence - b.sequence || a.title.localeCompare(b.title));

      const blockInfos: SettingBlockInfo[] = [];
      const blockGroupInfos: SettingBlockGroupInfo[] = [];

      for (const bGroup of sortedBlocks) {
        bGroup.decls.sort((a, b) => {
          const seqA = integer(a["sequence"], "setting sequence");
          const seqB = integer(b["sequence"], "setting sequence");
          if (seqA !== seqB) return seqA - seqB;
          return String(a["key"]).localeCompare(String(b["key"]));
        });

        const blockContainer = el("div", undefined, "settings-block-group");
        blockContainer.setAttribute("data-block", bGroup.title);
        const blockHeading = el("h3", bGroup.title, "settings-block-title");
        blockContainer.append(blockHeading);

        const blocksContainer = el("div", undefined, "settings-blocks");
        const currentBlockInfos: SettingBlockInfo[] = [];
        // An action link belongs to the information item it follows: both sit in one box, the link under the value.
        let infoBox: { body: HTMLElement; info: SettingBlockInfo; links?: HTMLElement } | undefined;

        for (const declaration of bGroup.decls) {
          const key = text(declaration["key"], "setting key");
          const labelStr = text(declaration["label"], "setting label");
          const kind = text(declaration["kind"], "setting type");
          const item = itemByKey.get(key);
          if (item) {
            if (item.kind === "module" || item.kind === "group") {
              const row = el("div", undefined, `setting-block ${item.kind === "module" ? "settings-module-toggle" : "settings-group-toggle"}`);
              row.dataset["settingKey"] = key;
              if (item.kind === "module") { row.dataset["moduleAddon"] = item.addon; row.dataset["moduleState"] = item.state; }
              else { row.dataset["group"] = item.group; row.dataset["groupState"] = item.state; }
              // An unavailable group that is on for every internal user shows on and can be switched off, never on.
              const offOnly = item.kind === "group" && item.state === "not_available" && item.on_for_all;
              const on = item.kind === "module" ? item.state === "installed" : item.state === "enabled" || offOnly;
              const technical = item.kind === "module" ? item.addon : item.group;
              const unavailable = item.state === "not_available" && !offOnly;
              const showReason = item.state === "not_available";
              if (showReason) row.classList.add("is-unavailable");
              const leftPane = el("div", undefined, "setting-left-pane");
              const rightPane = el("div", undefined, "setting-right-pane");
              const checkbox = el("input");
              checkbox.type = "checkbox";
              checkbox.id = `setting-${key}`;
              checkbox.name = key;
              checkbox.checked = on;
              if (unavailable) {
                checkbox.disabled = true;
                checkbox.dataset["readonly"] = "true";
              }
              checkbox.addEventListener("change", () => {
                const wanted = checkbox.checked;
                // Keep the confirmed server state until the change completes.
                checkbox.checked = on;
                if (disposed || pending || unavailable || (offOnly && wanted)) return;
                run(async () => {
                  try { await (item.kind === "module" ? toggleModule(item, wanted, checkbox) : toggleGroup(item, wanted)); } finally { focusToggle(key); }
                });
              });
              const toggleLabel = labelled("", checkbox);
              toggleLabel.classList.add("settings-toggle");
              leftPane.append(toggleLabel);
              const titleRow = el("div", undefined, "setting-title-row");
              const labelEl = el("label", item.label, "setting-label-text");
              labelEl.htmlFor = checkbox.id;
              titleRow.append(labelEl);
              rightPane.append(titleRow);
              let helpEl: HTMLElement | undefined;
              let helpText: string | undefined;
              if (showReason) {
                helpText = item.kind === "group" && item.reason !== null ? item.reason : `Not available in this server (${technical}).`;
                helpEl = el("div", helpText, "setting-help text-muted");
                helpEl.id = `setting-help-${key}`;
                checkbox.setAttribute("aria-describedby", helpEl.id);
                rightPane.append(helpEl);
              }
              row.append(leftPane, rightPane);
              blocksContainer.append(row);
              infoBox = undefined;
              const info: SettingBlockInfo = {
                element: row,
                titleEl: labelEl,
                ...(helpEl ? { helpEl, helpText } : {}),
                labelText: item.label,
                keyText: key,
                searchText: technical,
              };
              blockInfos.push(info);
              currentBlockInfos.push(info);
              continue;
            }
            const row = el("div", undefined, "setting-block settings-panel-item");
            row.dataset["settingKey"] = key;
            const body = el("div", undefined, "setting-right-pane");
            const label = el("span", item.label, "setting-label-text");
            let detail: HTMLElement | undefined;
            if (item.kind === "info") {
              const title = el("div", undefined, "setting-title-row");
              // A text value names itself ("Main Company"): its label stays for assistive technology and the search only.
              if (typeof item.value === "string") label.classList.add("visually-hidden");
              title.append(el("span", String(item.value), "settings-info-value"), label); body.append(title);
              if (item.detail !== null) { detail = el("div", item.detail, "setting-help settings-info-detail"); body.append(detail); }
            } else {
              const navigation = options.navigation;
              if (!navigation) throw new ClientError("unsupported", "Settings links require the application's navigation service.");
              const link = el("button", undefined, "link settings-action-link") as HTMLButtonElement;
              link.type = "button"; link.setAttribute("data-setting-link", "");
              const arrow = icon("chevron-right"); arrow.setAttribute("aria-hidden", "true"); link.append(arrow, label);
              link.addEventListener("click", () => {
                if (disposed || pending || linkPending) return;
                // Do not use operation(): canLeave must be able to call this service's Save.
                // Set inside the job: `run` may refuse it (a busy screen), and then nothing would clear the flag.
                run(async () => { linkPending = true; disable(); try { if (!disposed) await navigation.openTarget(item.target); } finally { linkPending = false; disable(); } });
              });
              if (infoBox) {
                // Found together by the search: the box by its link's name, the link by its box's text.
                infoBox.links ??= infoBox.body.appendChild(el("div", undefined, "settings-panel-links"));
                const entry = el("div", undefined, "settings-panel-link"); entry.dataset["settingKey"] = key;
                entry.append(link); infoBox.links.append(entry);
                const info: SettingBlockInfo = { element: entry, titleEl: label, labelText: item.label, keyText: key, searchText: `${infoBox.info.labelText} ${infoBox.info.keyText} ${infoBox.info.searchText ?? ""}` };
                infoBox.info.searchText = `${infoBox.info.searchText ?? ""} ${item.label} ${key}`;
                blockInfos.push(info); currentBlockInfos.push(info); continue;
              }
              body.append(link);
            }
            row.append(body); blocksContainer.append(row);
            const info: SettingBlockInfo = { element: row, titleEl: label, labelText: item.label, keyText: key,
              ...(detail ? { helpEl: detail, helpText: item.kind === "info" ? item.detail! : "" } : {}),
              searchText: item.kind === "info" ? `${String(item.value)} ${item.detail ?? ""}` : item.label };
            infoBox = item.kind === "info" ? { body, info } : undefined;
            blockInfos.push(info); currentBlockInfos.push(info); continue;
          }
          infoBox = undefined;
          const choices = declaration["choices"];
          const current = baseline.get(key);
          const rawHelp = declaration["help"];
          if (rawHelp !== undefined && rawHelp !== null && (typeof rawHelp !== "string" || !rawHelp || rawHelp.length > 1024)) throw new ClientError("server", "Invalid setting help.");
          const helpStr = typeof rawHelp === "string" ? rawHelp : undefined;

          if ((kind === "selection") !== (choices !== null)) throw new ClientError("server", "Setting choices do not match the declared type.");
          if (kind === "boolean" && typeof current !== "boolean" || ["integer", "float"].includes(kind) && typeof current !== "number" || ["string", "selection"].includes(kind) && typeof current !== "string") throw new ClientError("server", "Setting value does not match the declared type.");

          const blockEl = el("div", undefined, "setting-block");
          blockEl.setAttribute("data-setting-key", key);

          const leftPane = el("div", undefined, "setting-left-pane");
          const rightPane = el("div", undefined, "setting-right-pane");

          const titleRow = el("div", undefined, "setting-title-row");
          const titleSpan = el("label", labelStr, "setting-label-text");
          titleRow.append(titleSpan);
          rightPane.append(titleRow);

          let helpEl: HTMLElement | undefined;
          if (helpStr) {
            helpEl = el("div", helpStr, "setting-help text-muted");
            rightPane.append(helpEl);
          }

          if (typeof declaration["writable"] !== "boolean") throw new ClientError("server", "Invalid settings write permission.");
          const writable = declaration["writable"];
          const edit = (value: unknown): void => {
            if (disposed || pending || !writable) return;
            if (Object.is(value, baseline.get(key))) edits.delete(key); else edits.set(key, value);
          };
          let input: HTMLInputElement | HTMLButtonElement;
          let inputNode: HTMLElement;
          if (choices !== null) {
            const parsed = array(choices, "setting choices").map(raw => {
              const pair = array(raw, "setting choice");
              return { value: text(pair[0], "choice value"), label: text(pair[1], "choice label") };
            });
            if (!parsed.some(choice => choice.value === current)) throw new ClientError("server", "Setting value is outside its declared choices.");
            const control = createChoiceDropdown({ label: labelStr, choices: parsed, value: String(current), onChange: edit });
            choiceControls.push({ control, readonly: !writable });
            input = control.trigger; inputNode = control.element;
          } else {
            if (!["boolean", "integer", "float", "string"].includes(kind)) throw new ClientError("unsupported", `Unsupported setting type: ${kind}.`);
            const field = el("input");
            field.type = kind === "boolean" ? "checkbox" : kind === "integer" || kind === "float" ? "number" : "text";
            if (kind === "integer" || kind === "float") field.required = true;
            if (kind === "float") field.step = "any";
            field.checked = current === true; field.value = String(current);
            field.addEventListener("change", () => {
              if (disposed || pending || !writable) {
                const value = edits.has(key) ? edits.get(key) : baseline.get(key);
                field.value = String(value); field.checked = value === true; return;
              }
              edit(kind === "boolean" ? field.checked : kind === "integer" || kind === "float" ? Number(field.value) : field.value);
            });
            input = field; inputNode = field;
          }
          input.id = `setting-${key}`; titleSpan.htmlFor = input.id; input.name = key;
          if (helpEl) {
            helpEl.id = `setting-help-${key}`; input.setAttribute("aria-describedby", helpEl.id);
          }
          if (helpEl && kind !== "selection") {
            const defaultValue = declaration["default"];
            if (typeof defaultValue !== typeof current || typeof defaultValue === "number" && (!Number.isFinite(defaultValue) || kind === "integer" && !Number.isSafeInteger(defaultValue))) throw new ClientError("server", "Invalid setting default.");
            const reset = el("button", "Use default", "settings-action-link");
            reset.type = "button"; reset.dataset["settingDefault"] = key; reset.dataset["readonly"] = String(!writable);
            reset.setAttribute("aria-label", `Use default for ${labelStr}`);
            reset.addEventListener("click", () => {
              if (disposed || pending || !writable) return;
              const value = declaration["default"];
              edit(value);
              if (input instanceof HTMLInputElement) { input.value = String(value); input.checked = value === true; }
              input.focus();
            });
            rightPane.append(reset);
          }
          input.dataset["readonly"] = String(!writable);

          if (kind === "boolean") {
            const toggleLabel = labelled("", input);
            toggleLabel.classList.add("settings-toggle");
            leftPane.append(toggleLabel);
          } else {
            const inputWrapper = el("div", undefined, "setting-value-input");
            inputWrapper.append(inputNode);
            rightPane.append(inputWrapper);
          }

          blockEl.append(leftPane, rightPane);
          blocksContainer.append(blockEl);

          const info: SettingBlockInfo = {
            element: blockEl,
            titleEl: titleSpan,
            helpEl,
            labelText: labelStr,
            helpText: helpStr,
            keyText: key,
          };
          blockInfos.push(info);
          currentBlockInfos.push(info);
        }

        blockContainer.append(blocksContainer);
        section.append(blockContainer);
        blockGroupInfos.push({
          element: blockContainer,
          titleText: bGroup.title,
          blocks: currentBlockInfos,
        });
      }

      if (!appDecls.length && alternate) {
        const open = button(alternate.label, () => switchScope(alternate.value, appKey), "settings-link");
        section.append(el("p", "These settings are available in the other scope.", "settings-empty"), open);
      }
      content.append(section);
      sidebarList.append(tab);

      const groupInfo: AppGroupInfo = {
        appKey,
        appTitle,
        section,
        tab,
        blocks: blockInfos,
        blockGroups: blockGroupInfos,
      };
      appGroups.set(appKey, groupInfo);

      tab.addEventListener("click", () => {
        if (!appDecls.length && alternate) { switchScope(alternate.value, appKey); return; }
        selectApp(appKey); content.scrollTop = 0;
      });
    }

    if (!declarations.length && !items.length) {
      content.append(el("p", "No settings are available for this scope.", "settings-empty"));
    }
    if (!panelHost) {
      content.append(searchBox);
    }

    revealNavigation();
    disable();
    if (searchInput.value) {
      performFilter(searchInput.value);
    }
  };

  const describe = async (immutableScope: Record<string, unknown>) => {
    const description = object(await rpc.call("ir.config.setting", "erp_describe", { scope: immutableScope, context }));
    if (description["wire_major"] !== 1 || currentRegistry !== undefined && description["registry_version"] !== currentRegistry) throw new ClientError("unsupported", "Settings changed. Reload the application.");
    checkScope(description["scope"], immutableScope);
    const nextDeclarations = array(description["declarations"], "setting declarations").map(value => object(value));
    const keys = nextDeclarations.map(value => text(value["key"], "setting key"));
    // The server always places a setting (app, block, sequence); a missing one is a server fault, never a regrouping to "General".
    for (const value of nextDeclarations) {
      if (!text(value["app"], "setting app") || !text(value["block"], "setting block")) throw new ClientError("server", `Setting ${String(value["key"])} has an empty app or block.`);
      integer(value["sequence"], "setting sequence");
    }
    if (new Set(keys).size !== keys.length) throw new ClientError("server", "Duplicate settings declaration.");
    const nextItems = readSettingsItems(description, keys);
    if (nextItems.some(item => item.kind === "link") && !options.navigation) throw new ClientError("unsupported", "Settings links require the application's navigation service.");
    const apps = new Set([...nextDeclarations.map(value => text(value["app"], "setting app")), ...nextItems.map(item => item.app)]);
    return { nextDeclarations, nextItems, keys, apps };
  };

  const load = async (requested: Record<string, unknown>, entry = false, preferredApp?: string): Promise<void> => {
    const request = ++generation;
    let immutableScope = { ...requested }, current = await describe(immutableScope);
    appsByScope.set(scopeKey(immutableScope), current.apps);
    const alternate = scopeChoices.find(choice => choice.value !== scopeKey(immutableScope));
    if (entry && alternate) {
      const other = alternate.value === "global" ? { kind: "global" } : { kind: "company", company_id: currentId };
      // Read authorized app names for the rail, keeping values and writes in one explicit scope.
      try {
        const otherDescription = await describe(other);
        appsByScope.set(alternate.value, otherDescription.apps);
        if (initialApp && !current.apps.has(initialApp) && otherDescription.apps.has(initialApp)) {
          current = otherDescription; immutableScope = other;
        }
      } catch (error) {
        if (!(error instanceof ClientError && error.kind === "permission")) throw error;
      }
    }
    const {nextDeclarations, nextItems, keys} = current;
    const response = object(await rpc.call("ir.config.setting", "erp_read_values", { scope: immutableScope, keys, context }));
    if (response["wire_major"] !== 1) throw new ClientError("unsupported", "Unsupported settings version.");
    checkScope(response["scope"], immutableScope);
    const nextRevision = text(response["revision"], "settings revision"), entries = array(response["values"], "settings values"), nextValues = new Map<string, unknown>();
    for (const raw of entries) {
      const entry = object(raw), key = text(entry["key"], "setting key");
      if (!keys.includes(key) || nextValues.has(key) || entry["source"] !== "stored" && entry["source"] !== "default") throw new ClientError("server", "Unexpected settings value or source.");
      nextValues.set(key, entry["value"]);
    }
    if (nextValues.size !== keys.length) throw new ClientError("server", "Settings response omitted a requested key.");
    const apps = [...nextDeclarations, ...nextItems.map(item => ({ ...item }) as Record<string, unknown>)].map(value => text(value["app"], "setting app"));
    if (addonIcons === null && apps.some(app => !options.navigation?.roots.some(root => root.module === app))) addonIcons = await readAddonIcons();
    if (disposed || request !== generation) return;
    if (preferredApp) selectedApp = preferredApp;
    declarations = nextDeclarations; items = nextItems; baseline = nextValues; revision = nextRevision; scope = immutableScope; showScope(scopeKey(scope)); edits.clear(); ambiguous = false; draw();
  };

  // A redraw replaces the row: keyboard focus goes back to the toggle (or its label when the row is disabled), found again by its settings key.
  const focusToggle = (key: string): void => {
    if (disposed) return;
    const row = [...form.querySelectorAll<HTMLElement>(".setting-block[data-setting-key]")].find(candidate => candidate.dataset["settingKey"] === key);
    const checkbox = row?.querySelector<HTMLInputElement>('input[type="checkbox"]');
    if (checkbox && !checkbox.disabled) { checkbox.focus(); return; }
    // The row came back unavailable (disabled controls cannot take focus): the label keeps focus on the row.
    const label = row?.querySelector<HTMLElement>(".setting-label-text");
    if (label) { label.tabIndex = -1; label.focus(); }
  };

  // After a failed install or uninstall the page may be stale; say so when it cannot be read again (the caller rethrows).
  const reloadAfterFailure = async (): Promise<void> => {
    try { await load(scope, false, selectedApp ?? undefined); } catch { notice("Settings could not be reloaded. Reload the application."); }
  };

  // A successful install or uninstall publishes a new registry: the whole application reloads, as the Addons page does.
  const toggleModule = async (item: Extract<SettingsItem, { kind: "module" }>, install: boolean, opener: HTMLElement): Promise<void> => {
    await operation(async () => {
      if (edits.size && await choose("Unsaved changes", "Discard changes before installing or uninstalling an addon?", ["Discard", "Cancel"]) !== "Discard") return;
      if (install) {
        if (await choose("Install addon?", `Install ${item.label} (${item.addon}) and its dependencies?`, ["Install", "Cancel"]) !== "Install") return;
        try {
          await rpc.post("/erp/module/install", { modules: [item.addon] }, { write: true });
        } catch (error) {
          notice("Installation status may have changed. Reloading settings…");
          await reloadAfterFailure();
          throw error;
        }
        notice(`${item.label} installed. Reloading the application…`);
        window.location.reload();
        return;
      }
      let impact: Record<string, unknown>;
      try {
        impact = object(await rpc.call("ir.module.module", "erp_preview_uninstall", { modules: [item.addon], context }));
      } catch (error) {
        // A refused preview says why in the server's own words.
        if (error instanceof ClientError && error.kind === "validation") { notice(error.message || UNINSTALL_UNAVAILABLE); return; }
        notice(`Uninstall preview failed: ${(error as Error).message || "Unable to compute uninstall impact."}`);
        throw error;
      }
      await confirmUninstallDialog(
        rpc,
        { name: item.addon, label: item.label },
        impact,
        opener,
        () => {
          notice(`${item.label} uninstalled. Reloading the application…`);
          window.location.reload();
        },
        async (error) => {
          const conflict = error instanceof ClientError && error.status === 409;
          if (conflict) {
            notice("Database state changed since preview. A fresh confirmation is required.");
          } else {
            notice("Uninstall status may have changed. Reloading settings…");
          }
          await reloadAfterFailure();
          return conflict;
        }
      );
    });
  };

  // The server changes the internal-user group for the declared item, checks who may and audits it. The whole
  // application reloads: menus, views and fields follow the new groups.
  const toggleGroup = async (item: Extract<SettingsItem, { kind: "group" }>, enable: boolean): Promise<void> => {
    await operation(async () => {
      if (edits.size && await choose("Unsaved changes", "Discard changes before changing a feature for all users?", ["Discard", "Cancel"]) !== "Discard") return;
      const verb = enable ? "Turn on" : "Turn off";
      if (await choose(`${verb} ${item.label}?`, `${verb} ${item.label} for every internal user? The application reloads afterwards.`, [verb, "Cancel"]) !== verb) return;
      try {
        await rpc.call("ir.config.setting", "erp_toggle_group", { key: item.key, enabled: enable, context }, { write: true });
      } catch (error) {
        await reloadAfterFailure();
        throw error;
      }
      notice(`${item.label} turned ${enable ? "on" : "off"}. Reloading the application…`);
      window.location.reload();
    });
  };

  const save = async (): Promise<void> => {
    if (disposed) throw new ClientError("validation", "This settings view is closed.");
    // Hidden blocks cannot show a validation bubble: clear the search and open the app of the first invalid setting.
    if (searchInput.value) clearFilter();
    const invalid = [...form.querySelectorAll<HTMLInputElement | HTMLSelectElement>("input,select")].find(input => !input.checkValidity());
    const invalidApp = invalid?.closest<HTMLElement>("[data-app-section]")?.dataset["appSection"];
    if (invalidApp !== undefined) selectApp(invalidApp);
    if (!form.reportValidity()) throw new ClientError("validation", "Correct the invalid settings before saving.");
    return operation(async () => {
      if (ambiguous) throw new ClientError("ambiguous", "Discard to reload the settings and check the previous save before trying again.");
      if (!edits.size) return;
      if (!form.reportValidity()) throw new ClientError("validation", "Correct the invalid settings before saving.");
      const requested = { ...scope }, changes = [...edits].map(([key, value]) => ({ key, value }));
      try {
        const raw = await rpc.call("ir.config.setting", "erp_write_values", { scope: requested, expected_revision: revision, changes, context }, { write: true });
        ambiguous = true;
        const response = object(raw);
        if (response["wire_major"] !== 1) throw new ClientError("ambiguous", "Invalid settings save response. Reload to check the result.");
        checkScope(response["scope"], requested); text(response["revision"], "settings revision");
        await load(requested); if (!disposed) notice("Settings saved.");
      } catch (error) {
        if (error instanceof ClientError && (error.kind === "ambiguous" || error.status >= 500)) ambiguous = true;
        if (ambiguous) throw new ClientError("ambiguous", "The settings save could not be confirmed. Discard to reload the settings before another write.");
        throw error;
      }
    });
  };

  // The Discard button drops the edits and reads the stored values again (what the removed Reload button did): that also
  // recovers from an unconfirmed save (`ambiguous` blocks writes) and from a stale revision after another user saved.
  const discard = (): void => { if (disposed) return; if (pending) { notice("Wait for the settings operation to finish."); return; } edits.clear(); draw(); };
  const discardAndReload = (): void => run(() => operation(() => load(scope)));

  actions.append(
    actionButton("Save", () => run(save), "primary"),
    actionButton("Discard", discardAndReload)
  );

  const switchScope = (selected: string, preferredApp?: string): void => {
    if (pending || selected === scopeKey(scope)) return;
    run(() => operation(async () => {
      if (edits.size && await choose("Unsaved changes", "Discard changes before switching scope?", ["Discard", "Cancel"]) !== "Discard") return;
      const requested = selected === "global" ? { kind: "global" } : { kind: "company", company_id: integer(Number(selected), "company") };
      if (requested["kind"] === "company" && requested["company_id"] !== currentId) throw new ClientError("validation", "Unsupported company scope.");
      await load(requested, false, preferredApp);
      if (preferenceKey && !disposed) {
        try { window.sessionStorage.setItem(preferenceKey, selected === "global" ? "global" : "company"); }
        catch (error) { console.warn("Could not remember the Settings scope preference.", error); }
      }
    }));
  };
  scopeGroup.addEventListener("click", event => {
    const segment = (event.target as Element).closest<HTMLElement>("[data-scope]");
    if (segment) switchScope(segment.dataset["scope"]!);
  });
  scopeGroup.addEventListener("keydown", event => {
    const step = event.key === "ArrowRight" || event.key === "ArrowDown" ? 1 : event.key === "ArrowLeft" || event.key === "ArrowUp" ? -1 : 0;
    if (!step) return;
    event.preventDefault();
    const at = scopeButtons.findIndex(segment => segment === document.activeElement);
    const next = scopeButtons[(at + step + scopeButtons.length) % scopeButtons.length]!;
    next.focus(); next.click();
  });

  form.addEventListener("submit", event => { event.preventDefault(); run(save); });

  try { await operation(() => load(scope, true)); } catch (error) { dispose(); throw error; }
  return { busy: () => pending, dirty: () => edits.size > 0, save, discard, dispose };
}

// The server lifecycle states (crates/erp-modules/src/graph.rs ModuleState::as_str) with a label and a
// CSS-safe class token each; a state outside this table is a protocol error, never a default.
const ADDON_STATES: ReadonlyMap<string, { label: string; token: string }> = new Map([
  ["uninstalled", { label: "Not Installed", token: "uninstalled" }],
  ["installed", { label: "Installed", token: "installed" }],
  ["to install", { label: "To Install", token: "to-install" }],
  ["to upgrade", { label: "To Upgrade", token: "to-upgrade" }],
  ["to remove", { label: "To Remove", token: "to-remove" }],
  ["uninstallable", { label: "Not Installable", token: "uninstallable" }],
  ["unavailable", { label: "Unavailable", token: "unavailable" }],
]);
const INDUSTRIES = "Industries";
const INSTALLED_STATES = new Set(["installed", "to upgrade", "to remove"]);
const NOT_INSTALLED_STATES = new Set(["uninstalled", "uninstallable", "to install"]);
type AddonSection = "official" | "industries";
type AddonFilter = "app" | "extra" | "installed" | "not_installed";
type AddonGroupBy = "" | "category" | "state" | "author";
const ADDON_FILTERS: readonly (readonly [AddonFilter, string, number])[] = [
  ["app", "Apps", 0], ["extra", "Extra", 0], ["installed", "Installed", 1], ["not_installed", "Not Installed", 1],
];
const ADDON_GROUPS: readonly (readonly [AddonGroupBy, string])[] = [["", "No grouping"], ["category", "Category"], ["state", "Status"], ["author", "Author"]];
interface AddonRow {
  name: string; label: string; version: string; state: string; badge: { label: string; token: string };
  installable: boolean; summary: string; author: string; application: boolean;
  section: AddonSection; path: readonly string[];
  website: string; license: string; depends: readonly string[];
  /** The manifest's `lucide:<name>`; absent when the addon declares none (the tile shows its letter). */
  icon?: string;
}
// Icon tile colours: one of a fixed set, chosen from the technical name so an addon keeps its colour.
const ADDON_TONES = 8;
function addonTone(name: string): number {
  let hash = 0;
  for (const char of name) hash = (hash * 31 + char.charCodeAt(0)) >>> 0;
  return hash % ADDON_TONES;
}
// Only an absolute http(s) URL becomes a link; anything else is shown as text, never as an href.
function addonWebsite(value: string): URL | null {
  try {
    const url = new URL(value);
    return url.protocol === "https:" || url.protocol === "http:" ? url : null;
  } catch { return null; }
}
const categoryLabel = (row: AddonRow): string => [row.section === "industries" ? INDUSTRIES : "", ...row.path].filter(Boolean).join(" / ");

function addonRow(raw: unknown): AddonRow {
  const addon = object(raw);
  const name = text(addon["name"], "addon name");
  const state = text(addon["state"], "addon state");
  const badge = ADDON_STATES.get(state);
  if (badge === undefined) throw new ClientError("server", `Unknown addon state "${state}" for addon ${name}.`);
  const flag = (value: unknown, label: string): boolean => {
    if (typeof value !== "boolean") throw new ClientError("server", `Invalid ${label}.`);
    return value;
  };
  const parts = text(addon["category"], "addon category").split("/").map(part => part.trim()).filter(part => part !== "");
  const industry = parts[0] === INDUSTRIES;
  const path = industry ? parts.slice(1) : parts;
  return {
    name, state, badge,
    label: text(addon["label"], "addon label"),
    version: text(addon["version"], "version"),
    installable: flag(addon["installable"], "addon installable flag"),
    summary: text(addon["summary"], "addon summary"),
    author: text(addon["author"], "addon author"),
    application: flag(addon["application"], "addon application flag"),
    section: industry ? "industries" : "official",
    path: path.length > 0 ? path : ["Uncategorized"],
    website: text(addon["website"], "addon website"),
    license: text(addon["license"], "addon license"),
    depends: array(addon["depends"], "addon depends").map(item => text(item, "addon dependency")),
    ...(addon["icon"] === undefined ? {} : { icon: text(addon["icon"], "addon icon") }),
  };
}

export async function addonsView(
  host: HTMLElement,
  rpc: RpcClient,
  run: (job: () => Promise<void>) => void,
  notice: (message: string) => void,
  context: Record<string, unknown>
): Promise<ServiceController> {
  const sheet = el("div", undefined, "addons-view");
  host.append(sheet);
  let pending = false;
  let rows: AddonRow[] = [];
  // View state survives a catalog refresh, like a search panel selection.
  const view = { section: "official" as AddonSection, category: [] as string[], query: "", filters: new Set<AddonFilter>(["app"]), groupBy: "" as AddonGroupBy };

  const disable = (): void => {
    for (const control of sheet.querySelectorAll("button")) control.disabled = pending || control.dataset["locked"] === "true";
  };

  const operation = async (job: () => Promise<void>): Promise<void> => {
    if (pending) return;
    pending = true; disable();
    try { await job(); } finally { pending = false; disable(); }
  };

  const matchesSearch = (row: AddonRow): boolean => {
    const query = view.query.trim().toLowerCase();
    if (query && ![row.label, row.name, row.summary].some(value => value.toLowerCase().includes(query))) return false;
    for (const group of [0, 1]) {
      const active = ADDON_FILTERS.filter(([key, , g]) => g === group && view.filters.has(key)).map(([key]) => key);
      if (active.length === 0) continue;
      const hit = active.some(key => key === "app" ? row.application : key === "extra" ? !row.application : key === "installed" ? INSTALLED_STATES.has(row.state) : NOT_INSTALLED_STATES.has(row.state));
      if (!hit) return false;
    }
    return true;
  };
  const inCategory = (row: AddonRow, path: readonly string[]): boolean => path.every((part, index) => row.path[index] === part);

  const showInfo = (row: AddonRow, opener: HTMLElement): void => {
    const dialog = el("dialog", undefined, "addon-info-dialog");
    const heading = el("h2", row.label, "addon-info-title");
    heading.id = "addon-info-heading";
    dialog.setAttribute("aria-labelledby", heading.id);
    const head = el("header", undefined, "addon-info-head");
    const titles = el("div", undefined, "addon-info-titles");
    titles.append(heading, el("code", row.name, "addon-technical-name"));
    const headClose = button("×", () => dialog.close(), "message-dialog-close"); headClose.setAttribute("aria-label", "Close");
    head.append(tileIcon(row), titles, el("span", row.badge.label, `addon-badge addon-badge-${row.badge.token}`), headClose);
    const facts = el("dl", undefined, "addon-info-facts");
    const fact = (label: string, value: string | Node): void => {
      const term = el("dt", label), detail = el("dd");
      detail.append(value);
      facts.append(term, detail);
    };
    fact("Technical name", el("code", row.name));
    fact("Version", row.version || "—");
    fact("Category", categoryLabel(row));
    fact("Type", row.application ? "App" : "Extra");
    fact("Author", row.author || "—");
    fact("License", row.license || "—");
    const site = addonWebsite(row.website);
    if (site) {
      const link = el("a", site.host + (site.pathname === "/" ? "" : site.pathname), "addon-info-link");
      link.href = site.href; link.target = "_blank"; link.rel = "noopener noreferrer";
      fact("Website", link);
    } else fact("Website", row.website || "—");
    const depends = el("div", undefined, "addon-depends");
    if (row.depends.length === 0) depends.append(el("span", "None", "muted"));
    for (const name of row.depends) depends.append(el("code", name, "addon-depend"));
    fact("Depends on", depends);
    fact("Installable", row.installable ? "Yes" : "No");
    const body = el("div", undefined, "addon-info-body");
    body.append(el("p", row.summary || "No description provided.", "addon-info-summary"), facts);
    const footer = el("footer", undefined, "addon-info-footer");
    const close = button("Close", () => dialog.close());
    footer.append(close);
    dialog.append(head, body, footer);
    dialog.addEventListener("close", () => { dialog.remove(); if (opener.isConnected) opener.focus(); });
    document.body.append(dialog); dialog.showModal(); close.focus();
  };

  const tileIcon = (row: AddonRow): HTMLElement => {
    const tile = el("div", undefined, "addon-icon");
    if (row.icon === undefined) tile.dataset["letter"] = (row.label.charAt(0) || row.name.charAt(0)).toUpperCase();
    else { tile.classList.add("addon-icon-glyph"); tile.append(menuIcon(row.icon)); }
    tile.dataset["tone"] = String(addonTone(row.name));
    tile.setAttribute("aria-hidden", "true");
    return tile;
  };

  const stateBadge = (state: string): { label: string; token: string } => {
    const normalized = state.replace("_", " ");
    return ADDON_STATES.get(normalized) || ADDON_STATES.get(state) || { label: state, token: state.replace(/[\s_]+/g, "-") };
  };

  let detailGeneration = 0;
  const showDetail = async (row: AddonRow, opener: HTMLElement): Promise<void> => {
    const generation = ++detailGeneration;
    let detailData: Record<string, unknown>;
    try {
      detailData = object(await rpc.call("ir.module.module", "erp_addon_detail", { name: row.name, context }), "addon detail");
    } catch (error) {
      if (generation === detailGeneration) notice(`Failed to load addon details: ${(error as Error).message || "Unknown error"}`);
      return;
    }
    if (generation !== detailGeneration) return;

    const detailContainer = el("div", undefined, "addon-detail");

    const header = el("header", undefined, "addon-detail-head");
    const backBtn = button("Back", () => exitDetail(), "btn btn-secondary addon-detail-back");
    backBtn.prepend(icon("arrow-left"));

    const headMain = el("div", undefined, "addon-detail-head-main");
    const titles = el("div", undefined, "addon-detail-titles");
    const heading = el("h2", text(detailData["label"], "addon detail label"), "addon-detail-title");
    const stateStr = text(detailData["state"], "addon detail state");
    const badgeInfo = stateBadge(stateStr);
    const badge = el("span", badgeInfo.label, `addon-badge addon-badge-${badgeInfo.token}`);
    const titleRow = el("div", undefined, "addon-detail-title-row");
    titleRow.append(heading, badge);
    const technical = el("code", row.name, "addon-technical-name");
    technical.title = row.name;
    titles.append(titleRow, technical);

    const actions = el("div", undefined, "addon-detail-actions");
    const notes = el("div", undefined, "addon-detail-notes");
    const unavailable = (btn: HTMLButtonElement, id: string, reason: string): void => {
      btn.disabled = true;
      btn.dataset["locked"] = "true"; // keeps `disable()` from enabling it again when another operation ends
      btn.setAttribute("aria-describedby", id);
      const note = el("p", reason, "addon-refusal-note muted");
      note.id = id;
      notes.append(note);
    };
    if (stateStr === "uninstalled" && row.installable) {
      actions.append(button("Install", () => run(() => executeInstall(row)), "primary addon-install"));
    } else if (stateStr === "installed") {
      const upgradeBtn = button("Upgrade", () => run(() => executeUpgrade(row)), "primary addon-upgrade");
      const uninstallBtn = button("Uninstall", () => run(() => startUninstall(row, uninstallBtn)), "danger addon-uninstall");
      actions.append(upgradeBtn, uninstallBtn);
      const refusal = (key: string): string | null => { const value = detailData[key]; return value === null || value === undefined ? null : text(value, `addon detail ${key}`); };
      const upgradeRefusal = refusal("upgrade_refusal"), uninstallRefusal = refusal("uninstall_refusal");
      if (upgradeRefusal) unavailable(upgradeBtn, "addon-upgrade-unavailable", upgradeRefusal);
      if (uninstallRefusal) unavailable(uninstallBtn, "addon-uninstall-unavailable", uninstallRefusal);
    } else if (stateStr === "uninstalled") {
      notes.append(el("p", "This addon is marked as not installable, or its version does not fit this server.", "addon-transitional-note muted"));
    } else {
      notes.append(el("p", "Pending module operations require operator recovery. Refresh the catalog after background tasks finish.", "addon-transitional-note muted"));
    }

    headMain.append(tileIcon(row), titles, actions);
    header.append(backBtn, headMain, notes);

    const body = el("div", undefined, "addon-detail-body");
    const summaryText = text(detailData["summary"], "addon detail summary") || row.summary || "No description provided.";
    body.append(el("p", summaryText, "addon-detail-summary"));

    const facts = el("dl", undefined, "addon-detail-facts");
    const addFact = (label: string, val: string | Node): void => {
      const dd = el("dd");
      dd.append(val);
      const group = el("div", undefined, "addon-detail-fact");
      group.append(el("dt", label), dd);
      facts.append(group);
    };
    addFact("Version", String(detailData["available_version"] || row.version || "—"));
    addFact("Installed version", String(detailData["installed_version"] || "—"));
    addFact("Category", categoryLabel(row));
    addFact("Type", row.application ? "App" : "Extra");
    addFact("Author", text(detailData["author"], "addon detail author") || row.author || "—");
    addFact("License", row.license || "—");
    body.append(facts);

    const descText = text(detailData["description"], "addon detail description");
    if (descText) {
      body.append(el("p", descText, "addon-detail-description"));
    }

    const sections = el("div", undefined, "addon-detail-sections");
    const pageOf = (data: Record<string, unknown>, key: string): { items: unknown[]; cursor: string | null } => {
      const page = object(data[key], `addon ${key}`), cursor = page["next_cursor"];
      return { items: array(page["items"], `addon ${key}`), cursor: cursor === null ? null : text(cursor, `addon ${key} cursor`) };
    };
    // One collection (dependencies, dependents, menus, views): the first page comes with the detail, "Load more" asks
    // for the next one. A reply that arrives after another addon's detail was opened is dropped.
    const collection = (title: string, key: "dependencies" | "dependents" | "menus" | "views", empty: string, listClass: string, render: (item: unknown) => Node): void => {
      const section = el("section", undefined, "addon-detail-section");
      section.append(el("h3", title, "addon-section-title"));
      const list = el("div", undefined, listClass);
      const first = pageOf(detailData, key);
      if (first.items.length === 0) list.append(el("span", empty, "muted"));
      for (const item of first.items) list.append(render(item));
      section.append(list);
      let cursor = first.cursor;
      if (cursor) {
        const moreBtn = button("Load more", () => run(async () => {
          if (!cursor) return;
          moreBtn.disabled = true;
          try {
            const next = pageOf(object(await rpc.call("ir.module.module", "erp_addon_detail", { name: row.name, [`${key}_cursor`]: cursor, context })), key);
            if (generation !== detailGeneration) return;
            for (const item of next.items) list.append(render(item));
            cursor = next.cursor;
            if (!cursor) moreBtn.remove();
          } finally {
            moreBtn.disabled = false;
          }
        }), "addon-detail-more");
        section.append(moreBtn);
      }
      sections.append(section);
    };
    collection("Dependencies", "dependencies", "None", "addon-depends", item => el("code", text(item, "addon dependency"), "addon-depend"));
    collection("Dependents", "dependents", "None", "addon-depends", item => el("code", text(item, "addon dependent"), "addon-depend"));
    collection("Created Menus", "menus", "No menus created.", "addon-metadata-list", item => {
      const menu = object(item, "addon menu"), name = text(menu["name"], "addon menu name");
      const itemEl = el("div", undefined, "addon-metadata-item");
      const label = menu["label"] === undefined || menu["label"] === null ? name : text(menu["label"], "addon menu label") || name;
      itemEl.append(el("strong", label));
      if (label !== name) itemEl.append(el("code", name));
      return itemEl;
    });
    collection("Created Views", "views", "No views created.", "addon-metadata-list", item => {
      const view = object(item, "addon view");
      const itemEl = el("div", undefined, "addon-metadata-item");
      const line = el("div", undefined, "addon-metadata-line");
      line.append(el("strong", text(view["name"], "addon view name")), el("span", `[${text(view["kind"], "addon view kind")}]`, "badge addon-view-kind"));
      // A QWeb template (a report's, a layout's) belongs to no model: the server sends an empty name.
      const model = text(view["model"], "addon view model");
      itemEl.append(line, ...(model ? [el("code", model)] : []));
      return itemEl;
    });

    body.append(sections);
    detailContainer.append(header, body);

    const onKeyDown = (event: KeyboardEvent) => {
      if (event.key === "Escape") {
        event.preventDefault();
        exitDetail();
      }
    };
    detailContainer.addEventListener("keydown", onKeyDown);

    const exitDetail = () => {
      detailContainer.removeEventListener("keydown", onKeyDown);
      sheet.replaceChildren(layout);
      if (opener.isConnected) opener.focus();
    };

    sheet.replaceChildren(detailContainer);
    backBtn.focus();
  };

  const executeInstall = async (row: AddonRow): Promise<void> => {
    await operation(async () => {
      if (await choose("Install addon?", `Install ${row.label} and its dependencies?`, ["Install", "Cancel"]) !== "Install") return;
      try {
        await rpc.post("/erp/module/install", { modules: [row.name] }, { write: true });
        notice("Addon installed. Reloading the application…");
        window.location.reload();
      } catch (error) {
        notice("Installation status may have changed. Reload the catalog before trying again.");
        await render();
        throw error;
      }
    });
  };

  const executeUpgrade = async (row: AddonRow): Promise<void> => {
    await operation(async () => {
      if (await choose("Upgrade addon?", `Upgrade ${row.label} and its dependencies?`, ["Upgrade", "Cancel"]) !== "Upgrade") return;
      try {
        await rpc.post("/erp/module/upgrade", { modules: [row.name] }, { write: true });
        notice("Addon upgraded. Reloading the application…");
        window.location.reload();
      } catch (error) {
        notice("Upgrade status may have changed. Reload the catalog before trying again.");
        await render();
        throw error;
      }
    });
  };

  const startUninstall = async (row: AddonRow, opener: HTMLElement): Promise<void> => {
    let reviewing = false;
    try { await operation(async () => {
      let impact: Record<string, unknown>;
      try {
        impact = object(await rpc.call("ir.module.module", "erp_preview_uninstall", { modules: [row.name], context }));
      } catch (error) {
        if (error instanceof ClientError && error.kind === "validation") { notice(error.message || UNINSTALL_UNAVAILABLE); return; }
        notice(`Uninstall preview failed: ${(error as Error).message || "Unable to compute uninstall impact."}`);
        await render();
        throw error;
      }
      reviewing = true;
      await confirmUninstall(row, impact, opener);
    }); } finally {
      if (reviewing && opener.isConnected && !opener.matches(":disabled")) opener.focus();
    }
  };

  const confirmUninstall = (row: AddonRow, impact: Record<string, unknown>, opener: HTMLElement): Promise<void> =>
    confirmUninstallDialog(
      rpc,
      row,
      impact,
      opener,
      () => {
        notice("Addon uninstalled. Reloading the application…");
        window.location.reload();
      },
      async (error) => {
        const conflict = error instanceof ClientError && error.status === 409;
        if (conflict) {
          notice("Database state changed since preview. A fresh confirmation is required.");
        } else {
          notice("Uninstall status may have changed. Reload the catalog before trying again.");
        }
        await render();
        return conflict;
      }
    );

  const card = (row: AddonRow): HTMLElement => {
    const tile = el("article", undefined, "addon-card addon-tile");
    tile.dataset["addon"] = row.name;
    tile.tabIndex = 0;
    tile.setAttribute("role", "article");
    tile.setAttribute("aria-label", `${row.label} addon card`);

    tile.addEventListener("click", event => {
      if ((event.target as HTMLElement).closest(".addon-actions")) return;
      run(() => showDetail(row, tile));
    });
    tile.addEventListener("keydown", event => {
      if ((event.key === "Enter" || event.key === " ") && !(event.target as HTMLElement).closest(".addon-actions")) {
        event.preventDefault();
        run(() => showDetail(row, tile));
      }
    });

    const meta = el("div", undefined, "addon-tile-meta");
    meta.append(el(view.groupBy === "" ? "h2" : "h3", row.label, "addon-name"), el("code", row.name, "addon-technical-name"));
    const summary = el("p", row.summary || "No description provided.", "addon-summary");
    summary.title = row.summary;
    const facts = el("dl", undefined, "addon-facts");
    for (const [label, value, className] of [["Version", row.version || "—", ""], ["Author", row.author || "—", ""], ["Category", categoryLabel(row), "addon-category"]] as const) {
      const pair = el("div");
      pair.append(el("dt", label), el("dd", value, className));
      facts.append(pair);
    }
    const actions = el("div", undefined, "addon-actions");
    const info = button("Module info", () => showInfo(row, info), "addon-info-btn");
    const badge = el("span", row.badge.label, `addon-badge addon-badge-${row.badge.token}`);
    actions.append(info, badge);
    if (row.state === "uninstalled" && row.installable) {
      actions.append(button("Install", () => run(() => executeInstall(row)), "primary addon-install"));
    } else if (row.state === "installed") {
      const upgrade = button("Upgrade", () => run(() => executeUpgrade(row)), "primary addon-upgrade");
      const uninstall = button("Uninstall", () => run(() => startUninstall(row, uninstall)), "danger addon-uninstall");
      actions.append(upgrade, uninstall);
    }
    tile.append(tileIcon(row), meta, summary, facts, actions);
    return tile;
  };

  // Persistent controls: rebuilt panels never steal focus from the search box.
  const layout = el("div", undefined, "addons-layout");
  const panel = el("nav", undefined, "addons-panel");
  panel.setAttribute("aria-label", "Addon categories");
  const main = el("div", undefined, "addons-main");
  const toolbar = el("div", undefined, "addons-toolbar");
  const searchBox = el("label", undefined, "addons-search-box");
  const search = el("input", undefined, "addons-search");
  search.type = "search"; search.placeholder = "Search addons…"; search.setAttribute("aria-label", "Search addons");
  searchBox.append(icon("search"), search);
  search.addEventListener("input", () => { view.query = search.value; paint(); });
  search.addEventListener("keydown", event => { if (event.key === "Escape" && search.value) { event.preventDefault(); search.value = ""; view.query = ""; paint(); } });
  const filterBar = el("div", undefined, "addons-filters");
  filterBar.setAttribute("role", "group"); filterBar.setAttribute("aria-label", "Filters");
  const filterButtons = ADDON_FILTERS.map(([key, label]) => {
    const toggle = button(label, () => { if (view.filters.has(key)) view.filters.delete(key); else view.filters.add(key); paint(); }, "addons-filter");
    toggle.dataset["filter"] = key;
    return toggle;
  });
  filterBar.append(...filterButtons);
  const groupSelect = el("select", undefined, "addons-groupby");
  for (const [key, label] of ADDON_GROUPS) { const option = el("option", label); option.value = key; groupSelect.append(option); }
  groupSelect.addEventListener("change", () => { view.groupBy = groupSelect.value as AddonGroupBy; paint(); });
  const count = el("span", undefined, "addons-count muted");
  count.setAttribute("aria-live", "polite");
  const groupBy = labelled("Group by", groupSelect);
  groupBy.classList.add("addons-groupby-field");
  const refresh = button("Refresh catalog", () => run(() => operation(render)), "addons-refresh");
  refresh.prepend(icon("refresh-cw"));
  toolbar.append(searchBox, filterBar, groupBy, count, refresh);
  const results = el("div", undefined, "addons-results");
  // Per-addon demo failures come back in the reply and are shown.
  const demoZone = el("section", undefined, "addons-demo-zone");
  const demoButton = button("Load demo data", () => run(() => operation(async () => {
    const confirmed = "Yes, I understand the risks";
    if (await choose("Load demo data?", "Demo data should only be used on test databases. Once loaded it cannot be removed. Confirm that you want to irreversibly make this database a demo database.", [confirmed, "Oops, no!"]) !== confirmed) return;
    let result: Record<string, unknown>;
    try {
      result = object(await rpc.post("/erp/module/force-demo", { confirm: true }, { write: true }));
    } catch (error) {
      notice("Demo data status may have changed. Reload the catalog before trying again.");
      await render();
      throw error;
    }
    const failed = array(result["failed"], "failed").map(item => { const row = object(item); return `${text(row["module"], "failed module")}: ${text(row["error"], "failed error")}`; });
    if (failed.length > 0) {
      await choose("Demo data partly loaded", `The demo data of ${failed.length} addon(s) failed and was rolled back; the other addons have their demo data.\n\n${failed.join("\n\n")}`, ["Reload"]);
    }
    notice("Demo data loaded. Reloading the application…");
    window.location.reload();
  })), "danger addons-demo-load");
  demoZone.append(el("h2", "Demo data", "addons-panel-title"), el("p", "Load the demo data of every installed addon. This cannot be undone.", "muted"), demoButton);
  main.append(toolbar, results, demoZone);
  layout.append(panel, main);

  const panelItem = (label: string, amount: number, selected: boolean, depth: number, select: () => void): HTMLElement => {
    const item = button(label, select, "addons-panel-item");
    item.style.setProperty("--depth", String(depth));
    item.setAttribute("aria-pressed", String(selected));
    if (selected) item.classList.add("active");
    item.append(el("span", String(amount), "addons-panel-count"));
    return item;
  };

  const paint = (): void => {
    const searched = rows.filter(matchesSearch);
    const sectionRows = searched.filter(row => row.section === view.section);
    // A category that no longer exists after a catalog refresh falls back to All (the panel shows it selected).
    if (view.category.length > 0 && !rows.some(row => row.section === view.section && inCategory(row, view.category))) view.category = [];

    const apps = el("section", undefined, "addons-panel-section");
    apps.append(el("h2", "Apps", "addons-panel-title"));
    for (const [key, label] of [["official", "Official Apps"], [INDUSTRIES.toLowerCase(), INDUSTRIES]] as const) {
      const section = key as AddonSection;
      apps.append(panelItem(label, searched.filter(row => row.section === section).length, view.section === section, 0, () => { view.section = section; view.category = []; paint(); }));
    }
    const categories = el("section", undefined, "addons-panel-section");
    categories.append(el("h2", "Categories", "addons-panel-title"));
    categories.append(panelItem("All", sectionRows.length, view.category.length === 0, 0, () => { view.category = []; paint(); }));
    const nodes = new Map<string, string[]>();
    for (const row of rows.filter(item => item.section === view.section)) for (let depth = 1; depth <= row.path.length; depth++) nodes.set(row.path.slice(0, depth).join("/"), row.path.slice(0, depth));
    for (const path of [...nodes.values()].sort((a, b) => a.join("/").localeCompare(b.join("/")))) {
      const parent = path.slice(0, -1);
      if (parent.length > 0 && !parent.every((part, index) => view.category[index] === part)) continue;
      const selected = path.join("/") === view.category.join("/");
      categories.append(panelItem(path.at(-1)!, sectionRows.filter(row => inCategory(row, path)).length, selected, path.length - 1, () => { view.category = [...path]; paint(); }));
    }
    panel.replaceChildren(apps, categories);

    for (const toggle of filterButtons) {
      const on = view.filters.has(toggle.dataset["filter"] as AddonFilter);
      toggle.setAttribute("aria-pressed", String(on)); toggle.classList.toggle("active", on);
    }
    groupSelect.value = view.groupBy;
    const visible = sectionRows.filter(row => inCategory(row, view.category)).sort((a, b) => a.label.localeCompare(b.label));
    count.textContent = visible.length === 1 ? "1 addon" : `${visible.length} addons`;
    if (visible.length === 0) {
      results.replaceChildren(el("p", "No addons match the current search, filters and category.", "addons-empty muted"));
    } else if (view.groupBy === "") {
      const grid = el("div", undefined, "addons-grid");
      grid.append(...visible.map(card));
      results.replaceChildren(grid);
    } else {
      const keyOf = (row: AddonRow): string => view.groupBy === "category" ? categoryLabel(row) : view.groupBy === "state" ? row.badge.label : row.author || "Unknown author";
      const groups = new Map<string, AddonRow[]>();
      for (const row of visible) { const key = keyOf(row); if (!groups.has(key)) groups.set(key, []); groups.get(key)!.push(row); }
      results.replaceChildren(...[...groups.entries()].sort(([a], [b]) => a.localeCompare(b)).map(([key, members]) => {
        const group = el("section", undefined, "addons-group");
        const grid = el("div", undefined, "addons-grid");
        grid.append(...members.map(card));
        group.append(el("h2", `${key} (${members.length})`, "addons-group-title"), grid);
        return group;
      }));
    }
    disable();
  };

  const render = async (): Promise<void> => {
    const catalog = object(await rpc.call("ir.module.module", "erp_available_addons", { context }));
    if (catalog["wire_major"] !== 1) throw new ClientError("unsupported", "Unsupported addon catalog version.");
    rows = array(catalog["addons"], "addons").map(addonRow);
    if (layout.parentNode !== sheet) sheet.replaceChildren(layout);
    paint();
  };

  await operation(render);
  return {
    busy: () => pending,
    dirty: () => false,
    save: async () => { if (pending) throw new ClientError("validation", "Wait for addon installation to finish."); },
    discard: () => {}
  };
}

/** Same rule as the server's `is_settings_app_name`: an addon technical name, at most 64 bytes. */
const SETTINGS_APP = /^[a-z][a-z0-9_]{0,63}$/;

registerServiceController("web.settings", {
  model: "ir.config.setting",
  validateOptions(options) {
    const scopes = array(options["scope_kinds"], "setting scopes"), app = options["app"];
    if (Object.keys(options).length !== 2 || !Object.hasOwn(options, "app") || scopes.length === 0 || new Set(scopes).size !== scopes.length || scopes.some(scope => scope !== "global" && scope !== "company")) throw new ClientError("unsupported", "Unsupported settings options.");
    if (app !== null && (typeof app !== "string" || !SETTINGS_APP.test(app))) throw new ClientError("unsupported", "Unsupported settings app.");
  },
  create: env => settingsView(env.host, env.rpc, env.companies, env.run, env.notice, env.context, array(env.options["scope_kinds"], "setting scopes"), env.registry, env.controls, env.panel, env.options["app"] as string | null, env.settingsNavigation ? { navigation: env.settingsNavigation } : {}),
});

registerServiceController("web.addons", {
  model: "ir.module.module",
  validateOptions(options) { if (Object.keys(options).length) throw new ClientError("unsupported", "Unsupported addon options."); },
  create: env => addonsView(env.host, env.rpc, env.run, env.notice, env.context),
});
