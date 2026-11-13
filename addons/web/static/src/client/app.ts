/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
import { PanelCompatibilityError, PanelRecoverySafety, attemptPanelRecovery } from "./panel-recovery.js";
import { unavailablePanelMessage } from "./panel-metadata.js";
import { loginReturnUrl } from "./login-return.js";
import { createHotkeyController, type HotkeyBinding } from "@erpforall/elang";
import { reportHotkeyError } from "../../../../../web/src/runtime/hotkeys.js";
import { SearchModel, createSearchBar } from "./search/index.js";
import { readMenuModule, readSettingsTarget } from "./settings-items.js";
import { icon, menuIcon } from "./icons.js";
import { MessageServices } from "./messages.js";
import { noContentHelp } from "./message-widgets.js";
import { ErrorDialog } from "./error-dialog.js";
import { ClientErrorReporter, type ReportKind } from "./client-report.js";
import { MobileDrawer } from "./mobile-drawer.js";
import { documentScroller, lockDocumentScroll } from "./scroll-lock.js";
import { headerMenuSearch } from "./header-search.js";
import { resizeColumns } from "./columns.js";
import { createListTabs, decodeSelectionCounts } from "./list-tabs.js";
import { buildGroupByCatalogue, decodeGroupableFields } from "./group-by-catalogue.js";
import { createGroupRow, decodeGroups } from "./list-groups.js";
import { applyContext, archState, decodePivotFields, paramsState, PIVOT_PARAMS, PIVOT_MAX_LEVELS, PivotModel, readPivotView, resolveSpec, snapshotParams, type PivotGroupItem, type PivotLimits, type PivotParams, type PivotSnapshot } from "./pivot.js";
import { saveFile } from "./file-save.js";
import { mountPivot, type PivotCellTarget } from "./pivot-view.js";
import type { GroupByCatalogue, GroupableField } from "./search/types.js";
import { createContext, decodeAst, evalAst, parseBoundedJson } from "../../../../../web/packages/expr/src/index.js";
import { expressionValue, jsonValue } from "./field-relations.js";
import { array, describeViews, integer, noExpression, object, text, type ButtonNode, type Description, type Field, type Node, type View } from "./metadata.js";
import { actionButton, button, choose, confirmChoice, el, errorMessage, relabelActionButton } from "./dom.js";
import { buttonHidden, lockButtonControls, runObjectButton, buttonExpression, type ButtonEnvironment } from "./buttons.js";
import { viewButton, ButtonDebouncer, ButtonHotkeys } from "./view-button.js";
import { runUrlButton } from "./button-url.js";
import { CurrencyDisplayCache } from "./field-currency.js";
import { fieldWidgets, resolveFieldWidget } from "./registry.js";
import { ListGroupModel, type ListGroupNode } from "./list-group-model.js";
import type { ListGroupState } from "./list-group-state.js";
import type { NumericLocale } from "./numeric-values.js";
import "./fields.js";
import { decodeEditorLocale as readEditorLocale, type EditorLocale } from "./datetime-values.js";
import { RecordState, specification } from "./record.js";
import { parseRoute, routeUrl, rpcId, type Route } from "./route.js";
import "./services.js";
import { type ServiceController } from "./services.js";
import { serviceControllers } from "./registry.js";
import { mountControlPanel } from "./control-panel.js";
import { requestLoading } from "./loading.js";
import { actionResult } from "./actions.js";
import { CompanySelector, type CompanyItem } from "./company-selector.js";
import { reorderApps } from "./reorder-apps.js";
import { IDENTITY_METHOD, IDENTITY_MODEL, type Resubmission } from "./password-flow.js";
import { WizardDialog } from "./wizard-dialog.js";
import type { ActionResult } from "../generated/human-ui.js";
import { RecordForm, formFocus } from "./record-form.js";
import { MessagingStore } from "@erpforall/mail-store";
import { ClientError, RpcClient, type CallOptions } from "./transport.js";
import { attachSubviews } from "./x2many.js";
import { loadDatabaseTheme, loadUserColorScheme } from "./theme.js";
import { MetadataCache, invalidatesMetadata } from "./metadata-cache.js";
import { ExpiredMethodAction, MethodActions, type MethodAction } from "./method-actions.js";
import { BreadcrumbTrail, renderBreadcrumbs } from "./navigation.js";
import { commandProviders, commandCategories, commandSetups, systrayItems, userMenuItems, debugSections, debugItemProviders } from "./registry.js";
import { openCommandPalette } from "./command-palette.js";
import type { ActivityWindowAction } from "./activity-menu.js";
import { renderSystray, type SystrayEnvironment } from "./systray.js";
import { composeUserMenuItems, renderUserMenuItems, type UserMenuEnvironment } from "./user-menu.js";
import { renderDebugMenu, createDebugCommandProvider, type DebugEnvironment } from "./debug-menu.js";
import { createChoiceDropdown } from "./choice-dropdown.js";
import { attachMenuDropdown } from "./menu-dropdown.js";
import { navOverflowCount } from "./navbar-overflow.js";

import type { Company } from "../generated/human-ui.js";

/**
  The scope an action dialog belongs to. A dialog started from a mounted view carries that view's
 *  route generation and becomes obsolete when the route changes. A dialog started from the user
 *  menu (My Preferences) belongs to the signed-in session: a navigation that starts or finishes
 *  while it loads or stays open does not make it obsolete.
 */
const SESSION_SCOPE: unique symbol = Symbol("session scope");
type Binding = number | typeof SESSION_SCOPE;

const uncertainMethodMessage = "The action outcome is unknown. This form is read-only; reloading cannot authorize another attempt.";

/** The list this record view came from: the whole route (related-action identity, search) minus the record. */
function listRouteOf(route: Route): Route {
  const list = { ...route };
  delete list.id;
  delete list.view_type;
  return list;
}

/** The route of a view switch. The pivot keeps its own state (`pivot_*`) and its search model refuses a list's `search_groupby`, so that one is not carried over. */
function switchedRoute(route: Route, view: "list" | "form" | "pivot"): Route {
  const next: Route = { ...route, id: view === "form" ? route.id : undefined, view_type: view };
  if (view === "pivot") delete next.search_groupby;
  return next;
}

/** Labels remain available to assistive technology and tooltips. */
function decorateViewButton(target: HTMLButtonElement, kind: "list" | "form" | "pivot"): void {
  const label = el("span", kind === "list" ? "List" : kind === "pivot" ? "Pivot" : "Form", "view-button-label");
  target.replaceChildren(icon(kind === "list" ? "list" : kind === "pivot" ? "layout-grid" : "panels-top-left"), label);
  target.title = `${label.textContent} view`;
}

interface Menu {
  module: string | null;
  id: number;
  label: string;
  action: number | null;
  children: number[];
  parentId: number | null;
  icon: string | null;
}

/** The `[id, type]` view requests of a window action, in action order. */
function actionViews(action: Record<string, unknown>): unknown[][] {
  return array(action["views"], "action views").map(value => {
    const view = object(value);
    return [view["id"], view["type"]];
  });
}

export function isFalsy(value: unknown): boolean {
  if (value === false || value === 0) return true;
  if (typeof value === "string") {
    const s = value.trim().toLowerCase();
    return s === "false" || s === "0";
  }
  return false;
}

export class HumanApp {
  private readonly errors: ErrorDialog;
  readonly messages: MessageServices;
  private readonly reporter: ClientErrorReporter;
  private readonly lifetime = new AbortController();
  private readonly content: HTMLElement;
  private readonly noticeElement: HTMLElement;
  private readonly navigation: HTMLElement;
  private readonly header: HTMLElement;
  private readonly rpc: RpcClient;
  private readonly loading: ReturnType<typeof requestLoading>;
  private readonly registry: string;
  private readonly companies: Company[];
  private readonly currentCompanyId: number;
  private readonly activeCompanyIds: number[];
  private readonly allowedCompanyIds: number[];
  private readonly context: Record<string, unknown>;
  private readonly numericLocale: NumericLocale;
  private readonly editorLocale: EditorLocale;
  private readonly userName: string;
  private readonly userId: number;
  private readonly companyId: number;
  private readonly menus = new Map<number, Menu>();
  private readonly metadata = new MetadataCache();
  private readonly panelRecovery = new PanelRecoverySafety();
  private readonly methodActions = new MethodActions();
  /** An invocation identity, distinct from route data; only its completed result may hand off. */
  private pendingInvocation: { caller: RecordState; generation: number } | undefined;
  private rootMenuIds: number[] = [];
  private settingsRootMenuIds: number[] = [];
  private headerMenuSearchReady: (() => void) | undefined;
  private current: Route = {};
  private currentUrl = window.location.pathname + window.location.search;
  private generation = 0;
  /** Settles when the navigation now running ends, whether it committed, was superseded or failed. */
  private navigationInFlight: Promise<void> | undefined;
  /** Records of `edit="0"` views: once they have an id they are never written back. */
  private readonly lockedRecords = new WeakSet<RecordState>();
  private keyboardPending: "save" | "discard" | undefined;
  private historyIndex = 0;
  private restoringHistory: number | undefined;
  private traversalPending: { index?: number; url: URL } | undefined;
  private traversalRunning = false;
  private record: RecordState | undefined;
  private service: ServiceController | undefined;
  private busy = false;
  private wizard: WizardDialog | undefined;
  private actionPending = false;
  private uncertainMethod = false;
  /** Captured once, before the first teardown: later 401s must not lose the warning or return path. */
  private expiry: { readonly lostEdits: boolean; readonly signIn: string } | undefined;
  private expired = false;
  private readonly debug = new URL(location.href).searchParams.get("debug") === "1";
  private readonly breadcrumbs = new BreadcrumbTrail(32);
  private homeMenuOpen = false;
  private releaseHomeScroll?: (restore: boolean) => void;
  private mobileDrawer?: MobileDrawer;
  private userDropdownOpen = false;
  private messagingStore?: MessagingStore;

  private getMessagingStore(): MessagingStore {
    if (!this.messagingStore) {
      this.messagingStore = new MessagingStore({
        // A store resync problem must not open the blocking error dialog over an unrelated form flow.
        reportError: error => this.messages.notifications.add(error instanceof Error ? error.message : "The messaging store could not be updated.", { level: "danger" }),
      });
    }
    return this.messagingStore;
  }
  private userDropdown?: HTMLElement;
  private currentRootId: number | undefined;

  private listContext?: {
    ids: number[]; total: number; offset: number; limit: number;
    /** Loads the ids of another page of the same search and order, so the form pager can cross a page edge. */
    page?: (offset: number) => Promise<{ ids: number[]; total: number }>;
  };
  private disposeWidgets: (() => void)[] = [];
  private disposeListeners: (() => void)[] = [];
  private disposeHeader: (() => void)[] = [];
  private disposeNavMenus: (() => void)[] = [];
  private disposeNavMore: (() => void) | undefined;
  private navFitWidth = -1;
  private closeCompanyMenu: (() => void) | undefined;
  private readonly headerBootstrap: Record<string, unknown>;

  constructor(private readonly host: HTMLElement, bootstrapValue: unknown) {
    const bootstrap = object(bootstrapValue, "bootstrap");
    this.headerBootstrap = bootstrap;
    if (bootstrap["wire_major"] !== 1) throw new ClientError("unsupported", "Unsupported application version. Reload the page.");
    this.registry = text(bootstrap["registry_version"], "registry version");
    this.reporter = new ClientErrorReporter(window.fetch.bind(window), text(bootstrap["asset_version"], "asset version"));
    const identity = object(bootstrap["identity"]), locale = object(bootstrap["locale"]);
    if (identity["internal"] !== true) throw new ClientError("permission", "An internal user account is required.");
    this.userName = text(identity["name"], "user name");
    this.userId = integer(identity["uid"], "user id");
    this.companyId = integer(bootstrap["current_company_id"], "current company id");
    this.editorLocale = readEditorLocale(locale);
    // Numeric capabilities are validated by the consuming codec, preserving unrelated shell/editor usability.
    this.numericLocale = { decimal_point: locale["decimal_point"], thousands_sep: locale["thousands_sep"], grouping: locale["grouping"] } as NumericLocale;
    this.companies = array(bootstrap["companies"], "companies").map(raw => {
      const company = object(raw);
      const parentRaw = company["parent_id"];
      return {
        id: integer(company["id"], "company id"),
        name: text(company["name"], "company name"),
        parent_id: parentRaw === null || parentRaw === undefined ? null : integer(parentRaw, "parent company id"),
      };
    });
    this.currentCompanyId = integer(bootstrap["current_company_id"], "current company");
    const allowed = array(bootstrap["allowed_company_ids"], "allowed companies").map(id => integer(id, "company id"));
    const active = array(bootstrap["active_company_ids"], "active companies").map(id => integer(id, "company id"));
    this.allowedCompanyIds = allowed;
    this.activeCompanyIds = active;
    if (!active.length || active.some(id => !allowed.includes(id) || !this.companies.some(company => company.id === id)) ||
        !this.companies.some(company => company.id === this.currentCompanyId) ||
        !allowed.includes(this.currentCompanyId) || !active.includes(this.currentCompanyId)) {
      throw new ClientError("server", "Invalid company context.");
    }
    this.context = { allowed_company_ids: active, lang: locale["lang"], tz: text(locale["tz"], "timezone") };

    const region = (selector: string): HTMLElement => {
      const node = host.querySelector(selector);
      if (!(node instanceof HTMLElement)) throw new Error("Compiled shell region is missing.");
      return node;
    };
    this.content = region("[data-client-content]");
    host.querySelector("[data-skip-content]")?.addEventListener("click", () => {
      this.content.removeAttribute("data-navigation-focus");
      this.content.focus();
    });
    this.noticeElement = region("[data-client-notice]");
    this.noticeElement.removeAttribute("role"); this.noticeElement.removeAttribute("aria-live");
    this.messages = new MessageServices(this.noticeElement, error => this.reportError(error));
    this.errors = new ErrorDialog(this.messages.dialogs);
    this.navigation = region("[data-client-navigation]");
    this.header = region("[data-client-header]");
    this.loading = requestLoading(host);
    this.rpc = new RpcClient(window.fetch.bind(window), () => this.expire(), () => this.loading.begin(), model => {
      if (invalidatesMetadata(model)) { this.metadata.clear(); this.methodActions.clear(); }
    }, (event, options) => {
      if (this.expired || this.lifetime.signal.aborted) return;
      this.panelRecovery.observe(event, options);
      this.messages.network.observe(event, options);
    });
    this.listenForMenuIntent();

    this.renderHeader(bootstrap);

    const pop = (event: PopStateEvent): void => {
      let targetIndex: unknown;
      let isForeign = false;
      try {
        targetIndex = object(event.state, "navigation state")["erpIndex"];
      } catch {
        isForeign = true;
      }
      if (!isForeign && (typeof targetIndex !== "number" || !Number.isSafeInteger(targetIndex))) {
        isForeign = true;
      }
      if (!isForeign && this.restoringHistory === targetIndex) {
        this.restoringHistory = undefined;
        return;
      }
      this.restoringHistory = undefined;
      const targetUrl = new URL(window.location.href);
      const currentUrl = new URL(this.currentUrl, window.location.href);
      if (targetUrl.pathname === currentUrl.pathname && targetUrl.search === currentUrl.search && targetUrl.hash !== currentUrl.hash) {
        return;
      }
      const targetIdx = !isForeign ? (targetIndex as number) : undefined;
      this.traversalPending = { index: targetIdx, url: targetUrl };
      if (this.traversalRunning) return;
      this.traversalRunning = true;
      void (async () => {
        const restore = (target?: { index?: number; url: URL }): void => {
          const latest = target ?? this.traversalPending;
          this.traversalPending = undefined;
          if (latest && latest.index !== undefined && latest.index !== this.historyIndex) {
            this.restoringHistory = this.historyIndex;
            history.go(this.historyIndex - latest.index);
          } else {
            history.replaceState({ erpIndex: this.historyIndex }, "", this.currentUrl);
            this.notice(isForeign ? "This history entry belongs to an older application session. Reload to continue." : "Invalid application history entry.");
          }
        };
        try {
          while (this.traversalPending && !this.expired) {
            const target = this.traversalPending;
            this.traversalPending = undefined;
            if (!await this.canLeave()) {
              restore(target);
              return;
            }
            try {
              await this.navigate(parseRoute(target.url), true, true);
              if (target.index !== undefined) {
                this.historyIndex = target.index;
              }
            } catch (error) {
              this.traversalPending ??= target;
              throw error;
            }
          }
        } catch (error) {
          restore();
          this.reportError(error);
        } finally {
          this.traversalRunning = false;
        }
      })();
    };

    const unload = (event: BeforeUnloadEvent): void => {
      if (this.isDirty() || this.keyboardPending !== undefined || this.busy || this.actionPending || this.wizard?.busy) {
        event.preventDefault();
        event.returnValue = "";
      }
    };
    const hotkeys = createHotkeyController(this.host, { report: error => this.hotkeyError(error), bindings: () => {
      const debugMenu = this.host.querySelector<HTMLElement>(".debug-menu-dropdown");
      const debugOpen = debugMenu !== null && debugMenu.style.display !== "none";
      const active = this.homeMenuOpen || this.mobileDrawer?.isOpen || this.userDropdownOpen || debugOpen || this.navigation.querySelector(".nav-dropdown-menu.open");
      const list: HotkeyBinding[] = [];
      if (active) list.push({ kind: "callback", chord: "escape", target: this.host, allowEditable: true, run: () => this.closeKeyboardOverlay() });
      list.push({ kind: "callback", chord: "ctrl+k", target: this.host, allowEditable: true, run: () => this.openCommandPalette() });
      return list;
    } });
    const scriptError = (event: ErrorEvent): void => {
      if (event.error instanceof Error) this.reportError(event.error, "error");
    };
    const rejected = (event: PromiseRejectionEvent): void => this.reportError(event.reason, "unhandledrejection");
    window.addEventListener("error", scriptError);
    window.addEventListener("unhandledrejection", rejected);
    window.addEventListener("popstate", pop);
    window.addEventListener("beforeunload", unload);

    this.disposeListeners.push(
      () => window.removeEventListener("error", scriptError),
      () => window.removeEventListener("unhandledrejection", rejected),
      () => window.removeEventListener("popstate", pop),
      () => window.removeEventListener("beforeunload", unload),
      () => hotkeys.dispose()
    );
  }

  protected navigateLocation(url: string): void {
    window.location.assign(url);
  }

  private hotkeyError(error: unknown, obsolete = false): void {
    // RpcClient's session boundary already rendered expiry and removed protected scopes.
    // Re-reporting that exact handled denial after disposal would create an unhandled duplicate.
    if (this.expired && error instanceof ClientError && error.kind === "session") return;
    if (obsolete || this.expired || this.lifetime.signal.aborted) reportHotkeyError(error); else this.reportError(error);
  }
  private closeKeyboardOverlay(): void {
    if (this.homeMenuOpen) { this.closeHomeMenu(); return; }
    if (this.mobileDrawer?.isOpen) { this.closeMobileDrawer(); return; }
    if (this.userDropdownOpen) {
      this.userDropdownOpen = false; this.userDropdown?.classList.remove("open"); this.host.querySelector<HTMLElement>(".user-button")?.focus(); return;
    }
    const debugMenu = this.host.querySelector<HTMLElement>(".debug-menu-dropdown");
    if (debugMenu && debugMenu.style.display !== "none") {
      debugMenu.style.display = "none";
      const debugBtn = this.host.querySelector<HTMLElement>(".debug-menu-button");
      debugBtn?.setAttribute("aria-expanded", "false");
      debugBtn?.focus();
      return;
    }
    const menu = [...this.navigation.querySelectorAll<HTMLElement>(".nav-dropdown-menu.open")].at(-1);
    if (menu) { menu.classList.remove("open"); menu.parentElement?.querySelector<HTMLElement>(".nav-dropdown-toggle")?.focus(); }
  }

  public openCommandPalette(options?: { namespace?: string; initialSearch?: string }): void {
    const providers = [...commandProviders.entries()].map(([, p]) => p);
    // The authorized menus are a built-in provider, so "Con" finds Contacts; an empty query lists the first ones.
    providers.push({
      provide: ({ searchValue }) => {
        const query = searchValue.trim().normalize("NFKC").toLowerCase();
        return [...this.menus.values()].flatMap(menu => {
          const target = this.defaultActionForMenu(menu.id);
          if (!target || !menu.label.normalize("NFKC").toLowerCase().includes(query)) return [];
          return [{ id: `menu-${menu.id}`, name: menu.label, category: "Menus", action: () => this.navigate({ action: String(target.action), menu: String(target.menu) }) }];
        }).slice(0, 50);
      }
    });
    if (this.debug) {
      const debugEnv: DebugEnvironment = {
        rpc: this.rpc,
        context: this.context,
        model: this.current.model,
        activeId: this.current.id !== undefined ? Number(this.current.id) : undefined,
        run: job => this.run(job),
        report: err => this.reportError(err),
        navigate: route => this.navigate(route),
        loadPath: url => this.loadPath(url)
      };
      providers.push(createDebugCommandProvider(
        [...debugItemProviders.entries()].map(([, p]) => p),
        debugSections,
        debugEnv
      ));
    }
    const categories = [...commandCategories.entries()].map(([id, cat]) => ({
      id,
      ...cat
    }));
    const ns = options?.namespace || "default";
    const setup = commandSetups.has(ns)
      ? commandSetups.get(ns)
      : (commandSetups.has("default") ? commandSetups.get("default") : undefined);
    openCommandPalette({
      providers,
      categories,
      setup,
      namespace: options?.namespace,
      initialSearch: options?.initialSearch,
      run: job => this.run(job),
      report: err => this.reportError(err),
      navigate: route => this.navigate(route),
      loadPath: url => this.loadPath(url)
    });
  }

