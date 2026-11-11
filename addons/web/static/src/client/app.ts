/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
import { PanelCompatibilityError, PanelRecoverySafety, attemptPanelRecovery } from "./panel-recovery.js";
import { unavailablePanelMessage } from "./panel-metadata.js";
import { createHotkeyController, type HotkeyBinding } from "@erpforall/elang";
import { reportHotkeyError } from "../../../../../web/src/runtime/hotkeys.js";
import { SearchModel, createSearchBar } from "./search/index.js";
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
import { createContext, decodeAst, evalAst, parseBoundedJson } from "../../../../../web/packages/expr/src/index.js";
import { expressionValue, jsonValue } from "./field-relations.js";
import { array, describeViews, integer, noExpression, object, text, type ButtonNode, type Description, type Field, type Node, type View } from "./metadata.js";
import { actionButton, button, choose, confirmChoice, el, errorMessage } from "./dom.js";
import { buttonHidden, lockButtonControls, runObjectButton, buttonExpression, type ButtonEnvironment } from "./buttons.js";
import { viewButton, ButtonDebouncer, ButtonHotkeys } from "./view-button.js";
import { runUrlButton } from "./button-url.js";
import { CurrencyDisplayCache } from "./field-currency.js";
import { fieldWidgets, resolveFieldWidget } from "./registry.js";
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
import { IDENTITY_METHOD, IDENTITY_MODEL, type Resubmission } from "./password-flow.js";
import { WizardDialog } from "./wizard-dialog.js";
import type { ActionResult } from "../generated/human-ui.js";
import { RecordForm, formFocus } from "./record-form.js";
import { MessagingStore } from "@erpforall/mail-store";
import { ClientError, RpcClient, type CallOptions } from "./transport.js";
import { attachSubviews } from "./x2many.js";
import { loadDatabaseTheme } from "./theme.js";
import { MetadataCache, invalidatesMetadata } from "./metadata-cache.js";
import { ExpiredMethodAction, MethodActions, type MethodAction } from "./method-actions.js";

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

/** Labels remain available to assistive technology and tooltips. */
function decorateViewButton(target: HTMLButtonElement, kind: "list" | "form"): void {
  const label = el("span", kind === "list" ? "List" : "Form", "view-button-label");
  target.replaceChildren(icon(kind === "list" ? "list" : "panels-top-left"), label);
  target.title = `${label.textContent} view`;
}

