/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
import { array, integer, object, text } from "./metadata.js";
import { actionButton, button, choose, el, labelled } from "./dom.js";
import { icon } from "./icons.js";
import { registerServiceController } from "./registry.js";
import { ClientError, RpcClient } from "./transport.js";

export interface ServiceController { busy(): boolean; dirty(): boolean; save(): Promise<void>; discard(): void }

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

/** The addon's tile, the same one the Apps page shows: its initial on the addon's tone. Decorative; the name sits beside it. */
function appIcon(appKey: string, appTitle: string): HTMLElement {
  const tile = el("span", undefined, "addon-icon settings-app-icon");
  tile.dataset["letter"] = (appTitle.charAt(0) || appKey.charAt(0)).toUpperCase();
  tile.dataset["tone"] = String(addonTone(appKey));
  tile.setAttribute("aria-hidden", "true");
  return tile;
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
}

interface AppGroupInfo {
  appKey: string;
  appTitle: string;
  section: HTMLElement;
  tab: HTMLButtonElement;
  blocks: SettingBlockInfo[];
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
  initialApp: string | null = null
): Promise<ServiceController> {
  let scope: Record<string, unknown> = { kind: "global" }, revision = "", baseline = new Map<string, unknown>(), edits = new Map<string, unknown>(), pending = false, ambiguous = false, generation = 0;
  let declarations: Record<string, unknown>[] = [];
  const appGroups = new Map<string, AppGroupInfo>();
  // An addon's own
  // Configuration > Settings menu opens on its app (`app` widget option); without one the first app opens.
  let selectedApp: string | null = initialApp, firstDraw = true;

  const sheet = el("div", undefined, "content-sheet settings-sheet");
  const selector = el("select");
  const form = el("form", undefined, "record-form settings-form");
  const actions = el("div", undefined, "settings-toolbar-actions");

  if (scopeKinds.includes("global")) selector.append(new Option("Global settings", "global"));
  if (scopeKinds.includes("company")) for (const company of companies) selector.append(new Option(company.name, String(company.id)));
  if (!selector.options.length) throw new ClientError("server", "No supported settings scope was declared.");
  scope = selector.value === "global" ? { kind: "global" } : { kind: "company", company_id: Number(selector.value) };

  const scopeKey = (value: Record<string, unknown>): string => value["kind"] === "global" ? "global" : String(value["company_id"]);
  const checkScope = (value: unknown, requested: Record<string, unknown>): void => {
    const actual = object(value, "settings scope");
    if (actual["kind"] !== requested["kind"] || actual["company_id"] !== requested["company_id"] || Object.keys(actual).length !== Object.keys(requested).length) throw new ClientError("server", "Settings response scope does not match the request.");
  };

  const toolbar = el("div", undefined, "settings-toolbar");
  const scopeWrapper = labelled("Settings scope", selector);
  scopeWrapper.classList.add("settings-toolbar-scope");
  toolbar.append(actions, scopeWrapper);

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

  const content = el("div", undefined, "settings-content");
  content.setAttribute("role", "region");
  content.setAttribute("aria-label", "Settings content");
  content.append(el("p", "Loading settings…", "settings-loading"));

  const noResults = el("div", undefined, "settings-no-results");
  noResults.style.display = "none";
  noResults.append(el("p", "No settings found matching your search.", "muted"));
  content.append(noResults);

  container.append(sidebar, content);
  form.append(container);
  sheet.append(form);
  host.append(sheet);

  const disable = (): void => {
    for (const raw of sheet.querySelectorAll("input,select,button")) {
      const input = raw as HTMLInputElement;
      input.disabled = pending || input.dataset["readonly"] === "true";
    }
    for (const raw of toolbar.querySelectorAll("input,select,button")) {
      const input = raw as HTMLInputElement;
      input.disabled = pending || input.dataset["readonly"] === "true";
    }
  };

  const operation = async (job: () => Promise<void>): Promise<void> => {
    if (pending) throw new ClientError("validation", "Wait for the settings operation to finish.");
    pending = true; content.setAttribute("aria-busy", "true"); disable();
    try { await job(); } finally { pending = false; content.setAttribute("aria-busy", "false"); disable(); }
  };

  const performFilter = (queryText: string): void => {
    const query = queryText.trim().toLowerCase();
    let totalMatches = 0;
    for (const group of appGroups.values()) {
      let appMatches = 0;
      for (const blockInfo of group.blocks) {
        const { element, titleEl, helpEl, labelText, helpText, keyText } = blockInfo;
        const matches = !query ||
          labelText.toLowerCase().includes(query) ||
          keyText.toLowerCase().includes(query) ||
          (helpText !== undefined && helpText.toLowerCase().includes(query));
        if (matches) {
          element.style.display = "";
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
      group.section.style.display = query ? (appMatches > 0 ? "" : "none") : (group.appKey === selectedApp ? "" : "none");
      group.tab.style.display = appMatches > 0 || !query ? "" : "none";
    }
    noResults.style.display = totalMatches === 0 && query ? "" : "none";
  };

  const clearFilter = (): void => { searchInput.value = ""; performFilter(""); };
  const selectApp = (appKey: string): void => {
    selectedApp = appKey;
    for (const group of appGroups.values()) {
      group.tab.classList.toggle("active", group.appKey === appKey);
      group.tab.setAttribute("aria-selected", String(group.appKey === appKey));
    }
    clearFilter();
  };

  searchInput.addEventListener("input", () => {
    performFilter(searchInput.value);
  });
  searchInput.addEventListener("keydown", event => {
    if (event.key === "Escape" && searchInput.value) { event.preventDefault(); clearFilter(); }
  });

  const draw = (): void => {
    sidebarList.replaceChildren();
    content.replaceChildren(noResults);
    appGroups.clear();

    const grouped = new Map<string, Record<string, unknown>[]>();
    for (const declaration of declarations) {
      const key = text(declaration["key"], "setting key");
      const appKey = key.includes(".") ? key.split(".")[0]! : "general";
      if (!grouped.has(appKey)) grouped.set(appKey, []);
      grouped.get(appKey)!.push(declaration);
    }

    if (selectedApp !== null && !grouped.has(selectedApp)) {
      // The addon's own settings menu names an app with no declared settings: a configuration error, said loudly.
      if (firstDraw) throw new ClientError("validation", `The addon '${selectedApp}' declares no settings for this page.`);
      selectedApp = null; // A scope switch can leave the app without settings; its first app opens instead.
    }
    selectedApp ??= grouped.keys().next().value ?? null;
    firstDraw = false;
    for (const [appKey, appDecls] of grouped.entries()) {
      const appTitle = humanizeAppTitle(appKey);
      const selected = appKey === selectedApp;

      const tab = el("button", undefined, "settings-app-tab") as HTMLButtonElement;
      tab.type = "button";
      tab.setAttribute("role", "tab");
      tab.setAttribute("data-app", appKey);
      tab.setAttribute("aria-selected", String(selected));
      tab.append(appIcon(appKey, appTitle), el("span", appTitle, "settings-app-name"));
      if (selected) tab.classList.add("active");

      const section = el("section", undefined, "settings-section");
      section.setAttribute("data-app-section", appKey);
      if (!selected) section.style.display = "none";
      const heading = el("h2", undefined, "settings-section-title");
      heading.append(appIcon(appKey, appTitle), el("span", appTitle));
      section.append(heading);

      const blocksContainer = el("div", undefined, "settings-blocks");
      const blockInfos: SettingBlockInfo[] = [];

      for (const declaration of appDecls) {
        const key = text(declaration["key"], "setting key");
        const labelStr = text(declaration["label"], "setting label");
        const kind = text(declaration["kind"], "setting type");
        const choices = declaration["choices"];
        const current = baseline.get(key);
        const helpStr = typeof declaration["help"] === "string" ? declaration["help"] : undefined;

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

        const input = choices === null ? el("input") : el("select");
        if (input instanceof HTMLSelectElement) {
          for (const raw of array(choices, "setting choices")) {
            const pair = array(raw, "setting choice");
            input.append(new Option(text(pair[1], "choice label"), text(pair[0], "choice value")));
          }
          if (![...input.options].some(option => option.value === current)) throw new ClientError("server", "Setting value is outside its declared choices.");
        } else {
          if (!["boolean", "integer", "float", "string"].includes(kind)) throw new ClientError("unsupported", `Unsupported setting type: ${kind}.`);
          input.type = kind === "boolean" ? "checkbox" : kind === "integer" || kind === "float" ? "number" : "text";
          if (kind === "integer" || kind === "float") input.required = true;
          if (kind === "float") input.step = "any";
          input.checked = current === true;
        }

        input.value = String(current);
        input.id = `setting-${key}`;
        titleSpan.htmlFor = input.id;
        input.name = key;
        if (typeof declaration["writable"] !== "boolean") throw new ClientError("server", "Invalid settings write permission.");
        input.dataset["readonly"] = String(!declaration["writable"]);

        input.addEventListener("change", () => {
          if (pending || declaration["writable"] !== true) {
            input.value = String(edits.has(key) ? edits.get(key) : baseline.get(key));
            if (input instanceof HTMLInputElement && kind === "boolean") input.checked = (edits.has(key) ? edits.get(key) : baseline.get(key)) === true;
            return;
          }
          const value = input instanceof HTMLInputElement && kind === "boolean" ? input.checked : kind === "integer" || kind === "float" ? Number(input.value) : input.value;
          if (Object.is(value, baseline.get(key))) edits.delete(key); else edits.set(key, value);
        });

        if (kind === "boolean") {
          const toggleLabel = labelled("", input);
          toggleLabel.classList.add("settings-toggle");
          leftPane.append(toggleLabel);
        } else {
          const inputWrapper = el("div", undefined, "setting-value-input");
          inputWrapper.append(input);
          rightPane.append(inputWrapper);
        }

        blockEl.append(leftPane, rightPane);
        blocksContainer.append(blockEl);

        blockInfos.push({
          element: blockEl,
          titleEl: titleSpan,
          helpEl,
          labelText: labelStr,
          helpText: helpStr,
          keyText: key,
        });
      }

      section.append(blocksContainer);
      content.append(section);
      sidebarList.append(tab);

      const groupInfo: AppGroupInfo = {
        appKey,
        appTitle,
        section,
        tab,
        blocks: blockInfos,
      };
      appGroups.set(appKey, groupInfo);

      tab.addEventListener("click", () => { selectApp(appKey); content.scrollTop = 0; });
    }

    if (!declarations.length) {
      content.append(el("p", "No settings are available for this scope.", "settings-empty"));
    }
    if (!panelHost) {
      content.append(searchBox);
    }

    disable();
    if (searchInput.value) {
      performFilter(searchInput.value);
    }
  };

  const load = async (requested: Record<string, unknown>): Promise<void> => {
    const request = ++generation, immutableScope = { ...requested };
    const description = object(await rpc.call("ir.config.setting", "erp_describe", { scope: immutableScope, context }));
    if (description["wire_major"] !== 1 || expectedRegistry !== undefined && description["registry_version"] !== expectedRegistry) throw new ClientError("unsupported", "Settings changed. Reload the application.");
    checkScope(description["scope"], immutableScope);
    const nextDeclarations = array(description["declarations"], "setting declarations").map(value => object(value));
    const keys = nextDeclarations.map(value => text(value["key"], "setting key"));
    if (new Set(keys).size !== keys.length) throw new ClientError("server", "Duplicate settings declaration.");
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
    if (request !== generation) return;
    declarations = nextDeclarations; baseline = nextValues; revision = nextRevision; scope = immutableScope; selector.value = scopeKey(scope); edits.clear(); ambiguous = false; draw();
  };

  const save = async (): Promise<void> => {
    // Hidden blocks cannot show a validation bubble: clear the search and open the app of the first invalid setting.
    if (searchInput.value) clearFilter();
    const invalid = [...form.querySelectorAll<HTMLInputElement | HTMLSelectElement>("input,select")].find(input => !input.checkValidity());
    const invalidApp = invalid?.closest<HTMLElement>("[data-app-section]")?.dataset["appSection"];
    if (invalidApp !== undefined) selectApp(invalidApp);
    if (!form.reportValidity()) throw new ClientError("validation", "Correct the invalid settings before saving.");
    return operation(async () => {
      if (ambiguous) throw new ClientError("ambiguous", "Reload settings to check the previous save before trying again.");
      if (!edits.size) return;
      if (!form.reportValidity()) throw new ClientError("validation", "Correct the invalid settings before saving.");
      const requested = { ...scope }, changes = [...edits].map(([key, value]) => ({ key, value }));
      try {
        const raw = await rpc.call("ir.config.setting", "erp_write_values", { scope: requested, expected_revision: revision, changes, context }, { write: true });
        ambiguous = true;
        const response = object(raw);
        if (response["wire_major"] !== 1) throw new ClientError("ambiguous", "Invalid settings save response. Reload to check the result.");
        checkScope(response["scope"], requested); text(response["revision"], "settings revision");
        await load(requested); notice("Settings saved.");
      } catch (error) {
        if (error instanceof ClientError && (error.kind === "ambiguous" || error.status >= 500)) ambiguous = true;
        if (ambiguous) throw new ClientError("ambiguous", "The settings save could not be confirmed. Reload settings before another write.");
        throw error;
      }
    });
  };

  const discard = (): void => { if (pending) { notice("Wait for the settings operation to finish."); return; } edits.clear(); draw(); };

  actions.append(
    actionButton("Save", () => run(save), "primary"),
    actionButton("Discard", discard),
    actionButton("Reload", () => run(() => operation(async () => { if (edits.size && await choose("Reload settings?", "Unsaved changes will be discarded.", ["Reload", "Cancel"]) !== "Reload") return; await load(scope); })))
  );

  selector.addEventListener("change", () => {
    const selected = selector.value; selector.value = scopeKey(scope);
    if (pending) return;
    run(() => operation(async () => {
      if (edits.size && await choose("Unsaved changes", "Discard changes before switching scope?", ["Discard", "Cancel"]) !== "Discard") return;
      const requested = selected === "global" ? { kind: "global" } : { kind: "company", company_id: integer(Number(selected), "company") };
      if (requested["kind"] === "company" && !companies.some(company => company.id === requested["company_id"])) throw new ClientError("validation", "Unsupported company scope.");
      await load(requested);
    }));
  });

  form.addEventListener("submit", event => { event.preventDefault(); run(save); });

  await operation(() => load(scope));
  return { busy: () => pending, dirty: () => edits.size > 0, save, discard };
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
const UNINSTALL_NOTE = "Uninstall is not available in the web client; use the erp module uninstall CLI command.";

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
    head.append(tileIcon(row), titles, el("span", row.badge.label, `addon-badge addon-badge-${row.badge.token}`));
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
    if (row.state === "installed") body.append(el("p", UNINSTALL_NOTE, "addon-info-note muted"));
    const footer = el("footer", undefined, "addon-info-footer");
    const close = button("Close", () => dialog.close());
    footer.append(close);
    dialog.append(head, body, footer);
    dialog.addEventListener("close", () => { dialog.remove(); if (opener.isConnected) opener.focus(); });
    document.body.append(dialog); dialog.showModal(); close.focus();
  };

  const tileIcon = (row: AddonRow): HTMLElement => {
    const tile = el("div", undefined, "addon-icon");
    tile.dataset["letter"] = (row.label.charAt(0) || row.name.charAt(0)).toUpperCase();
    tile.dataset["tone"] = String(addonTone(row.name));
    tile.setAttribute("aria-hidden", "true");
    return tile;
  };

  const card = (row: AddonRow): HTMLElement => {
    const tile = el("article", undefined, "addon-card addon-tile");
    tile.dataset["addon"] = row.name;
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
      actions.append(button("Install", () => run(() => operation(async () => {
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
      })), "primary addon-install"));
    } else if (row.state === "installed") {
      // the v1 install UI grants no uninstall capability; offer it disabled, never a dialog that fails.
      const uninstall = button("Uninstall", () => {}, "addon-uninstall");
      uninstall.disabled = true; uninstall.dataset["locked"] = "true";
      uninstall.title = UNINSTALL_NOTE;
      const note = el("span", UNINSTALL_NOTE, "addon-note visually-hidden");
      note.id = `addon-note-${row.name}`;
      uninstall.setAttribute("aria-describedby", note.id);
      actions.append(uninstall, note);
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
  create: env => settingsView(env.host, env.rpc, env.companies, env.run, env.notice, env.context, array(env.options["scope_kinds"], "setting scopes"), env.registry, env.controls, env.panel, env.options["app"] as string | null),
});

registerServiceController("web.addons", {
  model: "ir.module.module",
  validateOptions(options) { if (Object.keys(options).length) throw new ClientError("unsupported", "Unsupported addon options."); },
  create: env => addonsView(env.host, env.rpc, env.run, env.notice, env.context),
});