  private renderHeader(bootstrap: Record<string, unknown>): void {
    for (const stop of this.disposeHeader) stop();
    this.disposeHeader = []; this.headerMenuSearchReady = undefined; this.userDropdownOpen = false;
    this.header.replaceChildren();

    const brand = button("", () => this.run(() => this.navigate({})), "brand");
    brand.setAttribute("aria-label", "ERPForAll home");
    // Both variants ship; the theme tokens in client.css show the one that fits the top bar.
    for (const [src, variant] of [["/erp/brand/logo.svg", "light"], ["/erp/brand/logo-dark.svg", "dark"]] as const) {
      const logo = el("img", undefined, `brand-logo brand-logo-${variant}`);
      logo.src = src; logo.alt = variant === "light" ? "ERPForAll" : ""; logo.width = 120; logo.height = 32;
      brand.append(logo);
    }
    this.header.append(brand);
    const office = document.documentElement.dataset["officeTheme"] === "office-blue";
    if (office) {
      const search = headerMenuSearch({
        entries: () => [...this.menus.values()].flatMap(menu => {
          const target = this.defaultActionForMenu(menu.id);
          return target ? [{ id: menu.id, label: menu.label, ...target }] : [];
        }),
        choose: entry => this.run(() => this.navigate({ action: String(entry.action), menu: String(entry.menu) }))
      });
      this.headerMenuSearchReady = search.ready;
      this.header.append(search.node); this.disposeHeader.push(search.dispose);
    }

    // The
    // element itself moves, so renderNavigation keeps owning its content. Office Blue places the same navigation in its sidebar.
    // header.replaceChildren() above detached it either way; Office Blue puts it before the bar (Tab order = visual order on a phone) and places both with its grid.
    if (!office) this.header.append(this.navigation); else this.header.before(this.navigation);

    const systray = el("div", undefined, "systray");
    const customSystrayContainer = el("div", undefined, "systray-custom-items");
    const systrayEnv: SystrayEnvironment = {
      rpc: this.rpc,
      context: this.context,
      run: job => this.run(job),
      report: err => this.reportError(err),
      messages: this.messages,
      openActivities: action => this.openActivities(action)
    };
    // A handle that arrives after this header was replaced is disposed, not added to the newer header's disposers.
    const headerStops = this.disposeHeader;
    renderSystray(customSystrayContainer, systrayItems.entries(), systrayEnv).then(handle => {
      if (this.disposeHeader === headerStops) headerStops.push(() => handle.dispose());
      else handle.dispose();
    }).catch(error => this.reportError(error));

    if (this.debug) {
      // No registered debug item provider: nothing to list, so no trigger (an empty dropdown is a sliver).
      const debugMenuContainer = el("div", undefined, "debug-menu-container");
      debugMenuContainer.hidden = debugItemProviders.entries().length === 0;
      const debugDropdown = el("div", undefined, "debug-menu-dropdown");
      let debugHandle: { dispose(): void } | undefined;
      this.disposeHeader.push(() => debugHandle?.dispose());

      const debugBtn = button("", () => {}, "debug-menu-button");
      debugBtn.setAttribute("aria-label", "Developer tools");
      debugBtn.append(icon("wrench"));

      const dropdownHelper = attachMenuDropdown({
        trigger: debugBtn,
        menu: debugDropdown,
        mode: "display",
        onOpen: () => {
          // Async content: show a row while it loads, never an empty sliver.
          debugDropdown.replaceChildren(el("p", "Loading developer tools…", "debug-menu-empty"));
          const debugEnv: DebugEnvironment = {
            rpc: this.rpc,
            context: this.context,
            model: this.current.model,
            activeId: this.current.id !== undefined ? Number(this.current.id) : undefined,
            run: job => this.run(job),
            report: err => this.reportError(err),
            navigate: route => this.navigate(route),
            loadPath: url => this.loadPath(url)
          };
          debugHandle?.dispose();
          debugHandle = undefined;
          renderDebugMenu(
            debugDropdown,
            [...debugItemProviders.entries()].map(([, p]) => p),
            debugSections,
            debugEnv,
            () => { dropdownHelper.close(false); }
          ).then(handle => {
            debugHandle?.dispose();
            debugHandle = handle;
            dropdownHelper.refresh();
          }).catch(error => this.reportError(error));
        }
      });
      this.disposeHeader.push(() => dropdownHelper.dispose());

      debugMenuContainer.append(debugBtn, debugDropdown);
      systray.append(debugMenuContainer);
    }

    systray.append(customSystrayContainer);
    const userMenuContainer = el("div", undefined, "user-menu");

    const avatar = el("div", undefined, "user-avatar");
    const firstLetter = this.userName.trim().charAt(0).toUpperCase() || "U";
    avatar.setAttribute("data-letter", firstLetter);

    const userNameSpan = el("span", this.userName, "user-name");
    const userButton = button("", () => {
      this.userDropdownOpen = !this.userDropdownOpen;
      userDropdown.classList.toggle("open", this.userDropdownOpen);
    }, "user-button");
    userButton.setAttribute("aria-label", "User profile and settings");
    userButton.setAttribute("aria-haspopup", "true");
    userButton.append(avatar, userNameSpan, icon("chevron-down"));

    const userDropdown = el("div", undefined, "user-dropdown");
    this.userDropdown = userDropdown;
    const userInfo = el("div", undefined, "user-dropdown-info");
    userInfo.append(el("strong", this.userName), el("span", "Internal User", "muted"));

    // The company switcher is a top-bar item of its own (current company name; the panel is the shared dropdown panel).
    const companyMenu = el("div", undefined, "company-menu");
    const companySection = el("div", undefined, "user-dropdown-company");
    const currentCompanyName = this.companies.find(company => company.id === this.currentCompanyId)?.name ?? "";
    const companySelect = el("select");
    companySelect.setAttribute("aria-label", "Current company");
    companySelect.style.display = "none";
    for (const company of this.companies) {
      companySelect.append(new Option(company.name, String(company.id)));
    }
    companySelect.value = String(this.currentCompanyId);
    companySelect.addEventListener("change", () => this.run(async () => {
      if (!await this.canLeave()) {
        companySelect.value = String(this.currentCompanyId);
        return;
      }
      const route = { ...this.current, cids: [companySelect.value] };
      this.navigateLocation(routeUrl(route));
    }));

    if (this.companies.length <= 1) {
      // One company: its name, nothing to open. The select stays (hidden) as the single place that switches the route.
      companyMenu.append(el("span", currentCompanyName, "company-menu-name"), companySelect);
    } else {
      const companyItems: CompanyItem[] = this.companies.map(c => ({
        id: c.id,
        name: c.name,
        parentId: c.parent_id,
      }));
      const companySelector = new CompanySelector(companyItems, this.activeCompanyIds, this.allowedCompanyIds);

      const switcher = el("div", undefined, "company-switcher");
      let searchFilter = "";

      const renderSwitcherContent = (): void => {
        switcher.replaceChildren();

        if (this.companies.length > 6) {
          const searchInput = el("input", undefined, "company-search-input") as HTMLInputElement;
          searchInput.type = "text";
          searchInput.placeholder = "Search companies...";
          searchInput.setAttribute("aria-label", "Search companies");
          searchInput.value = searchFilter;
          searchInput.addEventListener("input", () => {
            searchFilter = searchInput.value;
            renderTreeAndActions();
          });
          switcher.append(searchInput);
        }

        const treeContainer = el("div", undefined, "company-tree");
        treeContainer.setAttribute("role", "tree");
        const actionsContainer = el("div", undefined, "company-switcher-actions-wrapper");

        const renderTreeAndActions = (): void => {
          const activeEl = document.activeElement as HTMLElement | null;
          const focusedCompanyId = activeEl?.closest<HTMLElement>("[data-company-id]")?.getAttribute("data-company-id");
          const controlKind = activeEl?.matches(".company-checkbox, input[type=checkbox]")
            ? "checkbox"
            : activeEl?.matches(".company-switch-btn, button")
            ? "button"
            : activeEl?.matches(".company-row")
            ? "row"
            : undefined;
          treeContainer.replaceChildren();
          actionsContainer.replaceChildren();

          const visible = companySelector.computeVisibleCompanies(searchFilter);
          for (const entry of visible) {
            const row = el("div", undefined, "company-row");
            row.setAttribute("data-company-id", String(entry.company.id));
            row.setAttribute("role", "treeitem");
            row.setAttribute("aria-selected", companySelector.isCompanySelected(entry.company.id) ? "true" : "false");
            row.tabIndex = 0;
            if (entry.level > 0) {
              row.style.paddingLeft = `${entry.level * 16}px`;
            }
            if (entry.company.id === this.currentCompanyId) {
              row.classList.add("current-active");
            }

            const checkbox = el("input", undefined, "company-checkbox") as HTMLInputElement;
            checkbox.type = "checkbox";
            checkbox.checked = companySelector.isCompanySelected(entry.company.id);
            checkbox.disabled = !companySelector.isCompanyAllowed(entry.company.id);
            checkbox.setAttribute("aria-label", `Toggle ${entry.company.name}`);
            checkbox.title = companySelector.isCompanySelected(entry.company.id)
              ? `Hide ${entry.company.name} content`
              : `Show ${entry.company.name} content`;

            checkbox.addEventListener("change", () => {
              companySelector.switchCompany("toggle", entry.company.id);
              renderTreeAndActions();
            });

            const switchBtn = el("button", entry.company.name, "company-switch-btn") as HTMLButtonElement;
            switchBtn.type = "button";
            switchBtn.title = `Switch to ${entry.company.name}`;
            switchBtn.setAttribute("aria-label", `Switch to ${entry.company.name}`);
            if (entry.company.id === this.currentCompanyId) {
              switchBtn.classList.add("current");
              switchBtn.setAttribute("aria-current", "true");
            }
            switchBtn.addEventListener("click", () => this.run(async () => {
              companySelector.switchCompany("loginto", entry.company.id);
              if (!await this.canLeave()) {
                companySelector.reset();
                renderTreeAndActions();
                return;
              }
              const targetCids = companySelector.selectedCompanyIds.length
                ? companySelector.selectedCompanyIds
                : [entry.company.id];
              const route = { ...this.current, cids: targetCids.map(String) };
              this.navigateLocation(routeUrl(route));
            }));

            row.addEventListener("keydown", (e: KeyboardEvent) => {
              if (e.key === " ") {
                e.preventDefault();
                if (!checkbox.disabled) {
                  checkbox.checked = !checkbox.checked;
                  checkbox.dispatchEvent(new Event("change"));
                }
              } else if (e.key === "Enter" && !e.ctrlKey && !e.metaKey) {
                e.preventDefault();
                switchBtn.click();
              }
            });

            row.append(checkbox, switchBtn);
            treeContainer.append(row);
          }

          if (focusedCompanyId) {
            const targetRow = treeContainer.querySelector<HTMLElement>(`.company-row[data-company-id="${focusedCompanyId}"]`);
            if (targetRow) {
              if (controlKind === "checkbox") {
                targetRow.querySelector<HTMLElement>(".company-checkbox")?.focus();
              } else if (controlKind === "button") {
                targetRow.querySelector<HTMLElement>(".company-switch-btn")?.focus();
              } else {
                targetRow.focus();
              }
            }
          }

          if (companySelector.hasSelectionChanged) {
            const actions = el("div", undefined, "company-switcher-actions");
            const confirmBtn = el("button", "Confirm", "company-btn-confirm") as HTMLButtonElement;
            confirmBtn.type = "button";
            confirmBtn.addEventListener("click", () => this.run(async () => {
              if (!companySelector.selectedCompanyIds.length) {
                companySelector.reset();
                renderTreeAndActions();
                return;
              }
              if (!await this.canLeave()) {
                companySelector.reset();
                renderTreeAndActions();
                return;
              }
              const route = { ...this.current, cids: companySelector.selectedCompanyIds.map(String) };
              this.navigateLocation(routeUrl(route));
            }));

            const discardBtn = el("button", "Discard", "company-btn-discard") as HTMLButtonElement;
            discardBtn.type = "button";
            discardBtn.addEventListener("click", () => {
              companySelector.reset();
              renderTreeAndActions();
            });

            actions.append(confirmBtn, discardBtn);
            actionsContainer.append(actions);
          }
        };

        renderTreeAndActions();
        switcher.append(treeContainer, actionsContainer);
      };

      renderSwitcherContent();
      companySection.classList.add("company-dropdown");
      companySection.setAttribute("role", "group"); companySection.setAttribute("aria-label", "Companies");
      companySection.append(switcher, companySelect);
      const companyTrigger = button("", () => {}, "company-menu-button");
      companyTrigger.addEventListener("click", (event: MouseEvent) => {
        const willOpen = !companySection.classList.contains("open");
        companySection.classList.toggle("open", willOpen);
        companyTrigger.setAttribute("aria-expanded", String(willOpen));
        // Keyboard activation (detail 0) moves focus into the panel; a pointer opens it without a focus ring.
        if (willOpen && event.detail === 0) companySection.querySelector<HTMLElement>(".company-search-input, .company-row")?.focus();
      });
      companyTrigger.setAttribute("aria-haspopup", "dialog");
      companyTrigger.setAttribute("aria-expanded", "false");
      companyTrigger.setAttribute("aria-label", `Companies, current: ${currentCompanyName}`);
      companyTrigger.append(el("span", currentCompanyName, "company-menu-name"), icon("chevron-down"));
      companyMenu.append(companyTrigger, companySection);
      const closeCompanyMenu = (): void => { companySection.classList.remove("open"); companyTrigger.setAttribute("aria-expanded", "false"); };
      companyMenu.addEventListener("keydown", (e: KeyboardEvent) => {
        if (e.key === "Escape" && companySection.classList.contains("open")) { e.preventDefault(); e.stopPropagation(); closeCompanyMenu(); companyTrigger.focus(); return; }
        const down = e.key === "ArrowDown", up = e.key === "ArrowUp";
        if (!down && !up || e.target instanceof HTMLInputElement && e.target.type === "text") return;
        const stops = [...companySection.querySelectorAll<HTMLElement>(".company-row")];
        const at = stops.findIndex(row => row === document.activeElement || row.contains(document.activeElement));
        if (!stops.length) return;
        e.preventDefault();
        stops[(at + (down ? 1 : -1) + stops.length) % stops.length]!.focus();
      });
      this.closeCompanyMenu = closeCompanyMenu;

      const onCtrlEnter = (e: KeyboardEvent): void => {
        if ((e.ctrlKey || e.metaKey) && e.key === "Enter" && companySelector.hasSelectionChanged) {
          e.preventDefault();
          const confirmBtn = switcher.querySelector<HTMLButtonElement>(".company-btn-confirm");
          confirmBtn?.click();
        }
      };
      // The switcher sits inside its dropdown: one listener, or a single Ctrl+Enter confirms twice.
      companySection.addEventListener("keydown", onCtrlEnter);
    }

    const logoutForm = el("form");
    logoutForm.method = "post";
    logoutForm.action = "/erp/session/logout";
    const token = el("input");
    token.type = "hidden";
    token.name = "csrf_token";
    token.value = text(object(bootstrap["csrf"])["logout"], "logout token");
    const logoutBtn = el("button", "Sign out", "user-menu-row");
    logoutBtn.type = "submit";
    logoutBtn.prepend(icon("log-out"));
    logoutForm.append(token, logoutBtn);
    logoutForm.addEventListener("submit", event => {
      event.preventDefault();
      this.run(async () => {
        if (await this.canLeave()) logoutForm.submit();
      });
    });

    // "My Preferences" opens the dialog on the current user; it sits above Sign out.
    const preferencesBtn = el("button", "My preferences", "user-menu-row user-dropdown-preferences");
    preferencesBtn.type = "button";
    preferencesBtn.prepend(icon("user-cog"));
    preferencesBtn.addEventListener("click", () => {
      this.userDropdownOpen = false;
      userDropdown.classList.remove("open");
      // A navigation in flight (the first page after sign-in, or a menu the user just chose) would
      // refuse the request with a notice that the navigation then clears. Open the dialog once it
      // settles instead; any other busy operation still answers with the usual notice.
      const open = (): void => {
        const navigation = this.navigationInFlight;
        if (navigation) void navigation.then(open);
        else this.run(() => this.openPreferences());
      };
      open();
    });

    const customUserMenuContainer = el("div", undefined, "user-dropdown-custom-items");
    const userMenuEnv: UserMenuEnvironment = {
      userName: this.userName,
      userId: this.userId,
      currentCompanyId: this.currentCompanyId,
      companies: this.companies,
      rpc: this.rpc,
      run: job => this.run(job),
      report: err => this.reportError(err),
      navigate: route => this.navigate(route),
      loadPath: url => this.loadPath(url)
    };
    const customUserMenuItems = composeUserMenuItems(userMenuItems.entries(), userMenuEnv);
    const customUserMenuHandle = renderUserMenuItems(
      customUserMenuContainer,
      customUserMenuItems,
      userMenuEnv,
      () => {
        this.userDropdownOpen = false;
        userDropdown.classList.remove("open");
      }
    );
    this.disposeHeader.push(() => customUserMenuHandle.dispose());

    const shortcutsBtn = el("button", "Shortcuts", "user-menu-row");
    shortcutsBtn.type = "button";
    shortcutsBtn.prepend(icon("zap"));
    shortcutsBtn.append(el("kbd", /Mac|iPhone|iPad/.test(navigator.platform) ? "\u2318K" : "Ctrl+K", "user-menu-hint"));
    shortcutsBtn.addEventListener("click", () => {
      this.userDropdownOpen = false;
      userDropdown.classList.remove("open");
      this.openCommandPalette();
    });
    const divider = (): HTMLElement => { const rule = el("div", undefined, "user-menu-divider"); rule.setAttribute("role", "separator"); return rule; };
    userDropdown.append(userInfo, divider(), shortcutsBtn, preferencesBtn, customUserMenuContainer, divider(), logoutForm);
    for (const row of userDropdown.querySelectorAll<HTMLElement>(".user-menu-row")) row.setAttribute("role", "menuitem");
    userDropdown.setAttribute("role", "menu");
    userDropdown.addEventListener("keydown", (e: KeyboardEvent) => {
      const step = e.key === "ArrowDown" ? 1 : e.key === "ArrowUp" ? -1 : 0;
      if (!step) return;
      const rows = [...userDropdown.querySelectorAll<HTMLElement>(".user-menu-row, .user-menu-item")].filter(row => !row.hasAttribute("disabled"));
      if (!rows.length) return;
      e.preventDefault();
      const at = rows.indexOf(document.activeElement as HTMLElement);
      rows[(at + step + rows.length) % rows.length]!.focus();
    });
    userMenuContainer.append(userButton, userDropdown);
    if (currentCompanyName) systray.append(companyMenu);
    systray.append(userMenuContainer);
    this.header.append(systray);

    const onDocClick = (e: MouseEvent): void => {
      const target = e.target instanceof Element ? e.target : null;
      if (this.userDropdownOpen && target && !userMenuContainer.contains(target)) {
        this.userDropdownOpen = false;
        userDropdown.classList.remove("open");
      }
      if (target && !companyMenu.contains(target)) this.closeCompanyMenu?.();
      for (const drop of systray.querySelectorAll<HTMLElement>(".debug-menu-dropdown")) {
        if (drop.style.display !== "none" && target && !drop.parentElement?.contains(target)) {
          drop.style.display = "none";
        }
      }
      if (target) {
        for (const menu of this.navigation.querySelectorAll<HTMLElement>(".nav-dropdown-menu.open")) {
          const container = menu.closest(".nav-dropdown");
          if (!container || !container.contains(target)) {
            menu.classList.remove("open");
          }
        }
      }
    };
    document.addEventListener("click", onDocClick);
    this.disposeHeader.push(() => document.removeEventListener("click", onDocClick));
  }

  dispose(): void {
    this.generation++;
    const errors: unknown[] = [];
    const release = (stop: () => void): void => { try { stop(); } catch (error) { errors.push(error); } };
    release(() => this.lifetime.abort());
    release(() => this.wizard?.dispose()); this.wizard = undefined;
    release(() => this.loading.dispose());
    release(() => this.errors.dispose());
    release(() => this.mobileDrawer?.dispose());
    release(() => { const unlock = this.releaseHomeScroll; this.releaseHomeScroll = undefined; unlock?.(false); });
    for (const stop of [...this.disposeWidgets, ...this.disposeListeners, ...this.disposeHeader]) release(stop);
    for (const stop of this.disposeNavMenus) release(stop);
    this.disposeWidgets = []; this.disposeListeners = []; this.disposeHeader = []; this.disposeNavMenus = [];
    this.record = undefined; this.service = undefined; this.pendingInvocation = undefined; this.methodActions.clear();
    release(() => this.messages.dispose());
    if (errors.length) throw new AggregateError(errors, "Application scope cleanup failed.");
  }

  /**
   * Session denial: tear down protected state, then show the sign-in link with the
   * validated return path. This security transition bypasses the dirty veto, so it says when
   * unsaved edits were lost. Later 401s from other in-flight requests re-render the same page.
   */
  private expire(): void {
    if (!this.expiry) {
      let lostEdits: boolean;
      // A failing dirty check is reported but must not stop the teardown; assume edits were lost.
      try { lostEdits = this.isDirty(); } catch (error) { lostEdits = true; reportHotkeyError(error); }
      this.expiry = { lostEdits, signIn: loginReturnUrl(window.location) };
    }
    const { lostEdits, signIn } = this.expiry;
    this.expired = true;
    try { this.dispose(); }
    finally {
      const main = el("main");
      main.append(el("h1", "Session expired"), el("p", "Your session has expired. Sign in again."));
      if (lostEdits) {
        const lost = el("p", "Unsaved changes on this page were lost. Sign in, then enter them again.");
        lost.setAttribute("role", "alert"); main.append(lost);
      }
      const link = el("a", "Sign in"); link.href = signIn; main.append(link);
      this.host.replaceChildren(main);
    }
  }

  notice = (message: string): void => {
    if (!this.expired && !this.lifetime.signal.aborted) this.messages.notice(message);
  };

  private choose(title: string, message: string, choices: readonly string[], signal = this.lifetime.signal): Promise<string | null> {
    return choose(title, message, choices, signal, this.messages.dialogs);
  }

  private reportError(error: unknown, kind: ReportKind = "render"): void {
    if (this.expired || this.lifetime.signal.aborted) return;
    if (error instanceof ClientError && (error.kind === "network" || error.kind === "ambiguous")) this.messages.network.set("offline");
    this.reporter.report(kind, error);
    if(error instanceof PanelCompatibilityError) {
      // Editors can hold raw/invalid local input before RecordState marks a change. Restrict
      // automatic recovery to shell/list read state; every mounted editor stays manual.
      const safe = this.panelRecovery.safe && !this.record && !this.service && !this.wizard &&
        !this.isDirty() && !this.busy && !this.actionPending && this.keyboardPending === undefined &&
        !this.uncertainMethod && !this.messages.block.blocked && !this.messages.dialogs.active();
      if(attemptPanelRecovery(()=>window.sessionStorage,safe,()=>window.location.reload()))return;
    }
    this.errors.show(error);
  }

  run = (job: () => Promise<void>): void => {
    if (this.lifetime.signal.aborted) return;
    if (this.keyboardPending !== undefined || this.busy || this.actionPending || this.messages.block.blocked) {
      this.notice("Wait for the current operation to finish.");
      return;
    }
    void job().catch(error => this.hotkeyError(error));
  };

  private isDirty(): boolean {
    return this.uncertainMethod || this.wizard?.dirty === true || this.record?.dirty === true || this.service?.dirty() === true;
  }

  /** A same-origin page that is not a client route is opened by the browser, once the form may be left. */
  private async loadPath(url: string): Promise<void> {
    if (await this.canLeave()) window.location.assign(url);
  }

  private async canLeave(options?: { autosave?: boolean }): Promise<boolean> {
    if (this.keyboardPending === "save") return false;
    if (this.uncertainMethod) { this.notice(uncertainMethodMessage); return false; }
    if (this.actionPending) return false;
    // A child picker/editor/error scope must finish before its source route can be replaced.
    // This also covers browser-history navigation while native modality shields pointer input.
    const activeDialog = this.messages.dialogs.active();
    if (activeDialog && activeDialog !== this.wizard?.dialog) return false;
    if (this.wizard) {
      if (this.wizard.busy || !await this.wizard.canLeave()) return false;
      await this.wizard.close();
    }
    if (this.busy || this.record?.busy || this.service?.busy()) {
      this.notice("Wait for the current operation to finish.");
      return false;
    }
    if (!this.isDirty()) return true;
    if (options?.autosave) {
      if (this.record && this.record.id && this.lockedRecords.has(this.record)) {
        // The view forbids editing (edit="0"): never write through the breadcrumb autosave.
        this.record.discard();
        this.notice("This record is read-only; the change was not saved.");
        return true;
      }
      try {
        if (this.record) await this.record.save();
        else await this.service?.save();
        return true;
      } catch (error) {
        return this.messages.saveFailure(error, { discard: () => { this.record?.discard(); this.service?.discard(); } });
      }
    }
    const choice = await this.choose("Unsaved changes", "Save your changes before leaving this page?", ["Save", "Discard", "Cancel"]);
    if (choice === "Save") {
      try {
        if (this.record) await this.record.save();
        else await this.service?.save();
        return true;
      } catch (error) {
        return this.messages.saveFailure(error, { discard: () => { this.record?.discard(); this.service?.discard(); } });
      }
    }
    if (choice === "Discard") { this.record?.discard(); this.service?.discard(); return true; }
    return false;
  }