interface Menu {
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
  private headerMenuSearchReady: (() => void) | undefined;
  private current: Route = {};
  private currentUrl = window.location.pathname + window.location.search;
  private generation = 0;
  /** Settles when the navigation now running ends, whether it committed, was superseded or failed. */
  private navigationInFlight: Promise<void> | undefined;
  private keyboardPending: "save" | "discard" | undefined;
  private historyIndex = 0;
  private restoringHistory: number | undefined;
  private traversalPending: { index: number; url: URL } | undefined;
  private traversalRunning = false;
  private record: RecordState | undefined;
  private service: ServiceController | undefined;
  private busy = false;
  private wizard: WizardDialog | undefined;
  private actionPending = false;
  private uncertainMethod = false;
  private expired = false;
  private readonly debug = new URL(location.href).searchParams.get("debug") === "1";
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
      try {
        targetIndex = object(event.state, "navigation state")["erpIndex"];
      } catch {
        this.notice("This history entry belongs to an older application session. Reload to continue.");
        return;
      }
      if (typeof targetIndex !== "number" || !Number.isSafeInteger(targetIndex)) {
        this.notice("Invalid application history entry.");
        return;
      }
      if (this.restoringHistory === targetIndex) {
        this.restoringHistory = undefined;
        return;
      }
      this.restoringHistory = undefined;
      this.traversalPending = { index: targetIndex, url: new URL(window.location.href) };
      if (this.traversalRunning) return;
      this.traversalRunning = true;
      void (async () => {
        const restore = (): void => {
          const latest = this.traversalPending;
          this.traversalPending = undefined;
          if (latest && latest.index !== this.historyIndex) {
            this.restoringHistory = this.historyIndex;
            history.go(this.historyIndex - latest.index);
          }
        };
        try {
          if (!await this.canLeave()) {
            restore();
            return;
          }
          while (this.traversalPending && !this.expired) {
            const target = this.traversalPending;
            this.traversalPending = undefined;
            try {
              await this.navigate(parseRoute(target.url), true, true);
              this.historyIndex = target.index;
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
      const active = this.homeMenuOpen || this.mobileDrawer?.isOpen || this.userDropdownOpen || this.navigation.querySelector(".nav-dropdown-menu.open");
      return active ? [{ kind: "callback", chord: "escape", target: this.host, allowEditable: true, run: () => this.closeKeyboardOverlay() }] : [];
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
    const menu = [...this.navigation.querySelectorAll<HTMLElement>(".nav-dropdown-menu.open")].at(-1);
    if (menu) { menu.classList.remove("open"); menu.parentElement?.querySelector<HTMLElement>(".nav-dropdown-toggle")?.focus(); }
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
    // element itself moves, so renderNavigation keeps owning its content. The office theme keeps its own search row.
    // header.replaceChildren() above detached it either way; the office theme puts it back below the header.
    if (!office) this.header.append(this.navigation); else this.header.after(this.navigation);

    const systray = el("div", undefined, "systray");
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
    if (office) {
      const identity = el("span", undefined, "user-identity");
      // Company membership is validated before renderHeader runs.
      const company = this.companies.find(company => company.id === this.currentCompanyId)!;
      identity.append(userNameSpan, el("span", company.name, "user-company"));
      userButton.append(avatar, identity, icon("chevron-down"));
    } else userButton.append(avatar, userNameSpan, icon("chevron-down"));

    const userDropdown = el("div", undefined, "user-dropdown");
    this.userDropdown = userDropdown;
    const userInfo = el("div", undefined, "user-dropdown-info");
    userInfo.append(el("strong", this.userName), el("span", "Internal User", "muted"));

    const companySection = el("div", undefined, "user-dropdown-company");
    const companyLabel = el("label", this.companies.length > 1 ? "Companies" : "Company");
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
      const singleName = el("div", this.companies[0]?.name ?? "", "company-single-name");
      companySection.append(companyLabel, singleName, companySelect);
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

        if (this.companies.length > 3) {
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
          treeContainer.replaceChildren();
          actionsContainer.replaceChildren();

          const visible = companySelector.computeVisibleCompanies(searchFilter);
          for (const entry of visible) {
            const row = el("div", undefined, "company-row");
            row.setAttribute("data-company-id", String(entry.company.id));
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

            row.append(checkbox, switchBtn);
            treeContainer.append(row);
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
      companySection.append(companyLabel, switcher, companySelect);

      userDropdown.addEventListener("keydown", (e: KeyboardEvent) => {
        if ((e.ctrlKey || e.metaKey) && e.key === "Enter" && companySelector.hasSelectionChanged) {
          e.preventDefault();
          const confirmBtn = switcher.querySelector<HTMLButtonElement>(".company-btn-confirm");
          confirmBtn?.click();
        }
      });
    }

    const logoutForm = el("form");
    logoutForm.method = "post";
    logoutForm.action = "/erp/session/logout";
    const token = el("input");
    token.type = "hidden";
    token.name = "csrf_token";
    token.value = text(object(bootstrap["csrf"])["logout"], "logout token");
    const logoutBtn = el("button", "Sign out");
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
    const preferencesBtn = el("button", "My Preferences", "user-dropdown-preferences");
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

    userDropdown.append(userInfo, companySection, preferencesBtn, logoutForm);
    userMenuContainer.append(userButton, userDropdown);
    systray.append(userMenuContainer);
    this.header.append(systray);

    const onDocClick = (e: MouseEvent): void => {
      const target = e.target instanceof Element ? e.target : null;
      if (this.userDropdownOpen && target && !userMenuContainer.contains(target)) {
        this.userDropdownOpen = false;
        userDropdown.classList.remove("open");
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
    this.disposeWidgets = []; this.disposeListeners = []; this.disposeHeader = [];
    this.record = undefined; this.service = undefined; this.pendingInvocation = undefined; this.methodActions.clear();
    release(() => this.messages.dispose());
    if (errors.length) throw new AggregateError(errors, "Application scope cleanup failed.");
  }

  private expire(): void {
    this.expired = true;
    try { this.dispose(); }
    finally {
      const main = el("main");
      main.append(el("h1", "Session expired"), el("p", "Your session has expired. Sign in again."));
      const link = el("a", "Sign in"); link.href = "/erp/login"; main.append(link);
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

  private async canLeave(): Promise<boolean> {
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
      this.menus.set(id, { id, label: text(node["label"], "menu label"), action, children, parentId, icon });
    }
    for (const id of array(response["roots"], "menu roots")) {
      this.rootMenuIds.push(integer(id, "root menu"));
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
      this.content.querySelector<HTMLInputElement>(".home-menu-search")?.focus();
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

  private buildHomeContent(): HTMLElement {
    const homeContent = el("div", undefined, "home-menu-container");
    const searchInput = el("input", undefined, "home-menu-search");
    searchInput.type = "search";
    searchInput.placeholder = "Search apps…";
    searchInput.setAttribute("aria-label", "Search apps");

    const grid = el("div", undefined, "home-grid");
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

      const iconBox = el("div", undefined, "app-icon");
      iconBox.setAttribute("aria-hidden", "true");
      if (rootMenu.icon && !rootMenu.icon.startsWith("lucide:") && (rootMenu.icon.includes("/") || rootMenu.icon.startsWith("data:"))) {
        const img = el("img");
        img.src = rootMenu.icon.includes(",") ? "/" + rootMenu.icon.split(",")[0] + "/" + rootMenu.icon.split(",")[1] : rootMenu.icon;
        img.alt = "";
        iconBox.append(img);
      } else {
        iconBox.append(menuIcon(rootMenu.icon));
        const hue = (rootMenu.id * 137.5) % 360;
        iconBox.style.backgroundColor = `hsl(${hue}, 45%, 45%)`;
      }
      const caption = el("div", rootMenu.label, "app-caption");
      tile.append(iconBox, caption);
      grid.append(tile);
    }

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

  private renderNavigation(route: Route): void {
    const activeRoot = this.resolveActiveRoot(route);
    const rootChanged = this.currentRootId !== activeRoot?.id;

    if (rootChanged || !this.navigation.hasChildNodes()) {
      this.currentRootId = activeRoot?.id;
      this.mobileDrawer?.dispose();
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
              sections.append(entry);
            }
          } else if (this.hasLeafAction(child.id)) {
            const dropdown = el("div", undefined, "nav-dropdown");
            const menuList = el("div", undefined, "nav-dropdown-menu");
            const toggle = button(child.label + " ▾", () => {
              const willOpen = !menuList.classList.contains("open");
              for (const other of this.navigation.querySelectorAll<HTMLElement>(".nav-dropdown-menu.open")) {
                if (other !== menuList) other.classList.remove("open");
              }
              menuList.classList.toggle("open", willOpen);
            }, "nav-dropdown-toggle");
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
      this.mobileDrawer = new MobileDrawer(mobileDrawer, backdrop, burgerBtn, () => {
        if (document.documentElement.dataset["officeTheme"] === "office-blue" && window.innerWidth > 860) {
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

  private async navigate(route: Route, initial = false, fromHistory = false, invocation?: { caller: RecordState; generation: number }): Promise<void> {
    if(this.debug)route={...route,debug:"1"};
    if (invocation) {
      // A completed object call already saved/validated its record. Do not weaken canLeave's
      // global pending guard: only the exact active invocation can consume this handoff once.
      if (invocation !== this.pendingInvocation || invocation.generation !== this.generation || invocation.caller !== this.record ||
          invocation.caller.dirty || invocation.caller.busy || this.busy || this.wizard || !this.actionPending || this.uncertainMethod || this.expired) {
        throw new ClientError("validation", "The action's source is no longer available for navigation.");
      }
      this.pendingInvocation = undefined;
    } else if (!initial && !await this.canLeave() || this.expired) return;
    const previousGeneration = this.generation, previousList = this.listContext;
    let committed = false;
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
            const val = jsonValue(evalAst(ast, createContext({ names: { context: expressionValue(context, 0) } })));
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
            options: widget.options
          });
        } else if (view.type === "list") {
          const actionHelp = typeof (action as Record<string, unknown>)["help"] === "string" ? (action as Record<string, unknown>)["help"] as string : undefined;
          const hasForm = !entry || views.some(view => view[1] === "form");
          const canCreate = hasForm && view.canCreate !== false && !isFalsy(context["create"]);
          if (canCreate) controls.append(actionButton("New", () => this.run(() => this.navigate({ ...route, id: undefined, view_type: "form" })), "primary"));
          const list = await this.listView(staging, panel, actionTitle, description, view, route, generation, actionHelp, views as [number | null, string][], {baseDomain: entry?.result.domain ?? actionDomain, context, hasForm, canCreate});
          nextDispose = list.dispose;
          nextSettle = list.settle;
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
        nextDispose?.();
        panelDispose?.();
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
      this.content.focus();
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

  private async listView(staging: HTMLElement, panel: HTMLElement, actionTitle: string, description: Description, view: View, route: Route, generation: number, actionHelp?: string, availableViews?: [number | null, string][], scope?: {baseDomain: unknown[]; context: Record<string, unknown>; hasForm: boolean; canCreate: boolean}): Promise<{ dispose: () => void; settle: () => void }> {
    const context = scope?.context ?? this.context, baseDomain = scope?.baseDomain ?? [], hasForm = scope?.hasForm ?? true, canCreate = scope?.canCreate ?? hasForm;
    const fieldStops: (() => void)[] = [];
    const currencies = new CurrencyDisplayCache();
    let offset = 0, order = "id asc", request = 0, pageSize = view.pageLimit ?? 40;
    const officeBlue = document.documentElement.dataset["officeTheme"] === "office-blue";
    panel.classList.add("control-panel-list");
    const selectedIds = new Set<number>();
    const columnWidths = new Map<string, number>();
    let settleColumns: (() => void) | undefined;

    const breadcrumbs = panel.querySelector("[data-control-breadcrumbs]") || panel.querySelector("h1");
    if (breadcrumbs) {
      const title = el("h1", actionTitle, "breadcrumb-current");
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
      } else if (officeBlue) {
        const menu = route.menu ? this.menus.get(Number(route.menu)) : [...this.menus.values()].find(item => String(item.action) === route.action);
        const labels: string[] = [];
        let ancestor = menu;
        while (ancestor) {
          labels.unshift(ancestor.label);
          ancestor = ancestor.parentId === null ? undefined : this.menus.get(ancestor.parentId);
        }
        if (labels.length) breadcrumbs.prepend(el("span", labels.join(" › "), "list-menu-path"));
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
      initialDomain: baseDomain
    });

    // Restore search state from route and URL search params
    const urlParams = new URL(routeUrl(route), window.location.origin).searchParams;
    searchModel.loadFromUrlParams(urlParams);
    if (route.search && !urlParams.has("search_facets")) {
      const field = searchModel.getSearchableFields().find(f => f.name === "name") ?? searchModel.getSearchableFields()[0];
      if (!field) throw new ClientError("unsupported", "Text search for this model requires a later release.");
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
      if (newUrl !== this.currentUrl) {
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
        updateUrlWithSearch();
        await load();
      });
      return true;
    } }) : undefined;
    if (tabs) categoriesArea.append(tabs.element);

    const searchBar = createSearchBar(searchModel, () => this.run(async () => {
      offset = 0;
      updateUrlWithSearch();
      await load();
    }));

    const searchArea = panel.querySelector("[data-control-search]") || panel;
    if (searchArea !== panel) searchArea.replaceChildren(searchBar.element);
    else panel.append(searchBar.element);

    let disposeQuickFilters = (): void => {};
    if (officeBlue) {
      const menuToggle = searchBar.form.querySelector(".search-menu-toggle");
      if (menuToggle) {
        menuToggle.prepend(el("span", "More filters", "list-filter-label"));
        // Keep the real form/menu lifetime intact while giving each toolbar control its own surface.
        const searchInput = el("div", undefined, "list-search-input");
        for (const node of [searchBar.fieldSelect, searchBar.form.querySelector(".erp-search-input-container"), searchBar.form.querySelector(".search-clear-btn")]) {
          if (node) searchInput.append(node);
        }
        searchBar.form.prepend(searchInput);
      }
      // Only the real metadata-backed active/archive filters are exposed as a quick selector.
      const filters = searchModel.getFilters();
      if (filters.some(filter => filter.id === "filter_active") && filters.some(filter => filter.id === "filter_archived")) {
        const activeSelect = el("select", undefined, "list-active-filter");
        activeSelect.setAttribute("aria-label", "Active record filter");
        activeSelect.append(new Option("Active filter", "all"), new Option("Active", "active"), new Option("Archived", "archived"), new Option("Active and archived", "both"));
        const syncActive = (): void => {
          const current = searchModel.getFilters();
          const active = current.some(filter => filter.id === "filter_active" && filter.isActive);
          const archived = current.some(filter => filter.id === "filter_archived" && filter.isActive);
          activeSelect.value = active && archived ? "both" : active ? "active" : archived ? "archived" : "all";
        };
        activeSelect.addEventListener("change", () => this.run(async () => {
          const chosen = activeSelect.value;
          if (!["all", "active", "archived", "both"].includes(chosen)) throw new ClientError("validation", "Unknown active record filter.");
          for (const filter of searchModel.getFilters()) {
            if (filter.id !== "filter_active" && filter.id !== "filter_archived") continue;
            const wanted = chosen === "both" || chosen === (filter.id === "filter_active" ? "active" : "archived");
            if (filter.isActive !== wanted) searchModel.toggleFilter(filter.id);
          }
          offset = 0;
          updateUrlWithSearch();
          await load();
        }));
        const activeControl = el("div", undefined, "list-select-control");
        activeControl.append(activeSelect, icon("chevron-down"));
        searchBar.form.insertBefore(activeControl, menuToggle);
        syncActive();
        disposeQuickFilters = searchModel.subscribe(syncActive);
      }
    }

    const controls = panel.querySelector("[data-control-actions]") || staging;

    const selectionBox = el("div", undefined, "selection-box");
    selectionBox.style.display = "none";
    const selectionCount = el("span", "0 selected", "selection-count");

    const canDelete = view.canDelete !== false && !isFalsy(context["delete"]);
    let actionDropdown: HTMLElement | undefined;
    let actionMenu: HTMLElement | undefined;
    if (canDelete) {
      actionDropdown = el("div", undefined, "nav-dropdown action-dropdown");
      actionMenu = el("div", undefined, "nav-dropdown-menu action-menu");
      const actionToggle = actionButton("Actions", () => {
        actionMenu?.classList.toggle("open");
      }, "action-toggle-btn");
      const deleteBtn = actionButton("Delete", () => this.run(async () => {
        actionMenu?.classList.remove("open");
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
      actionMenu.append(deleteBtn);
      actionDropdown.append(actionToggle, actionMenu);
      selectionBox.append(selectionCount, actionDropdown);
    } else {
      selectionBox.append(selectionCount);
    }
    controls.append(selectionBox);

    const onDocClick = (e: MouseEvent): void => {
      if (actionDropdown && actionMenu && e.target instanceof Element && !actionDropdown.contains(e.target)) {
        actionMenu.classList.remove("open");
      }
    };
    document.addEventListener("click", onDocClick);

    const pager = el("div", undefined, "pager");
    const count = el("span", "", "pager-count muted");
    const previous = actionButton("Previous", () => this.run(async () => {
      offset = Math.max(0, offset - pageSize);
      await load();
    }), "pager-btn pager-prev");
    const next = actionButton("Next", () => this.run(async () => {
      offset += pageSize;
      await load();
    }), "pager-btn pager-next");
    previous.replaceChildren(icon("chevron-left")); previous.setAttribute("aria-label", "Previous page"); previous.title = "Previous page";
    next.replaceChildren(icon("chevron-right")); next.setAttribute("aria-label", "Next page"); next.title = "Next page";
    const pageNumbers = el("div", undefined, "pager-pages");
    pageNumbers.setAttribute("role", "group");
    pageNumbers.setAttribute("aria-label", "Pages");
    pager.append(count, previous, next);
    if (officeBlue) pager.insertBefore(pageNumbers, next);

    // View Switcher (List / Form buttons)
    const viewSwitcher = el("div", undefined, "view-switcher");
    if (availableViews && availableViews.length > 1) {
      for (const [, vType] of availableViews) {
        if (vType === "list" || vType === "form") {
          const vBtn = button(vType.charAt(0).toUpperCase() + vType.slice(1), () => this.run(async () => {
            await this.navigate({ ...route, id: vType === "form" ? route.id : undefined, view_type: vType as "list" | "form" });
          }), `view-btn ${vType === "list" ? "active" : ""}`);
          vBtn.setAttribute("aria-label", `${vType} view`);
          decorateViewButton(vBtn, vType);
          viewSwitcher.append(vBtn);
        }
      }
    }

    const toolsArea = panel.querySelector("[data-control-tools]");
    if (toolsArea) {
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
      const footer = el("nav", undefined, "list-footer");
      footer.setAttribute("aria-label", "Record pagination");
      const pageSizeSelect = el("select", undefined, "list-page-size");
      pageSizeSelect.setAttribute("aria-label", "Rows per page");
      const allowedPageSizes = [...new Set([20, 40, 50, 80, pageSize])].sort((a, b) => a - b);
      for (const size of allowedPageSizes) pageSizeSelect.append(new Option(`${size} rows / page`, String(size)));
      pageSizeSelect.value = String(pageSize);
      pageSizeSelect.addEventListener("change", () => this.run(async () => {
        const size = Number(pageSizeSelect.value);
        if (!allowedPageSizes.includes(size)) throw new ClientError("validation", "Unsupported list page size.");
        pageSize = size;
        offset = 0;
        await load();
      }));
      const pageSizeControl = el("div", undefined, "list-select-control");
      pageSizeControl.append(pageSizeSelect, icon("chevron-down"));
      pager.append(pageSizeControl);
      footer.append(pager);
      staging.append(footer);
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
      tabs?.element.setAttribute("aria-busy", "true");
      let loaded: [Record<string, unknown>, unknown];
      try {
        loaded = await Promise.all([fetchRows(offset, base), tabs && (category!.enableCounters || !category!.expand) ? this.rpc.call(description.model, "formatted_read_group", {
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
        try { response = await fetchRows(0, base); }
        catch (error) { if (mine !== request || generation !== this.generation || this.expired) return; tabs!.invalidate(); markListUnavailable(); throw error; }
        if (mine !== request || generation !== this.generation || this.expired) return;
      }
      // A new page, search, sort or reload shows other rows; a selection must never outlive what is on screen.
      selectedIds.clear();
      const length = integer(response["length"], "record count"), rows = array(response["records"], "records");
      count.textContent = rows.length ? `${offset + 1}–${offset + rows.length} / ${length}` : "0 records";
      previous.disabled = offset === 0;
      next.disabled = offset + rows.length >= length;
      if (officeBlue) {
        pageNumbers.replaceChildren();
        const totalPages = Math.ceil(length / pageSize), currentPage = Math.floor(offset / pageSize) + 1;
        const pages = new Set([1, totalPages, currentPage - 1, currentPage, currentPage + 1].filter(value => value >= 1 && value <= totalPages));
        let lastPage = 0;
        for (const number of [...pages].sort((a, b) => a - b)) {
          if (lastPage && number - lastPage > 1) pageNumbers.append(el("span", "…", "pager-ellipsis"));
          const pageButton = button(String(number), () => this.run(async () => {
            offset = (number - 1) * pageSize;
            await load();
            if (panel.isConnected) pageNumbers.querySelector<HTMLButtonElement>('[aria-current="page"]')?.focus();
          }), "pager-page");
          pageButton.setAttribute("aria-label", `Page ${number}`);
          if (number === currentPage) pageButton.setAttribute("aria-current", "page");
          pageNumbers.append(pageButton);
          lastPage = number;
        }
      }

      this.listContext = {
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
        if (field.kind === "integer" || field.kind === "float") th.classList.add("list-number");
        else if (field.kind === "boolean") th.classList.add("list-boolean");
        th.scope = "col";
        const isSorted = order.startsWith(field.name + " ");
        const isAsc = isSorted && order.endsWith("asc");
        th.setAttribute("aria-sort", isSorted ? (isAsc ? "ascending" : "descending") : "none");
        const indicator = isSorted ? (isAsc ? " ↑" : " ↓") : "";
        const sortBtn = button(field.label + indicator, () => this.run(async () => {
          order = `${field.name} ${isSorted && isAsc ? "desc" : "asc"}`;
          offset = 0;
          await load();
        }), "sort-button");
        th.append(sortBtn);
        header.append(th);
      }
      head.append(header);
      table.append(head, body);
      settleColumns = resizeColumns(table, view.fields.filter(field => !field.invisible), columnWidths);

      for (const stop of fieldStops.splice(0)) stop();
      for (const raw of rows) {
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
          fieldStops.push(handle.dispose);
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
              const arrow = el("span", "↗"); arrow.setAttribute("aria-hidden", "true"); open.append(arrow); cell.append(handle.node, open);
            }
          } else cell.append(handle.node);
          row.append(cell);
        });

        if (hasForm) row.addEventListener("click", event => {
          if ((event.target as HTMLElement).closest("input, button, a")) return;
          this.run(() => this.navigate({ ...route, id: String(id), view_type: "form" }));
        });

        body.append(row);
      }
      results.replaceChildren(table);
      // Size the columns now, before the browser paints the content layout (columns.ts).
      settleColumns();
      if (tabCounts) tabs!.publish(tabCounts);
      else tabs?.finish();
      updateSelectionUI();

      if (!rows.length) {
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
        results.replaceChildren(empty);
      }
    };

    try { await load(); } catch (error) { document.removeEventListener("click", onDocClick); disposeQuickFilters(); searchBar.dispose(); throw error; }
    return {
      dispose: () => { for (const stop of fieldStops.splice(0)) stop(); document.removeEventListener("click", onDocClick); disposeQuickFilters(); searchBar.dispose(); },
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
    throw new ClientError("unsupported", `No accessible form action is available for ${model}; broader related-record routing is tracked by a later release.`);
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
      await this.navigate({ ...route, id: String(target), view_type: "form" });
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
  private async openWindowDialog(result: Extract<ActionResult, { kind: "window" }>, caller: RecordState | undefined, binding: Binding, refresh: () => Promise<void>, source: WizardDialog | undefined, callerContext: Record<string, unknown>, onResult?: (result: ActionResult, button: string | undefined) => void, confirmed?: Resubmission): Promise<void> {
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
    scope = new WizardDialog({ numericLocale: this.numericLocale, name: result.name, state, view, rpc: this.rpc, context,debug:this.debug,uid:this.userId,
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
      if (button === "preference_save" && (result.kind === "close" || result.kind === "none")) this.reloadPage();
    });
  }

  /** Load a declared action under the current caller, then reuse the existing result admission. */
  private async declaredButtonAction(id:number,state:RecordState,fields:readonly Field[],context:Record<string,unknown>,binding:Binding,refresh:()=>Promise<void>,source?:WizardDialog):Promise<void> {
    const activeContext={...context,active_model:state.model,active_id:state.id,active_ids:state.id===undefined?[]:[state.id]};
    const action=object(await this.rpc.call("ir.actions.act_window","erp_load_web_action",{ids:[id],context:activeContext}),"declared action");
    if(action["wire_major"]!==1 || action["id"]!==id)throw new ClientError("server","Mismatched declared button action.");
    const target=action["target"];
    if(target!=="current" && target!=="new")throw new ClientError("unsupported","Additional button action targets require a later release.");
    const environment:ButtonEnvironment={model:state.model,context:activeContext,uid:this.userId,fields,values:()=>state.values,id:()=>state.id,dirty:()=>state.dirty,save:async()=>{},call:async()=>{throw new ClientError("server","Action expression cannot invoke an RPC.");},reload:async()=>{},confirm:async()=>false};
    const domain=action["domain"]===null?[]:buttonExpression(action["domain"],environment);
    const defaults=action["context"]===null?{}:buttonExpression(action["context"],environment);
    if(target==="new" && (!Array.isArray(domain) || domain.length))throw new ClientError("unsupported","Dialog action domains require a later release.");
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
    const debounce=new ButtonDebouncer();
    const hotkeys=new ButtonHotkeys(()=>surface?.controls().filter((control):control is HTMLButtonElement=>control instanceof HTMLButtonElement)??[],{blocked:()=>this.keyboardPending!==undefined||this.busy||this.actionPending||this.uncertainMethod||this.messages.block.blocked,report:error=>this.hotkeyError(error)});
    const dispose = (): void => { headingObserver?.disconnect(); formHotkeys?.dispose(); hotkeys.dispose();debounce.dispose();surface?.dispose(); };

    panel.classList.add("control-panel-form");
    const toolsArea = panel.querySelector("[data-control-tools]");
    if (toolsArea && availableViews && availableViews.length > 1) {
      const viewSwitcher = el("div", undefined, "view-switcher");
      for (const [, vType] of availableViews) {
        if (vType === "list" || vType === "form") {
          const vBtn = button(vType.charAt(0).toUpperCase() + vType.slice(1), () => this.run(async () => {
            await this.navigate({ ...route, id: vType === "list" ? undefined : route.id, view_type: vType as "list" | "form" });
          }), `view-btn ${vType === "form" ? "active" : ""}`);
          vBtn.setAttribute("aria-label", `${vType} view`);
          decorateViewButton(vBtn, vType);
          viewSwitcher.append(vBtn);
        }
      }
      toolsArea.replaceChildren(viewSwitcher);
    }

    const officeForm = document.documentElement.dataset["officeTheme"] === "office-blue";
    const headerHost = el("div", undefined, "form-record-header");
    const headerActionsHost = officeForm ? el("div", undefined, "form-record-actions") : undefined;
    const notebookTabsHost = officeForm ? el("div", undefined, "form-record-tabs") : undefined;
    const hasAside = view.nodes.some(node => node.kind === "root" && node.children.some(child => child.kind === "aside"));
    const asideHost = hasAside ? el("aside", undefined, "record-form-aside") : undefined;
    asideHost?.setAttribute("aria-label", "Record details");
    // The chatter slot exists only here, on the main record route; dialogs get no host.
    const hasChatter = view.nodes.some(node => node.kind === "root" && node.children.some(child => child.kind === "chatter"));
    const chatterHost = hasChatter ? el("aside", undefined, "record-form-chatter") : undefined;
    chatterHost?.setAttribute("aria-label", "Chatter");
    const breadcrumbs = panel.querySelector("[data-control-breadcrumbs]") || panel.querySelector("h1");
    const statusBadge = el("span", "Saved", "status-badge saved");
    const statusActions = el("div", undefined, "status-actions");
    statusActions.style.display = "none";
    const saveBtn = actionButton("Save", () => this.run(save), "primary status-btn-save");
    const discardBtn = actionButton("Discard", () => this.run(discard), "status-btn-discard");
    statusActions.append(saveBtn, discardBtn);
    const statusIndicator = el("div", undefined, "form-status-indicator");
    statusIndicator.append(statusBadge, statusActions);
    if (officeForm) {
      const commands = el("div", undefined, "form-record-commands");
      commands.append(statusIndicator, controls, headerActionsHost!);
      panel.append(commands);
    }

    const canEdit = view.canEdit !== false && !isFalsy(context["edit"]);
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
    };

    let syncPageHeading: (() => void) | undefined;
    const updateBreadcrumbs = (): void => {
      if (!breadcrumbs) return;
      breadcrumbs.replaceChildren();
      const backBtn = button(actionTitle, () => this.run(() => this.navigate({ ...route, id: undefined, view_type: undefined })), "breadcrumb-back link");
      backBtn.prepend(icon("arrow-left"));
      const title = titleField ? state.values[titleField.name] : undefined;
      const currentName = state.id ? (typeof title === "string" && title ? title : `#${state.id}`) : "New";
      const currentCrumb = el("span", currentName, "breadcrumb-current");
      {
        // The control panel already owns the Breadcrumbs navigation landmark.
        const trail = el("div", undefined, "form-breadcrumb-path");
        const menu = route.menu ? this.menus.get(Number(route.menu)) : [...this.menus.values()].find(item => String(item.action) === route.action);
        const relatedAction = route.method_action !== undefined;
        const ancestors: typeof menu[] = []; let ancestor = menu;
        while (ancestor) { ancestors.unshift(ancestor); ancestor = ancestor.parentId === null ? undefined : this.menus.get(ancestor.parentId); }
        for (const item of ancestors) {
          if (!item) continue;
          const part = item === menu && !relatedAction ? backBtn : item.action !== null
            ? button(item.label, () => this.run(() => this.navigate({action:String(item.action),menu:String(item.id)})), "link")
            : el("span", item.label);
          if (item === menu && !relatedAction) part.replaceChildren(document.createTextNode(item.label));
          trail.append(part, el("span", "›", "form-breadcrumb-divider"));
        }
        const related = route.method_action ? this.methodActions.get(route.method_action) : undefined;
        if (related) trail.append(this.originCrumb(related), el("span", "›", "form-breadcrumb-divider"));
        if (!menu || relatedAction) trail.append(backBtn, el("span", "›", "form-breadcrumb-divider"));
        const labelField = view.fields.find(field => field.name === "display_name" && !field.invisible)
          ?? view.fields.find(field => field.name === "name" && field.kind === "char" && !field.invisible)
          ?? view.fields.find(field => field.kind === "char" && !field.invisible);
        const displayName = labelField ? state.values[labelField.name] : undefined;
        currentCrumb.textContent = !state.id ? "New" : typeof displayName === "string" && displayName ? displayName : `#${state.id}`;
        currentCrumb.className = "breadcrumb-current form-breadcrumb-record"; currentCrumb.setAttribute("aria-current", "page");
        trail.append(currentCrumb); breadcrumbs.append(trail);
        if (!officeForm) breadcrumbs.append(statusIndicator);
      }
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
    if (officeForm) {
      const context = panel.querySelector<HTMLElement>("[data-control-extension]");
      if (context) { context.classList.add("form-record-context"); sheet.append(context); }
    }
    sheet.append(form);
    // Staging itself is discarded on navigation; keep the form styling anchor in the mounted tree.
    const viewport = el("div", undefined, "record-form-viewport");
    if (notebookTabsHost) viewport.append(notebookTabsHost);
    viewport.append(sheet);
    if (asideHost) viewport.append(asideHost);
    if (chatterHost) viewport.append(chatterHost);
    staging.append(viewport);

    // A declared title keeps its XML placement. A legacy form instead names the page in its breadcrumb.
    syncPageHeading = (): void => {
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
      if (element instanceof HTMLElement && (headerHost?.contains(element) || headerActionsHost?.contains(element))) return element.dataset["focusKey"];
      return formFocus(form, element);
    };
    const render = (focus = surface?.focus()): void => {
      surface!.render(focus); surface!.setDisabled(runningButton || this.uncertainMethod); updateStatus(); syncPageHeading?.();
      // Saving during an action can replace widgets and overflow controls while the action still runs.
      if (runningButton) for (const control of surface!.controls()) if (control instanceof HTMLButtonElement) control.disabled = true;
      if (this.uncertainMethod) {
        surface!.setDisabled(true);
        for (const control of surface!.controls()) control.disabled = true;
        for (const control of controls.querySelectorAll<HTMLButtonElement>("button")) if (!control.matches(".action-toggle-btn,.form-reload-btn")) control.disabled = true;
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
        await state.save();
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
      const control = viewButton(node,{activate:()=>debounce.run(node,node.details?.debounce_ms??0,()=>this.run(()=>clickButton(node))),model:state.model,debug:this.debug,messages:this.messages,report:error=>this.reportError(error),className:`form-header-btn ${node.variant}`});
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
        actionButton("New", () => this.run(() => this.navigate({ ...route, id: undefined, view_type: "form" })))
      );
    }

    const actionDropdown = el("div", undefined, "nav-dropdown form-actions-dropdown");
    const actionToggle = actionButton("Actions", () => {
      actionMenu.classList.toggle("open");
    }, "action-toggle-btn");
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
    actionMenu.append(reloadBtn);

    const canDelete = view.canDelete !== false && !isFalsy(context["delete"]);
    if (state.id && canDelete) {
      const deleteBtn = actionButton("Delete", () => this.run(async () => {
        actionMenu.classList.remove("open");
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

    actionDropdown.append(actionToggle, actionMenu);
    controls.append(actionDropdown);
    if (headerHost) panel.append(headerHost);

    const onDocClick = (e: MouseEvent): void => {
      if (e.target instanceof Element && !actionDropdown.contains(e.target)) {
        actionMenu.classList.remove("open");
      }
    };
    document.addEventListener("click", onDocClick);
    const stopActionMenu = (): void => document.removeEventListener("click", onDocClick);

    form.addEventListener("submit", event => {
      event.preventDefault();
      this.run(save);
    });

    surface = new RecordForm({ numericLocale: this.numericLocale, signal: this.lifetime.signal, form, state, view, headerHost, headerActionsHost, notebookTabsHost, asideHost, chatterHost, rpc: this.rpc, context: context,
      messagingStore: this.getMessagingStore(), uid: this.userId,
      run: this.run, report: error => this.reportError(error), messages: this.messages, editorLocale: this.editorLocale,
      notify: message => this.messages.notifications.add(message, { level: "danger" }),
      openRecord: (model, id) => this.openRelatedRecord(model, id), button: renderButton,
      changeValues: (changes, focus) => this.run(async () => {
        if (this.uncertainMethod || this.expired) throw new ClientError("ambiguous", uncertainMethodMessage);
        if (surface!.needsCommit()) await surface!.commit();
        state.changeMany(changes); updateStatus(); this.busy = true; surface!.setDisabled(true);
        for (const input of surface!.controls()) input.disabled = true;
        try { await state.onchange(Object.keys(changes)); this.messages.warning(state.warningDetails); }
        finally { this.busy = false; surface!.setDisabled(false); if (!this.expired) render(focus); }
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
        const work = (async () => {
          if (this.uncertainMethod) throw new ClientError("ambiguous", uncertainMethodMessage);
          if (surface!.needsCommit()) await surface!.commit();
          if (this.lifetime.signal.aborted || changeGeneration !== this.generation) return;
          state.change(name, value); updateStatus(); this.busy = true; surface!.setDisabled(true);
          for (const input of surface!.controls()) input.disabled = true;
          try { await state.onchange(name); this.messages.warning(state.warningDetails); }
          finally {
            this.busy = false;
            if (!this.lifetime.signal.aborted) { surface!.setDisabled(false); render(focus); }
          }
        })();
        scalarChange = work;
        return work.finally(() => { if (scalarChange === work) scalarChange = undefined; });
      })
    });
    try {
      render();
    formHotkeys = createHotkeyController(document.body, {
      blocked: () => this.keyboardPending !== undefined || this.busy || this.actionPending || this.uncertainMethod || state.writeUncertain || this.messages.block.blocked,
      report: error => this.hotkeyError(error),
      bindings: (): HotkeyBinding[] => state.dirty || !state.id || surface!.controls().some(control => control.closest(".field-dirty,.field-invalid")) ? [
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