  async start(resolveTheme = false): Promise<void> {
    if (this.lifetime.signal.aborted) return;
    if (resolveTheme) {
      await loadDatabaseTheme(this.rpc, this.registry, this.context);
      if (this.lifetime.signal.aborted) return;
      // A cosmetic choice never stops the application from starting: a failed read is reported and the device preference stays.
      await this.applyUserColorScheme();
      if (this.lifetime.signal.aborted) return;
      this.renderHeader(this.headerBootstrap);
    }
    const menuResponse = await this.rpc.call("ir.ui.menu", "erp_load_web_menus", { context: this.context }, { signal: this.lifetime.signal });
    if (this.lifetime.signal.aborted) return;
    const response = object(menuResponse);
    if (response["wire_major"] !== 1 || response["registry_version"] !== this.registry) {
      throw new ClientError("unsupported", "Menus changed. Reload the application.");
    }

    this.menus.clear();
    this.rootMenuIds = [];
    for (const value of array(response["nodes"], "menus")) {
      const node = object(value), id = integer(node["id"], "menu id");
      if (!id || this.menus.has(id)) throw new ClientError("server", "Invalid menu identity.");
      const parentId = node["parent_id"] === null ? null : integer(node["parent_id"], "parent id");
      const icon = node["icon"] === null || node["icon"] === undefined ? null : text(node["icon"], "menu icon");
      const action = node["action_id"] === null ? null : integer(node["action_id"], "action id");
      const children = array(node["children"], "menu children").map(child => integer(child, "menu child"));
      const module = readMenuModule(node["module"]);
      this.menus.set(id, { module, id, label: text(node["label"], "menu label"), action, children, parentId, icon });
    }
    for (const id of array(response["roots"], "menu roots")) {
      this.rootMenuIds.push(integer(id, "root menu"));
    }

    // Settings identity ties follow server order, independent of a personal home-menu reorder.
    this.settingsRootMenuIds = [...this.rootMenuIds];
    try {
      const settingsResponse = await this.rpc.call("res.users.settings", "get_res_users_settings", {}, { signal: this.lifetime.signal });
      const settings = object(settingsResponse);
      if (typeof settings["homemenu_config"] === "string" && settings["homemenu_config"].trim().length > 0) {
        const parsed = JSON.parse(settings["homemenu_config"]);
        if (Array.isArray(parsed)) {
          const customOrder = parsed.map(v => Number(v)).filter(n => Number.isInteger(n) && n > 0);
          this.rootMenuIds = reorderApps(this.rootMenuIds, customOrder);
        }
      }
    } catch (error) {
      // The saved order is optional, so startup goes on with the default order; the user is told it was not applied.
      if (!this.lifetime.signal.aborted) this.messages.notifications.add(`Your saved app order could not be applied: ${error instanceof Error ? error.message : String(error)}`, { level: "danger" });
    }

    const visited = new Set<number>();
    const walk = (id: number, depth: number): void => {
      const menu = this.menus.get(id);
      if (!menu || visited.has(id) || depth > 32) throw new ClientError("server", "Invalid menu tree.");
      visited.add(id);
      for (const child of menu.children) walk(child, depth + 1);
    };
    for (const id of this.rootMenuIds) walk(id, 0);
    if (visited.size !== this.menus.size) throw new ClientError("server", "Unreachable menu nodes.");
    this.headerMenuSearchReady?.();

    const initialRoute = parseRoute(new URL(window.location.href));
    try { await this.navigate(initialRoute, true); }
    catch (error) {
      if (error instanceof ExpiredMethodAction) {
        // A reload/new tab has no retained action payload. Offer explicit recovery to the
        // originating persisted action; never silently substitute it for the related list.
        const recovery = el("a", "Return to originating action", "method-action-recovery");
        recovery.href = routeUrl({ action: initialRoute.action, menu: initialRoute.menu, cids: initialRoute.cids });
        this.content.replaceChildren(recovery);
      }
      throw error;
    }
  }

  private resolveActiveRoot(route: Route): Menu | undefined {
    let targetMenu: Menu | undefined;
    if (route.menu) {
      const id = Number(route.menu);
      if (this.menus.has(id)) targetMenu = this.menus.get(id);
    }
    if (!targetMenu && route.action) {
      const actionId = Number(route.action);
      for (const menu of this.menus.values()) {
        if (menu.action === actionId) {
          targetMenu = menu;
          break;
        }
      }
    }
    if (!targetMenu) return undefined;
    let curr = targetMenu;
    const seen = new Set<number>();
    while (curr.parentId !== null && this.menus.has(curr.parentId) && !seen.has(curr.id)) {
      seen.add(curr.id);
      curr = this.menus.get(curr.parentId)!;
    }
    return curr;
  }

  /** Navigation and prefetch share one caller-bound action/view metadata producer. */
  private async windowAction(id: number, options: CallOptions = {}): Promise<Record<string, unknown>> {
    return object((await this.windowBundle(id, options))["action"]);
  }

  private async windowBundle(id: number, options: CallOptions = {}): Promise<Record<string, unknown>> {
    const args = { ids: [id], context: this.context };
    return object(await this.metadata.read(`action ${JSON.stringify(args)}`, async () => {
      const generation = this.metadata.generation;
      const bundle = object(await this.rpc.call("ir.actions.act_window", "erp_load_web_action_views",
        { ...args, wire_major: 1, options: { mobile: false } }, options));
      if (Object.keys(bundle).length !== 2 || !("action" in bundle) || !("views" in bundle)) {
        throw new ClientError("validation", "Invalid action and view metadata bundle.");
      }
      const action = object(bundle["action"]);
      if (action["wire_major"] !== 1 || action["target"] !== "current" || action["id"] !== id) {
        throw new ClientError("unsupported", "Unsupported or mismatched window action.");
      }
      const model = text(action["model"], "action model"), views = actionViews(action);
      // Admit both members before publishing either cache identity.
      describeViews(bundle["views"], model, this.registry);
      const described = object(bundle["views"]);
      const actionOwned = array(described["views"], "views").some(raw => {
        const view = object(raw), root = array(view["nodes"], "view nodes")[integer(view["root"], "view root")];
        const node = object(root, "view root");
        return view["type"] === "list" && (Object.hasOwn(node, "search_panel") || Object.hasOwn(node, "page_limit"));
      });
      // Legacy identical view requests may still share their companion; action-owned search/limit
      // metadata stays in the full bundle cached under this action's own identity.
      if (!actionOwned) this.metadata.seed(this.viewKey(model, views), bundle["views"], generation);
      return bundle;
    }));
  }

  /** Related-record discovery must not describe unrelated actions' views. */
  private async windowActionOnly(id: number): Promise<Record<string, unknown>> {
    const args = { ids: [id], context: this.context };
    return object(await this.metadata.read(`action-only ${JSON.stringify(args)}`,
      () => this.rpc.call("ir.actions.act_window", "erp_load_web_action", args)));
  }

  private viewArgs(views: unknown[][], context = this.context): Record<string, unknown> {
    return { views, wire_major: 1, options: { mobile: false }, context };
  }

  private viewKey(model: string, views: unknown[][], context = this.context): string {
    return `views ${model} ${JSON.stringify(this.viewArgs(views, context))}`;
  }

  /** The view descriptions of one action, shared through the page-lifetime metadata cache. */
  private async viewDescriptions(model: string, views: unknown[][], options: CallOptions = {}, context = this.context, actionId?: number): Promise<unknown> {
    if (actionId === undefined) return this.metadata.read(this.viewKey(model, views, context), () => this.rpc.call(model, "get_views", this.viewArgs(views, context), options));
    const bundle = await this.windowBundle(actionId, options), action = object(bundle["action"]);
    if (action["model"] !== model || JSON.stringify(actionViews(action)) !== JSON.stringify(views)) throw new ClientError("validation", "Mismatched action-specific view metadata.");
    return bundle["views"];
  }

  /** Load what a menu entry's navigation would load first, so the click waits only for its data. */
  private async prefetchMenu(menuId: number): Promise<void> {
    const target = this.defaultActionForMenu(menuId);
    if (!target || this.expired) return;
    const action = await this.windowAction(target.action, { background: true });
    if (action["wire_major"] !== 1 || action["target"] !== "current" || action["id"] !== target.action) return;
    await this.viewDescriptions(text(action["model"], "action model"), actionViews(action), { background: true }, this.context, target.action);
  }

  /** Pointer rest or keyboard focus on a menu entry starts its prefetch after a short intent delay. */
  private listenForMenuIntent(): void {
    let timer: ReturnType<typeof setTimeout> | undefined, entry: HTMLElement | undefined;
    // At most one prefetch runs at a time and only the latest waiting entry follows it, so sweeping
    // the pointer across a menu stays far below the ingress request-rate limit.
    let running = false, queued: number | undefined;
    const drain = (): void => {
      const id = queued;
      if (id === undefined || this.expired) { running = false; return; }
      queued = undefined;
      running = true;
      // Speculative: a failed prefetch is not cached, so the navigation that needs it repeats and reports it.
      this.prefetchMenu(id).catch(() => undefined).finally(drain);
    };
    const enter = (event: Event): void => {
      const found = event.target instanceof Element ? event.target.closest<HTMLElement>("[data-menu-id]") : null;
      if (!found || found === entry || !this.host.contains(found)) return;
      entry = found;
      clearTimeout(timer);
      const id = Number(found.dataset["menuId"]);
      if (!Number.isSafeInteger(id)) return;
      timer = setTimeout(() => {
        timer = undefined;
        queued = id;
        if (!running) drain();
      }, 80);
    };
    const leave = (event: Event): void => {
      const next = "relatedTarget" in event ? (event as FocusEvent).relatedTarget : null;
      if (!entry || next instanceof Node && entry.contains(next)) return;
      clearTimeout(timer);
      timer = undefined;
      entry = undefined;
    };
    for (const [type, listener] of [["pointerover", enter], ["focusin", enter], ["pointerout", leave], ["focusout", leave]] as const) {
      this.host.addEventListener(type, listener);
      this.disposeListeners.push(() => this.host.removeEventListener(type, listener));
    }
    this.disposeListeners.push(() => clearTimeout(timer));
  }

  private defaultActionForMenu(menuId: number): { action: number; menu: number } | undefined {
    const menu = this.menus.get(menuId);
    if (!menu) return undefined;
    if (menu.action !== null) return { action: menu.action, menu: menu.id };
    for (const childId of menu.children) {
      const found = this.defaultActionForMenu(childId);
      if (found) return found;
    }
    return undefined;
  }

  private toggleHomeMenu(): void {
    if (!this.current.action) {
      // Office Blue hides the home screen's search box where its header search is shown; focus the one on screen.
      const home = this.content.querySelector<HTMLInputElement>(".home-menu-search");
      (home && home.offsetParent !== null ? home : this.header.querySelector<HTMLInputElement>(".office-header-search input") ?? home)?.focus();
      return;
    }
    this.homeMenuOpen = !this.homeMenuOpen;
    const overlay = this.navigation.querySelector(".home-menu-overlay");
    if (overlay) {
      overlay.classList.toggle("hidden", !this.homeMenuOpen);
      if (this.homeMenuOpen) overlay.replaceChildren(this.buildHomeContent());
      else overlay.replaceChildren();
    }
    this.coverContent(overlay instanceof HTMLElement && this.homeMenuOpen, overlay instanceof HTMLElement ? overlay : null);
  }

  /** While the home menu covers the page, the page is hidden and inert: nothing in it (an open palette or
   * dropdown) can show through, and focus cannot land behind the menu. Its state is kept for when the menu closes.
   * The document is scrolled to the top and locked, so the header stays in place above the menu and a swipe on the
   * menu cannot scroll the page under it; the menu starts at the header's measured bottom edge. Closing the menu on
   * the same page restores the scroll position (`restoreScroll`); navigating away does not. */
  private coverContent(covered: boolean, overlay?: HTMLElement | null, restoreScroll = true): void {
    this.content.classList.toggle("home-menu-covered", covered);
    this.content.inert = covered;
    this.navigation.classList.toggle("home-menu-open", covered);
    if (covered && !this.releaseHomeScroll) {
      const scroller = documentScroller();
      const restoreTop = scroller.scrollTop;
      scroller.scrollTop = 0;
      const unlock = lockDocumentScroll();
      this.releaseHomeScroll = (restore: boolean) => {
        unlock();
        if (restore) documentScroller().scrollTop = restoreTop;
      };
    } else if (!covered && this.releaseHomeScroll) {
      const release = this.releaseHomeScroll;
      this.releaseHomeScroll = undefined;
      release(restoreScroll);
    }
    if (covered && overlay) overlay.style.top = `${Math.max(0, Math.round(this.header.getBoundingClientRect().bottom))}px`;
  }

  private closeHomeMenu(): void {
    this.homeMenuOpen = false;
    const overlay = this.navigation.querySelector(".home-menu-overlay");
    if (overlay) {
      overlay.classList.add("hidden");
      overlay.replaceChildren();
    }
    this.coverContent(false);
  }

  /** Reorders home menu root applications and persists the preference to `res.users.settings`. */
  async reorderApps(newOrder: number[]): Promise<void> {
    const previousOrder = [...this.rootMenuIds];
    const computedOrder = reorderApps(this.rootMenuIds, newOrder);
    this.rootMenuIds = computedOrder;
    try {
      await this.rpc.call("res.users.settings", "set_res_users_settings", {
        new_settings: {
          homemenu_config: JSON.stringify(computedOrder),
        },
      }, { write: true });
    } catch (error) {
      this.rootMenuIds = previousOrder;
      throw error;
    }
  }

  private buildHomeContent(): HTMLElement {
    const homeContent = el("div", undefined, "home-menu-container");
    const searchInput = el("input", undefined, "home-menu-search");
    searchInput.type = "search";
    searchInput.placeholder = "Search apps…";
    searchInput.setAttribute("aria-label", "Search apps");

    const grid = el("div", undefined, "home-grid");

    const moveTile = (menuId: number, direction: -1 | 1): void => {
      const currentOrder = [...this.rootMenuIds];
      const fromIndex = currentOrder.indexOf(menuId);
      if (fromIndex === -1) return;
      const toIndex = fromIndex + direction;
      if (toIndex < 0 || toIndex >= currentOrder.length) return;
      currentOrder.splice(fromIndex, 1);
      currentOrder.splice(toIndex, 0, menuId);
      this.run(async () => {
        const reorderPromise = this.reorderApps(currentOrder);
        renderGridTiles();
        grid.querySelector<HTMLButtonElement>(`[data-menu-id="${menuId}"]`)?.focus();
        try {
          await reorderPromise;
        } finally {
          renderGridTiles();
          grid.querySelector<HTMLButtonElement>(`[data-menu-id="${menuId}"]`)?.focus();
        }
      });
    };

    const renderGridTiles = (): void => {
      grid.replaceChildren();
      for (const rootId of this.rootMenuIds) {
        const rootMenu = this.menus.get(rootId);
        if (!rootMenu) continue;
        const tile = button("", () => this.run(async () => {
          this.closeHomeMenu();
          const target = this.defaultActionForMenu(rootMenu.id);
          if (target) await this.navigate({ action: String(target.action), menu: String(target.menu) });
          else if (rootMenu.action !== null) await this.navigate({ action: String(rootMenu.action), menu: String(rootMenu.id) });
        }), "app-tile");
        tile.dataset["menuId"] = String(rootMenu.id);
        tile.draggable = true;

        tile.addEventListener("keydown", (e: KeyboardEvent) => {
          if (e.ctrlKey || e.altKey || e.metaKey) {
            if (e.key === "ArrowLeft" || e.key === "ArrowUp") {
              e.preventDefault();
              moveTile(rootMenu.id, -1);
            } else if (e.key === "ArrowRight" || e.key === "ArrowDown") {
              e.preventDefault();
              moveTile(rootMenu.id, 1);
            }
          }
        });

        tile.addEventListener("dragstart", (e: DragEvent) => {
          tile.classList.add("app-tile-dragged");
          if (e.dataTransfer) {
            e.dataTransfer.effectAllowed = "move";
            e.dataTransfer.setData("text/plain", String(rootMenu.id));
          }
        });

        tile.addEventListener("dragend", () => {
          tile.classList.remove("app-tile-dragged");
        });

        tile.addEventListener("dragover", (e: DragEvent) => {
          e.preventDefault();
          if (e.dataTransfer) {
            e.dataTransfer.dropEffect = "move";
          }
        });

        tile.addEventListener("drop", (e: DragEvent) => {
          e.preventDefault();
          tile.classList.remove("app-tile-dragged");
          const draggedIdStr = e.dataTransfer?.getData("text/plain");
          if (!draggedIdStr) return;
          const draggedId = Number(draggedIdStr);
          if (!draggedId || draggedId === rootMenu.id) return;

          const currentOrder = [...this.rootMenuIds];
          const fromIndex = currentOrder.indexOf(draggedId);
          const toIndex = currentOrder.indexOf(rootMenu.id);
          if (fromIndex === -1 || toIndex === -1) return;

          currentOrder.splice(fromIndex, 1);
          currentOrder.splice(toIndex, 0, draggedId);

          this.run(async () => {
            const reorderPromise = this.reorderApps(currentOrder);
            renderGridTiles();
            try {
              await reorderPromise;
            } finally {
              renderGridTiles();
            }
          });
        });

        const iconBox = el("div", undefined, "app-icon");
        iconBox.setAttribute("aria-hidden", "true");
        if (rootMenu.icon && !rootMenu.icon.startsWith("lucide:") && (rootMenu.icon.includes("/") || rootMenu.icon.startsWith("data:"))) {
          const img = el("img");
          img.src = rootMenu.icon.includes(",") ? "/" + rootMenu.icon.split(",")[0] + "/" + rootMenu.icon.split(",")[1] : rootMenu.icon;
          img.alt = "";
          iconBox.append(img);
        } else {
          iconBox.append(menuIcon(rootMenu.icon));
          iconBox.classList.add("app-icon-fallback");
        }
        const caption = el("div", rootMenu.label, "app-caption");
        tile.append(iconBox, caption);
        grid.append(tile);
      }
    };

    renderGridTiles();

    searchInput.addEventListener("input", () => {
      const term = searchInput.value.toLowerCase().trim();
      for (const tile of grid.querySelectorAll<HTMLElement>(".app-tile")) {
        const textContent = tile.textContent?.toLowerCase() || "";
        tile.style.display = textContent.includes(term) ? "" : "none";
      }
    });

    homeContent.append(searchInput, grid);
    return homeContent;
  }

  private toggleMobileDrawer(): void {
    this.mobileDrawer?.toggleOpen();
  }

  private closeMobileDrawer(): void {
    this.mobileDrawer?.close();
  }

  private gridIcon(): SVGElement {
    const svg = icon("layout-grid");
    svg.classList.add("grid-icon");
    return svg;
  }

  /** Whether the subtree under `menuId` has at least one entry with an action (cycle-safe). */
  private hasLeafAction(menuId: number, visited = new Set<number>()): boolean {
    if (visited.has(menuId)) return false;
    visited.add(menuId);
    const menu = this.menus.get(menuId);
    if (!menu) return false;
    return menu.children.some(childId => {
      const child = this.menus.get(childId);
      return child !== undefined && !visited.has(childId) && (child.action !== null || this.hasLeafAction(childId, visited));
    });
  }

  /** The optional office layout uses the same authorized tree and route guards as every other menu. */
  private buildOfficeNavigation(route: Route): HTMLElement {
    const sidebar = el("div", undefined, "office-navigation");
    const active = route.menu ? Number(route.menu) : [...this.menus.values()].find(menu => String(menu.action) === route.action)?.id;
    const containsActive = (menu: Menu): boolean => menu.id === active || menu.children.some(id => {
      const child = this.menus.get(id);
      return child !== undefined && containsActive(child);
    });
    const append = (parent: HTMLElement, id: number): void => {
      const menu = this.menus.get(id);
      if (!menu || menu.action === null && !this.hasLeafAction(id)) return;
      const children = menu.children.filter(child => {
        const entry = this.menus.get(child);
        return entry !== undefined && (entry.action !== null || this.hasLeafAction(child));
      });
      const openAction = (): void => this.run(() => this.navigate({ action: String(menu.action), menu: String(menu.id) }));
      const entry = (label: string): HTMLButtonElement => {
        const target = button(label, openAction, "office-nav-item");
        target.dataset["menuId"] = String(menu.id);
        target.dataset["menuIcon"] = menu.icon ? "declared" : "default";
        target.prepend(menuIcon(menu.icon));
        return target;
      };
      if (!children.length) { if (menu.action !== null) parent.append(entry(menu.label)); return; }
      const group = el("details", undefined, "office-nav-group");
      group.open = containsActive(menu);
      const summary = el("summary");
      summary.append(menuIcon(menu.icon), el("span", menu.label), icon("chevron-down"));
      group.append(summary);
      const items = el("div", undefined, "office-nav-children");
      if (menu.action !== null) items.append(entry(`Open ${menu.label}`));
      for (const child of children) append(items, child);
      group.append(items);
      parent.append(group);
    };
    for (const root of this.rootMenuIds) append(sidebar, root);
    return sidebar;
  }

  /**
   * Keeps the top-bar menus on one row: the entries that do not fit are hidden and listed, in order, in one "More"
   * dropdown (a nested menu is listed flat under its label). Re-runnable: it starts from the full row every time.
   */
  private fitNavSections(sections: HTMLElement): void {
    this.disposeNavMore?.(); this.disposeNavMore = undefined;
    sections.querySelector(":scope > .nav-more")?.remove();
    const entries = [...sections.querySelectorAll<HTMLElement>(":scope > [data-nav-entry]")];
    for (const entry of entries) entry.hidden = false;
    const available = sections.clientWidth;
    this.navFitWidth = available;
    if (available === 0 || entries.length === 0) return; // not laid out (hidden bar, tests without layout)
    const more = el("div", undefined, "nav-dropdown nav-more");
    const list = el("div", undefined, "nav-dropdown-menu");
    const toggle = button("More", () => {}, "nav-dropdown-toggle");
    toggle.append(icon("chevron-down"));
    more.append(toggle, list);
    sections.append(more);
    const width = (element: Element): number => element.getBoundingClientRect().width;
    const lead = [...sections.children].filter(child => child !== more && !entries.includes(child as HTMLElement)).reduce((sum, child) => sum + width(child), 0);
    const gap = Number.parseFloat(getComputedStyle(sections).columnGap) || 0;
    const count = navOverflowCount(lead, entries.map(width), available, width(more), gap);
    if (count === 0) { more.remove(); return; }
    const moved = entries.slice(entries.length - count);
    for (const entry of moved) {
      entry.hidden = true;
      entry.querySelector(".nav-dropdown-menu.open")?.classList.remove("open"); // a menu that was open when its entry moved
    }
    const visited = new Set<number>();
    const item = (menu: Menu): void => {
      const entry = button(menu.label, () => {
        list.classList.remove("open");
        this.run(() => this.navigate({ action: String(menu.action), menu: String(menu.id) }));
      }, "dropdown-item");
      entry.dataset["menuId"] = String(menu.id);
      if (sections.querySelector(`[data-menu-id="${menu.id}"][aria-current=page]`)) entry.setAttribute("aria-current", "page");
      list.append(entry);
    };
    const group = (menu: Menu): void => {
      list.append(el("div", menu.label, "dropdown-header"));
      for (const memberId of menu.children) {
        const member = this.menus.get(memberId);
        if (!member || visited.has(member.id)) continue;
        visited.add(member.id);
        if (member.action !== null) item(member);
        else if (this.hasLeafAction(member.id)) group(member);
      }
    };
    for (const entry of moved) {
      const menu = this.menus.get(Number(entry.dataset["navEntry"]));
      if (!menu || visited.has(menu.id)) continue;
      visited.add(menu.id);
      if (menu.children.length === 0) item(menu); else group(menu);
    }
    const dropdown = attachMenuDropdown({
      trigger: toggle,
      menu: list,
      mode: "class-open",
      onOpen: () => {
        for (const other of this.navigation.querySelectorAll<HTMLElement>(".nav-dropdown-menu.open")) {
          if (other !== list) other.classList.remove("open");
        }
      }
    });
    // Same rule as the other top-bar menus: focus leaving the entry closes its panel.
    more.addEventListener("focusout", (event: FocusEvent) => {
      if (event.relatedTarget instanceof Node) {
        if (!more.contains(event.relatedTarget)) list.classList.remove("open");
        return;
      }
      window.setTimeout(() => { if (!more.contains(document.activeElement)) list.classList.remove("open"); }, 0);
    });
    this.disposeNavMore = () => dropdown.dispose();
  }

  private renderNavigation(route: Route): void {
    const activeRoot = this.resolveActiveRoot(route);
    const rootChanged = this.currentRootId !== activeRoot?.id;

    if (rootChanged || !this.navigation.hasChildNodes()) {
      this.currentRootId = activeRoot?.id;
      this.mobileDrawer?.dispose();
      for (const stop of this.disposeNavMenus) stop();
      this.disposeNavMenus = [];
      this.navigation.replaceChildren();

      const burgerBtn = button("", () => this.toggleMobileDrawer(), "burger-btn");
      burgerBtn.append(icon("menu"));
      burgerBtn.setAttribute("aria-label", "Toggle navigation");
      burgerBtn.setAttribute("data-burger-menu", "");

      const appSwitcher = button("", () => this.toggleHomeMenu(), "app-switcher-btn");
      appSwitcher.setAttribute("aria-label", "App switcher");
      appSwitcher.setAttribute("title", "App switcher");
      appSwitcher.setAttribute("data-app-switcher", "");
      appSwitcher.append(this.gridIcon());

      const sections = el("div", undefined, "app-sections");

      if (activeRoot) {
        const appBrand = button(activeRoot.label, () => this.run(async () => {
          const target = this.defaultActionForMenu(activeRoot.id);
          if (target) await this.navigate({ action: String(target.action), menu: String(target.menu) });
          else if (activeRoot.action !== null) await this.navigate({ action: String(activeRoot.action), menu: String(activeRoot.id) });
        }), "app-brand-btn");
        appBrand.dataset["menuId"] = String(activeRoot.id);
        sections.append(appBrand);

        for (const childId of activeRoot.children) {
          const child = this.menus.get(childId);
          if (!child) continue;
          if (child.children.length === 0) {
            if (child.action !== null) {
              const entry = button(child.label, () => this.run(() => this.navigate({ action: String(child.action), menu: String(child.id) })), "nav-item");
              entry.dataset["menuId"] = String(child.id);
              entry.dataset["navEntry"] = String(child.id);
              sections.append(entry);
            }
          } else if (this.hasLeafAction(child.id)) {
            const dropdown = el("div", undefined, "nav-dropdown");
            dropdown.dataset["navEntry"] = String(child.id);
            const menuList = el("div", undefined, "nav-dropdown-menu");
            const toggle = button(child.label, () => {}, "nav-dropdown-toggle");
            toggle.append(icon("chevron-down"));
            const navMenu = attachMenuDropdown({
              trigger: toggle,
              menu: menuList,
              mode: "class-open",
              onOpen: () => {
                for (const other of this.navigation.querySelectorAll<HTMLElement>(".nav-dropdown-menu.open")) {
                  if (other !== menuList) other.classList.remove("open");
                }
              }
            });
            this.disposeNavMenus.push(() => navMenu.dispose());
            dropdown.addEventListener("focusout", (event: FocusEvent) => {
              if (event.relatedTarget instanceof Node) {
                if (dropdown.contains(event.relatedTarget)) return;
                menuList.classList.remove("open");
                return;
              }
              window.setTimeout(() => {
                if (!dropdown.contains(document.activeElement)) menuList.classList.remove("open");
              }, 0);
            });
            // `visited` guards the recursion against a repeated or cyclic child list even though
            // `start` already refuses such a tree; categories without a reachable action are omitted.
            const visited = new Set<number>([child.id]);
            const appendDropdownItems = (parentId: number): void => {
              const parent = this.menus.get(parentId);
              if (!parent) return;
              for (const memberId of parent.children) {
                const member = this.menus.get(memberId);
                if (!member || visited.has(member.id)) continue;
                visited.add(member.id);
                if (member.action !== null) {
                  const item = button(member.label, () => {
                    menuList.classList.remove("open");
                    this.run(() => this.navigate({ action: String(member.action), menu: String(member.id) }));
                  }, "dropdown-item");
                  item.dataset["menuId"] = String(member.id);
                  menuList.append(item);
                } else if (this.hasLeafAction(member.id)) {
                  const header = el("div", member.label, "dropdown-header");
                  header.dataset["menuId"] = String(member.id);
                  menuList.append(header);
                  appendDropdownItems(member.id);
                }
              }
            };
            appendDropdownItems(child.id);

            dropdown.append(toggle, menuList);
            sections.append(dropdown);
          }
        }
      }

      // Every render is a navigation and closes the home menu (both branches below reset homeMenuOpen): the home route
      // shows its grid as the page itself. The scroll lock is released without restoring the previous page's position.
      const homeOverlay = el("div", undefined, "home-menu-overlay hidden");
      this.coverContent(false, homeOverlay, false);

      const backdrop = el("div", undefined, "mobile-drawer-backdrop hidden");
      backdrop.setAttribute("aria-hidden", "true");
      const mobileDrawer = el("div", undefined, "mobile-drawer hidden");
      mobileDrawer.id = "mobile-navigation-drawer";
      mobileDrawer.setAttribute("role", "dialog");
      mobileDrawer.setAttribute("aria-modal", "true");
      mobileDrawer.setAttribute("aria-labelledby", "mobile-navigation-title");

      const drawerHeader = el("div", undefined, "mobile-drawer-header");
      const drawerTitle = el("strong", activeRoot ? activeRoot.label : "ERPForAll");
      drawerTitle.id = "mobile-navigation-title";
      drawerHeader.append(drawerTitle);
      const drawerClose = button("", () => this.closeMobileDrawer(), "drawer-close-btn");
      drawerClose.append(icon("x"));
      drawerClose.setAttribute("aria-label", "Close drawer");
      drawerHeader.append(drawerClose);
      mobileDrawer.append(drawerHeader);
      const drawerContent = el("div", undefined, "mobile-drawer-content");

      if (activeRoot && activeRoot.children.length > 0) {
        const drawerSections = el("div", undefined, "mobile-drawer-sections");
        drawerSections.append(el("h3", "Sections", "drawer-section-title"));
        const drawerVisited = new Set<number>([activeRoot.id]);
        const appendDrawerItems = (parentId: number, level = 0): void => {
          const parent = this.menus.get(parentId);
          if (!parent) return;
          for (const memberId of parent.children) {
            const member = this.menus.get(memberId);
            if (!member || drawerVisited.has(member.id)) continue;
            drawerVisited.add(member.id);
            if (member.action !== null) {
              const item = button(member.label, () => {
                this.closeMobileDrawer();
                this.run(() => this.navigate({ action: String(member.action), menu: String(member.id) }));
              }, "drawer-item");
              item.dataset["menuId"] = String(member.id);
              if (level > 0) {
                item.style.paddingLeft = `${12 + level * 12}px`;
              }
              drawerSections.append(item);
            } else if (this.hasLeafAction(member.id)) {
              const subtitle = el("div", member.label, "drawer-section-subtitle");
              subtitle.dataset["menuId"] = String(member.id);
              if (level > 0) {
                subtitle.style.paddingLeft = `${12 + level * 12}px`;
              }
              drawerSections.append(subtitle);
              appendDrawerItems(member.id, level + 1);
            }
          }
        };
        appendDrawerItems(activeRoot.id);
        drawerContent.append(drawerSections);
      }

      const drawerApps = el("div", undefined, "mobile-drawer-apps");
      drawerApps.append(el("h3", "All Apps", "drawer-section-title"));
      for (const rootId of this.rootMenuIds) {
        const rootMenu = this.menus.get(rootId);
        if (!rootMenu) continue;
        const item = button(rootMenu.label, () => {
          this.closeMobileDrawer();
          const target = this.defaultActionForMenu(rootMenu.id);
          if (target) this.run(() => this.navigate({ action: String(target.action), menu: String(target.menu) }));
        }, "drawer-item");
        item.dataset["menuId"] = String(rootMenu.id);
        drawerApps.append(item);
      }
      drawerContent.append(drawerApps);
      mobileDrawer.append(drawerContent);

      this.navigation.append(burgerBtn, appSwitcher, sections, homeOverlay, backdrop, mobileDrawer, this.buildOfficeNavigation(route));
      // The row is fitted now, again once the web font has its real widths, and on every resize.
      this.fitNavSections(sections);
      void document.fonts?.ready.then(() => { if (sections.isConnected) this.fitNavSections(sections); });
      // A resize that leaves the row's width alone (height only) keeps an open More menu and its focus.
      const refit = (): void => { if (sections.clientWidth !== this.navFitWidth) this.fitNavSections(sections); };
      window.addEventListener("resize", refit);
      // The row also gets narrower without a window resize, when a systray entry (the activity menu) appears.
      const observer = typeof ResizeObserver === "undefined" ? undefined : new ResizeObserver(refit);
      observer?.observe(sections);
      this.disposeNavMenus.push(() => { window.removeEventListener("resize", refit); observer?.disconnect(); this.disposeNavMore?.(); this.disposeNavMore = undefined; });
      this.mobileDrawer = new MobileDrawer(mobileDrawer, backdrop, burgerBtn, () => {
        if (document.documentElement.dataset["officeTheme"] === "office-blue" && window.innerWidth > 768) {
          const target = this.navigation.querySelector<HTMLElement>(".office-navigation [aria-current=page]")
            ?? this.header.querySelector<HTMLButtonElement>("button.brand");
          // A user may have collapsed the selected branch before opening the narrow drawer.
          for (let group = target?.closest("details"); group; group = group.parentElement?.closest("details") ?? null) group.open = true;
          target?.focus();
        } else appSwitcher.focus();
      });
    }

    const activeMenu = route.menu ?? [...this.menus.values()].find(menu => route.action === String(menu.action))?.id.toString();
    if (route.action) {
      for (const entry of this.navigation.querySelectorAll<HTMLElement>("[data-menu-id]")) {
        if (entry.dataset["menuId"] === activeMenu) {
          entry.setAttribute("aria-current", "page");
          for (let group = entry.closest("details"); group; group = group.parentElement?.closest("details") ?? null) group.open = true;
        }
        else entry.removeAttribute("aria-current");
      }
    } else {
      for (const entry of this.navigation.querySelectorAll<HTMLElement>("[data-menu-id]")) {
        entry.removeAttribute("aria-current");
      }
    }
  }

  private async navigate(route: Route, initial = false, fromHistory = false, invocation?: { caller: RecordState; generation: number }, options?: { autosave?: boolean; target?: "current" | "main" | "new" }): Promise<void> {
    if(this.debug)route={...route,debug:"1"};
    if (invocation) {
      // A completed object call already saved/validated its record. Do not weaken canLeave's
      // global pending guard: only the exact active invocation can consume this handoff once.
      if (invocation !== this.pendingInvocation || invocation.generation !== this.generation || invocation.caller !== this.record ||
          invocation.caller.dirty || invocation.caller.busy || this.busy || this.wizard || !this.actionPending || this.uncertainMethod || this.expired) {
        throw new ClientError("validation", "The action's source is no longer available for navigation.");
      }
      this.pendingInvocation = undefined;
    } else if (!initial && !await this.canLeave(options) || this.expired) return;
    const previousGeneration = this.generation, previousList = this.listContext;
    let committed = false;
    if (options?.target === "main" || !route.action) {
      this.breadcrumbs.clear();
    }
    if (route.action !== this.current.action || route.method_action !== this.current.method_action) {
      this.listContext = undefined;
    }
    const generation = ++this.generation;
    this.busy = true;
    const finishLoading = this.loading.begin();
    const stagedDisposers: (() => void)[] = [];
    let settle!: () => void;
    const inFlight = new Promise<void>(resolve => { settle = resolve; });
    this.navigationInFlight = inFlight;
    try {
      const staging = el("div");
      let nextRecord: RecordState | undefined;
      let nextService: ServiceController | undefined;
      let nextDispose: (() => void) | undefined;
      let nextSettle: (() => void) | undefined;
      let panelDispose: (() => void) | undefined;

      if (!route.action) {
        this.homeMenuOpen = false;
        staging.append(el("h1", "Apps", "visually-hidden"), this.buildHomeContent());
      } else {
        this.homeMenuOpen = false;
        const actionId = rpcId(route.action);
        const entry = route.method_action ? this.methodActions.get(route.method_action) : undefined;
        if (entry && entry.origin.action !== route.action) throw new ClientError("validation", "Related action does not belong to this route.");
        const context = entry?.context ?? this.context;
        const action: Record<string, unknown> = entry
          ? { ...entry.result, wire_major: 1, target: "current", id: rpcId(route.action) }
          : await this.windowAction(actionId);
        if (action["wire_major"] !== 1 || action["target"] !== "current" || action["id"] !== rpcId(route.action)) {
          throw new ClientError("unsupported", "Unsupported or mismatched window action.");
        }
        let actionDomain: unknown[] = [];
        if (!entry) {
          if (action["domain"]) {
            const ast = decodeAst(parseBoundedJson(JSON.stringify(action["domain"])));
            const val = jsonValue(evalAst(ast, createContext({ names: { context: expressionValue(context, 0), uid: expressionValue(this.userId, 0) } })));
            if (!Array.isArray(val)) throw new ClientError("validation", "Action domain must evaluate to a list.");
            actionDomain = val;
          }
          noExpression(action["context"], "action contexts");
        }
        const model = text(action["model"], "action model");
        if (route.model && route.model !== model) throw new ClientError("validation", "URL model does not match the action.");
        const views = actionViews(action);
        const description = describeViews(await this.viewDescriptions(model, views, {}, context, entry ? undefined : actionId), model, this.registry);
        const defaultViewType = !route.id && views.some(v => v[1] === "list") ? "list" : String(views[0]?.[1]);
        const viewType = route.view_type ?? defaultViewType;
        const view = description.views.get(viewType);
        if (!view) throw new ClientError("unsupported", "The requested view is unavailable.");

        const actionTitle = text(action["name"], "action name");
        const mountedPanel = await mountControlPanel(staging, { title: actionTitle, model }, error => this.reportError(error));
        const panel = mountedPanel.panel, controls = mountedPanel.actions;
        panelDispose = mountedPanel.dispose;
        stagedDisposers.push(panelDispose);

        if (view.mode === "service") {
          if (route.id) throw new ClientError("validation", "A service action cannot open a record ID.");
          const widgets: Extract<Node, { kind: "widget" }>[] = [];
          const visit = (node: Node): void => {
            if (node.kind === "widget") widgets.push(node);
            else if ("children" in node) node.children.forEach(visit);
          };
          view.nodes.forEach(visit);
          if (widgets.length !== 1) throw new ClientError("unsupported", "A service form must contain exactly one registered widget.");
          const widget = widgets[0]!;
          nextService = await serviceControllers.get(widget.widget).create({
            host: staging,
            panel,
            controls,
            rpc: this.rpc,
            companies: this.companies,
            run: this.run,
            notice: this.notice, messages: this.messages, editorLocale: this.editorLocale,
            context,
            registry: this.registry,
            options: widget.options,
            settingsNavigation: {
              roots: this.settingsRootMenuIds.map(id => {
                const menu = this.menus.get(id);
                if (!menu) throw new ClientError("server", "Settings root menu is unavailable.");
                return { id: menu.id, module: menu.module, label: menu.label, icon: menu.icon };
              }),
              openTarget: async raw => {
                if (generation !== this.generation || this.expired || this.lifetime.signal.aborted) throw new ClientError("validation", "This Settings page is no longer available. Open Settings again.");
                const target = readSettingsTarget(raw);
                if (target.kind === "menu") {
                  const action = this.defaultActionForMenu(target.id);
                  if (!action) throw new ClientError("permission", "This Settings menu is unavailable or has no action. Reload Settings or contact your administrator.");
                  await this.navigate({ action: String(action.action), menu: String(action.menu) });
                } else {
                  await this.navigate({ action: String(target.id), ...(target.res_id === null ? {} : { id: String(target.res_id), view_type: "form" as const }) });
                }
              }
            }
          });
          const service = nextService;
          nextDispose = () => service.dispose?.();
          stagedDisposers.push(nextDispose);
        } else if (view.type === "list") {
          const actionHelp = typeof (action as Record<string, unknown>)["help"] === "string" ? (action as Record<string, unknown>)["help"] as string : undefined;
          const hasForm = !entry || views.some(view => view[1] === "form");
          const canCreate = hasForm && view.canCreate !== false && !isFalsy(context["create"]);
          if (canCreate) controls.append(actionButton("New", () => this.run(() => this.navigate({ ...route, id: undefined, view_type: "form" })), "primary"));
          const list = await this.listView(staging, panel, actionTitle, description, view, route, generation, actionHelp, views as [number | null, string][], {baseDomain: entry?.result.domain ?? actionDomain, context, hasForm, canCreate});
          nextDispose = list.dispose;
          nextSettle = list.settle;
          stagedDisposers.push(nextDispose);
        } else if (view.type === "pivot") {
          const pivot = await this.pivotView(staging, panel, actionTitle, description, view, route, generation, views as [number | null, string][], {baseDomain: entry?.result.domain ?? actionDomain, context});
          nextDispose = pivot.dispose;
          stagedDisposers.push(nextDispose);
        } else {
          await attachSubviews(description, view, async relation =>
            describeViews(await this.viewDescriptions(relation, [[null, "list"], [null, "form"]], {}, context), relation, this.registry));
          const canEdit = view.canEdit !== false && !isFalsy(context["edit"]);
          const formViewMeta = (!canEdit && route.id) ? {
            ...view,
            fields: view.fields.map(f => ({ ...f, readonly: true })),
          } : view;
          nextRecord = new RecordState(this.rpc, model, formViewMeta.fields, context);
          await nextRecord.load(route.id ? rpcId(route.id) : undefined);
          nextDispose = this.formView(staging, controls, panel, actionTitle, nextRecord, formViewMeta, route, views as [number | null, string][], context);
          stagedDisposers.push(nextDispose);
        }
      }

      if (generation !== this.generation || this.expired) {
        return;
      }
      for (const dispose of this.disposeWidgets) dispose();
      this.disposeWidgets = [...(nextDispose ? [nextDispose] : []), ...(panelDispose ? [panelDispose] : [])];
      stagedDisposers.length = 0;

      this.record = nextRecord;
      this.service = nextService;
      this.current = route;
      committed = true;

      this.renderNavigation(route);

      this.content.replaceChildren(...staging.childNodes);
      nextSettle?.();
      this.currentUrl = routeUrl(route);
      if (!fromHistory) {
        if (initial) history.replaceState({ erpIndex: this.historyIndex }, "", this.currentUrl);
        else history.pushState({ erpIndex: ++this.historyIndex }, "", this.currentUrl);
      }
      this.notice("");
      this.messages.navigation();
      this.messages.warning(this.record?.warningDetails);
      this.content.setAttribute("data-navigation-focus", "");
      // A new page starts at the top instead.
      this.content.focus({ preventScroll: true });
      documentScroller().scrollTop = 0;
    } finally {
      for (const dispose of stagedDisposers) dispose();
      if (!committed && generation === this.generation) {
        // The mounted source and its event bindings remain valid when destination staging fails.
        this.generation = previousGeneration;
        this.listContext = previousList;
      }
      finishLoading();
      this.busy = false;
      if (this.navigationInFlight === inFlight) this.navigationInFlight = undefined;
      settle();
    }
  }

  /** The bounds of the pivot table, from Settings (`web.pivot_*`): each has a default, a floor and a hard bound declared by the web addon. */
  private async pivotLimits(context: Record<string, unknown>): Promise<PivotLimits> {
    const keys = ["web.pivot_max_groups", "web.pivot_max_open_headers", "web.pivot_max_cells"];
    const response = object(await this.rpc.call("ir.config.setting", "erp_read_values", { scope: { kind: "global" }, keys, context }, { signal: this.lifetime.signal }), "pivot settings");
    if (response["wire_major"] !== 1) throw new ClientError("unsupported", "Unsupported settings response. Reload the page.");
    const values = array(response["values"], "pivot settings values").map(value => object(value, "pivot setting"));
    const read = (key: string): number => {
      const found = values.filter(value => value["key"] === key);
      const value = found.length === 1 ? found[0]!["value"] : undefined;
      if (typeof value !== "number" || !Number.isSafeInteger(value) || value < 1) throw new ClientError("server", `Invalid pivot setting ${key}.`);
      return value;
    };
    return { maxGroups: read(keys[0]!), maxOpen: read(keys[1]!), maxCells: read(keys[2]!) };
  }

  /** What a header can be expanded by: the model's groupable fields, its summable fields and the search view's group-by items. */
  private async pivotCatalogue(model: string, context: Record<string, unknown>): Promise<{ groupable: ReturnType<typeof decodePivotFields>["groupable"]; measures: ReturnType<typeof decodePivotFields>["measures"]; items: PivotGroupItem[]; /** What the search bar's Group By menu offers. */ groupBy: GroupByCatalogue }> {
    const [searchView, raw] = await Promise.all([
      this.viewDescriptions(model, [[null, "search"]], {}, context).catch((error: unknown) => {
        if (error instanceof ClientError && error.kind === "validation" && error.message.startsWith("No default view of type 'search'")) return undefined;
        throw error;
      }),
      this.rpc.call(model, "fields_get", { attributes: ["string", "type", "groupable", "selection", "aggregator"], context })
    ]);
    const decoded = decodePivotFields(raw);
    let nodes: Node[] = [];
    if (searchView !== undefined) {
      const search = describeViews(searchView, model, this.registry).views.get("search");
      if (!search) throw new ClientError("server", "The search view description is missing.");
      nodes = search.nodes;
    }
    const catalogue = buildGroupByCatalogue(nodes, ast => jsonValue(evalAst(ast, createContext({ names: { context: expressionValue(context, 0), uid: expressionValue(this.userId, 0) } }))),
      { groupable: decoded.groupable as unknown as Map<string, GroupableField>, excluded: new Map() }, true);
    const items: PivotGroupItem[] = [];
    for (const item of catalogue.items) {
      // The same check as the arch and the context: an interval on a field that is not a date is refused here, not by the server on use.
      try { items.push({ id: item.id, description: item.description, field: resolveSpec(item.interval ? `${item.fieldName}:${item.interval}` : item.fieldName, decoded.groupable) }); }
      catch (error) { if (!(error instanceof ClientError)) throw error; catalogue.skipped.push({ description: item.description, reason: error.message }); }
    }
    // The menu offers the items the pivot can group by, never the ones it left out.
    return { groupable: decoded.groupable, measures: decoded.measures, items, groupBy: { ...catalogue, items: catalogue.items.filter(item => items.some(found => found.id === item.id)) } };
  }

  /** The pivot view: the search bar's domain, grouped reads narrowed to the header that opens, the table, and its state in the address. */
  private async pivotView(staging: HTMLElement, panel: HTMLElement, actionTitle: string, description: Description, view: View, route: Route, generation: number, availableViews: [number | null, string][], scope: { baseDomain: unknown[]; context: Record<string, unknown> }): Promise<{ dispose: () => void }> {
    const { context, baseDomain } = scope;
    if (route.id !== undefined) throw new ClientError("validation", "The pivot view shows a whole table; the address cannot name one record.");
    const [limits, catalogue] = await Promise.all([this.pivotLimits(context), this.pivotCatalogue(description.model, context)]);
    const config = applyContext(readPivotView(view, catalogue), context);
    const params: PivotParams = {};
    for (const key of PIVOT_PARAMS) if (route[key] !== undefined) params[key] = route[key];
    const initial = Object.keys(params).length ? paramsState(params, config) : archState(config);
    panel.classList.add("control-panel-list");
    const breadcrumbs = panel.querySelector<HTMLElement>("[data-control-breadcrumbs]") || panel.querySelector<HTMLElement>("h1");
    if (breadcrumbs) {
      this.breadcrumbs.clear();
      this.breadcrumbs.push({
        id: `action-${route.action}-pivot`, title: actionTitle, isFormView: false,
        get route() { return { ...route }; }, get url() { return routeUrl(route); },
        onSelected: async () => { await this.navigate({ ...route }, false, false, undefined, { autosave: true }); }
      });
      const title = el("h1", actionTitle, "breadcrumb-current");
      title.setAttribute("aria-current", "page");
      breadcrumbs.replaceChildren(title);
    }
    const searchModel = new SearchModel({
      model: description.model, fields: description.fields, viewFields: view.fields,
      nameSearch: async (relation, name) => array(await this.rpc.call(relation, "web_name_search", { name, specification: { display_name: {} }, limit: 8, context }), "relation search").map(row => {
        const found = object(row);
        return { id: integer(found["id"], "relation id"), display_name: text(found["display_name"], "relation name") };
      }),
      userId: this.userId, companyId: this.companyId, initialDomain: baseDomain,
      // The search's Group By sets the rows (reference pivot_model.js), in the order chosen, with date intervals.
      groupBy: { load: async () => catalogue.groupBy, multiple: true, maxLevels: PIVOT_MAX_LEVELS }
    });
    searchModel.loadFromUrlParams(new URL(routeUrl(route), window.location.origin).searchParams);
    if (route.search && !new URL(routeUrl(route), window.location.origin).searchParams.has("search_facets")) {
      // The pivot arch declares only its groupings and measures, so `name` is looked for in the model's fields as well: the list builds this facet on `name`.
      const field = searchModel.getSearchableFields().find(f => f.name === "name") ?? (description.fields.get("name")?.kind === "char" ? description.fields.get("name") : undefined) ?? searchModel.getSearchableFields()[0];
      if (!field) throw new ClientError("unsupported", "Text search is not available for this model.");
      searchModel.addFieldFacet(field.name, route.search);
    }
    // The pivot's own state as an address: what differs from the arch is kept, so a reload (or a link) restores it.
    const routeFor = (snapshot: PivotSnapshot): Route => {
      const proposed = { ...route };
      for (const key of PIVOT_PARAMS) delete proposed[key];
      return Object.assign(proposed, snapshotParams(snapshot));
    };
    let groupingRows = false; // a search Group By is setting the rows: the address refusal then names what the user can change
    const accept = (snapshot: PivotSnapshot): void => {
      try { routeUrl(routeFor(snapshot)); }
      catch (error) {
        if (error instanceof ClientError && error.kind === "validation") throw new ClientError("validation", groupingRows
          ? "This table is too large to keep in the page address. Choose fewer Group By fields or narrow the search."
          : "This table is too large to keep in the page address. Collapse some headers or show fewer measures first.");
        throw error;
      }
    };
    const model = new PivotModel(config, ({ groupingSets, aggregates, domain }) => this.rpc.call(description.model, "formatted_read_grouping_sets", {
      domain, grouping_sets: groupingSets, aggregates, context
    }), () => searchModel.getDomain(), limits, initial, accept);
    await model.load();
    const writeAddress = (): void => {
      if (generation !== this.generation || this.expired) return; // the user left this pivot: the address is the next page's
      const proposed = routeFor(model.snapshot());
      const url = routeUrl(proposed);
      for (const key of PIVOT_PARAMS) { if (proposed[key] === undefined) delete route[key]; else route[key] = proposed[key]; }
      if (url !== this.currentUrl) { history.replaceState({ erpIndex: this.historyIndex }, "", url); this.currentUrl = url; }
    };
    // A cell opens the list of its records: a related list whose way back is this pivot's address, so the pivot comes back as it was left.
    const openCell = async (target: PivotCellTarget): Promise<void> => {
      const list = availableViews.find(item => item[1] === "list"), form = availableViews.find(item => item[1] === "form");
      const name = [...target.rowLabels, ...target.colLabels].join(" / ").slice(0, 200) || actionTitle;
      const result = actionResult({ wire_major: 1, result: { kind: "current_window", name, model: description.model, domain: target.domain, context: {},
        views: [{ id: list?.[0] ?? null, type: "list" }, ...(form ? [{ id: form[0], type: "form" }] : [])] } });
      if (result.kind !== "current_window") throw new ClientError("server", "The pivot cell did not open as a list.");
      const listContext = Object.fromEntries(Object.entries(context).filter(([key]) => !key.startsWith("pivot_")));
      const id = this.methodActions.add({ result, context: listContext, origin: { ...route }, originLabel: actionTitle });
      try { await this.navigate({ action: route.action, menu: route.menu, cids: route.cids, method_action: id, model: description.model, view_type: "list" }); }
      catch (error) { this.methodActions.remove(id); throw error; }
    };
    const pivot = mountPivot({ model, locale: this.numericLocale, title: actionTitle, run: job => this.run(job), openCell, changed: writeAddress,
      download: { model: description.model, send: async posted => { const { blob, fileName } = await this.rpc.postFile("/erp/pivot/export", posted); saveFile(blob, fileName); } } });

    let request = 0; // the same stale-answer guard the list uses: request number, route generation, expiry
    let appliedGroupBy = ""; // the search's groupings the rows follow; empty: the arch's (or the address's) rows
    const searchBar = createSearchBar(searchModel, () => this.run(async () => {
      const mine = ++request, current = (): boolean => mine === request && generation === this.generation && !this.expired;
      const parameters = new URLSearchParams();
      searchModel.serializeToUrlParams(parameters);
      const proposed = { ...route };
      for (const key of ["search", "search_facets", "search_filters", "search_groupby"] as const) {
        const value = parameters.get(key);
        if (value === null) delete proposed[key]; else proposed[key] = value;
      }
      const url = routeUrl(proposed); // validated whole before anything is committed
      for (const key of ["search", "search_facets", "search_filters", "search_groupby"] as const) { if (proposed[key] === undefined) delete route[key]; else route[key] = proposed[key]; }
      if (url !== this.currentUrl) { history.replaceState({ erpIndex: this.historyIndex }, "", url); this.currentUrl = url; }
      // The address is written before the read, as the list does: an answer that comes late never touches it.
      // A changed Group By sets the rows and reads the table (with the search); anything else reads it again and keeps the open headers.
      const wanted = searchModel.getGroupBy();
      try {
        if (wanted.join(",") !== appliedGroupBy) {
          groupingRows = true;
          try { await model.setRows(wanted.length ? wanted.map(spec => resolveSpec(spec, config.groupable)) : null); }
          catch (error) {
            // The rows did not change: the facet would say otherwise, so it goes, and the refusal says so.
            if (!(error instanceof ClientError) || error.kind !== "validation") throw error;
            const facet = searchModel.getFacets().find(item => item.type === "groupBy");
            if (facet) searchModel.removeFacet(facet.id);
            appliedGroupBy = ""; // no facet now: the rows stay as they are, and a later filter reads them again instead of resetting them
            throw new ClientError("validation", `Group By removed: ${error.message}`);
          } finally { groupingRows = false; }
          appliedGroupBy = wanted.join(",");
        } else await model.load();
      } finally { if (current()) pivot.render(); } // a failed read draws the table as unavailable
      // The reload keeps the expanded headers that still exist: the address follows.
      if (current()) writeAddress();
    }));
    const searchArea = panel.querySelector("[data-control-search]") || panel;
    if (searchArea !== panel) searchArea.replaceChildren(searchBar.element); else panel.append(searchBar.element);

    const switcher = el("div", undefined, "view-switcher");
    for (const [, vType] of availableViews) {
      if (vType !== "list" && vType !== "form" && vType !== "pivot" || availableViews.length < 2) continue;
      const vBtn = button(vType.charAt(0).toUpperCase() + vType.slice(1), () => this.run(async () => {
        const leaving = { ...route, id: undefined, view_type: vType as "list" | "form" | "pivot" };
        if (vType !== "pivot") for (const key of PIVOT_PARAMS) delete leaving[key];
        await this.navigate(leaving);
      }), `view-btn ${vType === "pivot" ? "active" : ""}`);
      vBtn.setAttribute("aria-label", `${vType} view`);
      if (vType === "pivot") vBtn.setAttribute("aria-current", "page");
      decorateViewButton(vBtn, vType);
      switcher.append(vBtn);
    }
    const toolsArea = panel.querySelector("[data-control-tools]");
    if (toolsArea) toolsArea.replaceChildren(switcher);
    else (panel.querySelector(".control-panel-heading") || panel).append(el("div", undefined, "control-panel-tools"), switcher);
    staging.append(pivot.element);
    return { dispose: () => { pivot.dispose(); searchBar.dispose(); } };
  }

  /** What the Group By menu offers for `model`: the search view's declared items and the fields the server can group by. */
  private async groupByCatalogue(model: string, context: Record<string, unknown>): Promise<GroupByCatalogue> {
    const [searchView, fields] = await Promise.all([
      this.viewDescriptions(model, [[null, "search"]], {}, context).catch((error: unknown) => {
        // A model with no search view has no declared items; the server answers it with this sentence (select_view).
        if (error instanceof ClientError && error.kind === "validation" && error.message.startsWith("No default view of type 'search'")) return undefined;
        throw error;
      }),
      this.rpc.call(model, "fields_get", { attributes: ["string", "type", "groupable", "selection", "aggregator", "currency_field"], context })
    ]);
    let nodes: Node[] = [];
    if (searchView !== undefined) {
      const search = describeViews(searchView, model, this.registry).views.get("search");
      if (!search) throw new ClientError("server", "The search view description is missing.");
      nodes = search.nodes;
    }
    return buildGroupByCatalogue(nodes, ast => jsonValue(evalAst(ast, createContext({ names: { context: expressionValue(context, 0), uid: expressionValue(this.userId, 0) } }))), decodeGroupableFields(fields, true), true, true);
  }

  private async listView(staging: HTMLElement, panel: HTMLElement, actionTitle: string, description: Description, view: View, route: Route, generation: number, actionHelp?: string, availableViews?: [number | null, string][], scope?: {baseDomain: unknown[]; context: Record<string, unknown>; hasForm: boolean; canCreate: boolean}): Promise<{ dispose: () => void; settle: () => void }> {
    const context = scope?.context ?? this.context, baseDomain = scope?.baseDomain ?? [], hasForm = scope?.hasForm ?? true, canCreate = scope?.canCreate ?? hasForm;
    const fieldStops: (() => void)[] = [];
    let groupedModel: ListGroupModel | undefined;
    const currencies = new CurrencyDisplayCache();
    let offset = 0, order = "id asc", request = 0, pageSize = view.pageLimit ?? 40;
    const officeBlue = document.documentElement.dataset["officeTheme"] === "office-blue";
    panel.classList.add("control-panel-list");
    const selectedIds = new Set<number>();
    // Groups the user opened (by group key); a new search, grouping or page starts folded again; a sort keeps them unless it moves the page or orders the groups.
    const resetGroups = (): void => { groupedModel?.dispose(); searchModel.clearListState(); };
    const columnWidths = new Map<string, number>();
    let settleColumns: (() => void) | undefined;

    const breadcrumbs = panel.querySelector<HTMLElement>("[data-control-breadcrumbs]") || panel.querySelector<HTMLElement>("h1");
    if (breadcrumbs) {
      this.breadcrumbs.clear();
      this.breadcrumbs.push({
        id: `action-${route.action}-list`,
        title: actionTitle,
        // Getters: the list's search facets are written into `route` after this crumb exists.
        get route() { return listRouteOf(route); },
        get url() { return routeUrl(listRouteOf(route)); },
        isFormView: false,
        onSelected: async () => {
          await this.navigate(listRouteOf(route), false, false, undefined, { autosave: true });
        }
      });
      const title = el("h1", actionTitle, "breadcrumb-current");
      title.setAttribute("aria-current", "page");
      breadcrumbs.replaceChildren(title);
      const related = route.method_action ? this.methodActions.get(route.method_action) : undefined;
      if (related) {
        // A list opened by a form button keeps the way back: menu path, the source record, this list.
        const trail = el("div", undefined, "form-breadcrumb-path");
        for (const item of this.menuTrail(related.origin)) trail.append(item, el("span", "›", "form-breadcrumb-divider"));
        trail.append(this.originCrumb(related), el("span", "›", "form-breadcrumb-divider"));
        title.className = "breadcrumb-current form-breadcrumb-record"; title.setAttribute("aria-current", "page");
        trail.append(title);
        breadcrumbs.replaceChildren(trail);
      } else {
        const menu = route.menu ? this.menus.get(Number(route.menu)) : [...this.menus.values()].find(item => String(item.action) === route.action);
        const labels: string[] = [];
        let ancestor = menu;
        while (ancestor) {
          labels.unshift(ancestor.label);
          ancestor = ancestor.parentId === null ? undefined : this.menus.get(ancestor.parentId);
        }
        if (labels.length) title.title = labels.join(" › ");
      }
    }

    const searchModel = new SearchModel({
      model: description.model,
      fields: description.fields,
      viewFields: view.fields,
      nameSearch: async (relation, name) => array(await this.rpc.call(relation, "web_name_search", {
        name, specification: { display_name: {} }, limit: 8, context: context
      }), "relation search").map(row => {
        const found = object(row);
        return { id: integer(found["id"], "relation id"), display_name: text(found["display_name"], "relation name") };
      }),
      userId: this.userId,
      companyId: this.companyId,
      initialDomain: baseDomain,
      groupBy: { load: () => this.groupByCatalogue(description.model, context), multiple: true, maxLevels: 32, persist: true, accept: params => {
        if (!params.has("search_groupby")) return;
        const proposed = { ...route };
        for (const key of ["search", "search_facets", "search_filters", "search_groupby"] as const) { const value = params.get(key); if (value === null) delete proposed[key]; else proposed[key] = value; }
        try { routeUrl(proposed); }
        catch (error) {
          if (error instanceof ClientError && error.kind === "validation") throw new ClientError("validation", "This search is too large to keep in the page address. Remove a filter or a Group By field first.");
          throw error;
        }
      } }
    });

    // Restore search state from route and URL search params
    const urlParams = new URL(routeUrl(route), window.location.origin).searchParams;
    if (urlParams.has("search_groupby")) await searchModel.loadGroupByCatalogue();
    searchModel.loadFromUrlParams(urlParams);
    const restoredList = searchModel.getListState();
    if (restoredList) { offset = restoredList.offset;
      if (restoredList.limit !== undefined) {
        if (![20, 40, 50, 80, pageSize].includes(restoredList.limit)) throw new ClientError("validation", "Invalid grouping page size.");
        pageSize = restoredList.limit;
      }
      if (restoredList.order) {
      if (!view.fields.some(field => restoredList.order.startsWith(`${field.name} `)) && restoredList.order !== "id asc") throw new ClientError("validation", "Invalid grouping sort.");
      order = restoredList.order;
    } }
    if (route.search && !urlParams.has("search_facets")) {
      const field = searchModel.getSearchableFields().find(f => f.name === "name") ?? searchModel.getSearchableFields()[0];
      if (!field) throw new ClientError("unsupported", "Text search is not available for this model.");
      searchModel.addFieldFacet(field.name, route.search);
    }

    const updateUrlWithSearch = (): void => {
      const parameters = new URLSearchParams();
      searchModel.serializeToUrlParams(parameters);
      const proposed = { ...route };
      for (const key of ["search", "search_facets", "search_filters", "search_groupby"] as const) {
        const value = parameters.get(key);
        if (value === null) delete proposed[key];
        else proposed[key] = value;
      }
      // Validate the complete encoded destination before changing the committed route or history.
      // SearchModel's character bound alone does not bound percent-encoded Unicode.
      const newUrl = routeUrl(proposed);
      for (const key of ["search", "search_facets", "search_filters", "search_groupby"] as const) {
        if (proposed[key] === undefined) delete route[key];
        else route[key] = proposed[key];
      }
      // While the list is still being staged the address is the previous page's (a record, a pivot): `navigate` writes the list's own.
      if (newUrl !== this.currentUrl && results.isConnected) {
        history.replaceState({ erpIndex: this.historyIndex }, "", newUrl);
        this.currentUrl = newUrl;
      }
    };

    const results = el("div", undefined, "table-scroll list-results");
    results.id = `list-results-${generation}`;
    const categoriesArea = panel.querySelector("[data-control-categories]") || panel;
    const category = view.searchPanel?.kind === "selection" ? view.searchPanel : undefined;
    const panelMessage = view.searchPanel?.kind === "sections" ? unavailablePanelMessage : view.searchPanel?.kind === "unsupported" ? view.searchPanel.message : category?.notice;
    if (panelMessage) {
      const message = el("p", panelMessage, "list-search-panel-message");
      message.setAttribute("role", "status"); categoriesArea.append(message);
    }
    if (!category && route.list_tab !== undefined) throw new ClientError("validation", "This list has no available selection category.");
    const initialTab = route.list_tab === "all" ? undefined : route.list_tab ?? (category?.defaultValue ? `value:${category.defaultValue}` : undefined);
    const tabs = category ? createListTabs({ field: category.field, panel: results, initial: initialTab, label: category.label, enableCounters: category.enableCounters, expand: category.expand, change: token => {
      if (this.keyboardPending !== undefined || this.busy || this.actionPending || this.messages.block.blocked) { this.notice("Wait for the current operation to finish."); return false; }
      this.run(async () => {
        route.list_tab = token ?? "all";
        offset = 0;
        resetGroups();
        updateUrlWithSearch();
        await load();
      });
      return true;
    } }) : undefined;
    if (tabs) categoriesArea.append(tabs.element);

    const searchBar = createSearchBar(searchModel, () => this.run(async () => {
      offset = 0;
      resetGroups();
      updateUrlWithSearch();
      await load();
    }));

    const searchArea = panel.querySelector("[data-control-search]") || panel;
    if (searchArea !== panel) searchArea.replaceChildren(searchBar.element);
    else panel.append(searchBar.element);

    let disposeQuickFilters = (): void => {};
    if (officeBlue) {
      // Only the real metadata-backed active/archive filters are exposed as a quick selector.
      const filters = searchModel.getFilters();
      if (filters.some(filter => filter.id === "filter_active") && filters.some(filter => filter.id === "filter_archived")) {
        const activeSelect = createChoiceDropdown({ label: "Active record filter", value: "all",
          choices: [{ value: "all", label: "Active filter" }, { value: "active", label: "Active" }, { value: "archived", label: "Archived" }, { value: "both", label: "Active and archived" }],
          onChange: chosen => this.run(async () => {
            for (const filter of searchModel.getFilters()) {
              if (filter.id !== "filter_active" && filter.id !== "filter_archived") continue;
              const wanted = chosen === "both" || chosen === (filter.id === "filter_active" ? "active" : "archived");
              if (filter.isActive !== wanted) searchModel.toggleFilter(filter.id);
            }
            offset = 0; resetGroups(); updateUrlWithSearch(); await load();
          })
        });
        activeSelect.element.classList.add("list-active-filter");
        const syncActive = (): void => {
          const current = searchModel.getFilters();
          const active = current.some(filter => filter.id === "filter_active" && filter.isActive);
          const archived = current.some(filter => filter.id === "filter_archived" && filter.isActive);
          activeSelect.setValue(active && archived ? "both" : active ? "active" : archived ? "archived" : "all");
        };
        searchBar.element.append(activeSelect.element);
        syncActive();
        const unsubscribe = searchModel.subscribe(syncActive);
        disposeQuickFilters = () => { unsubscribe(); activeSelect.dispose(); };
      }
    }

    const controls = panel.querySelector("[data-control-actions]") || staging;

    const selectionBox = el("div", undefined, "selection-box");
    selectionBox.style.display = "none";
    const selectionCount = el("span", "0 selected", "selection-count");

    const canDelete = view.canDelete !== false && !isFalsy(context["delete"]);
    let disposeActionMenu = (): void => {};
    if (canDelete) {
      const actionDropdown = el("div", undefined, "nav-dropdown action-dropdown");
      const actionMenu = el("div", undefined, "nav-dropdown-menu action-menu");
      const actionToggle = actionButton("Actions", () => {}, "action-toggle-btn");
      const dropdown = attachMenuDropdown({ trigger: actionToggle, menu: actionMenu, mode: "class-open" });
      disposeActionMenu = () => dropdown.dispose();
      const deleteBtn = actionButton("Delete", () => this.run(async () => {
        dropdown.close(false);
        if (!selectedIds.size) return;
        if (await this.choose("Delete selected records?", `Are you sure you want to delete ${selectedIds.size} record(s)? This action cannot be undone.`, ["Delete", "Cancel"]) !== "Delete") return;
        this.busy = true;
        try {
          await this.rpc.call(description.model, "unlink", { ids: Array.from(selectedIds), context: context }, { write: true });
          selectedIds.clear();
          this.notice("Record(s) deleted.");
        } finally {
          this.busy = false;
        }
        await load();
      }), "dropdown-item action-delete-btn danger");
      deleteBtn.setAttribute("role", "menuitem");
      actionMenu.append(deleteBtn);
      actionDropdown.append(actionToggle, actionMenu);
      selectionBox.append(selectionCount, actionDropdown);
    } else {
      selectionBox.append(selectionCount);
    }
    // Beside the title, not before it: the summary appears without moving the title in every appearance.
    (breadcrumbs ?? controls).append(selectionBox);

    const pager = el("div", undefined, "pager");
    const count = el("span", "", "pager-count muted");
    const previous = actionButton("Previous", () => this.run(async () => {
      offset = Math.max(0, offset - pageSize);
      searchModel.clearListState(); // another page of groups: the open groups were of this one
      await load();
    }), "pager-btn pager-prev");
    const next = actionButton("Next", () => this.run(async () => {
      offset += pageSize;
      searchModel.clearListState();
      await load();
    }), "pager-btn pager-next");
    previous.replaceChildren(icon("chevron-left")); previous.setAttribute("aria-label", "Previous page"); previous.title = "Previous page";
    next.replaceChildren(icon("chevron-right")); next.setAttribute("aria-label", "Next page"); next.title = "Next page";
    const pageNumbers = el("div", undefined, "pager-pages");
    pageNumbers.setAttribute("role", "group");
    pageNumbers.setAttribute("aria-label", "Pages");
    pager.append(count, previous, next);
    const pagerMenu = el("div", undefined, "nav-dropdown-menu pager-menu");
    const pagerToggle = button("", () => {}, "pager-options");
    pagerToggle.setAttribute("aria-label", "Pagination options"); pagerToggle.append(icon("chevron-down"));
    let closePageSize = (): void => {};
    const pagerDropdown = officeBlue ? attachMenuDropdown({ trigger: pagerToggle, menu: pagerMenu,
      items: () => [...pagerMenu.querySelectorAll<HTMLElement>(".pager-page, .list-page-size > .choice-dropdown-trigger")],
      onClose: () => closePageSize() }) : undefined;
    let disposePageSize = (): void => {};
    if (officeBlue) {
      const choices = el("div", undefined, "nav-dropdown");
      pagerMenu.append(pageNumbers); choices.append(pagerToggle, pagerMenu); pager.append(choices);
    }

    // View Switcher (List / Form buttons)
    const viewSwitcher = el("div", undefined, "view-switcher");
    if (availableViews && availableViews.length > 1) {
      for (const [, vType] of availableViews) {
        if (vType === "list" || vType === "form" || vType === "pivot") {
          const vBtn = button(vType.charAt(0).toUpperCase() + vType.slice(1), () => this.run(async () => {
            await this.navigate(switchedRoute(route, vType));
          }), `view-btn ${vType === "list" ? "active" : ""}`);
          vBtn.setAttribute("aria-label", `${vType} view`);
          decorateViewButton(vBtn, vType);
          viewSwitcher.append(vBtn);
        }
      }
    }

    const toolsArea = panel.querySelector("[data-control-tools]");
    if (toolsArea) {
      // Same order as the record form's control row: the pager, then the view switcher at the row's end.
      toolsArea.replaceChildren(pager, viewSwitcher);
    } else {
      const heading = panel.querySelector(".control-panel-heading") || panel;
      const tools = el("div", undefined, "control-panel-tools");
      tools.append(pager, viewSwitcher);
      heading.append(tools);
    }

    if (tabs) {
      results.id = `list-results-${description.model.replace(/\./g, "-")}`;
      results.setAttribute("role", "tabpanel");
      results.setAttribute("aria-label", `${category!.label} filtered records`);
      tabs.element.querySelectorAll("[role=tab]").forEach(tab => tab.setAttribute("aria-controls", results.id));
    }
    staging.append(results);
    if (officeBlue) {
      const allowedPageSizes = [...new Set([20, 40, 50, 80, pageSize])].sort((a, b) => a - b);
      const pageSizeControl = createChoiceDropdown({ label: "Rows per page", value: String(pageSize),
        choices: allowedPageSizes.map(size => ({ value: String(size), label: `${size} rows / page` })),
        onChange: chosen => this.run(async () => { pageSize = Number(chosen); offset = 0; searchModel.clearListState(); await load(); }) });
      pageSizeControl.element.classList.add("list-page-size");
      // An item of the pager's menu that opens its own choices.
      pageSizeControl.trigger.setAttribute("role", "menuitem");
      pagerMenu.append(pageSizeControl.element);
      closePageSize = () => pageSizeControl.close();
      disposePageSize = () => { pagerDropdown?.close(false); pageSizeControl.dispose(); pagerDropdown?.dispose(); };

    }

    const fetchRows = async (from: number, base = searchModel.getDomain()): Promise<Record<string, unknown>> => {
      return object(await this.rpc.call(description.model, "web_search_read", {
        domain: tabs ? tabs.rowDomain(base) : base,
        specification: specification(view.fields),
        offset: from,
        limit: pageSize,
        order,
        context: context
      }));
    };
    const activeAggregates = () => searchModel.getGroupAggregates().filter(a => view.fields.some(field => field.name === a.field && !field.invisible));
    const fetchGroups = async (from: number, base: unknown[], specs = searchModel.getGroupBy()): Promise<Record<string, unknown>> => object(await this.rpc.call(description.model, "web_read_group", {
      domain: tabs ? tabs.rowDomain(base) : base, groupby: specs,
      aggregates: [...new Set(["__count", ...activeAggregates().flatMap(a => [`${a.field}:${a.op}`, ...(a.currency ? [`${a.currency}:array_agg_distinct`] : [])])])],
      offset: from, limit: pageSize,
      ...(order.startsWith(`${specs[0]?.split(":")[0]} `) ? { order } : {}), context
    }));
    const validateGrouping = (open: ListGroupState["open"]): ListGroupState => {
      const state: ListGroupState = { v: 1, by: searchModel.getGroupBy(), offset, limit: pageSize, order, open };
      try { routeUrl({ ...route, search_groupby: JSON.stringify(state) }); }
      catch (error) {
        if (error instanceof ClientError && error.kind === "validation") throw new ClientError("validation", "Too many groups are open to keep in the page address. Close some groups first.");
        throw error;
      }
      return state;
    };
    const writeGrouping = (open: ListGroupState["open"]): void => {
      searchModel.setListState(validateGrouping(open)); updateUrlWithSearch();
    };
    const page = async (from: number): Promise<{ ids: number[]; total: number }> => {
      const answer = await fetchRows(from);
      return {
        ids: array(answer["records"], "records").map(r => integer(object(r)["id"], "record id")),
        total: integer(answer["length"], "record count")
      };
    };

    const markListUnavailable = (): void => {
      count.textContent = "Results unavailable";
      previous.disabled = true; next.disabled = true; pageNumbers.replaceChildren();
      selectedIds.clear(); selectionBox.style.display = "none"; selectionCount.textContent = "0 selected";
      if (results.isConnected) this.listContext = undefined;
      const message = el("p", "Unable to load these records. Adjust the search or reload the page.", "list-load-error");
      message.setAttribute("role", "alert"); results.replaceChildren(message);
    };
    const load = async (): Promise<void> => {
      const mine = ++request, base = searchModel.getDomain();
      const savedOpen = searchModel.getListState()?.open ?? [];
      groupedModel?.dispose();
      const groupName = searchModel.getGroupBy()[0], grouping = groupName === undefined ? undefined : searchModel.getGroupableField(groupName.split(":")[0]!);
      if (groupName !== undefined && !grouping) throw new ClientError("validation", `Cannot group by ${groupName}.`);
      const fetchMain = (from: number, domain: unknown[]): Promise<Record<string, unknown>> => grouping ? fetchGroups(from, domain) : fetchRows(from, domain);
      tabs?.element.setAttribute("aria-busy", "true");
      let loaded: [Record<string, unknown>, unknown];
      try {
        loaded = await Promise.all([fetchMain(offset, base), tabs && (category!.enableCounters || !category!.expand) ? this.rpc.call(description.model, "formatted_read_group", {
          domain: base, groupby: [category!.field.name], aggregates: ["__count"], context: context
        }) : Promise.resolve(undefined)]);
      } catch (error) {
        if (mine !== request || generation !== this.generation || this.expired) return;
        tabs?.invalidate(); markListUnavailable();
        throw error;
      }
      if (mine !== request || generation !== this.generation || this.expired) return;
      let response = loaded[0];
      const grouped = loaded[1];
      let tabCounts: ReturnType<typeof decodeSelectionCounts> | undefined;
      try { tabCounts = tabs && (category!.enableCounters || !category!.expand) ? decodeSelectionCounts(grouped, category!.field) : undefined; }
      catch (error) { tabs?.invalidate(); markListUnavailable(); throw error; }
      if (tabCounts && tabs!.reconcile(tabCounts)) {
        route.list_tab = "all"; offset = 0; updateUrlWithSearch();
        try { response = await fetchMain(0, base); }
        catch (error) { if (mine !== request || generation !== this.generation || this.expired) return; tabs!.invalidate(); markListUnavailable(); throw error; }
        if (mine !== request || generation !== this.generation || this.expired) return;
      }
      // A new page, search, sort or reload shows other rows; a selection must never outlive what is on screen.
      selectedIds.clear();
      let groups: ReturnType<typeof decodeGroups> | undefined;
      if (grouping) {
        try { groups = decodeGroups(response, grouping, pageSize, { spec: groupName!, aggregates: activeAggregates() }); }
        catch (error) { tabs?.invalidate(); markListUnavailable(); throw error; }
        // The groups shrank under an offset from the address, a reload or Back: show the last page, as a group's own pager does.
        if (offset > 0 && offset >= groups.length) {
          offset = groups.length ? Math.floor((groups.length - 1) / pageSize) * pageSize : 0;
          try { response = await fetchMain(offset, base); groups = decodeGroups(response, grouping, pageSize, { spec: groupName!, aggregates: activeAggregates() }); }
          catch (error) { if (mine !== request || generation !== this.generation || this.expired) return; tabs?.invalidate(); markListUnavailable(); throw error; }
          if (mine !== request || generation !== this.generation || this.expired) return;
        }
      }
      const length = groups ? groups.length : integer(response["length"], "record count");
      // Grouped: `rows` holds the records of the open groups, and the pager counts groups.
      let rows: unknown[] = groups ? [] : array(response["records"], "records");
      const shown = groups ? groups.groups.length : rows.length;
      count.textContent = shown ? `${offset + 1}–${offset + shown} / ${length}${groups ? " groups" : ""}` : groups ? "0 groups" : "0 records";
      previous.disabled = offset === 0;
      next.disabled = offset + shown >= length;
      if (officeBlue) {
        pageNumbers.replaceChildren();
        const totalPages = Math.ceil(length / pageSize), currentPage = Math.floor(offset / pageSize) + 1;
        const pages = new Set([1, totalPages, currentPage - 1, currentPage, currentPage + 1].filter(value => value >= 1 && value <= totalPages));
        let lastPage = 0;
        for (const number of [...pages].sort((a, b) => a - b)) {
          if (lastPage && number - lastPage > 1) pageNumbers.append(el("span", "…", "pager-ellipsis"));
          const pageButton = button(String(number), () => this.run(async () => {
            pagerDropdown?.close(true);
            offset = (number - 1) * pageSize;
            searchModel.clearListState();
            await load();
            if (panel.isConnected) pagerToggle.focus();
          }), "pager-page");
          pageButton.setAttribute("aria-label", `Page ${number}`);
          pageButton.setAttribute("role", "menuitem");
          if (number === currentPage) pageButton.setAttribute("aria-current", "page");
          pageNumbers.append(pageButton);
          lastPage = number;
        }
      }

      // A grouped list has no record order across groups yet (slice 2), so the form's pager has no neighbours.
      this.listContext = groups ? undefined : {
        ids: rows.map(r => integer(object(r)["id"], "record id")),
        total: length,
        offset,
        limit: pageSize,
        page
      };

      const table = el("table", undefined, "list-table table-hover"), head = el("thead"), header = el("tr"), body = el("tbody");
      table.setAttribute("aria-label", "Records");

      const updateSelectionUI = (): void => {
        selectionCount.textContent = `${selectedIds.size} selected`;
        selectionBox.style.display = selectedIds.size > 0 ? "inline-flex" : "none";
        headerCheckbox.checked = rows.length > 0 && selectedIds.size === rows.length;
        headerCheckbox.indeterminate = selectedIds.size > 0 && selectedIds.size < rows.length;
        // Grouped with every group folded there is nothing to select.
        headerCheckbox.disabled = Boolean(groups) && rows.length === 0;
        const trs = body.querySelectorAll("tr");
        trs.forEach(tr => {
          const rowId = Number(tr.getAttribute("data-id"));
          tr.classList.toggle("row-selected", selectedIds.has(rowId));
        });
      };

      const headerTh = el("th", undefined, "list-record-selector");
      headerTh.scope = "col";
      headerTh.append(el("span", "Select records", "visually-hidden"));
      const headerCheckbox = el("input", undefined, "list-select-all");
      headerCheckbox.type = "checkbox";
      headerCheckbox.setAttribute("aria-label", "Select all records");
      headerCheckbox.addEventListener("change", () => {
        if (headerCheckbox.checked) {
          for (const raw of rows) {
            selectedIds.add(integer(object(raw)["id"], "record id"));
          }
        } else {
          selectedIds.clear();
        }
        updateSelectionUI();
      });
      headerTh.append(headerCheckbox);
      header.append(headerTh);

      for (const field of view.fields.filter(field => !field.invisible)) {
        const th = el("th", undefined, "sortable-column");
        if (field.kind === "integer" || field.kind === "float" || field.kind === "monetary") th.classList.add("list-number");
        else if (field.kind === "boolean") th.classList.add("list-boolean");
        th.scope = "col";
        const isSorted = order.startsWith(field.name + " ");
        const isAsc = isSorted && order.endsWith("asc");
        th.setAttribute("aria-sort", isSorted ? (isAsc ? "ascending" : "descending") : "none");
        const sortBtn = button(field.label, () => this.run(async () => {
          order = `${field.name} ${isSorted && isAsc ? "desc" : "asc"}`;
          // Back to the first page, or the groups themselves reordered: the open groups may not be on the page that follows.
          if (offset > 0 || field.name === searchModel.getGroupBy()[0]?.split(":")[0]) searchModel.clearListState();
          offset = 0;
          await load();
        }), "sort-button");
        if (isSorted) {
          const arrow = icon("chevron-down"); // ascending rotates it in CSS; aria-sort carries the meaning
          arrow.classList.add("sort-indicator"); sortBtn.append(arrow);
        }
        th.append(sortBtn);
        header.append(th);
      }
      head.append(header);
      table.append(head, body);
      settleColumns = resizeColumns(table, view.fields.filter(field => !field.invisible), columnWidths);

      for (const stop of fieldStops.splice(0)) stop();

      const buildRow = (raw: unknown, stops: (() => void)[]): HTMLTableRowElement => {
        const record = object(raw), id = integer(record["id"], "record id"), row = el("tr");
        row.setAttribute("data-id", String(id));
        if (selectedIds.has(id)) row.classList.add("row-selected");

        const selectorTd = el("td", undefined, "list-record-selector");
        const rowCheckbox = el("input", undefined, "list-row-select");
        rowCheckbox.type = "checkbox";
        rowCheckbox.checked = selectedIds.has(id);
        rowCheckbox.setAttribute("aria-label", `Select record ${id}`);
        rowCheckbox.addEventListener("click", event => {
          event.stopPropagation();
        });
        rowCheckbox.addEventListener("change", () => {
          if (rowCheckbox.checked) selectedIds.add(id);
          else selectedIds.delete(id);
          updateSelectionUI();
        });
        selectorTd.addEventListener("click", event => {
          event.stopPropagation();
          if (event.target !== rowCheckbox) rowCheckbox.click();
        });
        selectorTd.append(rowCheckbox);
        row.append(selectorTd);

        view.fields.filter(field => !field.invisible).forEach((field, index) => {
          const cell = el("td");
          const adapter = resolveFieldWidget(field.widget ?? field.kind, "list");
          const handle = adapter.render({
            field: { ...field, readonly: true }, value: record[field.name], values: record, fields: view.fields.map(field => ({ ...field, readonly: true })), viewContext: "list",
            rpc: this.rpc, context, currencies, numericLocale: this.numericLocale, editorLocale: this.editorLocale,
            messages: this.messages, run: this.run, report: error => this.reportError(error), signal: this.lifetime.signal,
            openRecord: (model, related) => this.openRelatedRecord(model, related), change: () => { throw new ClientError("validation", "List display fields cannot be edited."); },
          });
          stops.push(handle.dispose);
          const widget = field.widget ?? field.kind;
          const readText = handle.node.querySelector<HTMLElement>(".field-read-value");
          if (["char", "text"].includes(widget) && adapter === fieldWidgets.get(widget) && !field.options?.password && readText) cell.title = readText.textContent ?? "";
          if (field.kind === "text") cell.classList.add("list-text");
          if (field.kind === "integer" || field.kind === "float" || field.kind === "monetary") cell.classList.add("list-number");
          else if (field.kind === "boolean") cell.classList.add("list-boolean");
          if (index === 0 && hasForm) {
            const open = button("", () => this.run(() => this.navigate({ ...route, id: String(id), view_type: "form" })), "link");
            open.dataset["recordOpen"] = String(id); open.title = `Open record ${id}`;
            const plainBuiltin = ["char", "text", "integer", "float", "monetary", "date", "datetime", "daterange", "boolean", "boolean_checkbox", "boolean_toggle", "selection", "badge", "label_selection", "badges_selection", "filterable_selection", "radio", "priority", "percentage"].includes(widget) && adapter === fieldWidgets.get(widget);
            const interactive = "input,select,textarea,button,a[href],[tabindex]";
            // Only known read adapters may be a primary value button. Links, reveals and trusted
            // custom/async action widgets retain a separate root-record action beside their output.
            if (plainBuiltin && !handle.node.matches(interactive) && !handle.node.querySelector(interactive)) {
              open.append(handle.node); cell.append(open);
            } else {
              open.classList.add("list-record-open"); open.setAttribute("aria-label", `Open record ${id}`);
              open.append(icon("chevron-right")); cell.append(handle.node, open);
            }
          } else cell.append(handle.node);
          row.append(cell);
        });

        if (hasForm) row.addEventListener("click", event => {
          if ((event.target as HTMLElement).closest("input, button, a")) return;
          this.run(() => this.navigate({ ...route, id: String(id), view_type: "form" }));
        });

        return row;
      };
      if (!groups) for (const raw of rows) body.append(buildRow(raw, fieldStops));
      results.replaceChildren(table);
      // Size the columns now, before the browser paints the content layout (columns.ts).
      settleColumns();
      if (tabCounts) tabs!.publish(tabCounts);
      else tabs?.finish();
      updateSelectionUI();

      if (groups) {
        const visible = view.fields.filter(field => !field.invisible), columns = 1 + visible.length;
        const model = new ListGroupModel({
          dimensions: searchModel.getGroupBy().map(spec => {
            const field = searchModel.getGroupableField(spec.split(":")[0]!);
            if (!field) throw new ClientError("validation", `Cannot group by ${spec}.`);
            return { spec, field };
          }), aggregates: activeAggregates(), limit: pageSize,
          groups: (extra, specs, from) => fetchGroups(from, [...base, ...extra], specs),
          records: (extra, from) => fetchRows(from, [...base, ...extra]),
          accept: open => { validateGrouping(open); }
        });
        groupedModel = model; model.adopt(response);
        const current = () => mine === request && generation === this.generation && !this.expired;
        const render = (focus?: { path: string; control?: "previous" | "next" }): void => {
          if (!current()) return;
          const aggregates = activeAggregates(); // once per drawing, not per row and column
          // A redraw removes the control that was pressed: it takes focus again, unless the user has moved on (as the pivot does).
          const keepFocus = body.contains(document.activeElement) || !document.activeElement || document.activeElement === document.body;
          for (const stop of fieldStops.splice(0)) stop();
          body.replaceChildren(); rows = [];
          const append = (nodes: ListGroupNode[]): void => { for (const node of nodes) {
            const head = createGroupRow({ group: node.group, columns, level: node.depth + 1, onToggle: () => {
              if (node.busy && !node.expanded) return;
              this.run(async () => {
                try {
                  if (node.expanded) model.close(node);
                  else { const opening = model.open(node); head.setBusy(node.busy); await opening; }
                  if (current()) writeGrouping(model.opened());
                } finally { head.setBusy(false); head.setExpanded(node.expanded); render({ path: node.path }); }
              });
            } });
            head.row.dataset["groupPath"] = node.path;
            head.toggle.style.paddingInlineStart = `calc(${node.depth} * var(--list-cell-x))`;
            head.setExpanded(node.expanded); head.setBusy(node.busy);
            const firstAggregate = visible.findIndex(field => aggregates.some(a => a.field === field.name));
            if (firstAggregate >= 0) {
              head.row.cells[0]!.colSpan = Math.max(1, firstAggregate + 1);
              for (const field of visible.slice(firstAggregate)) {
                const cell = el("td", undefined, "list-number");
                const a = aggregates.find(a => a.field === field.name);
                if (a) {
                  const value = node.group.aggregates?.[`${a.field}:${a.op}`];
                  if (value !== null && value !== false) {
                    const ids = a.currency ? node.group.aggregates?.[`${a.currency}:array_agg_distinct`] : undefined;
                    if (a.kind === "monetary" && (!Array.isArray(ids) || ids.length !== 1 || !Number.isSafeInteger(ids[0]) || Number(ids[0]) <= 0)) {
                      cell.textContent = "Unavailable"; cell.title = "An aggregate needs one known currency. Mixed-currency conversion is not available yet.";
                    } else {
                      const adapter = resolveFieldWidget(field.kind, "list");
                      const values = { [field.name]: value, ...(a.currency && Array.isArray(ids) ? { [a.currency]: ids[0] } : {}) };
                      const handle = adapter.render({ field: { ...field, readonly: true, ...(a.op === "avg" && field.kind === "integer" ? { kind: "float" as const } : {}) }, value, values, fields: view.fields, viewContext: "list", rpc: this.rpc, context, currencies, numericLocale: this.numericLocale, editorLocale: this.editorLocale, messages: this.messages, run: this.run, report: error => this.reportError(error), signal: this.lifetime.signal, change: () => { throw new ClientError("validation", "Group aggregates cannot be edited."); } });
                      fieldStops.push(handle.dispose); cell.append(handle.node);
                    }
                  }
                }
                head.row.append(cell);
              }
            }
            body.append(head.row);
            if (node.error) {
              const row = el("tr", undefined, "list-group-note"), cell = el("td"); cell.colSpan = columns;
              const message = el("span", `Could not load ${node.group.label}: ${node.error}`); message.setAttribute("role", "alert");
              cell.append(message, button("Retry", () => this.run(async () => { try { await model.page(node, node.offset); if (current()) writeGrouping(model.opened()); } finally { render({ path: node.path }); } })));
              row.append(cell); body.append(row);
            }
            if (node.expanded) {
              if (node.children.length) append(node.children);
              for (const record of node.records) { rows.push(record); body.append(buildRow(record, fieldStops)); }
              if (!node.error && node.total > pageSize) {
                const row = el("tr", undefined, "list-group-note"), cell = el("td"); cell.colSpan = columns;
                const page = (from: number, control: "previous" | "next") => this.run(async () => { try { await model.page(node, from); if (current()) writeGrouping(model.opened()); } finally { render({ path: node.path, control }); } });
                const previous = button("Previous", () => page(Math.max(0, node.offset - pageSize), "previous")), next = button("Next", () => page(node.offset + pageSize, "next"));
                previous.dataset["groupPath"] = node.path; previous.dataset["groupControl"] = "previous";
                next.dataset["groupPath"] = node.path; next.dataset["groupControl"] = "next";
                previous.setAttribute("aria-label", `Previous page of ${node.group.label}`); next.setAttribute("aria-label", `Next page of ${node.group.label}`);
                previous.disabled = node.busy || node.offset === 0; next.disabled = node.busy || node.offset + pageSize >= node.total;
                cell.append(previous, el("span", `${node.offset + 1}–${Math.min(node.offset + pageSize, node.total)} / ${node.total}`, "list-group-page-count"), next);
                row.append(cell); body.append(row);
              }
            }
          } };
          append(model.roots);
          const shown = new Set(rows.map(row => integer(object(row)["id"], "record id")));
          for (const id of selectedIds) if (!shown.has(id)) selectedIds.delete(id);
          updateSelectionUI();
          if (focus && keepFocus) {
            const target = focus.control && body.querySelector<HTMLButtonElement>(`button[data-group-control="${focus.control}"][data-group-path="${focus.path}"]:not(:disabled)`);
            (target || body.querySelector<HTMLButtonElement>(`[data-group-path="${focus.path}"] .list-group-toggle`))?.focus();
          }
          settleColumns?.();
        };
        render();
        try {
          const missing = await model.restore(savedOpen);
          if (current()) { if (missing.length) this.notice("Some saved groups are no longer in these results."); writeGrouping(model.opened()); render(); }
        } catch (error) { if (current()) { render(); this.notice(`Could not restore grouped records: ${errorMessage(error)}`); } }
      }

      if (!shown) {
        const empty = noContentHelp();
        empty.replaceChildren();
        if (searchModel.getFacets().length > 0) {
          empty.append(
            el("h2", "No matching records"),
            el("p", "Try another search or clear the filter.")
          );
        } else if (actionHelp) {
          const helpDiv = el("div", undefined, "action-help-text");
          helpDiv.textContent = actionHelp;
          empty.append(
            el("h2", `No ${actionTitle} records yet`),
            helpDiv
          );
        } else {
          empty.append(
            el("h2", `No ${actionTitle} records yet`),
            el("p", canCreate ? `Create your first ${actionTitle} record with New.` : "There is nothing to show here yet.")
          );
        }
        results.append(empty);
      }
    };

    try { await load(); } catch (error) { disposeActionMenu(); disposeQuickFilters(); disposePageSize(); searchBar.dispose(); throw error; }
    return {
      dispose: () => { for (const stop of fieldStops.splice(0)) stop(); groupedModel?.dispose(); disposeActionMenu(); disposeQuickFilters(); disposePageSize(); searchBar.dispose(); },
      settle: () => { settleColumns?.(); }
    };
  }

  /** Resolve only actions advertised by the authorized menu tree; normal navigation rechecks access and dirty state. */
  private async openRelatedRecord(model: string, id: number): Promise<void> {
    const generation = this.generation, checked = new Set<number>();
    for (const menu of this.menus.values()) {
      if (menu.action === null || checked.has(menu.action)) continue;
      checked.add(menu.action);
      const action = await this.windowActionOnly(menu.action);
      // A later user navigation cancels this read-only action discovery.
      if (generation !== this.generation) return;
      if (action["wire_major"] !== 1 || action["id"] !== menu.action || action["target"] !== "current") {
        throw new ClientError("unsupported", "Unsupported or mismatched related-record action.");
      }
      const actionModel = text(action["model"], "action model");
      const hasForm = array(action["views"], "action views").some(raw => object(raw)["type"] === "form");
      if (actionModel === model && hasForm) {
        await this.navigate({action:String(menu.action),menu:String(menu.id),model,id:String(id),view_type:"form"});
        return;
      }
    }
    throw new ClientError("unsupported", `No accessible form action is available for ${model}.`);
  }

  /** Opens the neighbour of a record in the list, loading the adjacent page when the record sits on a page edge. */
  private async stepRecord(route: Route, id: number, direction: -1 | 1): Promise<void> {
    const context = this.listContext;
    const idx = context ? context.ids.indexOf(id) : -1;
    if (!context || idx < 0) throw new ClientError("validation", "The record is no longer in the list.");
    let next = context;
    let target = context.ids[idx + direction];
    if (target === undefined) {
      const from = direction === 1 ? context.offset + context.ids.length : context.offset - context.limit;
      if (from < 0 || from >= context.total || !context.page) {
        throw new ClientError("validation", "There is no record in that direction.");
      }
      const loaded = await context.page(from);
      target = direction === 1 ? loaded.ids[0] : loaded.ids[loaded.ids.length - 1];
      if (target === undefined) throw new ClientError("server", "The adjacent page returned no records.");
      next = { ...context, ids: loaded.ids, total: loaded.total, offset: from };
    }
    this.listContext = next;
    try {
      await this.navigate({ ...route, id: String(target), view_type: "form" }, false, false, undefined, { autosave: true });
    } catch (error) {
      this.listContext = context;
      throw error;
    }
    if (this.current.id !== String(target)) this.listContext = context;
  }

  /** Whether work started under `binding` may still change the page. */
  private live(binding: Binding): boolean {
    return !this.expired && (binding === SESSION_SCOPE || binding === this.generation);
  }

  private myActivitiesAnchor: number | undefined;

  /** The id of the persisted "My Activities" action (`mail.mail_activity_action_my`): a list opened from the activity menu is anchored on it. */
  private async activitiesAnchor(): Promise<number> {
    if (this.myActivitiesAnchor !== undefined) return this.myActivitiesAnchor;
    // shortcut: matches res_model plus the exact stored domain text, so an edited or duplicated My Activities action makes every activity open fail; upgrade when employees can resolve an xml-id or the server returns the anchor id.
    const domainText = "[('user_id', '=', uid)]";
    const rows = array(await this.rpc.call("ir.actions.act_window", "search_read", {
      domain: [["res_model", "=", "mail.activity"], ["domain", "=", domainText]], fields: ["id"], limit: 2, context: this.context
    }), "My Activities action");
    if (rows.length !== 1) {
      // The user gets what to do; what was found (counts, domain, xml-id) goes to the console for the administrator.
      console.error(`My Activities anchor: found ${rows.length === 2 ? "2 or more" : rows.length} window actions on mail.activity with the domain ${domainText}; exactly one (mail.mail_activity_action_my) is needed.`);
      throw new ClientError("server", `The My Activities list is ${rows.length === 0 ? "missing" : "set up twice"}. Ask your administrator to restore it.`);
    }
    return this.myActivitiesAnchor = integer(object(rows[0], "My Activities action")["id"], "My Activities action id");
  }

  /**
    Opens the activity menu's list: `null` is "My Activities" itself, otherwise the checked action is admitted by the
   *  result normalizer and opened as a related list under that action. Focus then goes to the list's first control, when it has one. 
   */
  private async openActivities(action: ActivityWindowAction | null): Promise<void> {
    const anchor = await this.activitiesAnchor();
    const route: Route = { action: String(anchor) };
    let arrived: () => boolean;
    if (action === null) {
      await this.navigate(route);
      arrived = () => this.current.action === route.action && this.current.method_action === undefined;
    } else {
      const normalized = actionResult(await this.rpc.call("mail.activity", "erp_normalize_action_result", { wire_major: 1, result: action, context: this.context }));
      if (normalized.kind !== "current_window" || normalized.model !== action.res_model) throw new ClientError("server", "The activity list did not open as a list of the requested model.");
      const id = this.methodActions.add({ result: normalized, context: { ...this.context, ...normalized.context }, origin: { action: route.action! }, originLabel: "My Activities" });
      arrived = () => this.current.method_action === id;
      try { await this.navigate({ ...route, method_action: id, model: normalized.model, view_type: "list" }); }
      catch (error) { this.methodActions.remove(id); throw error; }
      if (!arrived()) this.methodActions.remove(id);
    }
    if (arrived()) {
      this.content.querySelector(".list-table")?.querySelector<HTMLElement>('button:not(:disabled), input:not(:disabled), select:not(:disabled), textarea:not(:disabled), a[href], [tabindex="0"]')?.focus();
    }
  }

  /** Links for the menu path of `route` (app, then menus); the last one opens the menu's action. */
  private menuTrail(route: Route): HTMLElement[] {
    const menu = route.menu ? this.menus.get(Number(route.menu)) : [...this.menus.values()].find(item => String(item.action) === route.action);
    const items: NonNullable<typeof menu>[] = [];
    for (let ancestor = menu; ancestor; ancestor = ancestor.parentId === null ? undefined : this.menus.get(ancestor.parentId)) items.unshift(ancestor);
    return items.map(item => item.action !== null
      ? button(item.label, () => this.run(() => this.navigate({ action: String(item.action), menu: String(item.id) })), "link")
      : el("span", item.label));
  }

  /** The breadcrumb link back to the record whose button opened a related action. */
  private originCrumb(entry: MethodAction): HTMLElement {
    return button(entry.originLabel, () => this.run(() => this.navigate({ ...entry.origin })), "link breadcrumb-origin");
  }

  private async consumeAction(result: ActionResult, caller: RecordState, binding: Binding, refresh: () => Promise<void>, source?: WizardDialog, callerContext = this.context, resubmit?: Resubmission): Promise<void> {
    if (!this.live(binding)) return;
    if (result.kind === "notification") {
      if (source && !result.then_close) source.notify(result);
      else this.messages.notifications.add(result.message, { ...(result.title ? { title: result.title } : {}), level: result.level });
      if (!result.then_close) { if (!source) await refresh(); return; }
      if (!source) throw new ClientError("validation", "Invalid transition: no action dialog to close.");
      await source.close(); return;
    }
    if (result.kind === "close" || result.kind === "none") {
      if (source) { await source.close(); return; }
      if (result.kind === "close") throw new ClientError("validation", "Invalid transition: no action dialog to close.");
      await refresh(); return;
    }
    if (result.kind === "current_window") {
      if (source) throw new ClientError("unsupported", "A dialog cannot open a current-window action yet.");
      const invocation = this.pendingInvocation;
      if (!invocation || invocation.caller !== caller || invocation.generation !== binding) throw new ClientError("validation", "The action's source is no longer available for navigation.");
      const context = { ...callerContext, active_model: caller.model, active_id: caller.id,
        active_ids: caller.id === undefined ? [] : [caller.id], ...result.context };
      const origin = { ...this.current };
      const name = caller.values["display_name"] ?? caller.values["name"];
      const originLabel = typeof name === "string" && name ? name : caller.id === undefined ? "Record" : `#${caller.id}`;
      const id = this.methodActions.add({ result, context, origin, originLabel });
      try {
        await this.navigate({ action: origin.action, menu: origin.menu, cids: origin.cids,
          method_action: id, model: result.model, view_type: "list" }, false, false, invocation);
      } catch (error) {
        this.methodActions.remove(id);
        // The method has already completed. Reload its known result without repeating it,
        // then report destination failure while retaining the mounted source.
        await refresh();
        throw error;
      }
      return;
    }
    // Only the identity confirmation carries the operation it answers.
    await this.openWindowDialog(result, caller, binding, refresh, source, callerContext, undefined,
      result.model === IDENTITY_MODEL ? resubmit : undefined);
  }

  /** The identity was confirmed: send the kept password operation again and open what it answers. Its own
   *  dialog was replaced by the confirmation, so a closing result only refreshes the caller. */
  private async resubmitPassword(resubmit: Resubmission, caller: RecordState, binding: Binding, refresh: () => Promise<void>, callerContext: Record<string, unknown>): Promise<void> {
    let value: unknown;
    try { value = await resubmit.call(); }
    catch (error) {
      // An interrupted resubmission may have changed a password: leave the caller read-only until it is reloaded,
      // and never send it again (item 4).
      if (error instanceof ClientError && error.kind === "ambiguous") this.uncertainMethod = true;
      throw error;
    }
    if (!this.live(binding)) return;
    const normalized = actionResult(await this.rpc.call(resubmit.model, "erp_normalize_action_result", { wire_major: 1, result: value, context: callerContext }));
    if (!this.live(binding)) return;
    if (normalized.kind === "close" || normalized.kind === "none") { await refresh(); return; }
    if (normalized.kind === "notification") {
      this.messages.notifications.add(normalized.message, { ...(normalized.title ? { title: normalized.title } : {}), level: normalized.level });
      await refresh(); return;
    }
    await this.consumeAction(normalized, caller, binding, refresh, undefined, callerContext, resubmit);
  }

  /** Open a `target="new"` window action as the one action dialog. `caller` is the record whose
   *  button produced the action; the user menu opens one without a caller record, and a result
   *  handed back from that dialog then names the dialog's own record as its caller. `onResult`
   *  runs after the dialog's own method result was consumed (the preferences dialog reloads the
   *  page there). */
  private async openWindowDialog(result: Extract<ActionResult, { kind: "window" }>, caller: RecordState | undefined, binding: Binding, refresh: () => Promise<void>, source: WizardDialog | undefined, callerContext: Record<string, unknown>, onResult?: (result: ActionResult, button: string | undefined) => void, confirmed?: Resubmission, profileTitle = false): Promise<void> {
    const previous = this.wizard;
    if (previous && ((previous !== source && previous.busy) || !await previous.canLeave(previous === source))) return;
    const context = { ...callerContext, ...(caller ? { active_model: caller.model, active_id: caller.id,
      active_ids: caller.id === undefined ? [] : [caller.id] } : {}), ...result.context };
    const description = describeViews(await this.rpc.call(result.model, "get_views", {
      views: result.views.map(view => [view.id, view.type]), wire_major: 1, options: { mobile: false }, context
    }), result.model, this.registry);
    if (!this.live(binding)) return;
    const view = description.views.get("form");
    if (!view || view.mode !== "record") throw new ClientError("unsupported", "A wizard requires a record form.");
    await attachSubviews(description, view, async relation => describeViews(await this.rpc.call(relation, "get_views", {
      views: [[null, "list"], [null, "form"]], wire_major: 1, options: { mobile: false }, context
    }), relation, this.registry));
    if (!this.live(binding)) return;
    const state = new RecordState(this.rpc, result.model, view.fields, context);
    await state.load(result.res_id ?? undefined);
    if (!this.live(binding)) return;
    let scope: WizardDialog;
    scope = new WizardDialog({ profileTitle, numericLocale: this.numericLocale, name: result.name, state, view, rpc: this.rpc, context,debug:this.debug,uid:this.userId,
      action:(id,effective)=>this.declaredButtonAction(id,state,view.fields,effective,binding,refresh,scope),
      url:href=>runUrlButton(href,{baseUrl:location.href,navigate:async route=>{if(!await scope.canLeave(true))return;await scope.close();await this.navigate(route);},openExternal:(url,target,features)=>{window.open(url,target,features);}}),
      openRecord: async (model, id) => { if (this.live(binding)) await this.openRelatedRecord(model, id); },
      report: error => this.hotkeyError(error, !this.live(binding)), notice: this.notice, messages: this.messages, editorLocale: this.editorLocale,
      result: async (value,effective=context,button,resubmit) => {
        const normalized = actionResult(await this.rpc.call(state.model, "erp_normalize_action_result", { wire_major: 1, result: value, context:effective }));
        if (this.wizard === scope && confirmed && button === IDENTITY_METHOD && normalized.kind === "close") {
          await scope.close();
          // a dialog that stayed open (an uncertain outcome) never sends the kept operation
          if (this.wizard !== scope && this.live(binding)) await this.resubmitPassword(confirmed, caller ?? state, binding, refresh, callerContext);
          return;
        }
        if (this.wizard === scope) {
          await this.consumeAction(normalized, caller ?? state, binding, refresh, scope, effective, resubmit);
          // only a dialog that really closed has finished: an uncertain save keeps it open
          if (this.wizard !== scope) onResult?.(normalized, button);
        }
      },
      closed: async () => {
        if (this.wizard !== scope) return;
        this.wizard = undefined;
        if (this.live(binding)) await refresh();
      }
    });
    previous?.dispose();
    this.wizard = scope;
    scope.mount();
  }

  private async applyUserColorScheme(): Promise<void> {
    try { await loadUserColorScheme(this.rpc, this.userId, this.context); } catch (error) { this.reportError(error); }
  }

  /** Reload the page so the session context (language, time zone, user name) is read again;
   *  replaceable so a test need not navigate. */
  reloadPage: () => void = () => { window.location.reload(); };

  /**
    "My Preferences": ask the server for the preferences action, admit it through the same
   *  bounded result normalizer as a button action, and open it on the current user's record. A
   *  method result that ends the dialog (Update Preferences) reloads the page; Cancel, Escape and
   *  the close button never reach the method and leave the page as it is. The
   *  dialog belongs to the session, not to the mounted route: the user menu is live before the
   *  first navigation has started, and that navigation must not drop the dialog.
   */
  private async openPreferences(): Promise<void> {
    const binding: Binding = SESSION_SCOPE;
    const context = this.context;
    const action = object(await this.rpc.call("res.users", "action_get", { context }), "preferences action");
    if (action["type"] !== "ir.actions.act_window" || action["target"] !== "new") {
      throw new ClientError("server", "The preferences action must be a dialog window action.");
    }
    const input = { type: "ir.actions.act_window", name: text(action["name"], "action name"), res_model: text(action["res_model"], "action model"),
      target: "new", views: action["views"], res_id: this.userId, context: {} };
    const normalized = actionResult(await this.rpc.call("res.users", "erp_normalize_action_result", { wire_major: 1, result: input, context }));
    if (!this.live(binding)) return;
    if (normalized.kind !== "window") throw new ClientError("server", "The preferences action did not open a dialog.");
    await this.openWindowDialog(normalized, undefined, binding, async () => {}, undefined, context, (result, button) => {
      if (button !== "preference_save" || (result.kind !== "close" && result.kind !== "none")) return;
      // The saved scheme shows at once; the reload then reads every session value again.
      void this.applyUserColorScheme().finally(() => this.reloadPage());
    }, undefined, true);
  }

  /** Load a declared action under the current caller, then reuse the existing result admission. */
  private async declaredButtonAction(id:number,state:RecordState,fields:readonly Field[],context:Record<string,unknown>,binding:Binding,refresh:()=>Promise<void>,source?:WizardDialog):Promise<void> {
    const activeContext={...context,active_model:state.model,active_id:state.id,active_ids:state.id===undefined?[]:[state.id]};
    const action=object(await this.rpc.call("ir.actions.act_window","erp_load_web_action",{ids:[id],context:activeContext}),"declared action");
    if(action["wire_major"]!==1 || action["id"]!==id)throw new ClientError("server","Mismatched declared button action.");
    const target=action["target"];
    if(target!=="current" && target!=="new")throw new ClientError("unsupported","This button action target is not supported.");
    const environment:ButtonEnvironment={model:state.model,context:activeContext,uid:this.userId,fields,values:()=>state.values,id:()=>state.id,dirty:()=>state.dirty,save:async()=>{},call:async()=>{throw new ClientError("server","Action expression cannot invoke an RPC.");},reload:async()=>{},confirm:async()=>false};
    const domain=action["domain"]===null?[]:buttonExpression(action["domain"],environment);
    const defaults=action["context"]===null?{}:buttonExpression(action["context"],environment);
    if(target==="new" && (!Array.isArray(domain) || domain.length))throw new ClientError("unsupported","A dialog action cannot carry a domain.");
    const resultInput={type:"ir.actions.act_window",name:text(action["name"],"action name"),res_model:text(action["model"],"action model"),target,views:actionViews(action),context:defaults,...(target==="current"?{domain}:{})};
    const normalized=actionResult(await this.rpc.call(state.model,"erp_normalize_action_result",{wire_major:1,result:resultInput,context:activeContext}));
    if(this.live(binding))await this.consumeAction(normalized,state,binding,refresh,source,activeContext);
  }

  private formView(staging: HTMLElement, controls: HTMLElement, panel: HTMLElement, actionTitle: string, state: RecordState, view: View, route: Route, availableViews?: [number | null, string][], context = this.context): () => void {
    const titleField = view.fields.find(f => f.name === "name" && f.kind === "char") || view.fields.find(f => f.kind === "char");
    let surface: RecordForm | undefined;
    let headingObserver: MutationObserver | undefined;
    let formHotkeys: ReturnType<typeof createHotkeyController> | undefined;
    let scalarChange: Promise<void> | undefined;
    let queuedAction: (() => Promise<void>) | undefined;
    const runFormAction = (job: () => Promise<void>): void => {
      if (this.lifetime.signal.aborted || binding !== this.generation || this.expired) return;
      if (scalarChange) {
        queuedAction = job;
        return;
      }
      this.run(job);
    };
    const debounce=new ButtonDebouncer();
    const hotkeys=new ButtonHotkeys(()=>surface?.controls().filter((control):control is HTMLButtonElement=>control instanceof HTMLButtonElement)??[],{blocked:()=>this.keyboardPending!==undefined||this.busy||this.actionPending||this.uncertainMethod||this.messages.block.blocked,report:error=>this.hotkeyError(error)});
    let disposed = false;
    const dispose = (): void => { if (disposed) return; disposed = true; headingObserver?.disconnect(); formHotkeys?.dispose(); hotkeys.dispose(); debounce.dispose(); surface?.dispose(); };

    panel.classList.add("control-panel-form");
    const toolsArea = panel.querySelector("[data-control-tools]");
    if (toolsArea && availableViews && availableViews.length > 1) {
      const viewSwitcher = el("div", undefined, "view-switcher");
      for (const [, vType] of availableViews) {
        if (vType === "list" || vType === "form" || vType === "pivot") {
          const vBtn = button(vType.charAt(0).toUpperCase() + vType.slice(1), () => runFormAction(async () => {
            await this.navigate(switchedRoute(route, vType));
          }), `view-btn ${vType === "form" ? "active" : ""}`);
          vBtn.setAttribute("aria-label", `${vType} view`);
          decorateViewButton(vBtn, vType);
          viewSwitcher.append(vBtn);
        }
      }
      toolsArea.replaceChildren(viewSwitcher);
    }

    const headerHost = el("div", undefined, "form-record-header");
    // Every appearance: the smart-button strip sits in the control row and the action row (header buttons + statusbar) heads the sheet column.
    const smartHost = el("div", undefined, "form-record-smart");
    const hasAside = view.nodes.some(node => node.kind === "root" && node.children.some(child => child.kind === "aside"));
    const asideHost = hasAside ? el("aside", undefined, "record-form-aside") : undefined;
    asideHost?.setAttribute("aria-label", "Record details");
    // The chatter slot exists only here, on the main record route; dialogs get no host.
    const hasChatter = view.nodes.some(node => node.kind === "root" && node.children.some(child => child.kind === "chatter"));
    const chatterHost = hasChatter ? el("aside", undefined, "record-form-chatter") : undefined;
    chatterHost?.setAttribute("aria-label", "Chatter");
    const breadcrumbs = panel.querySelector<HTMLElement>("[data-control-breadcrumbs]") || panel.querySelector<HTMLElement>("h1");
    const statusBadge = el("span", "Saved", "status-badge saved");
    const archivedBadge = el("span", "Archived", "status-badge archived");
    archivedBadge.hidden = true;
    // Archive is a rare, state-changing command: it lives in the Actions menu, never beside the primary header action.
    const archiveNames = new Set(["action_archive", "action_unarchive"]);
    const archivable = view.fields.some(field => field.name === "active") || view.nodes.some(node => node.kind === "button" && archiveNames.has(node.name));
    const archiveBtn = actionButton("Archive", () => {}, "dropdown-item form-archive-btn");
    archiveBtn.hidden = true;
    const statusActions = el("div", undefined, "status-actions");
    statusActions.style.display = "none";
    const saveBtn = actionButton("Save", () => runFormAction(save), "primary status-btn-save");
    const discardBtn = actionButton("Discard", () => runFormAction(discard), "status-btn-discard");
    statusActions.append(saveBtn, discardBtn);
    const statusIndicator = el("div", undefined, "form-status-indicator");
    statusIndicator.append(statusBadge, archivedBadge, statusActions);

    const canEdit = view.canEdit !== false && !isFalsy(context["edit"]);
    if (!canEdit) this.lockedRecords.add(state);
    const updateStatus = (): void => {
      if ((state.dirty || !state.id) && (canEdit || !state.id)) {
        statusBadge.className = "status-badge dirty";
        statusBadge.textContent = state.dirty ? "Unsaved changes" : "New";
        statusBadge.hidden = !state.dirty;
        statusActions.style.display = "inline-flex";
      } else {
        statusBadge.className = "status-badge saved";
        statusBadge.textContent = "Saved";
        statusBadge.hidden = true;
        statusActions.style.display = "none";
      }
      const archived = state.id !== undefined && state.values["active"] === false;
      archivedBadge.hidden = !archived;
      relabelActionButton(archiveBtn, archived ? "Unarchive" : "Archive");
      archiveBtn.hidden = !archivable || state.id === undefined || !canEdit;
    };

    // Default theme: the record menu sits beside the record name in the breadcrumb (set once it is built below).
    let recordMenu: HTMLElement | undefined;
    let syncPageHeading: (() => void) | undefined;
    const updateBreadcrumbs = (): void => {
      if (!breadcrumbs) return;
      breadcrumbs.replaceChildren();
      const title = titleField ? state.values[titleField.name] : undefined;
      const labelField = view.fields.find(field => field.name === "display_name" && !field.invisible)
        ?? view.fields.find(field => field.name === "name" && field.kind === "char" && !field.invisible)
        ?? view.fields.find(field => field.kind === "char" && !field.invisible);
      const displayName = labelField ? state.values[labelField.name] : undefined;
      const currentName = !state.id ? "New" : typeof displayName === "string" && displayName ? displayName : (typeof title === "string" && title ? title : `#${state.id}`);

      // Maintain BreadcrumbTrail for
      if (this.breadcrumbs.items.length === 0 || this.breadcrumbs.current?.route.action !== route.action) {
        this.breadcrumbs.clear();
        this.breadcrumbs.push({
          id: `action-${route.action}-list`,
          title: actionTitle,
          route: listRouteOf(route),
          url: routeUrl(listRouteOf(route)),
          isFormView: false,
          onSelected: async () => {
            await this.navigate(listRouteOf(route), false, false, undefined, { autosave: true });
          }
        });
      }
      if (this.breadcrumbs.current?.isFormView) {
        this.breadcrumbs.replaceCurrent({
          id: `action-${route.action}-form-${state.id ?? "new"}`,
          title: currentName,
          route: { ...route },
          url: routeUrl(route),
          isFormView: true,
          onSelected: async () => {}
        });
      } else {
        this.breadcrumbs.push({
          id: `action-${route.action}-form-${state.id ?? "new"}`,
          title: currentName,
          route: { ...route },
          url: routeUrl(route),
          isFormView: true,
          onSelected: async () => {}
        });
      }

      const isSmall = document.documentElement.dataset["mobile"] === "true";
      renderBreadcrumbs(breadcrumbs, this.breadcrumbs, {
        isSmall,
        onNavigate: async item => this.run(async () => {
          await this.navigate(item.route, false, false, undefined, { autosave: true });
        }),
        report: error => this.reportError(error)
      });
      // A form opened from a related list keeps the way back to the record whose button opened it.
      const origin = route.method_action ? this.methodActions.get(route.method_action) : undefined;
      const path = origin ? breadcrumbs.querySelector(".form-breadcrumb-path") : null;
      if (origin && path) path.prepend(this.originCrumb(origin), el("span", "›", "form-breadcrumb-divider"));
      if (recordMenu) breadcrumbs.append(recordMenu);
      breadcrumbs.append(statusIndicator);
      syncPageHeading?.();
    };
    updateBreadcrumbs();
    updateStatus();

    // Setup pager in control panel search area
    const searchArea = panel.querySelector("[data-control-search]");
    if (searchArea) {
      const pager = el("div", undefined, "pager form-pager");
      const pagerCount = el("span", "", "pager-count muted");
      const prevBtn = button("", () => {}, "pager-btn pager-prev");
      prevBtn.append(icon("chevron-left"));
      prevBtn.setAttribute("aria-label", "Previous record");
      const nextBtn = button("", () => {}, "pager-btn pager-next");
      nextBtn.append(icon("chevron-right"));
      nextBtn.setAttribute("aria-label", "Next record");
      pager.append(pagerCount, prevBtn, nextBtn);

      const updatePager = (): void => {
        if (state.id && this.listContext && this.listContext.ids.includes(state.id)) {
          const idx = this.listContext.ids.indexOf(state.id);
          pagerCount.textContent = `${this.listContext.offset + idx + 1} / ${this.listContext.total}`;
          prevBtn.disabled = idx === 0 && this.listContext.offset === 0;
          nextBtn.disabled = this.listContext.offset + idx + 1 >= this.listContext.total;
          const step = (direction: -1 | 1) => () => this.run(async () => {
            if (state.dirty && !await trySave()) return;
            await this.stepRecord(route, state.id!, direction);
          });
          prevBtn.onclick = step(-1);
          nextBtn.onclick = step(1);
        } else if (state.id) {
          pagerCount.textContent = "1 / 1";
          prevBtn.disabled = true;
          nextBtn.disabled = true;
        } else {
          pagerCount.textContent = "New";
          prevBtn.disabled = true;
          nextBtn.disabled = true;
        }
      };
      updatePager();
      searchArea.replaceChildren();
      toolsArea?.prepend(pager);
    }

    const sheet = el("div", undefined, "content-sheet"), form = el("form", undefined, "record-form");
    // Route every submit through save(), which reveals a hidden invalid page before native validation.
    form.noValidate = true;
    // The addon's context line (the control panel's extension slot) reads as the first line of the sheet.
    const contextLine = panel.querySelector<HTMLElement>("[data-control-extension]");
    if (contextLine) { contextLine.classList.add("form-record-context"); sheet.append(contextLine); }
    sheet.append(form);
    // Staging itself is discarded on navigation; keep the form styling anchor in the mounted tree.
    const viewport = el("div", undefined, "record-form-viewport");
    panel.classList.toggle("has-chatter", hasChatter); panel.classList.toggle("has-aside", hasAside);
    viewport.append(headerHost);
    viewport.append(sheet);
    if (asideHost) viewport.append(asideHost);
    if (chatterHost) viewport.append(chatterHost);
    staging.append(viewport);

    // A declared title keeps its XML placement. A legacy form instead names the page in its breadcrumb.
    syncPageHeading = (): void => {
      // The addon's context line reads as helper text under the title, wherever a render put the title.
      const titleBlock = form.querySelector(".form-title-block");
      if (contextLine && titleBlock && titleBlock.nextElementSibling !== contextLine) titleBlock.after(contextLine);
      // A render without a title block took the line away with it: it returns to the top of the sheet.
      else if (contextLine && !titleBlock && !sheet.contains(contextLine)) sheet.insertBefore(contextLine, form);
      const current = breadcrumbs?.querySelector<HTMLElement>(".form-breadcrumb-record");
      if (!current) return;
      const declared = [...form.querySelectorAll("h1")].some(heading => !heading.closest("[hidden]"));
      const tag = declared ? "span" : "h1";
      if (current.tagName.toLowerCase() !== tag) {
        const replacement = el(tag); replacement.className = current.className;
        replacement.setAttribute("aria-current", "page"); replacement.textContent = current.textContent;
        current.replaceWith(replacement);
      }
    };
    headingObserver = new MutationObserver(() => syncPageHeading?.());
    headingObserver.observe(form, { childList: true, subtree: true, attributes: true, attributeFilter: ["hidden"] });

    const focusTarget = (element: Element | null): string | undefined => {
      if (element instanceof HTMLElement && headerHost.contains(element)) return element.dataset["focusKey"];
      return formFocus(form, element);
    };
    const render = (focus = surface?.focus()): void => {
      if (disposed || this.lifetime.signal.aborted || this.expired) return;
      surface!.render(focus); surface!.setDisabled(runningButton || this.uncertainMethod); updateStatus(); syncPageHeading?.();
      // Saving during an action can replace widgets and overflow controls while the action still runs.
      if (runningButton) for (const control of surface!.controls()) if (control instanceof HTMLButtonElement) control.disabled = true;
      if (this.uncertainMethod) {
        surface!.setDisabled(true);
        for (const control of surface!.controls()) control.disabled = true;
        // The record menu lives in the breadcrumb in the default theme, outside `controls`.
        for (const control of [...controls.querySelectorAll<HTMLButtonElement>("button"), ...actionMenu.querySelectorAll<HTMLButtonElement>("button")]) if (!control.matches(".action-toggle-btn,.form-reload-btn")) control.disabled = true;
        saveBtn.disabled = true; discardBtn.disabled = true;
        this.notice(uncertainMethodMessage);
      }
    };

    // Resolves true once saved; false when browser validation stopped it. Server errors propagate.
    const trySave = async (): Promise<boolean> => {
      if (!canEdit && state.id) return false;
      if (this.uncertainMethod) throw new ClientError("ambiguous", uncertainMethodMessage);
      if (this.busy) return false;
      const binding = this.generation;
      this.busy = true;
      try {
        if (surface!.needsCommit()) await surface!.commit();
        if (this.lifetime.signal.aborted || binding !== this.generation) return false;
        if (!surface!.valid()) return false;
        surface!.setDisabled(true);
        // A refusal that names fields is shown on those fields; only an error that names none reaches the error dialog.
        try { await state.save(); } catch (error) { if (surface!.flagServerError(error)) return false; throw error; }
        if (this.lifetime.signal.aborted || binding !== this.generation) return true;
        render();
        this.current = { ...route, id: String(state.id), view_type: "form" };
        this.currentUrl = routeUrl(this.current);
        history.replaceState({ erpIndex: this.historyIndex }, "", this.currentUrl);
        updateBreadcrumbs(); updateStatus(); this.notice("Record saved.");
        return true;
      } finally {
        this.busy = false;
        if (!this.lifetime.signal.aborted && binding === this.generation) surface!.setDisabled(runningButton || this.uncertainMethod);
      }
    };
    const save = async (): Promise<void> => { await trySave(); };

    const discard = async (): Promise<void> => {
      state.discard();
      if (!state.id) await this.navigate({ ...route, id: undefined, view_type: "list" });
      else {
        render();
        updateStatus();
      }
    };

    // Header object buttons. `running` disables every header button for the whole click, which
    // also covers the re-render a save performs in the middle of it.
    let runningButton = false;
    const binding = this.generation;
    const brokenButtons = new Set<string>();
    const renderButton = (node: ButtonNode, parent: HTMLElement): HTMLButtonElement | undefined => {
      let failure: string | undefined;
      try { if (buttonHidden(node, view.fields, state.values)) return; }
      catch (error) {
        // A visibility expression that cannot run keeps the button inert and says why.
        failure = errorMessage(error);
        if (!brokenButtons.has(node.name)) { brokenButtons.add(node.name); this.reportError(error); }
      }
      // A declared Archive button is the same command as the Actions menu entry; it is not repeated in the header.
      if (node.kind === "button" && archiveNames.has(node.name)) return;
      const control = viewButton(node,{activate:()=>debounce.run(node,node.details?.debounce_ms??0,()=>runFormAction(()=>clickButton(node))),model:state.model,debug:this.debug,messages:this.messages,report:error=>this.reportError(error),className:`form-header-btn ${node.variant}`});
      control.dataset["focusKey"] = `button:${node.name}`;
      control.disabled = control.disabled || runningButton || this.uncertainMethod || failure !== undefined;
      if (failure !== undefined) control.title = `Unavailable: ${failure}`;
      parent.append(control);
      return control;
    };
    const clickButton = async (node: ButtonNode): Promise<void> => {
      if (runningButton || this.uncertainMethod || binding !== this.generation || this.expired) return;
      const focus = focusTarget(document.activeElement);
      runningButton = true; this.actionPending = true; surface!.setDisabled(true);
      const invocation = { caller: state, generation: binding };
      this.pendingInvocation = invocation;
      const releaseButtons = lockButtonControls([
        ...surface!.controls().filter((control): control is HTMLButtonElement => control instanceof HTMLButtonElement),
        ...panel.querySelectorAll<HTMLButtonElement>("button")
      ]);
      try {
        await runObjectButton(node, {
          model: state.model, context: context,uid:this.userId, fields:view.fields,values:()=>state.values,id: () => state.id, dirty: () => state.dirty,
          block:()=>this.messages.block.begin(),
          action:(id,effective)=>this.declaredButtonAction(id,state,view.fields,effective,binding,async()=>{await state.load(state.id);if(binding===this.generation&&!this.expired)render(focus);}),
          url:href=>runUrlButton(href,{baseUrl:location.href,navigate:route=>this.navigate(route,false,false,invocation),openExternal:(url,target,features)=>{window.open(url,target,features);}}),
          save,
          call: async (model, method, args) => {
            try { return await this.rpc.call(model, method, args, { write: true }); }
            catch (error) { if (error instanceof ClientError && error.kind === "ambiguous") this.uncertainMethod = true; throw error; }
          },
          result: async (value,effective=context,resubmit) => {
            let result: ActionResult;
            try {
              result = actionResult(await this.rpc.call(state.model, "erp_normalize_action_result", { wire_major: 1, result: value, context:effective }));
            } catch (error) { this.uncertainMethod = true; throw error; }
            if (binding !== this.generation || this.expired) return;
            await this.consumeAction(result, state, binding, async () => {
              await state.load(state.id);
              if (binding === this.generation && !this.expired) { render(focus); updateBreadcrumbs(); updateStatus(); }
            }, undefined, effective, resubmit);
          },
          reload: async () => {
            this.busy = true;
            try { await state.load(state.id); } finally { this.busy = false; }
          },
          confirm: async (title, message, accept, cancel="Cancel") => await confirmChoice({title,message,accept,cancel,signal:this.lifetime.signal,service:this.messages.dialogs}) && binding === this.generation && !this.expired
        });
      } finally {
        if (this.pendingInvocation === invocation) this.pendingInvocation = undefined;
        runningButton = false; this.actionPending = false;
        // End this invocation's transient lock. The render below reapplies uncertainty to writes,
        // while the persistent read-only Reload control remains available for recovery.
        releaseButtons();
        if (binding !== this.generation || this.expired) return;
        if (!this.wizard) { render(focus); updateBreadcrumbs(); updateStatus(); }
      }
    };

    controls.replaceChildren();
    const canCreate = view.canCreate !== false && !isFalsy(context["create"]);
    if (canCreate) {
      controls.append(
        actionButton("New", () => runFormAction(() => this.navigate({ ...route, id: undefined, view_type: "form" })))
      );
    }

    const actionDropdown = el("div", undefined, "nav-dropdown form-actions-dropdown");
    // The record menu sits beside the record name in every appearance.
    const actionToggle = button("", () => {}, "action-toggle-btn record-menu-btn");
    actionToggle.setAttribute("aria-label", "Actions"); actionToggle.setAttribute("aria-haspopup", "menu");
    actionToggle.title = "Actions"; actionToggle.append(el("span", undefined, "record-menu-dots"));
    const actionMenu = el("div", undefined, "nav-dropdown-menu form-action-menu");

    const reloadBtn = actionButton("Reload", () => this.run(async () => {
      actionMenu.classList.remove("open");
      if (state.dirty && await this.choose("Reload record?", "Unsaved edits will be discarded. Reload also checks the outcome of an interrupted save.", ["Reload", "Cancel"]) !== "Reload") return;
      this.busy = true;
      try {
        await state.load(state.id);
        render();
        updateStatus();
        this.notice(this.uncertainMethod ? uncertainMethodMessage : "Record reloaded.");
      } finally {
        this.busy = false;
      }
    }), "dropdown-item form-reload-btn");
    // Duplicate: the generic `copy` method (declared on every model) returns the new id; a dirty record is saved first.
    const refuseUncertain = (): void => { if (this.uncertainMethod) throw new ClientError("ambiguous", uncertainMethodMessage); };
    const duplicateBtn = actionButton("Duplicate", () => runFormAction(async () => {
      actionMenu.classList.remove("open");
      refuseUncertain();
      if (!state.id) return;
      if (state.dirty && !await trySave()) return;
      this.busy = true;
      let copied: unknown;
      try { copied = await this.rpc.call(state.model, "copy", { ids: [state.id], context: context }, { write: true }); }
      finally { this.busy = false; }
      const copiedId = Array.isArray(copied) && copied.length === 1 && typeof copied[0] === "number" ? copied[0] : undefined;
      if (copiedId === undefined) throw new ClientError("server", "Duplicate did not return the new record.");
      this.notice("Record duplicated.");
      await this.navigate({ ...route, id: String(copiedId), view_type: "form" });
    }), "dropdown-item form-duplicate-btn");
    duplicateBtn.hidden = !state.id || !canCreate;
    actionMenu.append(duplicateBtn);

    archiveBtn.onclick = () => runFormAction(async () => {
      actionMenu.classList.remove("open");
      refuseUncertain();
      const archiving = state.values["active"] !== false;
      if (archiving && await this.choose("Archive record?", "An archived record is hidden from lists and searches. Unarchive it from the Actions menu.", ["Archive", "Cancel"]) !== "Archive") return;
      if (state.dirty && !await trySave()) return;
      this.busy = true;
      try {
        await this.rpc.call(state.model, archiving ? "action_archive" : "action_unarchive", { ids: [state.id], context: context }, { write: true });
        await state.load(state.id);
      } finally {
        this.busy = false;
      }
      render(); updateStatus();
      this.notice(archiving ? "Record archived." : "Record unarchived.");
    });
    actionMenu.append(archiveBtn);

    const canDelete = view.canDelete !== false && !isFalsy(context["delete"]);
    if (state.id && canDelete) {
      const deleteBtn = actionButton("Delete", () => runFormAction(async () => {
        actionMenu.classList.remove("open");
        refuseUncertain();
        if (await this.choose("Delete record?", "This action cannot be undone.", ["Delete", "Cancel"]) !== "Delete") return;
        this.busy = true;
        try {
          await this.rpc.call(state.model, "unlink", { ids: [state.id], context: context }, { write: true });
          state.discard();
        } finally {
          this.busy = false;
        }
        await this.navigate({ ...route, id: undefined, view_type: "list" });
      }), "dropdown-item form-delete-btn danger");
      actionMenu.append(deleteBtn);
    }

    // The model's own bound actions would follow here, after a divider; this client loads none for the form route yet.
    // Reload (it also checks the outcome of an interrupted save) closes the menu.
    const menuDivider = el("div", undefined, "dropdown-divider"); menuDivider.setAttribute("role", "separator");
    actionMenu.append(menuDivider, reloadBtn);
    actionDropdown.append(actionToggle, actionMenu);
    // A menu may only own menu items and separators (the dropdown component sets role="menu" on the panel).
    for (const item of actionMenu.querySelectorAll<HTMLElement>("button")) item.setAttribute("role", "menuitem");
    const actionMenuDropdown = attachMenuDropdown({ trigger: actionToggle, menu: actionMenu, mode: "class-open", onOpen: () => { duplicateBtn.hidden = !state.id || !canCreate; } });
    recordMenu = actionDropdown; updateBreadcrumbs();
    if (smartHost) panel.append(smartHost);

    const stopActionMenu = (): void => actionMenuDropdown.dispose();

    form.addEventListener("submit", event => {
      event.preventDefault();
      runFormAction(save);
    });

    surface = new RecordForm({ numericLocale: this.numericLocale, signal: this.lifetime.signal, form, state, view, headerHost, smartHost, asideHost, chatterHost, rpc: this.rpc, context: context,
      canEdit,
      messagingStore: this.getMessagingStore(), uid: this.userId, userName: this.userName,
      run: this.run, report: error => this.reportError(error), messages: this.messages, editorLocale: this.editorLocale,
      notify: message => this.messages.notifications.add(message, { level: "warning" }),
      openRecord: (model, id) => this.openRelatedRecord(model, id), button: renderButton,
      changeValues: (changes, focus) => this.run(async () => {
        if (this.uncertainMethod || this.expired) throw new ClientError("ambiguous", uncertainMethodMessage);
        const changeGeneration = this.generation;
        let changeOk = false;
        const work = (async () => {
          if (surface!.needsCommit()) await surface!.commit();
          if (this.lifetime.signal.aborted || changeGeneration !== this.generation) return;
          state.changeMany(changes); updateStatus(); this.busy = true; surface!.setDisabled(true);
          for (const input of surface!.controls()) if (!(input instanceof HTMLButtonElement)) input.disabled = true;
          try {
            await state.onchange(Object.keys(changes));
            this.messages.warning(state.warningDetails);
            changeOk = true;
          }
          finally { this.busy = false; surface!.setDisabled(false); if (!this.expired) render(focus); }
        })();
        scalarChange = work;
        return work.finally(() => {
          if (scalarChange === work) scalarChange = undefined;
          if (changeOk && queuedAction && changeGeneration === this.generation && !this.lifetime.signal.aborted && !this.expired) {
            const next = queuedAction;
            queuedAction = undefined;
            this.run(next);
          } else {
            queuedAction = undefined;
          }
        });
      }),
      stage: (name, value) => {
        if (this.uncertainMethod || this.expired) throw new ClientError("ambiguous", uncertainMethodMessage);
        state.change(name, value); updateStatus();
      },
      commit: async (name, focus) => {
        if (this.uncertainMethod || this.expired) throw new ClientError("ambiguous", uncertainMethodMessage);
        try { await state.onchange(name); this.messages.warning(state.warningDetails); }
        finally { if (!this.expired) render(focus); }
      },
      change: (name, value, focus) => this.run(() => {
        const changeGeneration = this.generation;
        let changeOk = false;
        const work = (async () => {
          if (this.uncertainMethod) throw new ClientError("ambiguous", uncertainMethodMessage);
          if (surface!.needsCommit()) await surface!.commit();
          if (this.lifetime.signal.aborted || changeGeneration !== this.generation) return;
          state.change(name, value); updateStatus(); this.busy = true; surface!.setDisabled(true);
          for (const input of surface!.controls()) if (!(input instanceof HTMLButtonElement)) input.disabled = true;
          try {
            await state.onchange(name);
            this.messages.warning(state.warningDetails);
            changeOk = true;
          }
          finally {
            this.busy = false;
            if (!this.lifetime.signal.aborted) { surface!.setDisabled(false); render(focus); }
          }
        })();
        scalarChange = work;
        return work.finally(() => {
          if (scalarChange === work) scalarChange = undefined;
          if (changeOk && queuedAction && changeGeneration === this.generation && !this.lifetime.signal.aborted && !this.expired) {
            const next = queuedAction;
            queuedAction = undefined;
            this.run(next);
          } else {
            queuedAction = undefined;
          }
        });
      })
    });
    try {
      render();
    formHotkeys = createHotkeyController(document.body, {
      blocked: () => this.keyboardPending !== undefined || this.busy || this.actionPending || this.uncertainMethod || state.writeUncertain || this.messages.block.blocked,
      report: error => this.hotkeyError(error),
      bindings: (): HotkeyBinding[] => (canEdit || !state.id) && (state.dirty || !state.id || surface!.controls().some(control => control.closest(".field-dirty,.field-invalid"))) ? [
        { kind: "callback", chord: "alt+s", target: state.dirty || !state.id ? saveBtn : form, allowEditable: true, hint: "s", run: async () => {
          const binding = this.generation;
          try {
            const active = document.activeElement;
            if ((active instanceof HTMLInputElement || active instanceof HTMLTextAreaElement || active instanceof HTMLSelectElement) && surface!.controls().includes(active)) {
              const before = scalarChange;
              active.blur(); // Native change runs the existing field/onchange pipeline before Save.
              // Programmatic DOM fixtures (and controls that do not emit change on blur) need the same explicit commit event.
              if (scalarChange === before && active.closest(".field-dirty,.field-invalid")) active.dispatchEvent(new Event("change", { bubbles: true }));
            }
            // Blur/change preparation is synchronous; own every subsequent await across all UI entry points.
            this.keyboardPending = "save";
            if (scalarChange) {
              try { await scalarChange; }
              catch { return; } // The existing change owner reports this failure; never save after a failed change.
            }
            if (!this.lifetime.signal.aborted && binding === this.generation) await save();
          } finally { this.keyboardPending = undefined; }
        } },
        { kind: "callback", chord: "alt+j", target: state.dirty || !state.id ? discardBtn : form, allowEditable: true, hint: "j", run: async () => { this.keyboardPending = "discard"; try { await discard(); } finally { this.keyboardPending = undefined; } } }
      ] : []
    });

    }
    catch (error) { dispose(); stopActionMenu(); throw error; }
    return () => { dispose(); stopActionMenu(); };
  }
}
