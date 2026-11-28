/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
import { setClientLanguage, t } from "./translate.js";
import { clearProtectedTabState } from "./session-boundary.js";
import { PanelCompatibilityError, PanelRecoverySafety, attemptPanelRecovery } from "./panel-recovery.js";
import { loginReturnUrl } from "./login-return.js";
import { initializeAttachmentPolicy } from "./attachment-policy.js";
import { BIN_SIZE_CONTEXT, forgetBinaryContent } from "./binary-content.js";
import { actionHotkeyModifier, createHotkeyController, type HotkeyBinding } from "@erpforall/elang";
import { reportHotkeyError } from "../../../../../web/src/runtime/hotkeys.js";
import { dropAbsentDashboardFilters, mountDashboard } from "./dashboard.js";
import { admitRequestContext } from "./dashboard-card.js";
import { SearchModel, createSearchBar, readSearchArch, applySearchDefaults, splitSearchDefaults, namesViewFilter, namesFavorite, favoriteStore, SEARCH_DEFAULT_PREFIX, type SearchArch, type FavoriteItem } from "./search/index.js";
import { readMenuModule, readSettingsTarget } from "./settings-items.js";
import { icon, menuIcon } from "./icons.js";
import { MessageServices } from "./messages.js";
import { htmlFieldText } from "./html-field.js";
import { noContentHelp } from "./message-widgets.js";
import { waitForApproval } from "./approval.js";
import { ErrorDialog } from "./error-dialog.js";
import { ClientErrorReporter, type ReportKind } from "./client-report.js";
import { MobileDrawer } from "./mobile-drawer.js";
import { documentScroller, lockDocumentScroll } from "./scroll-lock.js";
import { headerMenuSearch } from "./header-search.js";
import { columnChoices } from "./column-choices.js";
import { resizeColumns } from "./columns.js";
import { createListTabs, decodeSelectionCounts } from "./list-tabs.js";
import { buildGroupByCatalogue, decodeGroupableFields } from "./group-by-catalogue.js";
import { createGroupRow, decodeGroups } from "./list-groups.js";
import { applyContext, archState, decodePivotFields, paramsState, PIVOT_PARAMS, PIVOT_MAX_LEVELS, PivotModel, readPivotView, resolveSpec, snapshotParams, type PivotGroupItem, type PivotLimits, type PivotParams, type PivotSnapshot } from "./pivot.js";
import { saveFile } from "./file-save.js";
import { mountPivot, type PivotCellTarget } from "./pivot-view.js";
import { CALENDAR_PARAMS, CalendarModel, paramsFromState, readCalendarView, stateFromParams, type CalendarParams } from "./calendar.js";
import { mountCalendar } from "./calendar-view.js";
import { GRAPH_MAX_GROUPINGS, GRAPH_PARAMS, GraphModel, graphGrouping, graphParams, graphParamsState, readGraphView, type GraphParams } from "./graph.js";
import { mountGraph, type GraphTarget } from "./graph-view.js";
import { decodeKanbanGroups, kanbanOpeningInfo, renderKanbanBoard, type KanbanBoard, type KanbanColumnState } from "./kanban-columns.js";
import { decodeKanbanFold, encodeKanbanFold, withKanbanColumn, type KanbanColumnKey, type KanbanFoldState } from "./kanban-fold.js";
import { expandScopedColumns, readBoardScope, readCatalogue } from "./kanban-scope.js";
import { selectCreateDialog } from "./select-create-dialog.js";
import { focusKanbanCard, kanbanLayout, kanbanRoot, renderKanbanCards } from "./kanban.js";
import { hierarchyRoot, renderHierarchyTree, type HierarchyState } from "./hierarchy.js";
import { readStoredMap } from "./map-view.js";
import { renderActivityMatrix, type ActivityItemRow, type ActivityTypeRow } from "./activity-view.js";
import { createSearchPanelSidebar, type SearchPanelSidebar } from "./search-panel.js";
import { displayValue } from "./fields.js";
import type { SectionsPanel } from "./panel-metadata.js";
import type { GroupByCatalogue, GroupableField, ListAggregate } from "./search/types.js";
import { createContext, decodeAst, evalAst, parseBoundedJson, type ValidatedAst } from "../../../../../web/packages/expr/src/index.js";
import { expressionValue, jsonValue } from "./field-relations.js";
import { array, describeViews, integer, object, text, type ButtonNode, type Description, type Field, type Node, type View } from "./metadata.js";
import { actionButton, button, choose, confirmChoice, el, errorMessage, relabelActionButton } from "./dom.js";
import { buttonHidden, lockButtonControls, runObjectButton, buttonExpression, type ButtonEnvironment } from "./buttons.js";
import { askButtonPrompt } from "./button-prompt.js";
import { viewButton, ButtonDebouncer, ButtonHotkeys } from "./view-button.js";
import { runUrlButton } from "./button-url.js";
import { CurrencyDisplayCache } from "./field-currency.js";
import { fieldWidgets, resolveFieldWidget, type ActionRequest, type RecordEvent, type ShellNavigation, type ViewLoader } from "./registry.js";
import { ListGroupModel, type ListGroupNode } from "./list-group-model.js";
import { decodeListState, type ListGroupState } from "./list-group-state.js";
import type { NumericLocale } from "./numeric-values.js";
import "./fields.js";
import { decodeEditorLocale as readEditorLocale, type EditorLocale } from "./datetime-values.js";
import { RecordState, specification } from "./record.js";
import { parseRoute, routeUrl, rpcId, type Route, isAddressSizeRefusal } from "./route.js";
import "./services.js";
import { type ServiceController } from "./services.js";
import { clientActionRedirects, clientActions, listHeaderExtensions, serviceControllers, type ClientActionHandle, type ListHeaderHandle } from "./registry.js";
import { mountControlPanel } from "./control-panel.js";
import { loadPrintReports, printDropdown, printRecords, type PrintableReport } from "./report-print.js";
import { listDataMenu, MAX_EXPORT_RECORDS } from "./list-data-menu.js";
import { requestLoading } from "./loading.js";
import { RefusedAction, isActionRefusal, mountRefusedActionPage } from "./refused-action.js";
import { actionResult } from "./actions.js";
import { CompanySelector, type CompanyItem } from "./company-selector.js";
import { reorderApps } from "./reorder-apps.js";
import { IDENTITY_METHOD, IDENTITY_MODEL, type Resubmission } from "./password-flow.js";
import { WizardDialog } from "./wizard-dialog.js";
import type { ActionResult } from "../generated/human-ui.js";
import { RecordForm, formFocus } from "./record-form.js";
import { renderDiscuss } from "./discuss.js";
import { DISCUSS_ACTION } from "./discuss-action.js";
import { MessagingStore } from "@erpforall/mail-store";
import { ClientError, RpcClient, newRequestId, type CallOptions } from "./transport.js";
import { attachSubviews } from "./x2many.js";
import { loadDatabaseTheme, loadUserColorScheme, officeShell } from "./theme.js";
import { MetadataCache, invalidatesMetadata } from "./metadata-cache.js";
import { ExpiredMethodAction, MethodActions, type MethodAction } from "./method-actions.js";
import { BreadcrumbTrail, renderBreadcrumbs, type BreadcrumbItem } from "./navigation.js";
import { NavigationStore } from "./navigation-store.js";
import { commandProviders, commandCategories, commandSetups, systrayItems, userMenuItems, debugSections, debugItemProviders } from "./registry.js";
import { openCommandPalette } from "./command-palette.js";
import type { ActivityWindowAction } from "./activity-menu.js";
import { renderSystray, type SystrayEnvironment } from "./systray.js";
import { composeUserMenuItems, renderUserMenuItems, type UserMenuEnvironment } from "./user-menu.js";
import { renderDebugMenu, createDebugCommandProvider, type DebugEnvironment } from "./debug-menu.js";
import { createChoiceDropdown } from "./choice-dropdown.js";
import { attachMenuDropdown } from "./menu-dropdown.js";
import { navOverflowCount } from "./navbar-overflow.js";
import { productName } from "./product-name.js";

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

/** The trail identity of a list or record page: its action, the related list it belongs to, and the record. */
function crumbId(route: Route, id?: number | string): string {
  const via = route.method_action ? `-via-${route.method_action}` : "";
  return `action-${route.action}${via}-${id === undefined ? "list" : `form-${id}`}`;
}

/** Gives the drawn current crumb the element `tag` (the `h1` that names the page, or a `span` where the page has its own). */
function retagCrumb(current: HTMLElement, tag: "h1" | "span"): void {
  if (current.tagName.toLowerCase() === tag) return;
  const replacement = el(tag); replacement.className = current.className;
  replacement.setAttribute("aria-current", "page"); replacement.textContent = current.textContent;
  current.replaceWith(replacement);
}

type SwitchableView = "list" | "form" | "pivot" | "calendar" | "graph" | "kanban" | "hierarchy" | "activity" | "map";
const SWITCHABLE_VIEWS: readonly string[] = ["list", "form", "pivot", "calendar", "graph", "kanban", "hierarchy", "activity", "map"];
const switchable = (view: string): view is SwitchableView => SWITCHABLE_VIEWS.includes(view);
/** The route of a view switch. The pivot, the calendar and the graph keep their own state (`pivot_*`, `calendar_*`, `graph_*`), and their search models refuse a list's `search_groupby` (so do kanban, hierarchy and activity), so that one is not carried over. */
function switchedRoute(route: Route, view: SwitchableView): Route {
  const next: Route = { ...route, id: view === "form" ? route.id : undefined, view_type: view };
  if (view === "pivot" || view === "calendar" || view === "graph" || view === "hierarchy" || view === "activity" || view === "map") delete next.search_groupby;
  else if ((view === "kanban" || route.view_type === "kanban") && next.search_groupby !== undefined) {
    let parsed: unknown;
    try { parsed = JSON.parse(next.search_groupby); } catch { throw new ClientError("validation", "Invalid grouping state."); }
    const by = decodeListState(parsed).by;
    if (by.length) next.search_groupby = JSON.stringify(by); else delete next.search_groupby;
  }
  if (view !== "kanban") delete next.kanban_fold;
  if (view !== "pivot") for (const key of PIVOT_PARAMS) delete next[key];
  if (view !== "calendar") for (const key of CALENDAR_PARAMS) delete next[key];
  if (view !== "graph") for (const key of GRAPH_PARAMS) delete next[key];
  return next;
}

/** Labels remain available to assistive technology and tooltips. */
function decorateViewButton(target: HTMLButtonElement, kind: SwitchableView): void {
  const label = el("span", kind === "list" ? "List" : kind === "pivot" ? "Pivot" : kind === "calendar" ? "Calendar" : kind === "graph" ? "Graph" : kind === "kanban" ? "Kanban" : kind === "hierarchy" ? "Hierarchy" : kind === "activity" ? "Activity" : kind === "map" ? "Map" : "Form", "view-button-label");
  target.replaceChildren(icon(kind === "list" ? "list" : kind === "pivot" ? "layout-grid" : kind === "calendar" ? "calendar-days" : kind === "graph" ? "chart-column" : kind === "kanban" ? "kanban" : kind === "hierarchy" ? "network" : kind === "activity" ? "clock" : kind === "map" ? "map" : "panels-top-left"), label);
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
  /** Which action model `action` names (ids are unique across action models). */
  kind: "window" | "client";
}

const MAX_CLIENT_STATE_BYTES = 16384;
type ClientResult = Extract<ActionResult, { kind: "client" }>;
type MountedClient = { key: string; name: string; handle: ClientActionHandle };

/** The `[id, type]` view requests of a window action, in action order. */
function actionViews(action: Record<string, unknown>): unknown[][] {
  return array(action["views"], "action views").map(value => {
    const view = object(value);
    return [view["id"], view["type"]];
  });
}

/** The view an action opens with when the address names none: its first view, except that an action whose first view
 *  is not a hierarchy or activity view opens on its list when it has one (a record id opens the first view). */
export function defaultViewType(views: readonly unknown[][], recordId: string | undefined): string {
  const first = String(views[0]?.[1]);
  if (first === "hierarchy" || first === "activity" || first === "map") return first;
  return !recordId && views.some(view => view[1] === "list") ? "list" : first;
}

/** Said above a view that read fewer rows than the server holds. */
function limitNotice(text: string): HTMLElement {
  const notice = el("p", text, "view-limit-notice muted");
  notice.setAttribute("role", "status");
  return notice;
}

export function isFalsy(value: unknown): boolean {
  if (value === false || value === 0) return true;
  if (typeof value === "string") {
    const s = value.trim().toLowerCase();
    return s === "false" || s === "0";
  }
  return false;
}

/** An `openAction` request from addon code, checked before anything is read: an XML id, a view mode, a domain list
 *  and a context of `default_<field>` keys only (as a dashboard card's). */
function admitAddonActionRequest(request: ActionRequest): ActionRequest {
  const refuse = (path: string, problem: string): never => { throw new ClientError("validation", `openAction: ${path} ${problem}.`); };
  if (typeof request !== "object" || request === null) return refuse("the request", "must be an object");
  if (typeof request.xmlId !== "string" || request.xmlId.length > 128 || !/^[a-z0-9_]+\.[A-Za-z0-9_]+$/.test(request.xmlId)) return refuse("the XML id", "must be module.name");
  if (request.viewMode !== "list" && request.viewMode !== "form" && request.viewMode !== null) return refuse("the view mode", "must be list, form or null");
  if (!Array.isArray(request.domain)) return refuse("the domain", "must be a list");
  const context = admitRequestContext(request.context, "the context", refuse);
  for (const key of Object.keys(context)) {
    if (key.startsWith("search_default_")) refuse(`the context[${JSON.stringify(key.slice(0, 64))}]`, "is not admitted: only default_<field> keys are");
  }
  return { xmlId: request.xmlId, viewMode: request.viewMode, domain: [...request.domain], context };
}

/** The records a form was opened from, which its pager steps through. */
type ListContext = {
  ids: number[]; total: number; offset: number; limit: number;
  /** Loads the ids of another page of the same search and order, so the form pager can cross a page edge. */
  page?: (offset: number) => Promise<{ ids: number[]; total: number }>;
};

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
  private readonly threadReconnect?: { company: number; csrf: string; retry: readonly [number, number] };
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
  /** Related lists opened by buttons; the tab's navigation store keeps them for a reload. */
  private readonly methodActions: MethodActions;
  /** An invocation identity, distinct from route data; only its completed result may hand off. */
  private pendingInvocation: { caller: RecordState; generation: number } | undefined;
  private rootMenuIds: number[] = [];
  private settingsRootMenuIds: number[] = [];
  private officeSidebarHidden?: boolean;
  private headerMenuSearchReady: (() => void) | undefined;
  private current: Route = {};
  /**
   * The systray's copy of the shell context, published to every systray item once. It is a
   * separate object so the open-record keys (`active_model`/`active_id`/`active_ids`) can be
   * updated in place as the mounted record changes without those keys ever leaking into the
   * RPC contexts built from `this.context`; `syncSystrayRecord` owns the keys.
   */
  private readonly systrayContext: Record<string, unknown>;
  private currentUrl = window.location.pathname + window.location.search;
  private generation = 0;
  /** The navigation generation whose page is on screen (`current`); a later one may still be staging. */
  private shownGeneration = 0;
  /** Number of the latest action a dashboard card asked to open (`openActionRequest`). */
  private actionRequest = 0;
  /** Settles when the navigation now running ends, whether it committed, was superseded or failed. */
  private navigationInFlight: Promise<void> | undefined;
  /** Records of `edit="0"` views: once they have an id they are never written back. */
  private readonly lockedRecords = new WeakSet<RecordState>();
  /** What a mounted form does once its record was saved (the address, the systray and the trail carry the saved record);
   *  a save outside the form's own Save (the leave prompt, a breadcrumb's autosave) runs it too. */
  private readonly savedRecords = new WeakMap<RecordState, () => void>();
  /** The Print reports of each model, read once per page: a list and its forms share one answer; a failed read is not kept. */
  private readonly printReports = new Map<string, Promise<PrintableReport[]>>();
  private keyboardPending: "save" | "discard" | undefined;
  private historyIndex = 0;
  private restoringHistory: number | undefined;
  private traversalPending: { index?: number; url: URL } | undefined;
  private traversalRunning = false;
  private record: RecordState | undefined;
  private service: ServiceController | undefined;
  /** Client action results kept for their route (`client:<n>`), bounded; and the state each client action left. */
  private readonly clientResults = new Map<string, ClientResult>();
  private readonly clientStates = new Map<string, unknown>();
  private readonly clientResultPage = newRequestId();
  private clientCounter = 0;
  private pendingClient: MountedClient | undefined;
  private clientMounted: MountedClient | undefined;
  private busy = false;
  private wizard: WizardDialog | undefined;
  private actionPending = false;
  private uncertainMethod = false;
  /** Captured once, before the first teardown: later 401s must not lose the warning or return path. */
  private expiry: { readonly lostEdits: boolean; readonly signIn: string } | undefined;
  private expired = false;
  private readonly debug = new URL(location.href).searchParams.get("debug") === "1";
  private readonly breadcrumbs = new BreadcrumbTrail(32);
  /** The trail of each history entry of this tab, kept across a reload (`navigation-store.ts`). */
  private readonly navigationStore: NavigationStore;
  private homeMenuOpen = false;
  private releaseHomeScroll?: (restore: boolean) => void;
  private mobileDrawer?: MobileDrawer;
  private userDropdownOpen = false;
  private messagingStore?: MessagingStore;
  private discuss?: ReturnType<typeof renderDiscuss>;

  private getDiscuss(): ReturnType<typeof renderDiscuss> {
    if (!this.discuss) {
      this.discuss = renderDiscuss({
        rpc: this.rpc, store: this.getMessagingStore(), uid: this.userId, userName: this.userName,
        context: this.context,
        ...(this.threadReconnect ? { policy: { ...this.threadReconnect, company: this.activeCompanyIds[0]! } } : {}),
        openWorkspace: () => this.run(async () => {
          const menu = [...this.menus.values()].find(item => item.parentId === null && item.module === "mail" && item.kind === "client" && item.action !== null);
          if (!menu?.action) throw new ClientError("permission", "The Discuss workspace is unavailable for this user.");
          await this.navigate({ action: String(menu.action), menu: String(menu.id) });
        }),
        openRecord: (model, id) => this.run(() => this.openRelatedRecord(model, id)),
      });
    }
    return this.discuss;
  }

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

  /** The kanban page a card was opened from (its address, page, scroll and card), so the way back shows it as it was left. */
  private kanbanResume?: { url: string; offset: number; scroll: number; id: number;
    /** On a grouped board: its own scroll offsets and how many cards each open column showed. */
    board?: { left: number; top: number; loaded: [KanbanColumnKey, number][] } };
  /** Display state of the list that opened the current record, scoped by its complete route. */
  private rememberList?: (id?: number) => void;
  private listResume?: { url: string; offset: number; limit: number; order: string; id?: number;
    scroll: number; left: number; top: number; widths: Map<string, number>; wrap: boolean };
  private listContext?: ListContext;
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
    // The client's own texts follow the user's language (translate.ts).
    setClientLanguage(this.editorLocale.lang, locale["direction"]);
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
    this.systrayContext = { ...this.context };

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
    this.navigationStore = new NavigationStore(this.userId, this.breadcrumbs.cap, error =>
      this.messages.notifications.add(`The breadcrumb trail cannot be kept across a reload in this tab: ${errorMessage(error)}`, { level: "warning" }));
    this.methodActions = new MethodActions({ save: (id, entry) => this.navigationStore.saveAction(id, entry), remove: id => this.navigationStore.removeAction(id) });
    this.navigation = region("[data-client-navigation]");
    this.header = region("[data-client-header]");
    this.loading = requestLoading(host);
    this.rpc = new RpcClient(window.fetch.bind(window), () => this.expire(), () => this.loading.begin(), model => {
      if (invalidatesMetadata(model)) { this.metadata.clear(); this.methodActions.clear(); }
      if (model === "approval.request.view") document.dispatchEvent(new Event("erp:activities-changed"));
    }, (event, options) => {
      if (this.expired || this.lifetime.signal.aborted) return;
      this.panelRecovery.observe(event, options);
      this.messages.network.observe(event, options);
    }, id => waitForApproval(this.messages.dialogs, this.rpc, id, this.lifetime.signal, this.approvalsMenu() !== undefined));
    this.listenForMenuIntent();
    initializeAttachmentPolicy(
      this.rpc,
      bootstrap["attachment_limits"],
      bootstrap["rpc_body_bytes"],
      text(object(bootstrap["csrf"])["logout"], "session token"),
    );

    const retry = bootstrap["thread_reconnect_ms"];
    if (retry !== undefined && retry !== null) {
      if (!Array.isArray(retry) || retry.length !== 2 || !retry.every(n => Number.isSafeInteger(n) && n > 0) || retry[1] < retry[0]) {
        throw new Error("Invalid thread reconnect policy.");
      }
      this.threadReconnect = { company: this.currentCompanyId, csrf: text(object(bootstrap["csrf"])["logout"], "session token"), retry: [retry[0], retry[1]] };
    }
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
        // Back on the page's own entry after a step that failed: it gets the page's address, which a save on the way out may have changed.
        history.replaceState({ erpIndex: targetIndex }, "", this.currentUrl);
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
            // The entry comes back with the trail it had; its page then cuts the trail to itself as usual.
            const before = [...this.breadcrumbs.items];
            try {
              const route = parseRoute(target.url);
              const kept = target.index === undefined ? undefined : this.navigationStore.trail(target.index, route);
              if (kept) this.breadcrumbs.replace(kept);
              await this.navigate(route, true, true);
              if (target.index !== undefined) {
                this.historyIndex = target.index;
                this.saveTrail();
              }
            } catch (error) {
              this.breadcrumbs.replace(before);
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
      list.push({ kind: "callback", chord: "ctrl+k", target: this.host, hint: "", allowEditable: true, run: () => this.openCommandPalette() });
      list.push({ kind: "callback", chord: "meta+k", target: this.host, allowEditable: true, run: () => this.openCommandPalette() });
      const create = this.content.querySelector<HTMLButtonElement>('[data-control-panel]:not(.control-panel-form) [data-control-actions] button[data-hotkey="c"]');
      if (create && !active && !this.busy && !this.actionPending && !this.keyboardPending && !this.uncertainMethod && !this.messages.block.blocked) {
        list.push({ kind: "dom", chord: actionHotkeyModifier(document) + "+c", target: create, allowEditable: true });
      }
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
          return [{ id: `menu-${menu.id}`, name: menu.label, icon: menu.icon, category: "Menus", action: () => this.navigate({ action: String(target.action), menu: String(target.menu) }) }];
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

  private renderSidebarToggle(): void {
    const key = `erp.office-sidebar.hidden.${this.userId}`;
    if (this.officeSidebarHidden === undefined) {
      this.officeSidebarHidden = false;
      try { this.officeSidebarHidden = window.localStorage.getItem(key) === "true"; }
      catch (error) { console.warn("Could not read the sidebar preference.", error); }
    }
    const toggle = button("", () => {
      this.officeSidebarHidden = !this.officeSidebarHidden;
      render();
      try { window.localStorage.setItem(key, String(this.officeSidebarHidden)); }
      catch (error) { console.warn("Could not save the sidebar preference.", error); }
    }, "office-sidebar-toggle");
    this.navigation.id ||= "erp-app-navigation";
    toggle.setAttribute("aria-controls", this.navigation.id);
    const render = (): void => {
      const hidden = this.officeSidebarHidden === true;
      this.header.parentElement?.classList.toggle("office-sidebar-hidden", hidden);
      toggle.setAttribute("aria-expanded", String(!hidden));
      toggle.title = hidden ? "Show sidebar" : "Hide sidebar";
      toggle.setAttribute("aria-label", toggle.title);
      toggle.replaceChildren(icon(hidden ? "chevron-right" : "chevron-left"));
    };
    render();
    this.header.append(toggle);
  }

  private renderHeader(bootstrap: Record<string, unknown>): void {
    for (const stop of this.disposeHeader) stop();
    this.disposeHeader = []; this.headerMenuSearchReady = undefined; this.userDropdownOpen = false;
    this.header.replaceChildren();

    const brand = button("", () => this.run(() => this.navigate({})), "brand");
    const product = productName();
    brand.setAttribute("aria-label", `${product} home`);
    // Both variants ship; the theme tokens in client.css show the one that fits the top bar.
    for (const [src, variant] of [["/erp/brand/logo.svg", "light"], ["/erp/brand/logo-dark.svg", "dark"]] as const) {
      const logo = el("img", undefined, `brand-logo brand-logo-${variant}`);
      logo.src = src; logo.alt = variant === "light" ? product : ""; logo.width = 120; logo.height = 32;
      brand.append(logo);
    }
    this.header.append(brand);
    const office = officeShell();
    if (office) {
      this.renderSidebarToggle();
      const search = headerMenuSearch({
        entries: () => [...this.menus.values()].flatMap(menu => {
          const target = this.defaultActionForMenu(menu.id);
          return target ? [{ id: menu.id, label: menu.label, icon: menu.icon, ...target }] : [];
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
    // A handle that arrives after this header was replaced is disposed, not added to the newer header's disposers.
    const headerStops = this.disposeHeader;
    const systrayNavigation = this.shellWideNavigation(() => this.disposeHeader === headerStops);
    const systrayEnv: SystrayEnvironment = {
      userId: this.userId,
      csrfToken: text(object(bootstrap["csrf"])["logout"], "session token"),
      rpc: this.rpc,
      context: this.context,
      assistantContext: this.systrayContext,
      openRecord: async (model, id) => { if (this.disposeHeader === headerStops) await this.openRelatedRecord(model, id); },
      run: job => this.run(job),
      report: err => this.reportError(err),
      messages: this.messages,
      openActivities: action => this.openActivities(action),
      ...(this.approvalsMenu() ? { openApprovals: () => this.openApprovals() } : {}),
      ...(this.threadReconnect ? { discussNotifications: this.getDiscuss().notifications } : {}),
      navigate: systrayNavigation.navigate,
      openAction: systrayNavigation.openAction
    };
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

    if (this.threadReconnect) {
      systray.append(this.getDiscuss().node);
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
              try { await this.rememberCompanies(targetCids); }
              catch (error) { companySelector.reset(); renderTreeAndActions(); throw error; }
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
              const chosen = [...companySelector.selectedCompanyIds];
              try { await this.rememberCompanies(chosen); }
              catch (error) { companySelector.reset(); renderTreeAndActions(); throw error; }
              const route = { ...this.current, cids: chosen.map(String) };
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
      companyTrigger.title = currentCompanyName;
      companyTrigger.append(icon("building"), el("span", currentCompanyName, "company-menu-name"), icon("chevron-down"));
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
        if (await this.canLeave()) { clearProtectedTabState(); logoutForm.submit(); }
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

    const approvalBtn = el("button", "Review approval", "user-menu-row");
    approvalBtn.type = "button";
    approvalBtn.prepend(icon("badge-check"));
    approvalBtn.addEventListener("click", () => {
      this.userDropdownOpen = false;
      userDropdown.classList.remove("open");
      this.run(() => this.openApprovals());
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
    userDropdown.append(userInfo, divider(), shortcutsBtn, preferencesBtn, ...(this.approvalsMenu() ? [approvalBtn] : []), customUserMenuContainer, divider(), logoutForm);
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
    release(() => this.discuss?.dispose()); this.discuss = undefined;
    release(() => this.loading.dispose());
    release(() => this.errors.dispose());
    release(() => this.mobileDrawer?.dispose());
    release(() => { const unlock = this.releaseHomeScroll; this.releaseHomeScroll = undefined; unlock?.(false); });
    for (const stop of [...this.disposeWidgets, ...this.disposeListeners, ...this.disposeHeader]) release(stop);
    for (const stop of this.disposeNavMenus) release(stop);
    this.disposeWidgets = []; this.disposeListeners = []; this.disposeHeader = []; this.disposeNavMenus = [];
    this.record = undefined; this.service = undefined; this.clientMounted = undefined; this.pendingClient = undefined; this.clientResults.clear(); this.clientStates.clear(); this.pendingInvocation = undefined; this.methodActions.clear();
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
      clearProtectedTabState();
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
      return this.saveBeforeLeaving();
    }
    const choice = await this.choose("Unsaved changes", "Save your changes before leaving this page?", ["Save", "Discard", "Cancel"]);
    if (choice === "Save") return this.saveBeforeLeaving();
    if (choice === "Discard") { this.discardBeforeLeaving(); return true; }
    return false;
  }

  /** Saves the page being left. A saved record goes on in the address and the trail as itself (a new one with its id),
   *  so a page stacked on it next leads back to the saved record. */
  private async saveBeforeLeaving(): Promise<boolean> {
    const record = this.record;
    try {
      if (record) await record.save();
      else await this.service?.save();
    } catch (error) {
      return this.messages.saveFailure(error, { discard: () => this.discardBeforeLeaving() });
    }
    if (record) this.savedRecords.get(record)?.();
    return true;
  }

  /** Discards the page being left. A new record that was never saved leaves the trail too: nothing is stacked on it. */
  private discardBeforeLeaving(): void {
    if (this.record && this.record.id === undefined && this.breadcrumbs.current?.id === crumbId(this.current, "new")) {
      this.breadcrumbs.replace(this.breadcrumbs.items.slice(0, -1));
    }
    this.record?.discard(); this.service?.discard();
  }

  async start(resolveTheme = false): Promise<void> {
    if (this.lifetime.signal.aborted) return;
    if (resolveTheme) {
      await loadDatabaseTheme(this.rpc, this.registry, this.context);
      if (this.lifetime.signal.aborted) return;
      // A cosmetic choice never stops the application from starting: a failed read is reported and the device preference stays.
      await this.applyUserColorScheme();
      if (this.lifetime.signal.aborted) return;
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
      const rawKind = node["action_kind"];
      if (rawKind !== undefined && rawKind !== null && rawKind !== "window" && rawKind !== "client") throw new ClientError("server", "Invalid menu action kind.");
      this.menus.set(id, { module, id, label: text(node["label"], "menu label"), action, children, parentId, icon, kind: rawKind === "client" ? "client" : "window" });
    }
    for (const id of array(response["roots"], "menu roots")) {
      this.rootMenuIds.push(integer(id, "root menu"));
    }

    if (resolveTheme || this.approvalsMenu()) this.renderHeader(this.headerBootstrap);

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
    // A reload keeps this entry's place in the tab's history and its trail; an address opened afresh has neither, and
    // its entries are numbered past the ones the tab keeps, whose trails Back still brings back.
    const state: unknown = history.state;
    const index = typeof state === "object" && state !== null ? (state as Record<string, unknown>)["erpIndex"] : undefined;
    if (typeof index === "number" && Number.isSafeInteger(index) && index >= 0) {
      this.historyIndex = index;
      const kept = this.navigationStore.trail(index, initialRoute);
      if (kept) this.breadcrumbs.replace(kept);
    } else this.historyIndex = this.navigationStore.nextIndex();
    if (initialRoute.action === undefined && await this.openHomeAction(initialRoute)) return;
    if (this.lifetime.signal.aborted) return;
    try { await this.navigate(initialRoute, true); }
    catch (error) {
      // A related list (or record) whose first load failed for any reason: the tab no longer holds it, the server no longer
      // admits it, or its page could not load. Offer explicit recovery to the originating persisted action; never
      // silently substitute it for the related list.
      if (error instanceof ExpiredMethodAction || initialRoute.method_action !== undefined && !this.isClientRoute(initialRoute)) {
        const recovery = el("a", "Return to originating action", "method-action-recovery");
        recovery.href = routeUrl({ action: initialRoute.action, menu: initialRoute.menu, cids: initialRoute.cids });
        this.content.replaceChildren(recovery);
      }
      throw error;
    }
  }

  /** The user's home action (`res.users.action_id`) in place of the app grid, on a first load whose address names no
   *  action. Answers whether it opened. The action loads through the usual access checks; one the user may not open,
   *  or a failed read, leaves the grid with a notice that says why. The logo and Back still reach the grid. */
  private async openHomeAction(initial: Route): Promise<boolean> {
    try {
      const rows = array(await this.rpc.call("res.users", "web_read", { ids: [this.userId], specification: { action_id: {} }, context: this.context }, { signal: this.lifetime.signal }), "user rows");
      if (rows.length !== 1) throw new ClientError("server", "Invalid home action response.");
      const value = object(rows[0], "user row")["action_id"];
      if (value === false) return false;
      const action = String(integer(value, "home action"));
      await this.navigate({ action, ...(initial.cids === undefined ? {} : { cids: initial.cids }) }, true, false, undefined, { showRefusedPage: false });
      return this.current.action === action;
    } catch (error) {
      if (this.lifetime.signal.aborted) return false;
      this.messages.notifications.add(`Your home action could not be opened, so the apps are shown instead: ${errorMessage(error)}`, { level: "warning" });
      return false;
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

  /** A window action's own domain, evaluated for the user and `context`. */
  private actionDomain(action: Record<string, unknown>, context: Record<string, unknown>): unknown[] {
    if (!action["domain"]) return [];
    const ast = decodeAst(parseBoundedJson(JSON.stringify(action["domain"])));
    const val = jsonValue(evalAst(ast, createContext({ names: { context: expressionValue(context, 0), uid: expressionValue(this.userId, 0) } })));
    if (!Array.isArray(val)) throw new ClientError("validation", "Action domain must evaluate to a list.");
    return val;
  }

  /** A window action's own context: the server admits only a literal dictionary of `default_<field>` and
   *  `search_default_<name>` text, safe integer or boolean values (`addons/web/src/metadata.rs`, `parse_action_context`);
   *  the same is checked here (the search defaults by `splitSearchDefaults`). */
  private actionContext(action: Record<string, unknown>): Record<string, unknown> {
    if (action["context"] === null) return {};
    const ast = decodeAst(parseBoundedJson(JSON.stringify(action["context"])));
    const value = jsonValue(evalAst(ast, createContext({ names: {} })));
    const context = object(value, "action context");
    for (const [key, item] of Object.entries(context)) {
      if (!/^default_[a-z_][a-z0-9_]*$/.test(key) && !key.startsWith(SEARCH_DEFAULT_PREFIX)) throw new ClientError("unsupported", `Unsupported action context key: ${key}.`);
      if (!(typeof item === "string" || typeof item === "boolean" || typeof item === "number" && Number.isSafeInteger(item))) {
        throw new ClientError("server", `Invalid action context value for ${key}.`);
      }
    }
    return context;
  }

  /** The `ShellNavigation` handed to addon code. `live` says whether the owner that received it is still the one on
   *  screen (after that both do nothing); `origin` is the page `openAction` leads back to, read when it is called. */
  private shellNavigation(live: () => boolean, origin: () => { route: Route; label: string }): ShellNavigation {
    const usable = (): boolean => live() && !this.expired && !this.lifetime.signal.aborted;
    return {
      navigate: async requested => {
        if (!usable()) return;
        const route: Route = { ...requested };
        routeUrl(route); // a malformed route fails here, before any guard runs
        if (route.method_action !== undefined) throw new ClientError("validation", "Addon navigation cannot name a method action: open the action with openAction instead.");
        const active = new Set(this.activeCompanyIds.map(String));
        if (route.cids !== undefined && (route.cids.length !== active.size || route.cids.some(id => !active.has(id)))) {
          // Other companies are a new company context: the page loads again with them, as the company switcher does.
          if (await this.canLeave()) this.navigateLocation(routeUrl(route));
          return;
        }
        await this.navigate(route);
      },
      openMenuAction: async xmlId => {
        if (usable()) await this.openMenuAction(xmlId);
      },
      openAction: async request => {
        if (!usable()) return;
        const admitted = admitAddonActionRequest(request);
        const from = origin();
        await this.openActionRequest(admitted, from.route, from.label);
      }
    };
  }

  /** `ShellNavigation` for code that lives as long as the shell (systray items): it leads back to the page open at the
   *  time of the call. */
  private shellWideNavigation(live: () => boolean): ShellNavigation {
    return this.shellNavigation(live, () => {
      const menu = this.current.menu === undefined ? undefined : this.menus.get(Number(this.current.menu));
      return { route: { ...this.current }, label: this.breadcrumbs.current?.title ?? menu?.label ?? "Back" };
    });
  }

  /** `ShellNavigation` for code that belongs to the page of navigation `generation` (a service, a client action, a
   *  field widget): it stops when that page is left and leads back to that page as it is at the time of the call (its
   *  address keeps search and record changes), or to `route` while the page is still being built; titled `label`. */
  private pageNavigation(generation: number, route: Route, label: () => string): ShellNavigation {
    return this.shellNavigation(() => generation === this.generation,
      () => ({ route: this.shownGeneration === generation ? { ...this.current } : { ...route }, label: label() }));
  }

  /** Opens a window action named by its XML id (dashboard cards, addon code): its list, or a new record in its form,
   *  filtered by the action's domain and the request's, with the request's context and the way back to `origin`.
   *  From a page without an action (the app grid), the way back is the opened action's own list. */
  private async openActionRequest(request: ActionRequest, origin: Route, originLabel: string): Promise<void> {
    // The list's stale-answer guard: request number, route generation, expiry. The read does not block the page, so
    // the user may go elsewhere or activate a control again before it answers; only the latest request of the page
    // that asked navigates.
    const mine = ++this.actionRequest, generation = this.generation;
    const action = object(await this.rpc.call("ir.actions.act_window", "erp_load_web_action_ref", { xml_id: request.xmlId, context: this.context }));
    if (mine !== this.actionRequest || generation !== this.generation || this.expired) return;
    if (action["wire_major"] !== 1 || action["target"] !== "current") throw new ClientError("unsupported", "Unsupported or mismatched window action.");
    // The action's defaults first, then the request's (a figure opened with `{}` still creates with the action's), and
    // the session context last, so neither the action nor the card replaces a session key. The request's context is
    // checked again here: any widget can hand one in, not only a parsed dashboard card.
    const context = { ...this.actionContext(action), ...admitRequestContext(request.context, "the request context"), ...this.context };
    const views = actionViews(action), list = views.find(view => view[1] === "list"), form = views.find(view => view[1] === "form");
    // A related window opens a list (and its form) only (actions.ts `current_window`); other view types come with that result.
    if (!list) throw new ClientError("unsupported", `The action ${request.xmlId} has no list view, so a dashboard card cannot open it yet.`);
    if (request.viewMode === "form" && !form) throw new ClientError("unsupported", `The action ${request.xmlId} has no form view to create a record in.`);
    const result = actionResult({ wire_major: 1, result: { kind: "current_window", name: text(action["name"], "action name"), model: text(action["model"], "action model"),
      domain: [...this.actionDomain(action, context), ...request.domain], context: {},
      views: [{ id: list[0] ?? null, type: "list" }, ...(form ? [{ id: form[0] ?? null, type: "form" }] : [])] } });
    if (result.kind !== "current_window") throw new ClientError("server", "The dashboard action did not open as a list.");
    // A method action lives under an action route; the app grid has none, so the opened action anchors itself.
    const anchor: Route = origin.action !== undefined ? { ...origin } : { action: String(integer(action["id"], "action id")), ...(origin.cids === undefined ? {} : { cids: origin.cids }) };
    const label = origin.action !== undefined ? originLabel : result.name;
    const id = this.methodActions.add({ result, context, origin: anchor, originLabel: label });
    try { await this.navigate({ action: anchor.action, menu: anchor.menu, cids: anchor.cids, method_action: id, model: result.model, view_type: request.viewMode === "form" ? "form" : "list" }); }
    catch (error) { this.methodActions.remove(id); throw error; }
    // A navigation that did not arrive (the page may not be left) keeps no entry behind.
    if (this.current.method_action !== id) this.methodActions.remove(id);
  }

  /** The reports the Print button offers for `model`, shared for the page's lifetime; a failed read is tried again next time. */
  private reportsOf(model: string): Promise<PrintableReport[]> {
    const known = this.printReports.get(model);
    if (known) return known;
    const loading = loadPrintReports(this.rpc, model, { background: true });
    this.printReports.set(model, loading);
    loading.catch(() => { if (this.printReports.get(model) === loading) this.printReports.delete(model); });
    return loading;
  }

  /** Navigation and prefetch share one caller-bound action/view metadata producer. */
  private async windowAction(id: number, options: CallOptions = {}): Promise<Record<string, unknown>> {
    return object((await this.windowBundle(id, options))["action"]);
  }

  private async windowBundle(id: number, options: CallOptions = {}): Promise<Record<string, unknown>> {
    const args = { ids: [id], context: this.context };
    return object(await this.metadata.read(this.bundleKey(id), async () => {
      const generation = this.metadata.generation;
      const bundle = object(await this.rpc.call("ir.actions.act_window", "erp_load_web_action_views",
        { ...args, wire_major: 1, options: { mobile: false } }, options));
      if (Object.keys(bundle).length !== 2 || !("action" in bundle) || !("views" in bundle)) {
        throw new ClientError("validation", "Invalid action and view metadata bundle.");
      }
      return this.admitBundle(bundle, id, generation);
    }));
  }

  private bundleKey(id: number): string {
    return `action ${JSON.stringify({ ids: [id], context: this.context })}`;
  }

  /** Checks an action and its views as one answer, then shares the views with plain view requests of the same views. */
  private admitBundle(bundle: Record<string, unknown>, id: number, generation: object): Record<string, unknown> {
    const action = object(bundle["action"]);
    if (action["wire_major"] !== 1 || (action["target"] !== "current" && action["target"] !== "new") || action["id"] !== id) {
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
  }

  /** The menu and action that open record `id` of `model`, resolved by the server in one call. The answer is kept for
   *  the page when it holds for every record of the model, and the action's views go into the metadata cache, so the
   *  navigation that follows reads neither again. Undefined when no accessible form action opens the record. */
  private async relatedTarget(model: string, id: number): Promise<{ action: number; menu: number } | undefined> {
    const modelKey = `related ${JSON.stringify({ model, context: this.context })}`;
    const known = this.metadata.peek(modelKey);
    if (known) {
      const target = object(await known);
      return { action: integer(target["action"], "related action"), menu: integer(target["menu"], "related menu") };
    }
    const generation = this.metadata.generation;
    const answer = await this.rpc.call("ir.actions.act_window", "erp_resolve_record_action",
      { model, record_id: id, wire_major: 1, options: { mobile: false }, context: this.context });
    if (answer === null) return undefined;
    const found = object(answer, "related record action");
    if (Object.keys(found).length !== 4 || typeof found["any_record"] !== "boolean") {
      throw new ClientError("validation", "Invalid related record action.");
    }
    const action = integer(object(found["action"])["id"], "related action"), menu = integer(found["menu"], "related menu");
    const bundle = this.admitBundle({ action: found["action"], views: found["views"] }, action, generation);
    if (object(bundle["action"])["target"] !== "current") throw new ClientError("unsupported", "Unsupported or mismatched window action.");
    if (object(bundle["action"])["model"] !== model) throw new ClientError("validation", "The related action opens another model.");
    this.metadata.seed(this.bundleKey(action), bundle, generation);
    if (found["any_record"]) this.metadata.seed(modelKey, { action, menu }, generation);
    return { action, menu };
  }

  private viewArgs(views: unknown[][], context = this.context): Record<string, unknown> {
    return { views, wire_major: 1, options: { mobile: false }, context };
  }

  private viewKey(model: string, views: unknown[][], context = this.context): string {
    return `views ${model} ${JSON.stringify(this.viewArgs(views, context))}`;
  }

  /** Plain view requests (dialogs, line views, relation editors) through the same page-lifetime cache. */
  private readonly viewLoader: ViewLoader = (model, views, context) => this.viewDescriptions(model, views, {}, context);

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
    if (!target || this.expired || this.menus.get(target.menu)?.kind === "client") return;
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

  /**
   * Stores the companies the user chose in the switcher (current first) in `res.users.settings`,
   * so a later load without `cids` in its address starts with them. A refused or failed save
   * stops the switch: the error is reported and the user stays where they are.
   */
  private async rememberCompanies(companyIds: readonly number[]): Promise<void> {
    await this.rpc.call("res.users.settings", "set_res_users_settings", {
      new_settings: { active_company_ids: [...companyIds] },
    }, { write: true });
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
        const appIcon = menuIcon(rootMenu.icon);
        iconBox.append(appIcon);
        // An addon image fills the tile; a glyph sits on the launcher background (also when the image fails to load).
        if (!(appIcon instanceof HTMLImageElement)) iconBox.classList.add("app-icon-fallback");
        else appIcon.addEventListener("error", () => iconBox.classList.add("app-icon-fallback"), { once: true });
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
    const focused = document.activeElement;
    const previousMore = sections.querySelector(":scope > .nav-more");
    const focusWasInMore = focused !== null && previousMore?.contains(focused) === true;
    const focusedMenuId = (focused instanceof HTMLElement ? focused.dataset["menuId"] : undefined)
      ?? previousMore?.querySelector<HTMLElement>("[data-menu-id]")?.dataset["menuId"];
    this.disposeNavMore?.(); this.disposeNavMore = undefined;
    previousMore?.remove();
    const entries = [...sections.querySelectorAll<HTMLElement>(":scope > [data-nav-entry]")];
    for (const entry of entries) entry.hidden = false;
    // A refit may hide the focused section or remove the focused More row. Keep focus on a visible opener.
    const restoreFocus = (overflow?: HTMLButtonElement): void => {
      if (focusWasInMore) {
        const original = entries.find(entry => entry.dataset["menuId"] === focusedMenuId
          || [...entry.querySelectorAll<HTMLElement>("[data-menu-id]")].some(item => item.dataset["menuId"] === focusedMenuId));
        const target = original && !original.hidden
          ? (original instanceof HTMLButtonElement ? original : original.querySelector<HTMLButtonElement>(":scope > button"))
          : overflow;
        target?.focus();
      } else if (focused && entries.some(entry => entry.hidden && entry.contains(focused))) overflow?.focus();
    };
    const available = sections.clientWidth;
    this.navFitWidth = available;
    if (available === 0 || entries.length === 0) return; // not laid out (hidden bar, tests without layout)
    const more = el("div", undefined, "nav-dropdown nav-more");
    const list = el("div", undefined, "nav-dropdown-menu");
    const toggle = button("More", () => {}, "nav-dropdown-toggle");
    const moreChevron = icon("chevron-down");
    moreChevron.classList.add("nav-menu-chevron");
    toggle.append(moreChevron);
    more.append(toggle, list);
    sections.append(more);
    const width = (element: Element): number => element.getBoundingClientRect().width;
    const lead = [...sections.children].filter(child => child !== more && !entries.includes(child as HTMLElement)).reduce((sum, child) => sum + width(child), 0);
    const gap = Number.parseFloat(getComputedStyle(sections).columnGap) || 0;
    const count = navOverflowCount(lead, entries.map(width), available, width(more), gap);
    if (count === 0) { more.remove(); restoreFocus(); return; }
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
      entry.prepend(menuIcon(menu.icon ?? "lucide:file-text"));
      entry.dataset["menuId"] = String(menu.id);
      if (sections.querySelector(`[data-menu-id="${menu.id}"][aria-current=page]`)) entry.setAttribute("aria-current", "page");
      list.append(entry);
    };
    const group = (menu: Menu): void => {
      const heading = el("div", menu.label, "dropdown-header");
      heading.prepend(menuIcon(menu.icon ?? "lucide:folder-open"));
      list.append(heading);
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
    restoreFocus(toggle);
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
        appBrand.prepend(menuIcon(activeRoot.icon));
        appBrand.dataset["menuId"] = String(activeRoot.id);
        sections.append(appBrand);

        for (const childId of activeRoot.children) {
          const child = this.menus.get(childId);
          if (!child) continue;
          if (child.children.length === 0) {
            if (child.action !== null) {
              const entry = button(child.label, () => this.run(() => this.navigate({ action: String(child.action), menu: String(child.id) })), "nav-item");
              entry.prepend(menuIcon(child.icon ?? "lucide:file-text"));
              entry.dataset["menuId"] = String(child.id);
              entry.dataset["navEntry"] = String(child.id);
              sections.append(entry);
            }
          } else if (this.hasLeafAction(child.id)) {
            const dropdown = el("div", undefined, "nav-dropdown");
            dropdown.dataset["navEntry"] = String(child.id);
            const menuList = el("div", undefined, "nav-dropdown-menu");
            const toggle = button(child.label, () => {}, "nav-dropdown-toggle");
            const entryIcon = menuIcon(child.icon ?? "lucide:folder-open");
            entryIcon.classList.add("menu-entry-icon");
            const chevron = icon("chevron-down");
            chevron.classList.add("nav-menu-chevron");
            toggle.prepend(entryIcon);
            toggle.append(chevron);
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
                  item.prepend(menuIcon(member.icon ?? "lucide:file-text"));
                  item.dataset["menuId"] = String(member.id);
                  menuList.append(item);
                } else if (this.hasLeafAction(member.id)) {
                  const header = el("div", member.label, "dropdown-header");
                  header.prepend(menuIcon(member.icon ?? "lucide:folder-open"));
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
      const drawerTitle = el("strong", activeRoot ? activeRoot.label : productName());
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
              item.prepend(menuIcon(member.icon ?? "lucide:file-text"));
              item.dataset["menuId"] = String(member.id);
              if (level > 0) {
                item.style.paddingLeft = `${12 + level * 12}px`;
              }
              drawerSections.append(item);
            } else if (this.hasLeafAction(member.id)) {
              const subtitle = el("div", member.label, "drawer-section-subtitle");
              subtitle.prepend(menuIcon(member.icon ?? "lucide:folder-open"));
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
        item.prepend(menuIcon(rootMenu.icon));
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
        if (officeShell() && window.innerWidth > 768) {
          if (this.officeSidebarHidden) {
            this.header.querySelector<HTMLButtonElement>(".office-sidebar-toggle")?.focus();
            return;
          }
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

  /** Publish the mounted record's identity into the systray context as `active_model`/`active_id`/
   *  `active_ids` (the action-context key names), or remove all three when no record is open (a list,
   *  a service view, the home page). The keys exist only as a complete valid pair, matching the
   *  assistant panel's fail-loud read of them. */
  private syncSystrayRecord(): void {
    const record = this.record;
    const id = record?.id;
    if (record && id !== undefined && Number.isSafeInteger(id) && id > 0) {
      this.systrayContext["active_model"] = record.model;
      this.systrayContext["active_id"] = id;
      this.systrayContext["active_ids"] = [id];
    } else {
      delete this.systrayContext["active_model"];
      delete this.systrayContext["active_id"];
      delete this.systrayContext["active_ids"];
    }
  }

  /** The related list `id` of this page, or the one the tab kept from before a reload: that one is admitted again by the
   *  server exactly as when its button first opened it (model, views, literal domain and defaults, under the caller's
   *  rights and the page's companies), and opens as the server answers now. One the tab does not hold is expired. */
  private async methodAction(id: string): Promise<MethodAction> {
    try { return this.methodActions.get(id); }
    catch (error) {
      const kept = error instanceof ExpiredMethodAction ? this.navigationStore.action(id) : undefined;
      if (!kept) throw error;
      const { result } = kept, context = { ...kept.context, ...this.clientSourceContext(kept.sourceCompanyId) };
      // One record goes back as itself (`res_id`, no domain); the server checks again that the user may read it.
      const input = { type: "ir.actions.act_window", name: result.name, res_model: result.model, target: "current",
        views: result.views.map(view => [view.id ?? false, view.type]),
        ...result.res_id === undefined ? { domain: result.domain } : { res_id: result.res_id }, context: result.context };
      const admitted = actionResult(await this.rpc.call(result.model, "erp_normalize_action_result", { wire_major: 1, result: input, context }));
      if (admitted.kind !== "current_window" || admitted.model !== result.model || admitted.res_id !== result.res_id) throw new ClientError("server", "The related page did not open again as it was opened: reopen it from the page it came from.");
      const entry: MethodAction = { ...kept, result: admitted, context };
      this.methodActions.restore(id, entry);
      return entry;
    }
  }

  /** The trail's last entry when it is the page on screen (its form or its list); a client screen or a service page has
   *  none, and the trail left under one belongs to an older page. */
  private pageCrumb(): BreadcrumbItem | undefined {
    const top = this.breadcrumbs.current;
    return top?.id === (this.record ? crumbId(this.current, this.current.id ?? "new") : crumbId(this.current)) ? top : undefined;
  }

  /** Keeps the trail on screen for the current history entry. */
  private saveTrail(): void {
    this.navigationStore.saveTrail(this.historyIndex, this.breadcrumbs.items);
  }

  /** Opens a trail entry through the normal, server-checked navigation. An entry that cannot be opened again (its record
   *  is gone, the user may no longer open it, the tab no longer holds its related list) drops out of the trail and
   *  `redraw` draws the rest, which stays usable; the failure is still reported. A lost connection drops nothing. */
  private async openCrumb(item: BreadcrumbItem, redraw: () => void): Promise<void> {
    const generation = this.generation;
    try { await this.navigate(item.route, false, false, undefined, { autosave: true, showRefusedPage: false }); }
    catch (error) {
      // Only a refusal (the record is gone, the user may not open it, the related list expired) drops the entry; a lost
      // connection or a server failure may pass, and the entry stays to be tried again. The failed navigation already put
      // the trail back as it was for this page.
      if (generation === this.generation && !this.expired && error instanceof ClientError && (error.kind === "validation" || error.kind === "permission")) {
        const focused = document.activeElement;
        this.breadcrumbs.replace(this.breadcrumbs.items.filter(entry => entry.id !== item.id));
        this.saveTrail();
        redraw();
        // The redraw replaced the crumb that had the focus: it goes to the way back that is left (the error dialog returns it there).
        if (focused instanceof HTMLElement && !focused.isConnected) this.content.querySelector<HTMLElement>(".breadcrumb-back")?.focus();
      }
      throw error;
    }
  }

  private async navigate(route: Route, initial = false, fromHistory = false, invocation?: { caller: RecordState; generation: number }, options?: { autosave?: boolean; target?: "current" | "main" | "new"; showRefusedPage?: boolean; stack?: boolean }): Promise<void> {
    if(this.debug)route={...route,debug:"1"};
    // The page's companies stay in the address: a route that names none (a menu, a breadcrumb, addon code) keeps
    // them, so a reload or a copied link opens the same companies. A history entry is restored as it was recorded.
    if (route.cids === undefined && this.current.cids !== undefined && !fromHistory) route = { ...route, cids: [...this.current.cids] };
    if (invocation) {
      // A completed object call already saved/validated its record. Do not weaken canLeave's
      // global pending guard: only the exact active invocation can consume this handoff once.
      if (invocation !== this.pendingInvocation || invocation.generation !== this.generation || invocation.caller !== this.record ||
          invocation.caller.dirty || invocation.caller.busy || this.busy || this.wizard || !this.actionPending || this.uncertainMethod || this.expired) {
        throw new ClientError("validation", "The action's source is no longer available for navigation.");
      }
      this.pendingInvocation = undefined;
    } else if (!initial && !await this.canLeave(options) || this.expired) return;
    // Staging the destination changes the trail (a list clears it, a form goes on top); a navigation that does not
    // commit puts it back for the page that stays.
    const previousGeneration = this.generation, previousList = this.listContext, previousTrail = [...this.breadcrumbs.items];
    let committed = false;
    let pendingDialog: Extract<ActionResult, { kind: "window" }> | undefined;
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
      stage: {
      const requestedRoute = route;
      let persistedClient: ClientResult | undefined;
      let persistedWindow: Record<string, unknown> | undefined;
      const loadWindow = async (id: number): Promise<Record<string, unknown>> => this.windowAction(id).catch((error: unknown) => {
        throw options?.showRefusedPage !== false && isActionRefusal(error) ? new RefusedAction() : error;
      });
      if (route.action && !route.method_action) {
        const id = rpcId(route.action);
        if (this.isClientRoute(route)) persistedClient = await this.loadClientAction(id);
        else if ([...this.menus.values()].some(menu => menu.action === id) || this.metadata.peek(this.bundleKey(id))) {
          persistedWindow = await loadWindow(id);
        } else {
          // Numeric bookmarks without a matching menu carry no action type. The server resolves their
          // shared action key and returns only the metadata authorized by the concrete action loader.
          const metadataGeneration = this.metadata.generation;
          const bundle = object(await this.rpc.call("ir.actions.act_window", "erp_load_web_action_views",
            { ids: [id], wire_major: 1, options: { mobile: false }, context: this.context, bookmark: true })
            .catch((error: unknown) => {
              throw options?.showRefusedPage !== false && isActionRefusal(error) ? new RefusedAction() : error;
            }));
          if ("result" in bundle) {
            const result = actionResult(bundle);
            if (result.kind !== "client" || result.id !== id) throw new ClientError("server", "Mismatched client action.");
            persistedClient = result;
          } else {
            const admitted = this.admitBundle(bundle, id, metadataGeneration);
            this.metadata.seed(this.bundleKey(id), admitted, metadataGeneration);
            persistedWindow = object(admitted["action"]);
          }
        }
        if (persistedClient && clientActionRedirects.has(persistedClient.tag)) {
          const redirect = clientActionRedirects.get(persistedClient.tag);
          route = await redirect({ route, rpc: this.rpc, context: this.context, signal: this.lifetime.signal });
          if (!route.action || route.action === requestedRoute.action || route.method_action) {
            throw new ClientError("validation", "Invalid destination for a retired client action.");
          }
          persistedWindow = await loadWindow(rpcId(route.action));
          persistedClient = undefined;
        }
      }
      const staging = el("div");
      let nextRecord: RecordState | undefined;
      let nextService: ServiceController | undefined;
      let nextDispose: (() => void) | undefined;
      let nextSettle: (() => void) | undefined;
      let nextRestore: (() => void) | undefined;
      let nextClient: MountedClient | undefined;
      let panelDispose: (() => void) | undefined;

      if (!route.action) {
        this.homeMenuOpen = false;
        staging.append(el("h1", "Apps", "visually-hidden"), this.buildHomeContent());
      } else if (persistedClient || !persistedWindow && this.isClientRoute(route)) {
        this.homeMenuOpen = false;
        this.pendingClient = undefined;
        const mountedClient = await this.mountClientAction(route, staging, generation, stagedDisposers, persistedClient);
        panelDispose = mountedClient.panelDispose;
        nextService = mountedClient.service;
        nextDispose = mountedClient.dispose;
        nextClient = this.pendingClient;
      } else {
        this.homeMenuOpen = false;
        const actionId = rpcId(route.action);
        const entry = route.method_action ? await this.methodAction(route.method_action) : undefined;
        if (entry && entry.origin.action !== route.action) throw new ClientError("validation", "Related action does not belong to this route.");
        const action: Record<string, unknown> = entry
          ? { ...entry.result, wire_major: 1, target: "current", id: rpcId(route.action) }
          : persistedWindow ?? await loadWindow(actionId);
        if (action["wire_major"] !== 1 || action["id"] !== rpcId(route.action)) {
          throw new ClientError("unsupported", "Unsupported or mismatched window action.");
        }
        if (action["target"] === "new") {
          if (entry || route.id || route.method_action) {
            throw new ClientError("unsupported", "Unsupported or mismatched window action.");
          }
          const normalized = await this.normalizeWizardAction(action);
          if (generation !== this.generation || this.expired) return;
          pendingDialog = normalized;
          break stage;
        }
        if (action["target"] !== "current") {
          throw new ClientError("unsupported", "Unsupported or mismatched window action.");
        }
        // `search_default_<name>` keys only choose the search the view opens with; they never travel with its requests.
        const { context, defaults: searchDefaults } = splitSearchDefaults(entry?.context ?? { ...this.context, ...this.actionContext(action) });
        const actionDomain: unknown[] = entry ? [...entry.result.domain] : this.actionDomain(action, context);
        const model = text(action["model"], "action model");
        if (route.model && route.model !== model) throw new ClientError("validation", "URL model does not match the action.");
        const views = actionViews(action);
        const description = describeViews(await this.viewDescriptions(model, views, {}, context, entry ? undefined : actionId), model, this.registry);
        const viewType = route.view_type ?? defaultViewType(views, route.id);
        const view = description.views.get(viewType);
        if (!view) throw new ClientError("unsupported", "The requested view is unavailable.");

        if (route.kanban_scope !== undefined && view.type !== "kanban") {
          const board = description.views.get("kanban");
          const scope = board && kanbanRoot(board).groupScope;
          if (!scope) throw new ClientError("validation", "This action has no scoped kanban.");
          const selected = await readBoardScope(this.rpc, model, scope, rpcId(route.kanban_scope), context);
          if (!selected) throw new ClientError("validation", `This ${scope.label} is no longer available. Open the board to choose another.`);
          actionDomain.push([scope.name, "=", selected.id]);
          if (view.type === "form" && route.id === undefined) context[`default_${scope.name}`] = selected.id;
        }
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
          if (widget.widget === "web.settings") mountedPanel.update({ title: "Settings", model });
          nextService = await serviceControllers.get(widget.widget).create({
            ...this.pageNavigation(generation, route, () => actionTitle),
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
              scopePreferenceKey: `erp.settings.scope.${this.userId}.${actionId}`,
              currentCompanyId: this.currentCompanyId,
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
                  await this.navigate({ action: String(action.action), menu: String(action.menu) }, false, false, undefined, { showRefusedPage: false });
                } else {
                  await this.navigate({ action: String(target.id), ...(target.res_id === null ? {} : { id: String(target.res_id), view_type: "form" as const }) }, false, false, undefined, { showRefusedPage: false });
                }
              }
            }
          });
          const service = nextService;
          nextDispose = () => service.dispose?.();
          stagedDisposers.push(nextDispose);
        } else if (view.type === "list") {
          const actionHelp = typeof (action as Record<string, unknown>)["help"] === "string" ? (action as Record<string, unknown>)["help"] as string : undefined;
          // shortcut: an action without a form view gets no New and no row opening (the reference opens the model's
          // default form, and an editable list adds a row inline); revisit when this client has both.
          const hasForm = views.some(view => view[1] === "form");
          const canCreate = hasForm && view.canCreate !== false && !isFalsy(context["create"]);
          if (canCreate) controls.append(actionButton("New", () => this.run(() => this.navigate({ ...route, id: undefined, view_type: "form" })), "primary"));
          const list = await this.listView(staging, panel, actionTitle, description, view, route, generation, actionHelp, views as [number | null, string][], {baseDomain: actionDomain, context, hasForm, canCreate, searchDefaults});
          nextDispose = list.dispose;
          nextSettle = list.settle;
          nextRestore = list.restore;
          stagedDisposers.push(nextDispose);
        } else if (view.type === "kanban") {
          const hasForm = views.some(view => view[1] === "form");
          const canCreate = hasForm && view.canCreate !== false && !isFalsy(context["create"]);
          const kanban = await this.kanbanView(staging, panel, actionTitle, description, view, route, generation, views as [number | null, string][], {baseDomain: actionDomain, context, hasForm, searchDefaults});
          if (canCreate && kanban.canCreateInScope) controls.append(actionButton("New", () => this.run(() => this.navigate({ ...route, id: undefined, view_type: "form" })), "primary"));
          nextDispose = kanban.dispose;
          nextRestore = kanban.restore;
          stagedDisposers.push(nextDispose);
        } else if (view.type === "pivot") {
          const pivot = await this.pivotView(staging, panel, actionTitle, description, view, route, generation, views as [number | null, string][], {baseDomain: actionDomain, context, searchDefaults});
          nextDispose = pivot.dispose;
          stagedDisposers.push(nextDispose);
        } else if (view.type === "calendar") {
          const calendar = await this.calendarView(staging, panel, actionTitle, description, view, route, generation, views as [number | null, string][], {baseDomain: actionDomain, context, searchDefaults});
          nextDispose = calendar.dispose;
          stagedDisposers.push(nextDispose);
        } else if (view.type === "graph") {
          const graph = await this.graphView(staging, panel, actionTitle, description, view, route, generation, views as [number | null, string][], {baseDomain: actionDomain, context, searchDefaults});
          nextDispose = graph.dispose;
          stagedDisposers.push(nextDispose);
        } else if (view.type === "hierarchy") {
          const hasForm = views.some(view => view[1] === "form");
          const canCreate = hasForm && view.canCreate !== false && !isFalsy(context["create"]);
          if (canCreate) controls.append(actionButton("New", () => this.run(() => this.navigate({ ...route, id: undefined, view_type: "form" })), "primary"));
          const hierarchy = await this.hierarchyView(staging, panel, actionTitle, description, view, route, generation, views as [number | null, string][], {baseDomain: actionDomain, context, hasForm, searchDefaults});
          nextDispose = hierarchy.dispose;
          stagedDisposers.push(nextDispose);
        } else if (view.type === "activity" || view.type === "map") {
          const hasForm = views.some(view => view[1] === "form");
          const canCreate = hasForm && view.canCreate !== false && !isFalsy(context["create"]);
          if (canCreate) controls.append(actionButton("New", () => this.run(() => this.navigate({ ...route, id: undefined, view_type: "form" })), "primary"));
          const activity = await this.activityOrMapView(staging, panel, actionTitle, description, view, route, generation, views as [number | null, string][], {baseDomain: actionDomain, context, hasForm, searchDefaults});
          nextDispose = activity.dispose;
          stagedDisposers.push(nextDispose);
        } else {
          await attachSubviews(description, view, async (relation, refs) =>
            describeViews(await this.viewDescriptions(relation, [[null, "list"], [null, "form"]], {}, refs ? { ...context, ...refs } : context), relation, this.registry));
          const canEdit = view.canEdit !== false && !isFalsy(context["edit"]);
          const formViewMeta = (!canEdit && route.id) ? {
            ...view,
            fields: view.fields.map(f => ({ ...f, readonly: true })),
          } : view;
          nextRecord = new RecordState(this.rpc, model, formViewMeta.fields, context);
          await nextRecord.load(route.id ? rpcId(route.id) : undefined);
          nextDispose = this.formView(staging, controls, panel, actionTitle, nextRecord, formViewMeta, route, views as [number | null, string][], context, options?.stack === true);
          stagedDisposers.push(nextDispose);
        }
      }

      if (generation !== this.generation || this.expired) {
        return;
      }
      this.captureClientState();
      this.clientMounted = nextClient;
      if (nextRecord && routeUrl(listRouteOf(this.current)) === routeUrl(listRouteOf(route))) this.rememberList?.(nextRecord.id);
      for (const dispose of this.disposeWidgets) dispose();
      this.disposeWidgets = [...(nextDispose ? [nextDispose] : []), ...(panelDispose ? [panelDispose] : [])];
      stagedDisposers.length = 0;

      this.record = nextRecord;
      this.service = nextService;
      if (this.listResume && (!nextRecord || this.listResume.url !== routeUrl(listRouteOf(route)))) this.listResume = undefined;
      this.current = route;
      this.shownGeneration = generation;
      committed = true;
      // The mounted record changed (a form loaded, or a list/service left no record open): the
      // systray context must carry the new open-record identity for the assistant's session panel.
      this.syncSystrayRecord();

      this.renderNavigation(route);

      this.content.replaceChildren(...staging.childNodes);
      nextSettle?.();
      this.currentUrl = routeUrl(route);
      if (fromHistory && route !== requestedRoute) history.replaceState(history.state, "", this.currentUrl);
      else if (!fromHistory) {
        if (initial) history.replaceState({ erpIndex: this.historyIndex }, "", this.currentUrl);
        else history.pushState({ erpIndex: ++this.historyIndex }, "", this.currentUrl);
        this.saveTrail();
      }
      this.notice("");
      this.messages.navigation();
      this.messages.warning(this.record?.warningDetails);
      this.content.setAttribute("data-navigation-focus", "");
      // A new page starts at the top instead.
      this.content.focus({ preventScroll: true });
      documentScroller().scrollTop = 0;
      nextRestore?.();
      }
    } catch (error) {
      if (!(error instanceof RefusedAction)) throw error;
      if (generation !== this.generation || this.expired) return;
      this.showRefusedAction(route, initial, fromHistory);
      committed = true;
    } finally {
      for (const dispose of stagedDisposers) dispose();
      if (!committed && generation === this.generation) {
        // The mounted source and its event bindings remain valid when destination staging fails.
        this.generation = previousGeneration;
        this.listContext = previousList;
        this.breadcrumbs.replace(previousTrail);
      }
      finishLoading();
      this.busy = false;
      if (this.navigationInFlight === inFlight) this.navigationInFlight = undefined;
      settle();
    }
    if (pendingDialog && !this.expired && this.generation === previousGeneration) {
      const origin = { ...this.current };
      await this.openWindowDialog(
        pendingDialog,
        undefined,
        previousGeneration,
        async () => {
          if (origin.action) {
            this.captureClientState();
            await this.navigate(origin, true, true);
          }
        },
        undefined,
        this.context,
      );
    }
  }

  /**
   * Commit the refused-action page in place of the previous content: the shell stays, no menu entry is current, and the
   * address becomes the home route (same companies), replacing a first load or a history step so Back does not return to
   * the refusal. Nothing the server said about the action is shown.
   */
  private showRefusedAction(route: Route, initial: boolean, fromHistory: boolean): void {
    const home: Route = { cids: route.cids, debug: route.debug };
    const page = mountRefusedActionPage({
      rpc: this.rpc,
      navigate: next => this.navigate(next),
      goHome: () => this.run(() => this.navigate(home)),
      report: error => this.reportError(error),
    });
    this.captureClientState();
    this.clientMounted = undefined;
    for (const dispose of this.disposeWidgets) dispose();
    this.disposeWidgets = [() => page.dispose()];
    this.record = undefined;
    this.service = undefined;
    this.breadcrumbs.clear();
    this.current = home;
    this.renderNavigation(home);
    this.content.replaceChildren(page.element);
    this.currentUrl = routeUrl(home);
    if (initial || fromHistory) history.replaceState({ erpIndex: this.historyIndex }, "", this.currentUrl);
    else history.pushState({ erpIndex: ++this.historyIndex }, "", this.currentUrl);
    if (!fromHistory) this.saveTrail();
    this.notice("");
    this.messages.navigation();
    this.content.setAttribute("data-navigation-focus", "");
    this.content.focus({ preventScroll: true });
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
    const [nodes, raw] = await Promise.all([
      this.searchViewNodes(model, context),
      this.rpc.call(model, "fields_get", { attributes: ["string", "type", "groupable", "selection", "aggregator"], context })
    ]);
    const decoded = decodePivotFields(raw);
    const catalogue = buildGroupByCatalogue(nodes, this.searchExpression(context),
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

  /** Registers the action list's dashboard cards on a view's own search model, so a card kept in the address (the list wrote
   *  `dashboard_<name>`) narrows this view the same way; the list registers the same ids. A card that cannot be offered now (no strip,
   *  hidden for this user, read failed, a related list) is dropped from the address with a warning instead of refusing the view.
   *  Returns the address parameters to restore the search from. */
  private async restoreDashboardFilters(searchModel: SearchModel, description: Description, route: Route, generation: number, baseDomain: unknown[], context: Record<string, unknown>): Promise<URLSearchParams> {
    const params = new URL(routeUrl(route), window.location.origin).searchParams;
    let asked: unknown;
    try { asked = JSON.parse(params.get("search_filters") ?? "[]"); } catch { return params; } // malformed: the search model refuses it by name
    const cards = Array.isArray(asked) ? asked.filter((id): id is string => typeof id === "string" && id.startsWith("dashboard_")) : [];
    if (!cards.length) return params;
    const list = description.views.get("list"), strip = route.method_action === undefined ? list?.dashboard : undefined;
    const action = route.action === undefined ? undefined : rpcId(route.action);
    if (strip && action !== undefined && list?.id !== undefined) {
      const current = () => generation === this.generation && !this.expired;
      const dashboard = mountDashboard({
        config: strip, current,
        read: (domain, period) => this.rpc.call(description.model, "erp_read_dashboard", { action_id: action, view_id: list.id, revision: strip.revision, period, domain_scope: domain, context }),
        apply: () => undefined, selected: name => cards.includes(`dashboard_${name}`),
      });
      let read: Map<string, unknown[]>;
      try { await dashboard.refresh(baseDomain); read = dashboard.domains(); } finally { dashboard.dispose(); } // dispose clears the domains
      const label = (name: string) => strip.items.find(item => item.name === name)!.label;
      searchModel.addDeclaredFilters([...read].map(([name, domain]) => ({ id: `dashboard_${name}`, description: label(name), domain, isActive: false })));
    }
    const known = new Set(searchModel.getFilters().map(filter => filter.id));
    const dropped = cards.filter(id => !known.has(id));
    if (!dropped.length) return params;
    dropAbsentDashboardFilters(params, known);
    const kept = params.get("search_filters");
    if (kept === null) delete route.search_filters; else route.search_filters = kept;
    if (generation === this.generation && !this.expired) {
      const names = dropped.map(id => strip?.items.find(item => `dashboard_${item.name}` === id)?.label ?? id.slice("dashboard_".length));
      this.messages.notifications.add(`The dashboard card ${names.map(name => `“${name}”`).join(", ")} could not be applied here, so this view shows the records without it.`, { level: "warning" });
    }
    return params;
  }

  /** The pivot view: the search bar's domain, grouped reads narrowed to the header that opens, the table, and its state in the address. */
  private async pivotView(staging: HTMLElement, panel: HTMLElement, actionTitle: string, description: Description, view: View, route: Route, generation: number, availableViews: [number | null, string][], scope: { baseDomain: unknown[]; context: Record<string, unknown>; searchDefaults?: ReadonlyMap<string, unknown> }): Promise<{ dispose: () => void }> {
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
        get route() { return { ...route }; }, get url() { return routeUrl(route); }
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
    await this.prepareSearch(searchModel, description.model, context, route, () => this.restoreDashboardFilters(searchModel, description, route, generation, baseDomain, context), scope.searchDefaults ?? new Map(), generation);
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
        // "Too large" is said only for the two size bounds of the address: its length and the list of expanded headers. Any other refusal keeps
        // its own message (a field named twice on an axis used to be reported as size).
        if (isAddressSizeRefusal(error)) throw new ClientError("validation", groupingRows
          ? "This table is too large to keep in the page address. Choose fewer Group By fields or narrow the search."
          : "This table is too large to keep in the page address. Collapse some headers or show fewer measures first.");
        throw error;
      }
    };
    const pivotSections = await this.sectionsPanelFor(description, view, context, "pivot");
    let pivotSidebar: SearchPanelSidebar | undefined;
    const pivotDomain = (): unknown[] => [...searchModel.getDomain(), ...(pivotSidebar?.getDomain() ?? [])];
    const model = new PivotModel(config, ({ groupingSets, aggregates, domain }) => this.rpc.call(description.model, "formatted_read_grouping_sets", {
      domain, grouping_sets: groupingSets, aggregates, context
    }), pivotDomain, limits, initial, accept);
    if (pivotSections) {
      pivotSidebar = createSearchPanelSidebar({
        model: description.model, panel: pivotSections, rpc: this.rpc, context, selection: route.search_panel,
        onChange: selection => this.run(async () => { this.keepSearchPanel(route, selection); void pivotSidebar?.refresh(searchModel.getDomain()); await model.load(); pivot.render(); }),
        report: error => this.reportError(error),
      });
      void pivotSidebar.refresh(searchModel.getDomain());
    }
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
    const pivot = mountPivot({ model, variant: view.nodes[0]?.kind === "pivot" ? view.nodes[0].variant : undefined, locale: this.numericLocale, title: actionTitle, run: job => this.run(job), openCell, changed: writeAddress,
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
      void pivotSidebar?.refresh(searchModel.getDomain());
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
      if (!switchable(vType) || availableViews.length < 2) continue;
      const vBtn = button(vType.charAt(0).toUpperCase() + vType.slice(1), () => this.run(async () => {
        await this.navigate(vType === "pivot" ? { ...route, id: undefined, view_type: "pivot" } : switchedRoute(route, vType));
      }), `view-btn ${vType === "pivot" ? "active" : ""}`);
      vBtn.setAttribute("aria-label", `${vType} view`);
      if (vType === "pivot") vBtn.setAttribute("aria-current", "page");
      decorateViewButton(vBtn, vType as SwitchableView);
      switcher.append(vBtn);
    }
    const toolsArea = panel.querySelector("[data-control-tools]");
    if (toolsArea) toolsArea.replaceChildren(switcher);
    else (panel.querySelector(".control-panel-heading") || panel).append(el("div", undefined, "control-panel-tools"), switcher);
    this.wrapWithSearchPanel(staging, pivot.element, pivotSidebar);
    return { dispose: () => { pivot.dispose(); pivotSidebar?.dispose(); searchBar.dispose(); } };
  }

  /** The calendar view: the search bar's domain, the events of the visible range, the grids and the side panel, and the scale and day in the address. */
  private async calendarView(staging: HTMLElement, panel: HTMLElement, actionTitle: string, description: Description, view: View, route: Route, generation: number, availableViews: [number | null, string][], scope: { baseDomain: unknown[]; context: Record<string, unknown>; searchDefaults?: ReadonlyMap<string, unknown> }): Promise<{ dispose: () => void }> {
    const { context, baseDomain } = scope;
    if (route.id !== undefined) throw new ClientError("validation", "The calendar view shows a range of records; the address cannot name one record.");
    const config = readCalendarView(view), zone = this.editorLocale.tz;
    const params: CalendarParams = {
      ...(route.calendar_scale === undefined ? {} : { calendar_scale: route.calendar_scale }),
      ...(route.calendar_date === undefined ? {} : { calendar_date: route.calendar_date }),
    };
    const initial = stateFromParams(params, config.root, zone);
    const response = object(await this.rpc.call("ir.config.setting", "erp_read_values", { scope: { kind: "global" }, keys: ["web.calendar_max_events"], context }, { signal: this.lifetime.signal }), "calendar settings");
    if (response["wire_major"] !== 1) throw new ClientError("unsupported", "Unsupported settings response. Reload the page.");
    const found = array(response["values"], "calendar settings values").map(value => object(value, "calendar setting")).filter(value => value["key"] === "web.calendar_max_events");
    const maxEvents = found.length === 1 ? found[0]!["value"] : undefined;
    if (typeof maxEvents !== "number" || !Number.isSafeInteger(maxEvents) || maxEvents < 1) throw new ClientError("server", "Invalid calendar setting web.calendar_max_events.");
    panel.classList.add("control-panel-list");
    const breadcrumbs = panel.querySelector<HTMLElement>("[data-control-breadcrumbs]") || panel.querySelector<HTMLElement>("h1");
    if (breadcrumbs) {
      this.breadcrumbs.clear();
      this.breadcrumbs.push({
        id: `action-${route.action}-calendar`, title: actionTitle, isFormView: false,
        get route() { return { ...route }; }, get url() { return routeUrl(route); }
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
      userId: this.userId, companyId: this.companyId, initialDomain: baseDomain
    });
    // The list's dashboard cards narrow the calendar the same way (the pivot does this too).
    await this.prepareSearch(searchModel, description.model, context, route, () => this.restoreDashboardFilters(searchModel, description, route, generation, baseDomain, context), scope.searchDefaults ?? new Map(), generation);
    if (route.search && !new URL(routeUrl(route), window.location.origin).searchParams.has("search_facets")) {
      // The calendar arch names few fields, so `name` is looked for in the model's fields as well: the list builds this facet on `name`.
      const field = searchModel.getSearchableFields().find(f => f.name === "name") ?? (description.fields.get("name")?.kind === "char" ? description.fields.get("name") : undefined) ?? searchModel.getSearchableFields()[0];
      if (!field) throw new ClientError("unsupported", "Text search is not available for this model.");
      searchModel.addFieldFacet(field.name, route.search);
    }
    const read = { display_name: {}, ...specification(config.readFields) };
    const model = new CalendarModel(config, ({ domain, limit }) => this.rpc.call(description.model, "web_search_read", {
      domain, specification: read, limit, order: `${config.root.dateStart.name} asc, id asc`, context
    }), () => searchModel.getDomain(), { maxEvents }, zone, initial);
    let refused: unknown;
    try { await model.load(); }
    catch (error) { if (!(error instanceof ClientError) || error.kind !== "validation") throw error; refused = error; } // the bound: the view opens and says why it is empty
    const current = (): boolean => generation === this.generation && !this.expired;
    const writeAddress = (): void => {
      if (!current()) return; // the user left this calendar: the address is the next page's
      const proposed = { ...route, ...paramsFromState(model.state) };
      const url = routeUrl(proposed);
      Object.assign(route, paramsFromState(model.state));
      if (url !== this.currentUrl) { history.replaceState({ erpIndex: this.historyIndex }, "", url); this.currentUrl = url; }
    };
    const leave = (): Route => { const next = { ...route }; for (const key of CALENDAR_PARAMS) delete next[key]; return next; };
    const calendar = mountCalendar({
      model, locale: this.editorLocale, popovers: this.messages.popovers, run: job => this.run(job), changed: writeAddress,
      open: async event => {
        if (!availableViews.some(item => item[1] === "form")) throw new ClientError("unsupported", "This action has no form view to open the record in.");
        await this.navigate({ ...leave(), id: String(event.id), view_type: "form" });
      },
      access: async event => {
        const allowed = async (operation: string): Promise<boolean> => {
          const value = await this.rpc.call(description.model, "has_access", { ids: [event.id], operation, context });
          if (typeof value !== "boolean") throw new ClientError("server", "Invalid calendar record access.");
          return value;
        };
        const [edit, remove] = await Promise.all([config.root.edit && allowed("write"), config.root.delete && allowed("unlink")]);
        return { edit, remove };
      },
      update: async (event, vals) => { await this.rpc.call(description.model, "write", { ids: [event.id], vals, context }, { write: true }); },
      create: async (values, name) => {
        const defaults = Object.fromEntries(Object.entries(values).map(([key, value]) => [`default_${key}`, value]));
        if (name) {
          await this.rpc.call(description.model, "name_create", { name, context: { ...context, ...defaults } }, { write: true });
          return;
        }
        const form = availableViews.find(item => item[1] === "form");
        if (!form) throw new ClientError("unsupported", "This action has no form for creating an event.");
        const list = availableViews.find(item => item[1] === "list");
        const result = actionResult({ wire_major: 1, result: { kind: "current_window", name: actionTitle, model: description.model, domain: baseDomain, context: {},
          views: [{ id: list?.[0] ?? null, type: "list" }, { id: form[0], type: "form" }] } });
        if (result.kind !== "current_window") throw new ClientError("server", "Invalid calendar create action.");
        const id = this.methodActions.add({ result, context: { ...context, ...defaults }, origin: { ...route }, originLabel: actionTitle });
        try { await this.navigate({ action: route.action, menu: route.menu, cids: route.cids, method_action: id, model: description.model, view_type: "form" }); }
        finally { if (this.current.method_action !== id) this.methodActions.remove(id); }
      },
      remove: async event => {
        if (await this.choose("Delete this event?", `“${event.title}” and its record are removed for good.`, ["Delete", "Cancel"]) !== "Delete") return false;
        await this.rpc.call(description.model, "unlink", { ids: [event.id], context }, { write: true });
        return true;
      },
      failed: refused === undefined ? undefined : (refused as ClientError).message,
    });

    let request = 0; // the same stale-answer guard the list uses: request number, route generation, expiry
    const searchBar = createSearchBar(searchModel, () => this.run(async () => {
      const mine = ++request;
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
      await calendar.reload(() => mine === request && current());
    }));
    const searchArea = panel.querySelector("[data-control-search]") || panel;
    if (searchArea !== panel) searchArea.replaceChildren(searchBar.element); else panel.append(searchBar.element);

    const switcher = el("div", undefined, "view-switcher");
    for (const [, vType] of availableViews) {
      if (!switchable(vType) || availableViews.length < 2) continue;
      const vBtn = button(vType.charAt(0).toUpperCase() + vType.slice(1), () => this.run(async () => {
        await this.navigate(vType === "calendar" ? { ...route, id: undefined, view_type: "calendar" } : switchedRoute(route, vType));
      }), `view-btn ${vType === "calendar" ? "active" : ""}`);
      vBtn.setAttribute("aria-label", `${vType} view`);
      if (vType === "calendar") vBtn.setAttribute("aria-current", "page");
      decorateViewButton(vBtn, vType);
      switcher.append(vBtn);
    }
    const toolsArea = panel.querySelector("[data-control-tools]");
    if (toolsArea) toolsArea.replaceChildren(switcher);
    else (panel.querySelector(".control-panel-heading") || panel).append(el("div", undefined, "control-panel-tools"), switcher);
    staging.append(calendar.element);
    return { dispose: () => { calendar.dispose(); searchBar.dispose(); } };
  }

  /** The graph view: the search bar's domain and Group By (two levels), one grouped read per change of what is drawn, the chart, and its state in the address. */
  private async graphView(staging: HTMLElement, panel: HTMLElement, actionTitle: string, description: Description, view: View, route: Route, generation: number, availableViews: [number | null, string][], scope: { baseDomain: unknown[]; context: Record<string, unknown>; searchDefaults?: ReadonlyMap<string, unknown> }): Promise<{ dispose: () => void }> {
    const { context, baseDomain } = scope;
    if (route.id !== undefined) throw new ClientError("validation", "The graph view draws a whole chart; the address cannot name one record.");
    const [settings, catalogue] = await Promise.all([
      this.rpc.call("ir.config.setting", "erp_read_values", { scope: { kind: "global" }, keys: ["web.graph_max_groups"], context }, { signal: this.lifetime.signal }),
      this.pivotCatalogue(description.model, context)
    ]);
    const response = object(settings, "graph settings");
    if (response["wire_major"] !== 1) throw new ClientError("unsupported", "Unsupported settings response. Reload the page.");
    const values = array(response["values"], "graph settings values").map(value => object(value, "graph setting"));
    const maxGroups = values.length === 1 && values[0]!["key"] === "web.graph_max_groups" ? values[0]!["value"] : undefined;
    if (typeof maxGroups !== "number" || !Number.isSafeInteger(maxGroups) || maxGroups < 1) throw new ClientError("server", "Invalid graph setting web.graph_max_groups.");
    const config = readGraphView(view, catalogue);
    const params: GraphParams = {};
    for (const key of GRAPH_PARAMS) if (route[key] !== undefined) params[key] = route[key];
    const initial = graphParamsState(params, config);
    panel.classList.add("control-panel-list");
    const breadcrumbs = panel.querySelector<HTMLElement>("[data-control-breadcrumbs]") || panel.querySelector<HTMLElement>("h1");
    if (breadcrumbs) {
      this.breadcrumbs.clear();
      this.breadcrumbs.push({
        id: `action-${route.action}-graph`, title: actionTitle, isFormView: false,
        get route() { return { ...route }; }, get url() { return routeUrl(route); }
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
      // The search's Group By sets the axis, then the series (reference graph_model.js), with date intervals.
      groupBy: { load: async () => catalogue.groupBy, multiple: true, maxLevels: GRAPH_MAX_GROUPINGS }
    });
    // The list's dashboard cards kept in the address narrow the chart the same way.
    await this.prepareSearch(searchModel, description.model, context, route, () => this.restoreDashboardFilters(searchModel, description, route, generation, baseDomain, context), scope.searchDefaults ?? new Map(), generation);
    if (route.search && !new URL(routeUrl(route), window.location.origin).searchParams.has("search_facets")) {
      const field = searchModel.getSearchableFields().find(f => f.name === "name") ?? (description.fields.get("name")?.kind === "char" ? description.fields.get("name") : undefined) ?? searchModel.getSearchableFields()[0];
      if (!field) throw new ClientError("unsupported", "Text search is not available for this model.");
      searchModel.addFieldFacet(field.name, route.search);
    }
    const graphSections = await this.sectionsPanelFor(description, view, context, "graph");
    let graphSidebar: SearchPanelSidebar | undefined;
    const graphDomain = (): unknown[] => [...searchModel.getDomain(), ...(graphSidebar?.getDomain() ?? [])];
    const model = new GraphModel(config, ({ groupingSets, aggregates, domain }) => this.rpc.call(description.model, "formatted_read_grouping_sets", {
      domain, grouping_sets: groupingSets, aggregates, context
    }), graphDomain, maxGroups, initial);
    if (graphSections) {
      graphSidebar = createSearchPanelSidebar({
        model: description.model, panel: graphSections, rpc: this.rpc, context, selection: route.search_panel,
        onChange: selection => this.run(async () => { this.keepSearchPanel(route, selection); void graphSidebar?.refresh(searchModel.getDomain()); await model.load(); graph.render(); }),
        report: error => this.reportError(error),
      });
      void graphSidebar.refresh(searchModel.getDomain());
    }
    await model.load();
    // Only what differs from the arch is kept, so a reload (or a link) draws the same chart.
    const writeAddress = (): void => {
      if (generation !== this.generation || this.expired) return; // the user left this graph: the address is the next page's
      const proposed: Route = { ...route };
      for (const key of GRAPH_PARAMS) delete proposed[key];
      Object.assign(proposed, graphParams(model.current, config));
      const url = routeUrl(proposed);
      for (const key of GRAPH_PARAMS) { if (proposed[key] === undefined) delete route[key]; else route[key] = proposed[key]; }
      if (url !== this.currentUrl) { history.replaceState({ erpIndex: this.historyIndex }, "", url); this.currentUrl = url; }
    };
    // A bar or slice opens the list of its records; the way back is this graph's address, so the chart comes back as it was left.
    const open = async (target: GraphTarget): Promise<void> => {
      const list = availableViews.find(item => item[1] === "list"), form = availableViews.find(item => item[1] === "form");
      const name = target.labels.join(" / ").slice(0, 200) || actionTitle;
      const result = actionResult({ wire_major: 1, result: { kind: "current_window", name, model: description.model, domain: target.domain, context: {},
        views: [{ id: list?.[0] ?? null, type: "list" }, ...(form ? [{ id: form[0], type: "form" }] : [])] } });
      if (result.kind !== "current_window") throw new ClientError("server", "The chart did not open its records as a list.");
      const listContext = Object.fromEntries(Object.entries(context).filter(([key]) => !key.startsWith("graph_")));
      const id = this.methodActions.add({ result, context: listContext, origin: { ...route }, originLabel: actionTitle });
      try { await this.navigate({ action: route.action, menu: route.menu, cids: route.cids, method_action: id, model: description.model, view_type: "list" }); }
      catch (error) { this.methodActions.remove(id); throw error; }
    };
    const graph = mountGraph({ model, download: blob => saveFile(blob, `${description.model}-chart.svg`), locale: this.numericLocale, run: job => this.run(job), open, changed: writeAddress });

    let request = 0; // the list's stale-answer guard: request number, route generation, expiry
    let appliedGroupBy = ""; // the search's groupings the chart follows; empty: the arch's (or the address's)
    const searchBar = createSearchBar(searchModel, () => this.run(async () => {
      const mine = ++request, current = (): boolean => mine === request && generation === this.generation && !this.expired;
      const parameters = new URLSearchParams();
      searchModel.serializeToUrlParams(parameters);
      const proposed = { ...route };
      for (const key of ["search", "search_facets", "search_filters", "search_groupby"] as const) {
        const value = parameters.get(key);
        if (value === null) delete proposed[key]; else proposed[key] = value;
      }
      const url = routeUrl(proposed);
      for (const key of ["search", "search_facets", "search_filters", "search_groupby"] as const) { if (proposed[key] === undefined) delete route[key]; else route[key] = proposed[key]; }
      if (url !== this.currentUrl) { history.replaceState({ erpIndex: this.historyIndex }, "", url); this.currentUrl = url; }
      const wanted = searchModel.getGroupBy();
      void graphSidebar?.refresh(searchModel.getDomain());
      try {
        if (wanted.join(",") !== appliedGroupBy) {
          try { await model.setGroupBy(wanted.length ? wanted.map(spec => graphGrouping(spec, config.groupable)) : null); }
          catch (error) {
            // The chart did not change: the facet would say otherwise, so it goes, and the refusal says so.
            if (!(error instanceof ClientError) || error.kind !== "validation") throw error;
            const facet = searchModel.getFacets().find(item => item.type === "groupBy");
            if (facet) searchModel.removeFacet(facet.id);
            appliedGroupBy = "";
            throw new ClientError("validation", `Group By removed: ${error.message}`);
          }
          appliedGroupBy = wanted.join(",");
        } else await model.load();
      } finally { if (current()) graph.render(); } // a failed read draws the chart as unavailable
      if (current()) writeAddress();
    }));
    const searchArea = panel.querySelector("[data-control-search]") || panel;
    if (searchArea !== panel) searchArea.replaceChildren(searchBar.element); else panel.append(searchBar.element);

    const switcher = el("div", undefined, "view-switcher");
    for (const [, vType] of availableViews) {
      if (!switchable(vType) || availableViews.length < 2) continue;
      const vBtn = button(vType.charAt(0).toUpperCase() + vType.slice(1), () => this.run(async () => {
        await this.navigate(vType === "graph" ? { ...route, id: undefined, view_type: "graph" } : switchedRoute(route, vType));
      }), `view-btn ${vType === "graph" ? "active" : ""}`);
      vBtn.setAttribute("aria-label", `${vType} view`);
      if (vType === "graph") vBtn.setAttribute("aria-current", "page");
      decorateViewButton(vBtn, vType as SwitchableView);
      switcher.append(vBtn);
    }
    const toolsArea = panel.querySelector("[data-control-tools]");
    if (toolsArea) toolsArea.replaceChildren(switcher);
    else (panel.querySelector(".control-panel-heading") || panel).append(el("div", undefined, "control-panel-tools"), switcher);
    this.wrapWithSearchPanel(staging, graph.element, graphSidebar);
    return { dispose: () => { graph.dispose(); graphSidebar?.dispose(); searchBar.dispose(); } };
  }

  /** The bounds of the kanban board, from Settings (`web.kanban_*`): each has a default, a floor and a hard bound declared by the web addon. */
  private async kanbanLimits(context: Record<string, unknown>): Promise<{ maxColumns: number; maxCards: number }> {
    const keys = ["web.kanban_max_columns", "web.kanban_max_column_cards"];
    const response = object(await this.rpc.call("ir.config.setting", "erp_read_values", { scope: { kind: "global" }, keys, context }, { signal: this.lifetime.signal }), "kanban settings");
    if (response["wire_major"] !== 1) throw new ClientError("unsupported", "Unsupported settings response. Reload the page.");
    const values = array(response["values"], "kanban settings values").map(value => object(value, "kanban setting"));
    // The hard bound of each declaration (addons/web/src/kanban_settings.rs): a value past it is refused here too, never used.
    const read = (key: string, hardBound: number): number => {
      const found = values.filter(value => value["key"] === key);
      const value = found.length === 1 ? found[0]!["value"] : undefined;
      if (typeof value !== "number" || !Number.isSafeInteger(value) || value < 1 || value > hardBound) throw new ClientError("server", `Invalid kanban setting ${key}.`);
      return value;
    };
    return { maxColumns: read(keys[0]!, 1000), maxCards: read(keys[1]!, 4000) };
  }

  /** The kanban view: the search bar's domain and either the ungrouped card grid with its pager (M4.KANBAN-CLIENT-CARDS) or, when the
   *  search groups or the arch names a default grouping, the board of columns (M4.KANBAN-CLIENT-COLUMNS: one grouped read, fold and
   *  unfold, load more per column, the folded columns in the address). Both keep the way back to the card that was opened.
   *  Progress bar, quick create, the column menu and drag and drop are M4.KANBAN-CLIENT-COLUMNS.PROGRESS, .QUICK-CREATE, .MENU and M4.KANBAN-CLIENT-DRAG. */
  private async kanbanView(staging: HTMLElement, panel: HTMLElement, actionTitle: string, description: Description, view: View, route: Route, generation: number, availableViews: [number | null, string][], scope: { baseDomain: unknown[]; context: Record<string, unknown>; hasForm: boolean; searchDefaults?: ReadonlyMap<string, unknown> }): Promise<{ dispose: () => void; restore: () => void; canCreateInScope: boolean }> {
    const { context, baseDomain, hasForm } = scope;
    if (route.id !== undefined) throw new ClientError("validation", "The kanban view shows many records; the address cannot name one record.");
    const cardNavigation = this.pageNavigation(generation, route, () => actionTitle);
    const root = kanbanRoot(view), layout = kanbanLayout(view);
    const scopeField = root.groupScope;
    if (route.kanban_scope !== undefined && !scopeField) throw new ClientError("validation", "This action has no scoped kanban.");
    const selected = scopeField ? await readBoardScope(this.rpc, description.model, scopeField, route.kanban_scope ? rpcId(route.kanban_scope) : undefined, context) : undefined;
    if (selected && scopeField) {
      route.kanban_scope = String(selected.id);
      context[`default_${scopeField.name}`] = selected.id;
    }
    const scopedDomain = [...baseDomain, ...(scopeField ? [selected ? [scopeField.name, "=", selected.id] : ["id", "=", 0]] : [])];
    const resume = this.kanbanResume?.url === routeUrl(route) ? this.kanbanResume : undefined;
    this.kanbanResume = undefined;
    const pageSize = view.pageLimit ?? 40;
    let offset = resume?.offset ?? 0, request = 0, total = 0;
    panel.classList.add("control-panel-list");
    const breadcrumbs = panel.querySelector<HTMLElement>("[data-control-breadcrumbs]") || panel.querySelector<HTMLElement>("h1");
    if (breadcrumbs) {
      this.breadcrumbs.clear();
      this.breadcrumbs.push({
        id: `action-${route.action}-kanban`, title: actionTitle, isFormView: false,
        get route() { return { ...route }; }, get url() { return routeUrl(route); },
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
      userId: this.userId, companyId: this.companyId, initialDomain: scopedDomain,
      // The Group By menu of the list; the board draws the first grouping (the reference's kanban does the same).
      groupBy: { load: () => this.groupByCatalogue(description.model, context), multiple: true, maxLevels: 32, persist: true, accept: params => {
        const proposed = { ...route };
        for (const key of ["search", "search_facets", "search_filters", "search_groupby"] as const) { const value = params.get(key); if (value === null) delete proposed[key]; else proposed[key] = value; }
        try { routeUrl(proposed); }
        catch (error) {
          if (error instanceof ClientError && error.kind === "validation") throw new ClientError("validation", "This search is too large to keep in the page address. Remove a filter or a Group By field first.");
          throw error;
        }
      } }
    });
    if (route.search_groupby || root.defaultGroupBy) await searchModel.loadGroupByCatalogue();
    await this.prepareSearch(searchModel, description.model, context, route, () => this.restoreDashboardFilters(searchModel, description, route, generation, scopedDomain, context), scope.searchDefaults ?? new Map(), generation);
    searchModel.clearListState(); // a list's open groups and paging are not the board's
    if (route.search && !new URL(routeUrl(route), window.location.origin).searchParams.has("search_facets")) {
      const field = searchModel.getSearchableFields().find(f => f.name === "name") ?? searchModel.getSearchableFields()[0];
      if (!field) throw new ClientError("unsupported", "Text search is not available for this model.");
      searchModel.addFieldFacet(field.name, route.search);
    }
    // The arch's default grouping applies whenever the search names none; a date is grouped by month (the reference's default interval).
    const defaultSpec = root.defaultGroupBy ? (root.defaultGroupBy.kind === "date" || root.defaultGroupBy.kind === "datetime" ? `${root.defaultGroupBy.name}:month` : root.defaultGroupBy.name) : undefined;
    const grouping = (): { spec: string; field: GroupableField } | undefined => {
      const spec = searchModel.getGroupBy()[0] ?? defaultSpec;
      if (spec === undefined) return undefined;
      const field = searchModel.getGroupableField(spec.split(":")[0]!);
      if (!field) throw new ClientError("validation", `Cannot group by ${spec}.`);
      return { spec, field };
    };

    const sectionsPanel = await this.sectionsPanelFor(description, view, context, "kanban");
    let sidebar: SearchPanelSidebar | undefined;
    const combinedDomain = (): unknown[] => [...searchModel.getDomain(), ...(sidebar?.getDomain() ?? [])];

    const results = el("div", undefined, "kanban-view");
    const body = el("div", undefined, "kanban-results");
    if (scopeField) {
      const picker = button(selected?.display_name ?? `Choose ${scopeField.label}`, () => {
        const dialog = selectCreateDialog({ dialogs: this.messages.dialogs, rpc: this.rpc, model: scopeField.relation!, domain: [], context,
          title: `Choose ${scopeField.label}`, signal: this.lifetime.signal,
          select: choice => {
            if (choice.id === selected?.id || generation !== this.generation) return;
            dialog.close();
            this.run(async () => {
              await this.navigate({ ...route, kanban_scope: String(choice.id), kanban_fold: undefined });
              if (this.current.kanban_scope === String(choice.id)) this.content.querySelector<HTMLElement>(".kanban-scope button")?.focus();
            });
          },
        });
      }, "erp-button");
      picker.setAttribute("aria-label", selected ? `${scopeField.label}: ${selected.display_name}. Change ${scopeField.label}` : `Choose ${scopeField.label}`);
      picker.setAttribute("aria-haspopup", "dialog");
      const toolbar = el("div", undefined, "kanban-scope");
      toolbar.append(el("span", scopeField.label), picker);
      results.append(toolbar);
    }
    results.append(body);
    const currencies = new CurrencyDisplayCache();
    const readonlyFields = view.fields.map(field => ({ ...field, readonly: true }));
    let cards: { node: HTMLElement; dispose: () => void } | undefined;
    const pager = el("div", undefined, "pager");
    const count = el("span", "", "pager-count muted");
    const previous = actionButton("Previous", () => this.run(async () => { offset = Math.max(0, offset - pageSize); await load(); }), "pager-btn pager-prev");
    const next = actionButton("Next", () => this.run(async () => { offset += pageSize; await load(); }), "pager-btn pager-next");
    previous.replaceChildren(icon("chevron-left")); previous.setAttribute("aria-label", "Previous page"); previous.title = "Previous page";
    next.replaceChildren(icon("chevron-right")); next.setAttribute("aria-label", "Next page"); next.title = "Next page";
    pager.append(count, previous, next);

    // The board's state. A column is the drawn state plus what its reads need; `fold` is what the user changed, for one grouping.
    type BoardColumn = KanbanColumnState & { domain: unknown[]; value: KanbanColumnKey; unfiltered: number; sum?: number; segment?: string | null };
    const progress = root.children.find(child => child.kind === "kanban_progressbar");
    const sumField = progress?.sumField;
    const sum: ListAggregate | undefined = sumField && (sumField.kind === "integer" || sumField.kind === "float" || sumField.kind === "monetary")
      ? {field: sumField.name, kind: sumField.kind, op: "sum"} : undefined;
    const segmentDomain = (column: BoardColumn): unknown[] => {
      if (!progress || column.segment === undefined) return [];
      return column.segment === null ? ["!", [progress.field.name, "in", progress.colors.map(item => item.value)]] : [[progress.field.name, "=", column.segment]];
    };
    let progressCounts: Record<string, unknown> = {};
    let mutationPending = false;
    let board: KanbanBoard | undefined, columns: BoardColumn[] = [], boardSpec: string | undefined, boardDomain: unknown[] = [];
    let limits: { maxColumns: number; maxCards: number } | undefined;
    let fold: KanbanFoldState | undefined = route.kanban_fold === undefined ? undefined : decodeKanbanFold(route.kanban_fold);
    // A state of another grouping (a shared link whose Group By was changed) is dropped; `navigate` commits the address from `route`.
    if (fold && fold.by !== grouping()?.spec) { fold = undefined; delete route.kanban_fold; }
    let resumeLoaded = resume?.board ? new Map(resume.board.loaded) : undefined;
    const recordCount = (value: number): string => value === 1 ? "1 record" : `${value} records`;
    const clearBoard = (): void => { board?.dispose(); board = undefined; columns = []; boardSpec = undefined; };
    const unavailable = (sentence: string, label: string): void => {
      body.removeAttribute("aria-busy");
      count.textContent = label; previous.disabled = true; next.disabled = true;
      if (results.isConnected) this.listContext = undefined;
      cards?.dispose(); cards = undefined; clearBoard();
      const message = el("p", sentence, "list-load-error");
      message.setAttribute("role", "alert"); body.replaceChildren(message);
    };

    const fetchRows = async (from: number): Promise<{ records: Record<string, unknown>[]; length: number }> => {
      forgetBinaryContent(this.rpc);
      const answer = object(await this.rpc.call(description.model, "web_search_read", { domain: combinedDomain(), specification: specification(view.fields), offset: from, limit: pageSize, context: { ...context, ...BIN_SIZE_CONTEXT } }));
      forgetBinaryContent(this.rpc);
      return { records: array(answer["records"], "records").map(row => object(row)), length: integer(answer["length"], "record count") };
    };
    const open = hasForm && root.canOpen ? (id: number): void => this.run(async () => {
      this.kanbanResume = { url: routeUrl(route), offset, scroll: documentScroller().scrollTop, id, ...(board ? { board: {
        left: board.node.scrollLeft, top: board.node.scrollTop, loaded: columns.flatMap(column => column.folded ? [] : [[column.value, column.records.length] as [KanbanColumnKey, number]]),
      } } : {}) };
      await this.navigate({ ...route, id: String(id), view_type: "form" });
    }) : undefined;
    const renderField = (field: Field, record: Record<string, unknown>): { node: HTMLElement; dispose: () => void } => {
      const adapter = resolveFieldWidget(field.widget ?? field.kind, "card");
      return adapter.render({
        field: { ...field, readonly: true }, value: record[field.name], values: record, model: description.model, fields: readonlyFields, viewContext: "list",
        rpc: this.rpc, context, currencies, numericLocale: this.numericLocale, editorLocale: this.editorLocale,
        messages: this.messages, run: this.run, report: error => this.reportError(error), signal: this.lifetime.signal,
        openRecord: (model, related) => this.openRelatedRecord(model, related), change: () => { throw new ClientError("validation", "Kanban card fields cannot be edited here."); },
        ...cardNavigation,
      });
    };
    const noRecords = (): HTMLElement => {
      const empty = noContentHelp();
      empty.replaceChildren(...(searchModel.getFacets().some(facet => facet.type !== "groupBy") ? [el("h2", "No matching records"), el("p", "Try another search or clear the filter.")] : [el("h2", "No records yet"), el("p", hasForm ? "Create a record to begin working here." : "Records appear here once they exist.")]));
      return empty;
    };

    // ---- the board (grouped) ----
    /** The form's pager steps through the cards the board shows, column by column; a folded column's cards are not in it. */
    const boardChanged = (): void => {
      const ids = columns.flatMap(column => column.folded ? [] : column.records.map(row => integer(row["id"], "record id")));
      this.listContext = { ids, total: ids.length, offset: 0, limit: Math.max(ids.length, 1) };
      count.textContent = recordCount(columns.reduce((sum, column) => sum + column.unfiltered, 0));
    };
    const writeFold = (state: KanbanFoldState): void => {
      const value = encodeKanbanFold(state), proposed = { ...route };
      if (value === undefined) delete proposed.kanban_fold; else proposed.kanban_fold = value;
      let url: string;
      try { url = routeUrl(proposed); } // validated whole before anything is committed
      catch (error) {
        if (isAddressSizeRefusal(error)) throw new ClientError("validation", "Too many columns are folded or opened to keep in the page address with this search. Remove a filter first.");
        throw error;
      }
      if (value === undefined) delete route.kanban_fold; else route.kanban_fold = value;
      if (url !== this.currentUrl && results.isConnected) { history.replaceState({ erpIndex: this.historyIndex }, "", url); this.currentUrl = url; }
      fold = state;
    };
    /** Reads the next step of one column's cards. A failure belongs to the column: it says so there and the others stay as they are. */
    const readColumn = async (column: BoardColumn): Promise<void> => {
      const mine = request, shown = board, bound = limits;
      if (!shown || !bound || column.busy) return;
      const current = (): boolean => mine === request && shown === board && generation === this.generation && !this.expired;
      const want = Math.min(pageSize, bound.maxCards - column.records.length);
      if (want <= 0) return;
      column.busy = true; delete column.error; shown.refresh(column.key);
      try {
        const answer = object(await this.rpc.call(description.model, "web_search_read", { domain: [...boardDomain, ...column.domain, ...segmentDomain(column)], specification: specification(view.fields), offset: column.records.length, limit: want, context: { ...context, ...BIN_SIZE_CONTEXT } }));
        if (!current()) return;
        const have = new Set(column.records.map(row => row["id"]));
        // A card that moved while the user read (a record changed its place in the order) is not drawn twice.
        column.records.push(...array(answer["records"], "records").map(row => object(row)).filter(row => !have.has(integer(row["id"], "record id"))));
        column.count = integer(answer["length"], "record count");
      } catch (error) {
        if (!current()) return;
        column.error = "Unable to load these records.";
        throw error;
      } finally {
        if (current()) { column.busy = false; shown.refresh(column.key); boardChanged(); }
      }
    };
    const toggleColumn = async (column: BoardColumn): Promise<void> => {
      // The address is written first: a fold it cannot keep is refused before the board changes.
      writeFold(withKanbanColumn(fold ?? { by: boardSpec!, folded: [], opened: [] }, column.value, !column.folded));
      column.folded = !column.folded;
      delete column.error;
      if (!column.folded && !column.records.length && column.count > 0) { await readColumn(column); return; }
      board?.refresh(column.key); boardChanged();
    };
    const loadBoard = async (group: { spec: string; field: GroupableField }, current: () => boolean): Promise<void> => {
      body.setAttribute("aria-busy", "true");
      // What each open column showed: kept across a reload of the same grouping, and on the way back from a record.
      const loaded = resumeLoaded ?? new Map<KanbanColumnKey, number>(boardSpec === group.spec ? columns.flatMap(column => column.folded ? [] : [[column.value, column.records.length] as [KanbanColumnKey, number]]) : []);
      resumeLoaded = undefined;
      const domain = combinedDomain();
      let answer: ReturnType<typeof decodeKanbanGroups>, bound: { maxColumns: number; maxCards: number };
      try {
        const settings = limits ??= await this.kanbanLimits(context);
        bound = settings;
        const step = Math.min(pageSize, settings.maxCards);
        const info = kanbanOpeningInfo(fold, step, new Map<KanbanColumnKey, number>([...loaded].map(([value, shown]): [KanbanColumnKey, number] => [value, Math.min(shown, settings.maxCards)])));
        const scoped = selected && scopeField && group.spec === root.defaultGroupBy?.name
          ? await readCatalogue(this.rpc, root.defaultGroupBy.relation!, [[scopeField.name, "=", selected.id]], context, settings.maxColumns)
          : undefined;
        if (!current()) return;
        if (scoped && scoped.length > settings.maxColumns) {
          unavailable(`This ${scopeField!.label} has ${scoped.length} columns. The board limit is ${settings.maxColumns}.`, "Too many columns");
          return;
        }
        if (scoped) domain.push([group.field.name, "in", scoped.choices.map(choice => choice.id)]);
        answer = decodeKanbanGroups(await this.rpc.call(description.model, "web_read_group", {
          domain, groupby: [group.spec], aggregates: sum ? ["__count", `${sum.field}:sum`] : ["__count"], limit: settings.maxColumns, auto_unfold: true, ...(info ? { opening_info: info } : {}),
          unfold_read_specification: specification(view.fields), unfold_read_default_limit: step, context: { ...context, ...BIN_SIZE_CONTEXT }
        }), group.field, group.spec, settings.maxColumns, settings.maxCards, sum);
        if (scoped) {
          answer = { columns: expandScopedColumns(group.field.name, scoped.choices, answer.columns, fold?.folded ?? []), length: scoped.length };
        }
      } catch (error) {
        if (!current()) return;
        unavailable("Unable to load these records. Adjust the search or reload the page.", "Results unavailable");
        throw error;
      }
      if (!current()) return;
      body.removeAttribute("aria-busy");
      pager.replaceChildren(count); // a board has no pages: Previous and Next leave the pager, the count stays
      // More columns than the bound: said by name, never drawn in part (the search bar stays to narrow or regroup).
      if (answer.length > bound.maxColumns) {
        unavailable(`This grouping has ${answer.length} columns; a board shows at most ${bound.maxColumns} (Settings, "Kanban view: most columns a board may show"). Narrow the search or group by another field.`, "Too many columns");
        return;
      }
      if (progress && answer.columns.length) {
        progressCounts = object(await this.rpc.call(description.model, "read_progress_bar", {
          domain, group_by: group.spec, progress_bar: { field: progress.field.name, colors: Object.fromEntries(progress.colors.map(item => [item.value, item.tone])) }, context
        }), "kanban progress counts");
        if (!current()) return;
      }
      cards?.dispose(); cards = undefined; clearBoard();
      boardSpec = group.spec; boardDomain = domain;
      columns = answer.columns.map(column => ({ key: column.key, label: column.label, count: column.count, unfiltered: column.count, sum: column.sum, domain: column.domain, value: column.value, folded: column.records === undefined, records: [...column.records ?? []], busy: false }));
      if (!columns.length) { boardChanged(); body.replaceChildren(noRecords()); return; }
      const byKey = (key: string): BoardColumn => {
        const found = columns.find(column => column.key === key);
        if (!found) throw new ClientError("validation", "This kanban column is no longer on the board.");
        return found;
      };
      const field = description.fields.get(group.field.name);
      const assignable = field && !field.readonly && field.kind !== "datetime" && field.kind !== "date";
      const headerControls = (state: KanbanColumnState): HTMLElement => {
        const column = byKey(state.key);
        const controls = el("div", undefined, "kanban-column-controls");
        if (progress) {
          const lookup = typeof column.value === "boolean" ? (column.value ? "True" : "False") : String(column.value);
          const counts = Object.hasOwn(progressCounts, lookup) ? object(progressCounts[lookup], "column progress counts") : {};
          const segments: {value: string | null; label: string; n: number; tone: string}[] = progress.colors.map(item => ({
            ...item,
            label: progress.field.selection?.find(([key]) => key === item.value)?.[1] ?? item.value,
            n: integer(counts[item.value] ?? 0, "progress count"),
          }));
          const other = column.unfiltered - segments.reduce((total, segment) => total + segment.n, 0);
          // The grouped and progress reads take separate snapshots; concurrent writes can change their counts.
          if (other > 0) segments.push({value: null, label: "Other", n: other, tone: "muted"});
          if (progress.help) controls.title = progress.help;
          if (sumField && column.sum !== undefined) controls.append(el("span", `${sumField.label}: ${column.sum.toLocaleString(this.editorLocale.lang.replaceAll("_", "-"))}`));
          for (const item of segments) {
            const { label, n } = item;
            const segment = button(`${label} (${n})`, () => this.run(async () => {
              if (column.busy) return;
              column.segment = column.segment === item.value ? undefined : item.value;
              column.records = [];
              await readColumn(column);
            }), `kanban-segment tone-${item.tone}`);
            segment.disabled = column.busy || (n === 0 && column.segment !== item.value);
            segment.setAttribute("aria-pressed", String(column.segment === item.value));
            controls.append(segment);
          }
        }
        if (field?.relation && typeof column.value === "number" && field.kind === "many2one") {
          const id = column.value, relation = field.relation;
          controls.append(button("Open column", () => this.run(() => this.openRelatedRecord(relation, id)), "erp-button"));
        }
        const active = description.fields.get("active");
        if (root.archivable && active?.kind === "boolean" && !active.readonly && view.canEdit !== false && context["edit"] !== false) {
          controls.append(button("Archive column records", () => this.run(async () => {
            if (mutationPending) return;
            mutationPending = true;
            try {
              const reply = object(await this.rpc.call(description.model, "web_search_read", {
                domain: [...boardDomain, ...column.domain, ...segmentDomain(column), ["active", "=", true]],
                specification: {}, limit: bound.maxCards, context,
              }));
              const ids = array(reply["records"], "column records").map(row => integer(object(row)["id"], "record id"));
              if (integer(reply["length"], "column count") > ids.length) throw new ClientError("validation", "Narrow this column before archiving its records.");
              if (!ids.length || !current()) return;
              if (await this.rpc.call(description.model, "has_access", { ids, operation: "write", context }) !== true) throw new ClientError("permission", "You cannot archive all records in this column.");
              if (await this.choose("Archive column records?", `Archive ${recordCount(ids.length)} in ${column.label}?`, ["Archive", "Cancel"]) !== "Archive") return;
              await this.rpc.call(description.model, "action_archive", { ids, context }, { write: true });
              if (current()) await load();
            } finally { mutationPending = false; }
          }), "erp-button"));
        }
        if (root.quickCreate && view.canCreate !== false && context["create"] !== false && assignable) {
          const add = button("Add card", () => {
            const form = el("form"), input = el("input"), submit = el("button", "Create");
            input.required = true;
            input.setAttribute("aria-label", `New card in ${column.label}`);
            submit.type = "submit"; form.append(input, submit);
            const cancel = (): void => { form.replaceWith(add); add.focus(); };
            input.addEventListener("keydown", event => { if (event.key === "Escape") { event.preventDefault(); cancel(); } });
            form.addEventListener("submit", event => {
              event.preventDefault();
              const name = input.value.trim();
              if (!name || mutationPending) return;
              this.run(async () => {
                mutationPending = true; submit.disabled = true;
                try {
                  await this.rpc.call(description.model, "name_create", { name, context: { ...context, [`default_${field.name}`]: column.value } }, { write: true });
                  if (current()) await load();
                } finally { mutationPending = false; submit.disabled = false; }
              });
            });
            add.replaceWith(form); input.focus();
          }, "erp-button");
          controls.append(add);
        }
        return controls;
      };
      board = renderKanbanBoard({
        headerControls,
        ...(assignable && view.canEdit !== false && context["edit"] !== false && root.recordsDraggable ? {
          move: (id: number, target: KanbanColumnState, card: HTMLElement) => this.run(async () => {
            if (mutationPending) return;
            mutationPending = true;
            const oldBoard = board?.node;
            const left = oldBoard?.scrollLeft ?? 0, top = oldBoard?.scrollTop ?? 0;
            try {
              await this.rpc.call(description.model, "write", { ids: [id], vals: { [field.name]: byKey(target.key).value }, context }, { write: true });
              if (current()) await load();
            } finally {
              mutationPending = false;
              if (generation === this.generation && board) {
                board.node.scrollLeft = left; board.node.scrollTop = top;
                if (card.isConnected) card.focus({ preventScroll: true }); else focusKanbanCard(board.node, id);
              }
            }
          })
        } : {}),
        columns, layout, renderField, maxCards: bound.maxCards, ...(open ? { open } : {}),
        onToggle: column => this.run(() => toggleColumn(byKey(column.key))),
        onLoadMore: column => this.run(() => readColumn(byKey(column.key))),
        onRetry: column => this.run(() => readColumn(byKey(column.key))),
      });
      body.replaceChildren(board.node);
      boardChanged();
    };

    const load = async (): Promise<void> => {
      const mine = ++request, current = (): boolean => mine === request && generation === this.generation && !this.expired;
      if (scopeField && !selected) {
        unavailable(`Choose an available ${scopeField.label} to view this board.`, "No selection");
        return;
      }
      void sidebar?.refresh(searchModel.getDomain());
      const group = grouping();
      if (group) return loadBoard(group, current);
      clearBoard();
      pager.replaceChildren(count, previous, next);
      body.setAttribute("aria-busy", "true");
      let page: { records: Record<string, unknown>[]; length: number };
      try { page = await fetchRows(offset); }
      catch (error) {
        if (!current()) return;
        unavailable("Unable to load these records. Adjust the search or reload the page.", "Results unavailable");
        throw error;
      }
      if (!current()) return;
      body.removeAttribute("aria-busy");
      if (!page.records.length && offset > 0 && page.length > 0) { offset = Math.floor((page.length - 1) / pageSize) * pageSize; return load(); } // the page shrank under a search
      total = page.length;
      count.textContent = total ? `${offset + 1}-${offset + page.records.length} / ${total}` : "0 records";
      previous.disabled = offset === 0; next.disabled = offset + page.records.length >= total;
      this.listContext = { ids: page.records.map(row => integer(row["id"], "record id")), total, offset, limit: pageSize, page: async from => {
        const rows = await fetchRows(from);
        return { ids: rows.records.map(row => integer(row["id"], "record id")), total: rows.length };
      } };
      cards?.dispose();
      if (!page.records.length) {
        cards = undefined;
        body.replaceChildren(noRecords());
        return;
      }
      cards = renderKanbanCards({ layout, records: page.records, open, renderField });
      body.replaceChildren(cards.node);
    };
    if (sectionsPanel) {
      sidebar = createSearchPanelSidebar({
        model: description.model, panel: sectionsPanel, rpc: this.rpc, context, selection: route.search_panel,
        onChange: selection => this.run(async () => { this.keepSearchPanel(route, selection); offset = 0; await load(); }),
        report: error => this.reportError(error),
      });
    }
    await load();

    const searchBar = createSearchBar(searchModel, () => this.run(async () => {
      const parameters = new URLSearchParams();
      searchModel.serializeToUrlParams(parameters);
      const proposed = { ...route };
      for (const key of ["search", "search_facets", "search_filters", "search_groupby"] as const) {
        const value = parameters.get(key);
        if (value === null) delete proposed[key]; else proposed[key] = value;
      }
      // The folded columns belong to one grouping: another grouping starts from the server's defaults.
      const regrouped = fold !== undefined && fold.by !== grouping()?.spec;
      if (regrouped) delete proposed.kanban_fold;
      const url = routeUrl(proposed); // validated whole before anything is committed
      for (const key of ["search", "search_facets", "search_filters", "search_groupby"] as const) { if (proposed[key] === undefined) delete route[key]; else route[key] = proposed[key]; }
      if (regrouped) { delete route.kanban_fold; fold = undefined; }
      if (url !== this.currentUrl) { history.replaceState({ erpIndex: this.historyIndex }, "", url); this.currentUrl = url; }
      offset = 0;
      await load();
    }));
    const searchArea = panel.querySelector("[data-control-search]") || panel;
    if (searchArea !== panel) searchArea.replaceChildren(searchBar.element); else panel.append(searchBar.element);

    const switcher = el("div", undefined, "view-switcher");
    for (const [, vType] of availableViews) {
      if (!switchable(vType) || availableViews.length < 2) continue;
      const vBtn = button(vType, () => this.run(async () => { await this.navigate(switchedRoute(route, vType as SwitchableView)); }), `view-btn ${vType === "kanban" ? "active" : ""}`);
      vBtn.setAttribute("aria-label", `${vType} view`);
      if (vType === "kanban") vBtn.setAttribute("aria-current", "page");
      decorateViewButton(vBtn, vType as SwitchableView);
      switcher.append(vBtn);
    }
    const toolsArea = panel.querySelector("[data-control-tools]");
    if (toolsArea) toolsArea.replaceChildren(pager, switcher);
    else {
      const tools = el("div", undefined, "control-panel-tools");
      tools.append(pager, switcher);
      (panel.querySelector(".control-panel-heading") || panel).append(tools);
    }
    this.wrapWithSearchPanel(staging, results, sidebar);
    return {
      canCreateInScope: !scopeField || selected !== undefined,
      dispose: () => { request++; cards?.dispose(); board?.dispose(); sidebar?.dispose(); searchBar.dispose(); },
      // After the page is in the document and scrolled to its top: the way back from a card restores its scroll and focus.
      restore: () => {
        const shown = board?.node ?? cards?.node;
        if (!resume || !shown) return;
        documentScroller().scrollTop = resume.scroll;
        if (board && resume.board) { board.node.scrollLeft = resume.board.left; board.node.scrollTop = resume.board.top; }
        focusKanbanCard(shown, resume.id);
      }
    };
  }


  /** Resolves a `SectionsPanel` from the already-loaded view bundle without extra RPCs when possible. */
  private async sectionsPanelFor(description: Description, view: View, context: Record<string, unknown>, viewType: string): Promise<SectionsPanel | undefined> {
    const candidate = view.searchPanel ?? description.views.get("list")?.searchPanel ?? description.views.get("search")?.searchPanel;
    if (candidate?.kind === "sections") {
      return candidate.viewTypes.includes(viewType) ? candidate : undefined;
    }
    if (view.type === "list" || description.views.has("list") || description.views.has("search")) {
      return undefined;
    }
    return this.searchViewPanel(description.model, context, viewType);
  }

  /** The nodes of `model`'s default search view (shared through the metadata cache); none when the model has no search view. */
  private async searchViewPanel(model: string, context: Record<string, unknown>, viewType: string): Promise<SectionsPanel | undefined> {
    const searchView = await this.viewDescriptions(model, [[null, "search"]], {}, context).catch((error: unknown) => {
      if (error instanceof ClientError && error.kind === "validation" && error.message.startsWith("No default view of type 'search'")) return undefined;
      throw error;
    });
    if (!searchView || typeof searchView !== "object" || Array.isArray(searchView) || !("views" in searchView)) return undefined;
    const search = describeViews(searchView, model, this.registry).views.get("search");
    if (search?.searchPanel?.kind === "sections" && search.searchPanel.viewTypes.includes(viewType)) {
      return search.searchPanel;
    }
    return undefined;
  }

  /** The search panel's selection goes into the address, so a reload, Back or Forward and a copied link keep the filter. */
  private keepSearchPanel(route: Route, selection: string): void {
    const url = routeUrl({ ...route, search_panel: selection }); // validated whole before anything is committed
    route.search_panel = selection;
    if (url !== this.currentUrl) { history.replaceState({ erpIndex: this.historyIndex }, "", url); this.currentUrl = url; }
  }

  private wrapWithSearchPanel(staging: HTMLElement, content: HTMLElement, sidebar: SearchPanelSidebar | undefined): void {
    if (!sidebar) {
      staging.append(content);
      return;
    }
    const shell = el("div", undefined, "search-panel-layout");
    const main = el("div", undefined, "search-panel-main");
    main.append(content);
    shell.append(sidebar.element, main);
    staging.append(shell);
  }

  private async hierarchyView(staging: HTMLElement, panel: HTMLElement, actionTitle: string, description: Description, view: View, route: Route, generation: number, availableViews: [number | null, string][], scope: { baseDomain: unknown[]; context: Record<string, unknown>; hasForm: boolean; searchDefaults?: ReadonlyMap<string, unknown> }): Promise<{ dispose: () => void }> {
    const { context, baseDomain, hasForm } = scope;
    if (route.id !== undefined) throw new ClientError("validation", "The hierarchy view shows many records; the address cannot name one record.");
    const root = hierarchyRoot(view);
    panel.classList.add("control-panel-list");
    const breadcrumbs = panel.querySelector<HTMLElement>("[data-control-breadcrumbs]") || panel.querySelector<HTMLElement>("h1");
    if (breadcrumbs) {
      this.breadcrumbs.clear();
      this.breadcrumbs.push({
        id: `action-${route.action}-hierarchy`, title: actionTitle, isFormView: false,
        get route() { return { ...route }; }, get url() { return routeUrl(route); },
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
    });
    await this.prepareSearch(searchModel, description.model, context, route, () => this.restoreDashboardFilters(searchModel, description, route, generation, baseDomain, context), scope.searchDefaults ?? new Map(), generation);

    const sectionsPanel = await this.sectionsPanelFor(description, view, context, "hierarchy");
    let sidebar: SearchPanelSidebar | undefined;
    const combinedDomain = (): unknown[] => [...searchModel.getDomain(), ...(sidebar?.getDomain() ?? [])];

    const container = el("div", undefined, "hierarchy-view");
    const body = el("div", undefined, "hierarchy-results");
    container.append(body);
    let tree: { node: HTMLElement; dispose: () => void } | undefined;
    let request = 0;
    // What the user unfolded survives the reload after a reassignment.
    const treeState: HierarchyState = {};

    const open = hasForm ? (id: number): void => this.run(async () => {
      await this.navigate({ ...route, id: String(id), view_type: "form" });
    }) : undefined;

    const load = async (): Promise<void> => {
      const mine = ++request, current = (): boolean => mine === request && generation === this.generation && !this.expired;
      body.setAttribute("aria-busy", "true");
      void sidebar?.refresh(searchModel.getDomain());
      let records: Record<string, unknown>[];
      let total: number;
      try {
        const answer = object(await this.rpc.call(description.model, "web_search_read", {
          domain: combinedDomain(),
          specification: specification(view.fields),
          offset: 0,
          limit: 200,
          context,
        }));
        records = array(answer["records"], "records").map(row => object(row));
        total = integer(answer["length"], "record count");
      } catch (error) {
        if (!current()) return;
        body.removeAttribute("aria-busy");
        if (body.isConnected) this.listContext = undefined;
        tree?.dispose(); tree = undefined;
        const message = el("p", "Unable to load this hierarchy. Adjust the search or reload the page.", "list-load-error");
        message.setAttribute("role", "alert"); body.replaceChildren(message);
        throw error;
      }
      if (!current()) return;
      body.removeAttribute("aria-busy");
      tree?.dispose();
      // A record opened from the chart steps through the loaded records with the form pager.
      this.listContext = { ids: records.map(row => integer(row["id"], "record id")), total: records.length, offset: 0, limit: records.length };
      if (!records.length) {
        tree = undefined;
        const empty = noContentHelp();
        empty.replaceChildren(el("h2", t("No records yet")), el("p", t("Create a record to build the organization chart.")));
        body.replaceChildren(empty);
        return;
      }
      const focusId = typeof context["hierarchy_res_id"] === "number" ? context["hierarchy_res_id"] : undefined;
      const locale = this.editorLocale?.lang.replaceAll("_", "-") ?? "en-US";
      tree = renderHierarchyTree({
        view,
        records,
        ...(focusId !== undefined ? { focusId } : {}),
        ...(open ? { open } : {}),
        ...(root.draggable ? {
          onReassign: async (childId, newParentId) => {
            await this.run(async () => {
              await this.rpc.call(description.model, "write", {
                ids: [childId],
                vals: { [root.parentField.name]: newParentId },
                context,
              }, { write: true });
              await load();
            });
          },
        } : {}),
        formatField: (field, rec) => displayValue(field, rec[field.name], locale),
        state: treeState,
        dialogs: this.messages.dialogs,
      });
      // A record whose parent was not read shows as a top-level card; say so instead of showing a wrong chart silently.
      body.replaceChildren(...(total > records.length ? [limitNotice(t("Showing {shown} of {total} records: a record whose parent is not among them shows at the top level. Narrow the search to see the rest.", { shown: records.length, total }))] : []), tree.node);
    };

    if (sectionsPanel) {
      sidebar = createSearchPanelSidebar({
        model: description.model, panel: sectionsPanel, rpc: this.rpc, context, selection: route.search_panel,
        onChange: selection => this.run(() => { this.keepSearchPanel(route, selection); return load(); }),
        report: error => this.reportError(error),
      });
    }
    await load();

    const searchBar = createSearchBar(searchModel, () => this.run(async () => {
      const parameters = new URLSearchParams();
      searchModel.serializeToUrlParams(parameters);
      const proposed = { ...route };
      for (const key of ["search", "search_facets", "search_filters", "search_groupby"] as const) {
        const value = parameters.get(key);
        if (value === null) delete proposed[key]; else proposed[key] = value;
      }
      const url = routeUrl(proposed);
      for (const key of ["search", "search_facets", "search_filters", "search_groupby"] as const) { if (proposed[key] === undefined) delete route[key]; else route[key] = proposed[key]; }
      if (url !== this.currentUrl) { history.replaceState({ erpIndex: this.historyIndex }, "", url); this.currentUrl = url; }
      await load();
    }));
    const searchArea = panel.querySelector("[data-control-search]") || panel;
    if (searchArea !== panel) searchArea.replaceChildren(searchBar.element); else panel.append(searchBar.element);

    const switcher = el("div", undefined, "view-switcher");
    for (const [, vType] of availableViews) {
      if (!switchable(vType) || availableViews.length < 2) continue;
      const vBtn = button(vType, () => this.run(async () => { await this.navigate(switchedRoute(route, vType)); }), `view-btn ${vType === "hierarchy" ? "active" : ""}`);
      vBtn.setAttribute("aria-label", `${vType} view`);
      if (vType === "hierarchy") vBtn.setAttribute("aria-current", "page");
      decorateViewButton(vBtn, vType);
      switcher.append(vBtn);
    }
    const toolsArea = panel.querySelector("[data-control-tools]");
    if (toolsArea) toolsArea.replaceChildren(switcher);
    else (panel.querySelector(".control-panel-heading") || panel).append(el("div", undefined, "control-panel-tools"), switcher);

    this.wrapWithSearchPanel(staging, container, sidebar);
    return { dispose: () => { request++; tree?.dispose(); sidebar?.dispose(); searchBar.dispose(); } };
  }

  private async activityOrMapView(staging: HTMLElement, panel: HTMLElement, actionTitle: string, description: Description, view: View, route: Route, generation: number, availableViews: [number | null, string][], scope: { baseDomain: unknown[]; context: Record<string, unknown>; hasForm: boolean; searchDefaults?: ReadonlyMap<string, unknown> }): Promise<{ dispose: () => void }> {
    const { context, baseDomain, hasForm } = scope;
    if (route.id !== undefined) throw new ClientError("validation", "The activity view shows many records; the address cannot name one record.");
    panel.classList.add("control-panel-list");
    const breadcrumbs = panel.querySelector<HTMLElement>("[data-control-breadcrumbs]") || panel.querySelector<HTMLElement>("h1");
    if (breadcrumbs) {
      this.breadcrumbs.clear();
      this.breadcrumbs.push({
        id: `action-${route.action}-${view.type}`, title: actionTitle, isFormView: false,
        get route() { return { ...route }; }, get url() { return routeUrl(route); },
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
    });
    await this.prepareSearch(searchModel, description.model, context, route, () => this.restoreDashboardFilters(searchModel, description, route, generation, baseDomain, context), scope.searchDefaults ?? new Map(), generation);

    const container = el("div", undefined, `${view.type}-view`);
    const body = el("div", undefined, `${view.type}-results`);
    container.append(body);
    let matrix: { node: HTMLElement; dispose: () => void } | undefined;
    let request = 0;

    const open = hasForm ? (id: number): void => this.run(async () => {
      await this.navigate({ ...route, id: String(id), view_type: "form" });
    }) : undefined;

    const sectionsPanel = await this.sectionsPanelFor(description, view, context, view.type);
    let sidebar: SearchPanelSidebar | undefined;
    const effectiveDomain = (): unknown[] => [...searchModel.getDomain(), ...(sidebar?.getDomain() ?? [])];

    const load = async (): Promise<void> => {
      const mine = ++request, current = (): boolean => mine === request && generation === this.generation && !this.expired;
      body.setAttribute("aria-busy", "true");
      void sidebar?.refresh(searchModel.getDomain());
      let records: Record<string, unknown>[];
      let types: ActivityTypeRow[] = [];
      let activities: ActivityItemRow[] = [];
      const cut: string[] = [];
      try {
        if (view.type === "map") {
          const map = await readStoredMap(this.rpc, description.model, view, effectiveDomain(), context, open);
          if (current()) { body.replaceChildren(map); body.removeAttribute("aria-busy"); }
          return;
        }
        const [recResp, typeResp] = await Promise.all([
          this.rpc.call(description.model, "web_search_read", { domain: effectiveDomain(), specification: specification(view.fields), offset: 0, limit: 80, context }),
          this.rpc.call("mail.activity.type", "web_search_read", {
            domain: ["|", ["res_model", "=", false], ["res_model", "=", description.model]],
            specification: { name: {}, sequence: {} },
            offset: 0, limit: 30, order: "sequence asc, id asc", context,
          }),
        ]);
        records = array(object(recResp)["records"], "records").map(row => object(row));
        const recordTotal = integer(object(recResp)["length"], "record count");
        if (recordTotal > records.length) cut.push(t("Showing {shown} of {total} records. Narrow the search to see the rest.", { shown: records.length, total: recordTotal }));
        const typeTotal = integer(object(typeResp)["length"], "activity type count");
        types = array(object(typeResp)["records"], "activity types").map(row => {
          const item = object(row);
          return { id: integer(item["id"], "type id"), name: typeof item["name"] === "string" ? item["name"] : `#${String(item["id"])}` };
        });
        if (typeTotal > types.length) cut.push(t("Showing {shown} of {total} activity types.", { shown: types.length, total: typeTotal }));
        const recIds = records.map(r => r["id"]).filter((id): id is number => typeof id === "number");
        if (recIds.length > 0) {
          const actResp = await this.rpc.call("mail.activity", "web_search_read", {
            domain: [["res_model", "=", description.model], ["res_id", "in", recIds]],
            specification: { res_id: {}, activity_type_id: { fields: { display_name: {} } }, summary: {}, date_deadline: {}, state: {}, user_id: { fields: { display_name: {} } } },
            offset: 0, limit: 500, context,
          });
          const activityRows = array(object(actResp)["records"], "activities");
          const activityTotal = integer(object(actResp)["length"], "activity count");
          if (activityTotal > activityRows.length) cut.push(t("Showing {shown} of {total} activities of these records.", { shown: activityRows.length, total: activityTotal }));
          activities = activityRows.flatMap(row => {
            const item = object(row);
            const id = item["id"], resId = item["res_id"];
            const rawType = item["activity_type_id"];
            const typeId = typeof rawType === "number" ? rawType : (rawType && typeof rawType === "object" && typeof (rawType as Record<string, unknown>)["id"] === "number" ? (rawType as Record<string, unknown>)["id"] as number : null);
            if (typeof id !== "number" || typeof resId !== "number" || typeId === null) return [];
            const rawState = item["state"];
            const state: "overdue" | "today" | "planned" = rawState === "overdue" || rawState === "today" ? rawState : "planned";
            const rawUser = item["user_id"];
            const userName = rawUser && typeof rawUser === "object" && typeof (rawUser as Record<string, unknown>)["display_name"] === "string" ? (rawUser as Record<string, unknown>)["display_name"] as string : "";
            return [{
              id, res_id: resId, activity_type_id: typeId,
              summary: typeof item["summary"] === "string" ? item["summary"] : "",
              date_deadline: typeof item["date_deadline"] === "string" ? item["date_deadline"] : "",
              state, user_name: userName,
            }];
          });
        }
      } catch (error) {
        if (!current()) return;
        body.removeAttribute("aria-busy");
        if (body.isConnected) this.listContext = undefined;
        matrix?.dispose(); matrix = undefined;
        const message = el("p", `Unable to load ${view.type === "map" ? "locations" : "activities"}. Adjust the search or reload the page.`, "list-load-error");
        message.setAttribute("role", "alert"); body.replaceChildren(message);
        throw error;
      }
      if (!current()) return;
      body.removeAttribute("aria-busy");
      matrix?.dispose();
      this.listContext = { ids: records.map(row => integer(row["id"], "record id")), total: records.length, offset: 0, limit: records.length };
      const locale = this.editorLocale?.lang.replaceAll("_", "-") ?? "en-US";
      matrix = renderActivityMatrix({
        model: description.model, view, records, types, activities,
        rpc: this.rpc, context, userId: this.userId,
        ...(open ? { open } : {}),
        formatField: (field, rec) => displayValue(field, rec[field.name], locale),
        onChanged: () => load(),
        report: error => this.reportError(error),
        dialogs: this.messages.dialogs,
      });
      body.replaceChildren(...cut.map(limitNotice), matrix.node);
    };

    if (sectionsPanel) {
      sidebar = createSearchPanelSidebar({
        model: description.model, panel: sectionsPanel, rpc: this.rpc, context, selection: route.search_panel,
        onChange: selection => this.run(() => { this.keepSearchPanel(route, selection); return load(); }),
        report: error => this.reportError(error),
      });
    }
    await load();
    const searchBar = createSearchBar(searchModel, () => this.run(async () => {
      const parameters = new URLSearchParams();
      searchModel.serializeToUrlParams(parameters);
      const proposed = { ...route };
      for (const key of ["search", "search_facets", "search_filters", "search_groupby"] as const) {
        const value = parameters.get(key);
        if (value === null) delete proposed[key]; else proposed[key] = value;
      }
      const url = routeUrl(proposed);
      for (const key of ["search", "search_facets", "search_filters", "search_groupby"] as const) { if (proposed[key] === undefined) delete route[key]; else route[key] = proposed[key]; }
      if (url !== this.currentUrl) { history.replaceState({ erpIndex: this.historyIndex }, "", url); this.currentUrl = url; }
      await load();
    }));
    const searchArea = panel.querySelector("[data-control-search]") || panel;
    if (searchArea !== panel) searchArea.replaceChildren(searchBar.element); else panel.append(searchBar.element);

    const switcher = el("div", undefined, "view-switcher");
    for (const [, vType] of availableViews) {
      if (!switchable(vType) || availableViews.length < 2) continue;
      const vBtn = button(vType, () => this.run(async () => { await this.navigate(switchedRoute(route, vType)); }), `view-btn ${vType === view.type ? "active" : ""}`);
      vBtn.setAttribute("aria-label", `${vType} view`);
      if (vType === view.type) vBtn.setAttribute("aria-current", "page");
      decorateViewButton(vBtn, vType);
      switcher.append(vBtn);
    }
    const toolsArea = panel.querySelector("[data-control-tools]");
    if (toolsArea) toolsArea.replaceChildren(switcher);
    else (panel.querySelector(".control-panel-heading") || panel).append(el("div", undefined, "control-panel-tools"), switcher);

    this.wrapWithSearchPanel(staging, container, sidebar);
    return { dispose: () => { request++; matrix?.dispose(); sidebar?.dispose(); searchBar.dispose(); } };
  }

  private async searchViewNodes(model: string, context: Record<string, unknown>): Promise<Node[]> {
    const searchView = await this.viewDescriptions(model, [[null, "search"]], {}, context).catch((error: unknown) => {
      // A model with no search view has no declared items; the server answers it with this sentence (select_view).
      if (error instanceof ClientError && error.kind === "validation" && error.message.startsWith("No default view of type 'search'")) return undefined;
      throw error;
    });
    if (searchView === undefined) return [];
    const search = describeViews(searchView, model, this.registry).views.get("search");
    if (!search) throw new ClientError("server", "The search view description is missing.");
    return search.nodes;
  }

  /** Reads a search view expression (a filter's domain or context) with the names the search view may use. */
  private searchExpression(context: Record<string, unknown>): (ast: ValidatedAst) => unknown {
    return ast => jsonValue(evalAst(ast, createContext({ names: { context: expressionValue(context, 0), uid: expressionValue(this.userId, 0) } })));
  }

  /** What the Group By menu offers for `model`: the search view's declared items and the fields the server can group by. */
  private async groupByCatalogue(model: string, context: Record<string, unknown>): Promise<GroupByCatalogue> {
    const [nodes, fields] = await Promise.all([
      this.searchViewNodes(model, context),
      this.rpc.call(model, "fields_get", { attributes: ["string", "type", "groupable", "selection", "aggregator", "currency_field"], context })
    ]);
    return buildGroupByCatalogue(nodes, this.searchExpression(context), decodeGroupableFields(fields, true), true, true);
  }

  /** Declares the search view's named filters on a view's search model (read when first needed), restores the search
   *  from the address and, when the address holds no search (a first open), applies the action's `search_default_<name>`
   *  values. A default that cannot be applied is named in a warning and the view opens without it. Returns the address
   *  parameters. */
  private async prepareSearch(searchModel: SearchModel, model: string, context: Record<string, unknown>, route: Route, params: () => Promise<URLSearchParams> | URLSearchParams, defaults: ReadonlyMap<string, unknown>, generation: number): Promise<URLSearchParams> {
    let arch: Promise<SearchArch> | undefined;
    const readArch = (): Promise<SearchArch> => arch ??= this.searchViewNodes(model, context).then(nodes => readSearchArch(nodes, this.searchExpression(context)), error => { arch = undefined; throw error; });
    searchModel.declareViewFilters(readArch);
    // A list of a saved action keeps its favorites in `ir.filters`; a related list (opened by a button) has none.
    const actionId = route.method_action === undefined && route.action !== undefined ? rpcId(route.action) : undefined;
    if (actionId !== undefined) searchModel.declareFavorites(favoriteStore(this.rpc, model, actionId, context));
    const restored = await params();
    // An address that names one of the view's filters or a favorite needs them before it can be restored.
    if (defaults.size || namesViewFilter(restored)) await searchModel.loadViewFilters();
    if (namesFavorite(restored)) await searchModel.loadFavorites();
    if (searchModel.loadFromUrlParams(restored) || route.search) { if (defaults.size) searchModel.keepEmptySearch(); return restored; }
    // A first open: the caller's default favorite wins over the action's search defaults, as in the reference ERP.
    let favorite: FavoriteItem | undefined;
    if (searchModel.hasFavorites()) {
      try { await searchModel.loadFavorites(); favorite = searchModel.defaultFavorite(); }
      catch (error) {
        if (!(error instanceof ClientError) || error.kind === "session") throw error;
        if (generation === this.generation && !this.expired) this.messages.notifications.add(`Your default favorite could not be applied: ${error.message}`, { level: "warning" });
      }
    }
    if (favorite || defaults.size) searchModel.keepEmptySearch();
    if (favorite) {
      try { searchModel.toggleFavorite(favorite.id); return restored; }
      catch (error) {
        if (!(error instanceof ClientError) || (error.kind !== "validation" && error.kind !== "unsupported")) throw error;
        if (generation === this.generation && !this.expired) this.messages.notifications.add(`Your default favorite could not be applied: ${error.message}`, { level: "warning" });
      }
    }
    if (!defaults.size) return restored;
    const unapplied = await applySearchDefaults(searchModel, await readArch(), defaults, async (relation, id) => {
      const rows = array(await this.rpc.call(relation, "web_read", { ids: [id], specification: { display_name: {} }, context }), "default record");
      if (rows.length !== 1) throw new ClientError("validation", `Record ${id} of ${relation} is not available.`);
      return text(object(rows[0])["display_name"], "default record name");
    });
    if (unapplied.length && generation === this.generation && !this.expired) {
      this.messages.notifications.add(`This view opened without the default search ${unapplied.map(item => `“${item.name}” (${item.reason})`).join(", ")}.`, { level: "warning" });
    }
    return restored;
  }

  private async listView(staging: HTMLElement, panel: HTMLElement, actionTitle: string, description: Description, view: View, route: Route, generation: number, actionHelp?: string, availableViews?: [number | null, string][], scope?: {baseDomain: unknown[]; context: Record<string, unknown>; hasForm: boolean; canCreate: boolean; searchDefaults?: ReadonlyMap<string, unknown>}): Promise<{ dispose: () => void; settle: () => void; restore: () => void }> {
    const context = scope?.context ?? this.context, baseDomain = scope?.baseDomain ?? [], hasForm = scope?.hasForm ?? true, canCreate = scope?.canCreate ?? hasForm;
    const cellNavigation = this.pageNavigation(generation, route, () => actionTitle);
    const fieldStops: (() => void)[] = [];
    let groupedModel: ListGroupModel | undefined;
    const currencies = new CurrencyDisplayCache();
    const resume = this.listResume?.url === routeUrl(listRouteOf(route)) ? this.listResume : undefined;
    let offset = resume?.offset ?? 0, order = resume?.order ?? "id asc", request = 0, pageSize = resume?.limit ?? view.pageLimit ?? 40;
    const officeBlue = officeShell();
    panel.classList.add("control-panel-list");
    const selectedIds = new Set<number>();
    // Groups the user opened (by group key); a new search, grouping or page starts folded again; a sort keeps them unless it moves the page or orders the groups.
    const resetGroups = (): void => { groupedModel?.dispose(); searchModel.clearListState(); };
    let redrawColumns: (() => void) | undefined;
    const choices = columnChoices(view.fields, JSON.stringify([this.userId, "list", description.model, view.id ?? route.action]),
      this.messages.popovers, () => redrawColumns?.(), error => this.reportError(error));
    const columnWidths = resume?.widths ?? new Map<string, number>();
    let settleColumns: (() => void) | undefined;

    const breadcrumbs = panel.querySelector<HTMLElement>("[data-control-breadcrumbs]") || panel.querySelector<HTMLElement>("h1");
    if (breadcrumbs) {
      const listCrumb = {
        id: crumbId(route),
        title: actionTitle,
        // Getters: the list's search facets are written into `route` after this crumb exists.
        get route() { return listRouteOf(route); },
        get url() { return routeUrl(listRouteOf(route)); },
        isFormView: false
      };
      const related = route.method_action ? this.methodActions.get(route.method_action) : undefined;
      // A list opened by a form's button goes on top of that form; its own crumb (or browser Back) cuts the trail back to it.
      const returning = !!related && this.breadcrumbs.items.some(item => item.id === listCrumb.id);
      const stacked = !!related && !returning && this.breadcrumbs.current?.id === crumbId(related.origin, related.origin.id ?? "new");
      if (returning) { this.breadcrumbs.truncateTo(listCrumb.id); this.breadcrumbs.replaceCurrent(listCrumb); }
      else {
        if (!stacked) this.breadcrumbs.clear();
        this.breadcrumbs.push(listCrumb);
      }
      const title = el("h1", actionTitle, "breadcrumb-current");
      title.setAttribute("aria-current", "page");
      breadcrumbs.replaceChildren(title);
      if (returning || stacked) {
        // Refreshing a refused breadcrumb must leave the adjacent list menu and selection controls mounted.
        const trail = el("div", undefined, "list-crumb-trail");
        breadcrumbs.replaceChildren(trail);
        const draw = (): void => {
          renderBreadcrumbs(trail, this.breadcrumbs, {
            isSmall: document.documentElement.dataset["mobile"] === "true",
            onNavigate: async item => this.run(() => this.openCrumb(item, draw)),
            report: error => this.reportError(error)
          });
          // A list has no heading of its own: its crumb names the page, as on a list that starts the trail.
          const current = trail.querySelector<HTMLElement>(".breadcrumb-current");
          if (current) retagCrumb(current, "h1");
        };
        draw();
      } else if (related) {
        // A list opened by a form button keeps the way back: menu path, the source record, this list.
        const trail = el("div", undefined, "form-breadcrumb-path");
        const menus = this.menuTrail(related.origin);
        // An origin that is a page of the menu's own action (a dashboard, a pivot, a chart) takes the place of the menu's last
        // link: both name that page ("Dashboard › Accounting Dashboard"), and only the origin crumb brings it back as it was left.
        // Drop the last menu link only when that menu opens the origin's action (a hand-typed route under another menu keeps it).
        const leaf = related.origin.menu ? this.menus.get(Number(related.origin.menu)) : [...this.menus.values()].find(item => String(item.action) === related.origin.action);
        if (!related.origin.id && leaf && String(leaf.action) === related.origin.action) menus.pop();
        for (const item of menus) trail.append(item, el("span", "›", "form-breadcrumb-divider"));
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

    // A list opened from a form button or a pivot cell keeps the ORIGIN page's action in its address (the way back), so the strip's
    // counts would be read under that action's domain, not this list's: such a list shows no strip.
    const strip = route.method_action === undefined ? view.dashboard : undefined;
    const dashboardAction = route.action === undefined ? undefined : rpcId(route.action);
    if (strip && (dashboardAction === undefined || view.id === undefined)) throw new ClientError("unsupported", "Dashboard requires a saved window action and list view.");
    const dashboard = strip ? mountDashboard({
      config: strip,
      read: (domain, period) => this.rpc.call(description.model, "erp_read_dashboard", {
        action_id: dashboardAction, view_id: view.id, revision: strip.revision,
        period, domain_scope: domain, context,
      }),
      apply: (name, domain) => this.run(async () => {
        if (!searchModel.getFilters().some(f => f.id === `dashboard_${name}`)) searchModel.addDeclaredFilters([{
          id: `dashboard_${name}`, description: strip!.items.find(item => item.name === name)!.label, domain, isActive: false,
        }]);
        searchModel.toggleFilter(`dashboard_${name}`);
        await applySearch();
      }),
      selected: name => searchModel.getFilters().some(f => f.id === `dashboard_${name}` && f.isActive),
      current: () => generation === this.generation && !this.expired,
    }) : undefined;
    // Addon headers of this action list read above the dashboard and rows; they are created just before the first load.
    const headerSlot = el("div", undefined, "list-headers");
    staging.append(headerSlot);
    const listHeaders: ListHeaderHandle[] = [];
    let headerRefresh: AbortController | undefined;
    const disposeHeaders = (): void => {
      headerRefresh?.abort();
      for (const header of listHeaders.splice(0)) header.dispose();
      headerSlot.replaceChildren();
    };
    if (dashboard) {
      await dashboard.refresh(baseDomain);
      searchModel.addDeclaredFilters([...dashboard.domains()].map(([name,domain]) => ({
        id: `dashboard_${name}`, description: strip!.items.find(item => item.name === name)!.label,
        domain, isActive: false,
      })));
      staging.append(dashboard.element);
    }

    // Restore search state from route and URL search params
    const urlParams = new URL(routeUrl(route), window.location.origin).searchParams;
    if (urlParams.has("search_groupby")) await searchModel.loadGroupByCatalogue();
    // A card that is not offered now (its read failed, the strip is hidden for this user, or this is a related list) cannot stay active
    // from the address: the list opens without it, and the strip says why it is unavailable.
    dropAbsentDashboardFilters(urlParams, new Set(searchModel.getFilters().map(filter => filter.id)));
    // The address this navigation commits must not keep the dropped card, or a reload or a shared link brings it back.
    const keptFilters = urlParams.get("search_filters");
    if (keptFilters === null) delete route.search_filters; else route.search_filters = keptFilters;
    await this.prepareSearch(searchModel, description.model, context, route, () => urlParams, scope?.searchDefaults ?? new Map(), generation);
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
    results.classList.toggle("list-wrap", resume?.wrap === true);
    const categoriesArea = panel.querySelector("[data-control-categories]") || panel;
    const category = view.searchPanel?.kind === "selection" ? view.searchPanel : undefined;
    const panelMessage = view.searchPanel?.kind === "unsupported" ? view.searchPanel.message : category?.notice;
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

    const applySearch = async (): Promise<void> => {
      offset = 0;
      resetGroups();
      updateUrlWithSearch();
      await load();
    };
    const searchBar = createSearchBar(searchModel, () => this.run(applySearch));

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
    const removeSelection = (): void => this.run(async () => {
      if (!selectedIds.size) return;
      const ids = Array.from(selectedIds);
      if (await this.choose("Delete selected records?", `Are you sure you want to delete ${ids.length} record(s)? This action cannot be undone.`, ["Delete", "Cancel"]) !== "Delete") return;
      this.busy = true;
      try {
        await this.rpc.call(description.model, "unlink", { ids, context }, { write: true });
        selectedIds.clear();
        this.notice("Record(s) deleted.");
      } finally {
        this.busy = false;
      }
      await load(true);
    });
    const print = printDropdown(this.rpc, description.model, report => this.run(async () => {
      if (!selectedIds.size) return;
      await printRecords({ rpc: this.rpc, dialogs: this.messages.dialogs, report: error => this.reporter.report("render", error) }, report, description.model, Array.from(selectedIds));
    }), error => this.reportError(error), () => this.reportsOf(description.model));
    selectionCount.setAttribute("aria-live", "polite");
    selectionBox.append(selectionCount, print.node);

    const dataMenu = listDataMenu({
      rpc: this.rpc, model: description.model, context, dialogs: this.messages.dialogs,
      canImport: canCreate && view.canEdit !== false && !isFalsy(context["edit"]),
      ...(canDelete ? { deleteSelected: removeSelection } : {}),
      wrapRows: { active: resume?.wrap === true, change: active => results.classList.toggle("list-wrap", active) },
      csrf: text(object(this.headerBootstrap["csrf"])["logout"], "session token"),
      visibleFields: () => choices.visible().filter(field => field.name !== "id").map(field => field.name),
      selectedIds: () => Array.from(selectedIds),
      filteredIds: async () => {
        const base = listDomain();
        const found = await this.rpc.call(description.model, "search", { domain: tabs ? tabs.rowDomain(base) : base, limit: MAX_EXPORT_RECORDS + 1, order, context });
        if (!Array.isArray(found) || found.some(id => !Number.isSafeInteger(id))) throw new ClientError("server", "The server did not answer the search with record ids.");
        if (found.length > MAX_EXPORT_RECORDS) throw new ClientError("validation", `More than ${MAX_EXPORT_RECORDS} records match. Narrow the search or select the records to export.`);
        return found as number[];
      },
      imported: async () => { await load(true); },
      notice: message => this.notice(message),
      report: error => this.reportError(error),
    });
    (breadcrumbs ?? controls).append(dataMenu.node, selectionBox);
    const disposeActionMenu = (): void => { print.dispose(); dataMenu.dispose(); };

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
        if (switchable(vType)) {
          const vBtn = button(vType.charAt(0).toUpperCase() + vType.slice(1), () => this.run(async () => {
            await this.navigate(switchedRoute(route, vType as SwitchableView));
          }), `view-btn ${vType === "list" ? "active" : ""}`);
          vBtn.setAttribute("aria-label", `${vType} view`);
          decorateViewButton(vBtn, vType as SwitchableView);
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
    const listSections = tabs ? undefined : await this.sectionsPanelFor(description, view, context, "list");
    let listSidebar: SearchPanelSidebar | undefined;
    if (listSections) {
      listSidebar = createSearchPanelSidebar({
        model: description.model, panel: listSections, rpc: this.rpc, context, selection: route.search_panel,
        onChange: selection => this.run(async () => { this.keepSearchPanel(route, selection); offset = 0; resetGroups(); await load(); }),
        report: error => this.reportError(error),
      });
    }
    this.wrapWithSearchPanel(staging, results, listSidebar);
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

    const listDomain = (): unknown[] => [...searchModel.getDomain(), ...(listSidebar?.getDomain() ?? [])];
    const fetchRows = async (from: number, base = listDomain()): Promise<Record<string, unknown>> => {
      forgetBinaryContent(this.rpc);
      const answer = object(await this.rpc.call(description.model, "web_search_read", {
        domain: tabs ? tabs.rowDomain(base) : base,
        specification: specification(view.fields),
        offset: from,
        limit: pageSize,
        order,
        context: { ...context, ...BIN_SIZE_CONTEXT }
      }));
      // again on arrival: a cell still on screen may have read an old file while this read travelled
      forgetBinaryContent(this.rpc);
      return answer;
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
      selectedIds.clear(); dataMenu.updateSelection(); selectionBox.style.display = "none"; selectionCount.textContent = "0 selected";
      if (results.isConnected) this.listContext = undefined;
      const message = el("p", "Unable to load these records. Adjust the search or reload the page.", "list-load-error");
      message.setAttribute("role", "alert"); results.replaceChildren(message);
    };
    const load = async (refreshDashboard = false): Promise<void> => {
      const mine = ++request, searchDomain = searchModel.getDomain(), base = [...searchDomain, ...(listSidebar?.getDomain() ?? [])];
      void listSidebar?.refresh(searchDomain);
      void dashboard?.refresh(tabs ? tabs.rowDomain(base) : base, refreshDashboard);
      // A header failure is reported like any client error and never stops the rows from loading; a superseded one is not.
      headerRefresh?.abort();
      const headerSignal = (headerRefresh = new AbortController()).signal, headerDomain = tabs ? tabs.rowDomain(base) : base;
      for (const header of listHeaders) {
        void (async () => header.refresh(structuredClone(headerDomain), { signal: headerSignal }))()
          .catch(error => { if (!headerSignal.aborted) this.reportError(error); });
      }
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
      const readGroups = () => grouping ? decodeGroups(response, grouping, pageSize, { spec: groupName!, aggregates: activeAggregates() }) : undefined;
      let groups: ReturnType<typeof decodeGroups> | undefined, length: number;
      try {
        groups = readGroups();
        length = groups?.length ?? integer(response["length"], "record count");
        // A record or group may disappear while its form is open. Resume on the nearest available page.
        if (offset > 0 && offset >= length) {
          offset = length ? Math.floor((length - 1) / pageSize) * pageSize : 0;
          response = await fetchMain(offset, base);
          if (mine !== request || generation !== this.generation || this.expired) return;
          groups = readGroups();
          length = groups?.length ?? integer(response["length"], "record count");
        }
      } catch (error) {
        if (mine !== request || generation !== this.generation || this.expired) return;
        tabs?.invalidate(); markListUnavailable(); throw error;
      }
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

      const body = el("tbody");

      const updateSelectionUI = (): void => {
        dataMenu.updateSelection();
        selectionCount.textContent = `${selectedIds.size} selected`;
        selectionBox.style.display = selectedIds.size > 0 ? "inline-flex" : "none";
        headerCheckbox.checked = rows.length > 0 && selectedIds.size === rows.length;
        headerCheckbox.indeterminate = selectedIds.size > 0 && selectedIds.size < rows.length;
        // Grouped with every group folded there is nothing to select.
        headerCheckbox.disabled = Boolean(groups) && rows.length === 0;
        const trs = body.querySelectorAll("tr");
        trs.forEach(tr => {
          const rowId = Number(tr.getAttribute("data-id"));
          const selected = selectedIds.has(rowId);
          tr.classList.toggle("row-selected", selected);
          const box = tr.querySelector<HTMLInputElement>("input.list-row-select");
          if (box) box.checked = selected;
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
      const drawTable = (): HTMLTableElement => {
        const visible = choices.visible();
        const table = el("table", undefined, "list-table table-hover"), head = el("thead"), header = el("tr");
        table.setAttribute("aria-label", "Records");
        header.append(headerTh);

        for (const field of visible) {
          const th = el("th", undefined, "sortable-column");
          if (field.kind === "integer" || field.kind === "float" || field.kind === "monetary") th.classList.add("list-number");
          else if (field.kind === "boolean") th.classList.add("list-boolean");
          th.scope = "col";
          const isSorted = order.startsWith(field.name + " ");
          const isAsc = isSorted && order.endsWith("asc");
          th.setAttribute("aria-sort", isSorted ? (isAsc ? "ascending" : "descending") : "none");
          const sortBtn = button("", () => this.run(async () => {
            order = `${field.name} ${isSorted && isAsc ? "desc" : "asc"}`;
            // Back to the first page, or the groups themselves reordered: the open groups may not be on the page that follows.
            if (offset > 0 || field.name === searchModel.getGroupBy()[0]?.split(":")[0]) searchModel.clearListState();
            offset = 0;
            await load();
          }), "sort-button");
          sortBtn.append(el("span", field.label, "sort-label"));
          sortBtn.title = field.label;
          if (isSorted) {
            const arrow = icon("chevron-down"); // ascending rotates it in CSS; aria-sort carries the meaning
            arrow.classList.add("sort-indicator"); sortBtn.append(arrow);
          }
          th.append(sortBtn);
          header.append(th);
        }
        choices.attach(header.cells[header.cells.length - 1]);
        head.append(header);
        table.append(head, body);
        settleColumns = resizeColumns(table, visible, columnWidths);

        return table;
      };

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

        choices.visible().forEach((field, index) => {
          const cell = el("td");
          const adapter = resolveFieldWidget(field.widget ?? field.kind, "list");
          const handle = adapter.render({
            field: { ...field, readonly: true }, value: record[field.name], values: record, model: description.model, fields: view.fields.map(field => ({ ...field, readonly: true })), viewContext: "list",
            rpc: this.rpc, context, currencies, numericLocale: this.numericLocale, editorLocale: this.editorLocale,
            messages: this.messages, run: this.run, report: error => this.reportError(error), signal: this.lifetime.signal,
            // No openRecord: a list cell shows a related record as text and a click anywhere on the row opens the row's own
            // record, as in the reference client; the related record is one click further, in the form.
            change: () => { throw new ClientError("validation", "List display fields cannot be edited."); },
            ...cellNavigation,
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
            const plainBuiltin = ["char", "text", "integer", "float", "monetary", "date", "datetime", "daterange", "boolean", "boolean_checkbox", "boolean_toggle", "selection", "badge", "label_selection", "badges_selection", "filterable_selection", "radio", "priority", "percentage", "many2one"].includes(widget) && adapter === fieldWidgets.get(widget);
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
      let drawRows = (): void => {
        for (const stop of fieldStops.splice(0)) stop();
        body.replaceChildren();
        for (const raw of rows) body.append(buildRow(raw, fieldStops));
      };
      if (!groups) drawRows();
      results.replaceChildren(drawTable());
      redrawColumns = () => {
        const prior = results.querySelector("table");
        if (!prior) return;
        drawRows();
        prior.replaceWith(drawTable());
        settleColumns?.();
      };
      // Size the columns now, before the browser paints the content layout (columns.ts).
      settleColumns?.();
      if (tabCounts) tabs!.publish(tabCounts);
      else tabs?.finish();
      updateSelectionUI();

      if (groups) {
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
          const visible = choices.visible(), columns = 1 + visible.length;
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
        drawRows = render;
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
            el("h2", t("No matching records")),
            el("p", t("Try another search or clear the filter."))
          );
        } else if (actionHelp) {
          const helpDiv = el("div", undefined, "action-help-text");
          helpDiv.textContent = htmlFieldText(actionHelp);
          empty.append(
            el("h2", t("No {name} records yet", { name: actionTitle })),
            helpDiv
          );
        } else {
          empty.append(
            el("h2", t("No {name} records yet", { name: actionTitle })),
            el("p", canCreate ? t("Create your first {name} record with New.", { name: actionTitle }) : t("There is nothing to show here yet."))
          );
        }
        results.append(empty);
      }
    };

    try {
      for (const [, extension] of listHeaderExtensions.entries()) {
        if (extension.model !== description.model) continue;
        const header = extension.create({ model: description.model, title: actionTitle, rpc: this.rpc, context });
        listHeaders.push(header);
        headerSlot.append(header.element);
      }
      await load();
    } catch (error) { disposeHeaders(); listSidebar?.dispose(); disposeActionMenu(); disposeQuickFilters(); disposePageSize(); searchBar.dispose(); dashboard?.dispose(); throw error; }
    const remember = (id?: number): void => {
      this.listResume = { url: routeUrl(listRouteOf(route)), offset, limit: pageSize, order, id,
        scroll: documentScroller().scrollTop, left: results.scrollLeft, top: results.scrollTop,
        widths: columnWidths, wrap: results.classList.contains("list-wrap") };
    };
    return {
      dispose: () => {
        if (this.rememberList === remember) this.rememberList = undefined;
        choices.dispose(); for (const stop of fieldStops.splice(0)) stop(); disposeHeaders(); listSidebar?.dispose(); groupedModel?.dispose(); disposeActionMenu(); disposeQuickFilters(); disposePageSize(); searchBar.dispose(); dashboard?.dispose();
      },
      settle: () => { this.rememberList = remember; settleColumns?.(); },
      restore: () => {
        if (!resume) return;
        documentScroller().scrollTop = resume.scroll;
        results.scrollLeft = resume.left;
        results.scrollTop = resume.top;
        if (resume.id !== undefined) results.querySelector<HTMLButtonElement>(`[data-record-open="${resume.id}"]`)?.focus({ preventScroll: true });
      }
    };
  }

  /** Whether `route` opens an `ir.actions.client`: a kept method result, or a menu entry of that kind. */
  private isClientRoute(route: Route): boolean {
    if (!route.action) return false;
    // A report drill keeps the source menu in its URL. The method result determines
    // which view to restore when traversing that history entry.
    if (route.method_action) return route.method_action.startsWith(this.clientResultPage + ":");
    return [...this.menus.values()].some(menu => menu.kind === "client" && String(menu.action) === route.action);
  }

  private keepClientResult(result: ClientResult): string {
    const key = `${this.clientResultPage}:${++this.clientCounter}`;
    this.clientResults.set(key, result);
    if (this.clientResults.size > 64) { const oldest = this.clientResults.keys().next().value; if (oldest !== undefined) this.clientResults.delete(oldest); }
    return key;
  }

  private approvalsMenu(): Menu | undefined {
    return [...this.menus.values()].find(menu =>
      menu.module === "approvals" && menu.parentId === null);
  }

  private async openApprovals(): Promise<void> {
    if (!this.approvalsMenu()) throw new ClientError("permission", "The Approvals menu is unavailable for this user.");
    await this.openMenuAction("approvals.requests_window");
  }

  private async openMenuAction(xmlId: string): Promise<void> {
    const generation = this.generation, request = ++this.actionRequest;
    const action = object(await this.rpc.call("ir.actions.act_window", "erp_load_web_action_ref",
      { xml_id: xmlId, context: this.context }, { signal: this.lifetime.signal }));
    if (this.expired || this.lifetime.signal.aborted || request !== this.actionRequest || generation !== this.generation) return;
    if (action["wire_major"] !== 1 || action["target"] !== "current") throw new ClientError("unsupported", "Expected a current window action.");
    const id = integer(action["id"], "menu action");
    const menu = [...this.menus.values()].find(entry => entry.kind === "window" && entry.action === id);
    if (!menu) throw new ClientError("permission", "This menu is unavailable for this user.");
    await this.navigate({ action: String(id), menu: String(menu.id) });
  }

  private async loadClientAction(id: number): Promise<ClientResult> {
    const result = actionResult(await this.rpc.call("ir.actions.client", "erp_load_web_client_action", { ids: [id], wire_major: 1, context: this.context }));
    if (result.kind !== "client" || result.id !== id) throw new ClientError("server", "Mismatched client action.");
    return result;
  }

  /** Mount the registered screen of a client action into `staging`: an unregistered tag mounts nothing. */
  private async mountClientAction(route: Route, staging: HTMLElement, generation: number, stagedDisposers: (() => void)[], persisted?: ClientResult): Promise<{ panelDispose: () => void; service: ServiceController; handle: ClientActionHandle; dispose: () => void }> {
    const kept = route.method_action ? this.clientResults.get(route.method_action) : undefined;
    if (route.method_action && !kept) throw new ClientError("validation", "This client action has expired. Open its originating menu again.");
    const result = kept ?? persisted ?? await this.loadClientAction(rpcId(route.action!));
    if (!clientActions.has(result.tag)) throw new ClientError("unsupported", `Client action '${result.tag}' is declared by the server but not registered by this page. Reload the page.`);
    const factory = clientActions.get(result.tag);
    const key = route.method_action ?? `${result.tag}:${result.id}`;
    const mounted = await mountControlPanel(staging, { title: result.name, model: "" }, error => this.reportError(error));
    stagedDisposers.push(mounted.dispose);
    const handle = await factory.create({
      ...this.pageNavigation(generation, route, () => result.name),
      route: { ...route },
      ...(result.tag === DISCUSS_ACTION ? { mountDiscuss: (host: HTMLElement) => this.getDiscuss().openWorkspace(host, result.name) } : {}),
      host: staging, panel: mounted.panel, controls: mounted.actions, rpc: this.rpc, companies: this.companies, run: this.run,
      notice: this.notice, messages: this.messages, editorLocale: this.editorLocale,
      // ORM company context and the assistant dock both use the first selected company.
      userId: this.userId, currentCompanyId: this.activeCompanyIds[0]!,
      csrfToken: text(object(this.headerBootstrap["csrf"])["logout"], "session token"),
      action: { id: result.id, name: result.name, tag: result.tag, params: result.params },
      restored: this.clientStates.get(key) ?? null,
      open: (input, options) => this.openFromClientAction(input, generation, result.name, options?.sourceCompanyId),
      openWizard: async xmlId => {
        if (!this.live(generation) || this.wizard) return false;
        const action = object(await this.rpc.call("ir.actions.act_window", "erp_load_web_action_ref", { xml_id: xmlId, context: this.context }));
        if (!this.live(generation) || this.wizard) return false;
        const result = await this.normalizeWizardAction(action);
        if (!this.live(generation) || this.wizard) return false;
        const dialog = await this.openWindowDialog(result, undefined, generation, async () => {}, undefined, this.context);
        return dialog ? dialog.completion : false;
      },
      openDraft: async (xmlId, defaults) => {
        if (generation !== this.generation || this.expired) throw new ClientError("validation", "Reopen this screen before creating a draft.");
        const context: Record<string, unknown> = {};
        for (const [field, value] of Object.entries(defaults)) {
          if (!/^[a-z][a-z0-9_]*$/.test(field) || typeof value !== "string") throw new ClientError("validation", "Invalid draft default.");
          context[`default_${field}`] = value;
        }
        await this.openActionRequest({ xmlId, viewMode: "form", domain: [], context }, route, result.name);
      }
    });
    let disposed = false;
    const dispose = (): void => { if (disposed) return; disposed = true; handle.dispose(); };
    stagedDisposers.push(dispose);
    this.pendingClient = { key, name: result.name, handle };
    const service: ServiceController = {
      busy: () => handle.edits?.busy() ?? false,
      dirty: () => handle.edits?.dirty() ?? false,
      save: async () => { await handle.edits?.save(); },
      discard: () => handle.edits?.discard(),
      dispose,
    };
    return { panelDispose: mounted.dispose, handle, dispose, service };
  }

  /** Keep what the screen being left exports, bounded; a larger state is dropped with a notice and the screen restarts from its defaults. */
  private captureClientState(): void {
    const mounted = this.clientMounted;
    if (!mounted?.handle.captureState) return;
    try {
      const state = mounted.handle.captureState();
      if (state === undefined) return;
      if (new TextEncoder().encode(JSON.stringify(state)).length > MAX_CLIENT_STATE_BYTES) {
        this.clientStates.delete(mounted.key);
        this.messages.notifications.add(`${mounted.name} could not keep its place: its state is over ${MAX_CLIENT_STATE_BYTES} bytes.`, { level: "warning" });
        return;
      }
      this.clientStates.set(mounted.key, structuredClone(state));
      if (this.clientStates.size > 32) { const oldest = this.clientStates.keys().next().value; if (oldest !== undefined) this.clientStates.delete(oldest); }
    } catch (error) { this.reportError(error); }
  }

  /** A source drilldown stays in the company which produced the report, including after reload. */
  private clientSourceContext(sourceCompanyId?: number): Record<string, unknown> {
    if (sourceCompanyId === undefined) return this.context;
    if (sourceCompanyId !== this.activeCompanyIds[0]) throw new ClientError("validation", "Reopen the source in its original company.");
    return { ...this.context, allowed_company_ids: [sourceCompanyId] };
  }

  /** `open(result)` of a client action screen: a related list or another client action, through the normal navigation. */
  private async openFromClientAction(input: unknown, generation: number, originLabel: string, sourceCompanyId?: number): Promise<void> {
    if (generation !== this.generation || this.expired) throw new ClientError("validation", "This screen is no longer the open action. Open it again.");
    const context = this.clientSourceContext(sourceCompanyId);
    const normalized = actionResult(await this.rpc.call("res.users", "erp_normalize_action_result", { wire_major: 1, result: input, context }));
    if (generation !== this.generation || this.expired) return;
    if (sourceCompanyId !== undefined && normalized.kind !== "current_window") throw new ClientError("validation", "The source must open in the current window.");
    const origin = { ...this.current };
    if (normalized.kind === "current_window") {
      const id = this.methodActions.add({ result: normalized, context: { ...context, ...normalized.context }, origin, originLabel,
        ...(sourceCompanyId === undefined ? {} : { sourceCompanyId }) });
      try {
        await this.navigate({ action: origin.action, menu: origin.menu, cids: origin.cids, method_action: id, model: normalized.model,
          ...normalized.res_id === undefined ? { view_type: "list" } : { id: String(normalized.res_id), view_type: "form" } },
          false, false, undefined, normalized.res_id === undefined ? undefined : { stack: true });
      } catch (error) { this.methodActions.remove(id); throw error; }
      if (this.current.method_action !== id) this.methodActions.remove(id);
      return;
    }
    if (normalized.kind === "window" && normalized.res_id !== null) {
      await this.openWindowDialog(normalized, undefined, generation, async () => {
        this.captureClientState();
        await this.navigate(origin, true, true);
      }, undefined, this.context);
      return;
    }
    if (normalized.kind === "client") {
      const key = this.keepClientResult(normalized);
      try { await this.navigate({ action: origin.action, menu: origin.menu, cids: origin.cids, method_action: key }); }
      catch (error) { this.clientResults.delete(key); throw error; }
      return;
    }
    throw new ClientError("unsupported", `This client action cannot open the requested ${normalized.kind} result.`);
  }

  /** Opens a record a form links to, on top of that form: its crumb leads back to it. The server resolves the action
   *  among the caller's menus; normal navigation rechecks access and dirty state. */
  private async openRelatedRecord(model: string, id: number): Promise<void> {
    const generation = this.generation;
    const target = await this.relatedTarget(model, id);
    // A later user navigation cancels this read-only resolution.
    if (generation !== this.generation || this.expired) return;
    if (!target) throw new ClientError("unsupported", `No accessible form action is available for ${model}.`);
    await this.navigate({ action: String(target.action), menu: String(target.menu), model, id: String(id), view_type: "form" },
      false, false, undefined, { stack: true });
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
    await this.moveInList(route, target, context, next);
  }

  /** Leaves a new record for the last record of the list it was created from. A new record goes after that record, so
   *  once `saved` it joins the list there and the pager leads back to it. */
  private async stepFromNew(route: Route, saved: number | undefined): Promise<void> {
    const context = this.listContext;
    if (!context) throw new ClientError("validation", "The record is no longer in the list.");
    let { ids, offset, total } = context, reloaded = false;
    if (offset + ids.length < total) {
      if (!context.page) throw new ClientError("validation", "There is no record in that direction.");
      offset = Math.max(0, total - context.limit);
      ({ ids, total } = await context.page(offset));
      if (offset + ids.length < total) {
        offset = Math.max(0, total - context.limit);
        ({ ids, total } = await context.page(offset));
      }
      reloaded = true;
    }
    // A reloaded page may already hold the saved record wherever the list's order puts it: it goes last all the same.
    ids = ids.filter(id => id !== saved);
    const target = ids[ids.length - 1];
    if (target === undefined) throw new ClientError("validation", "There is no record in that direction.");
    const nextIds = saved === undefined ? ids : [...ids, saved];
    const baseTotal = reloaded ? total : total + (saved === undefined ? 0 : 1);
    const nextTotal = Math.max(baseTotal, offset + nextIds.length);
    const next = { ...context, ids: nextIds, offset, total: nextTotal };
    await this.moveInList(route, target, context, next);
  }

  /** Opens record `target` of the list `next`; the list stays `context` when the form does not arrive there. */
  private async moveInList(route: Route, target: number, context: ListContext, next: ListContext): Promise<void> {
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
      await source.close(true); return;
    }
    if (result.kind === "report") {
      // The print request checks the report and the records again; a dialog that asked for it is done. It closes first:
      // closing it closes the dialogs above it, and the progress dialog of a large print must stay open.
      if (source) await source.close(true);
      await printRecords({ rpc: this.rpc, dialogs: this.messages.dialogs, report: error => this.reporter.report("render", error) }, { report: result.report, name: result.name }, result.model, result.ids);
      if (!source) await refresh();
      return;
    }
    if (result.kind === "close" || result.kind === "none") {
      if (source) { await source.close(true); return; }
      if (result.kind === "close") throw new ClientError("validation", "Invalid transition: no action dialog to close.");
      await refresh(); return;
    }
    if (result.kind === "current_window") {
      // From a dialog: the dialog closes first (as a link inside it does), then the result opens on the main page, on
      // top of the page under the dialog. A dialog that stays open (it may not be left) opens nothing.
      if (source) {
        if (!await source.canLeave(true)) return;
        await source.close(true);
        if (this.wizard === source || !this.live(binding)) return;
      }
      const invocation = source ? undefined : this.pendingInvocation;
      if (!source && (!invocation || invocation.caller !== caller || invocation.generation !== binding)) throw new ClientError("validation", "The action's source is no longer available for navigation.");
      const context = { ...callerContext, active_model: caller.model, active_id: caller.id,
        active_ids: caller.id === undefined ? [] : [caller.id], ...result.context };
      const origin = { ...this.current };
      // The way back names the page the result opens from: its record, or its own crumb (a dialog the user menu opened
      // has no caller on the page, and its own record is not that page).
      const page = this.record, name = page?.values["display_name"] ?? page?.values["name"];
      const originLabel = typeof name === "string" && name ? name : page ? page.id === undefined ? "Record" : `#${page.id}`
        : this.pageCrumb()?.title ?? this.clientMounted?.name ?? "Previous page";
      const id = this.methodActions.add({ result, context, origin, originLabel });
      try {
        // One record opens straight in its form, on top of the page the button is on.
        await this.navigate({ action: origin.action, menu: origin.menu, cids: origin.cids, method_action: id, model: result.model,
          ...result.res_id === undefined ? { view_type: "list" } : { id: String(result.res_id), view_type: "form" } },
          false, false, invocation, result.res_id === undefined ? undefined : { stack: true });
      } catch (error) {
        this.methodActions.remove(id);
        // The method has already completed. Reload its known result without repeating it,
        // then report destination failure while retaining the mounted source. A closed dialog refreshed its page already.
        if (!source) await refresh();
        throw error;
      }
      if (source && this.current.method_action !== id) this.methodActions.remove(id);
      return;
    }
    if (result.kind === "client") {
      if (source) throw new ClientError("unsupported", "A dialog cannot open a client action yet.");
      const invocation = this.pendingInvocation;
      if (!invocation || invocation.caller !== caller || invocation.generation !== binding) throw new ClientError("validation", "The action's source is no longer available for navigation.");
      const origin = { ...this.current }, key = this.keepClientResult(result);
      try { await this.navigate({ action: origin.action, menu: origin.menu, cids: origin.cids, method_action: key }, false, false, invocation); }
      catch (error) { this.clientResults.delete(key); await refresh(); throw error; }
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

  private async normalizeWizardAction(action: Record<string, unknown>): Promise<Extract<ActionResult, { kind: "window" }>> {
    if (action["wire_major"] !== 1 || action["target"] !== "new") {
      throw new ClientError("unsupported", "A wizard requires a modal window action.");
    }
    const defaults = this.actionContext(action);
    const { context } = splitSearchDefaults({ ...defaults, ...this.context });
    if (this.actionDomain(action, context).length) throw new ClientError("unsupported", "A dialog action cannot carry a domain.");
    const model = text(action["model"], "action model");
    const normalized = actionResult(await this.rpc.call(model, "erp_normalize_action_result", {
      wire_major: 1,
      result: { type: "ir.actions.act_window", name: text(action["name"], "action name"), res_model: model,
        target: "new", views: actionViews(action), context: defaults },
      context: this.context,
    }));
    if (normalized.kind !== "window") throw new ClientError("unsupported", "The wizard action did not return a modal form.");
    return normalized;
  }

  /** Open a `target="new"` window action as the one action dialog. `caller` is the record whose
   *  button produced the action; the user menu opens one without a caller record, and a result
   *  handed back from that dialog then names the dialog's own record as its caller. `onResult`
   *  runs after the dialog's own method result was consumed (the preferences dialog reloads the
   *  page there). */
  private async openWindowDialog(result: Extract<ActionResult, { kind: "window" }>, caller: RecordState | undefined, binding: Binding, refresh: () => Promise<void>, source: WizardDialog | undefined, callerContext: Record<string, unknown>, onResult?: (result: ActionResult, button: string | undefined) => void, confirmed?: Resubmission, profileTitle = false): Promise<WizardDialog | undefined> {
    const previous = this.wizard;
    if (previous && ((previous !== source && previous.busy) || !await previous.canLeave(previous === source))) return;
    const context = { ...callerContext, ...(caller ? { active_model: caller.model, active_id: caller.id,
      active_ids: caller.id === undefined ? [] : [caller.id] } : {}), ...result.context };
    const description = describeViews(await this.viewLoader(result.model, result.views.map((view): [number | null, string] => [view.id, view.type]), context), result.model, this.registry);
    if (!this.live(binding)) return;
    const view = description.views.get("form");
    if (!view || view.mode !== "record") throw new ClientError("unsupported", "A wizard requires a record form.");
    await attachSubviews(description, view, async (relation, refs) => describeViews(await this.viewLoader(relation, [[null, "list"], [null, "form"]], refs ? { ...context, ...refs } : context), relation, this.registry));
    if (!this.live(binding)) return;
    const state = new RecordState(this.rpc, result.model, view.fields, context);
    await state.load(result.res_id ?? undefined);
    if (!this.live(binding) || this.wizard !== previous) return;
    let scope: WizardDialog;
    scope = new WizardDialog({ actorName: this.userName, profileTitle, numericLocale: this.numericLocale, name: result.name, state, view, rpc: this.rpc, context,debug:this.debug,uid:this.userId,
      action:(id,effective)=>this.declaredButtonAction(id,state,view.fields,effective,binding,refresh,scope),
      url:href=>runUrlButton(href,{baseUrl:location.href,navigate:async route=>{if(!await scope.canLeave(true))return;await scope.close();await this.navigate(route);},openExternal:(url,target,features)=>{window.open(url,target,features);}}),
      openRecord: async (model, id) => { if (this.live(binding)) await this.openRelatedRecord(model, id); }, views: this.viewLoader,
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
    return scope;
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

  private formView(staging: HTMLElement, controls: HTMLElement, panel: HTMLElement, actionTitle: string, state: RecordState, view: View, route: Route, availableViews?: [number | null, string][], context = this.context, stack = false): () => void {
    const titleField = view.fields.find(f => f.name === "name" && f.kind === "char") || view.fields.find(f => f.kind === "char");
    // Built while its navigation stages, so the current generation is this form's.
    const fieldNavigation = this.pageNavigation(this.generation, route, () => {
      const title = titleField ? state.values[titleField.name] : undefined;
      return typeof title === "string" && title.trim() ? title : actionTitle;
    });
    let surface: RecordForm | undefined;
    let headingObserver: MutationObserver | undefined;
    let formHotkeys: ReturnType<typeof createHotkeyController> | undefined;
    let scalarChange: Promise<void> | undefined;
    let queuedAction: (() => Promise<void>) | undefined;
    let createButton: HTMLButtonElement | undefined;
    const runFormAction = (job: () => Promise<void>): void => {
      if (this.lifetime.signal.aborted || binding !== this.generation || this.expired) return;
      if (scalarChange) {
        queuedAction = job;
        return;
      }
      this.run(job);
    };
    const debounce=new ButtonDebouncer();
    const hotkeys = new ButtonHotkeys(
      () => {
        const buttons = surface?.controls().filter((control): control is HTMLButtonElement => control instanceof HTMLButtonElement) ?? [];
        if (createButton) buttons.push(createButton);
        return buttons;
      },
      { allowEditable: true, blocked: () => this.keyboardPending !== undefined || this.busy && !scalarChange || queuedAction !== undefined ||
          this.actionPending || this.uncertainMethod || state.writeUncertain || this.messages.block.blocked, report: error => this.hotkeyError(error) }
    );
    let disposed = false;
    const dispose = (): void => { if (disposed) return; disposed = true; headingObserver?.disconnect(); formHotkeys?.dispose(); hotkeys.dispose(); debounce.dispose(); surface?.dispose(); };

    panel.classList.add("control-panel-form");
    const toolsArea = panel.querySelector("[data-control-tools]");
    if (toolsArea && availableViews && availableViews.length > 1) {
      const viewSwitcher = el("div", undefined, "view-switcher");
      for (const [, vType] of availableViews) {
        if (switchable(vType)) {
          const vBtn = button(vType.charAt(0).toUpperCase() + vType.slice(1), () => runFormAction(async () => {
            await this.navigate(switchedRoute(route, vType as SwitchableView));
          }), `view-btn ${vType === "form" ? "active" : ""}`);
          vBtn.setAttribute("aria-label", `${vType} view`);
          decorateViewButton(vBtn, vType as SwitchableView);
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
    // A read-first form (`edit_mode="read"`) shows its saved record as read values with Edit; Save or Discard return to it.
    const editBtn = button(t("Edit"), () => runFormAction(edit), "primary status-btn-edit");
    editBtn.hidden = true; editBtn.title = "Edit (E)";
    const statusIndicator = el("div", undefined, "form-status-indicator");
    // A field can commit on blur between pressing and releasing Save. A newly visible badge
    // comes after the commands so it does not move the button away from the pointer.
    statusIndicator.append(editBtn, statusActions, statusBadge, archivedBadge);

    const canEdit = view.canEdit !== false && !isFalsy(context["edit"]);
    const readFirst = view.editMode === "read" && canEdit;
    if (!canEdit) this.lockedRecords.add(state);
    const hasLocalEdit = (): boolean => surface?.hasLocalEdits() === true;
    const updateStatus = (): void => {
      const dirty = state.dirty || hasLocalEdit();
      editBtn.hidden = surface?.mode !== "read";
      if (surface?.mode === "read") {
        statusBadge.hidden = true;
        statusActions.style.display = "none";
      } else if ((dirty || !state.id) && (canEdit || !state.id)) {
        statusBadge.className = "status-badge dirty";
        statusBadge.textContent = dirty ? "Unsaved changes" : "New";
        statusBadge.hidden = !dirty;
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

      // A saved new record's crumb opens that record, not another new form.
      const here: Route = state.id === undefined ? { ...route } : { ...route, id: String(state.id), view_type: "form" };
      const crumb = { id: crumbId(route, state.id ?? "new"), title: currentName, route: here, url: routeUrl(here), isFormView: true };
      // A form already in the trail (its crumb, browser Back) cuts the trail back to itself.
      const returning = this.breadcrumbs.items.some(item => item.id === crumb.id);
      if (returning) this.breadcrumbs.truncateTo(crumb.id);
      // A record opened from a form goes on top of that form, once: later renders (after a save) replace it. Only on the
      // form being left: a form crumb left over under a client screen or a service page is not that page.
      const oneRecord = stack;
      const stacked = oneRecord && !returning && this.breadcrumbs.current?.isFormView === true && this.pageCrumb() !== undefined;
      stack = false;
      if (stacked) this.breadcrumbs.push(crumb);
      else {
        if (this.breadcrumbs.items.length === 0 || this.breadcrumbs.current?.route.action !== route.action) {
          this.breadcrumbs.clear();
          // A one-record result that starts the trail gets the action's crumb only when that crumb opens a list (or another
          // multi-record view), not a blank form.
          if (!oneRecord || !availableViews || availableViews.some(([, type]) => type === "list") || availableViews[0]?.[1] !== "form") {
            this.breadcrumbs.push({ id: crumbId(route), title: actionTitle, route: listRouteOf(route), url: routeUrl(listRouteOf(route)), isFormView: false });
          }
        }
        if (this.breadcrumbs.current?.isFormView) this.breadcrumbs.replaceCurrent(crumb);
        else this.breadcrumbs.push(crumb);
      }

      const isSmall = document.documentElement.dataset["mobile"] === "true";
      renderBreadcrumbs(breadcrumbs, this.breadcrumbs, {
        isSmall,
        onNavigate: async item => this.run(() => this.openCrumb(item, updateBreadcrumbs)),
        report: error => this.reportError(error)
      });
      // A form opened from a related list keeps the way back to the record whose button opened it.
      // It is left out when the
      // back link already leads to that page: a record reached without its list, by the browser's Forward for one.
      const origin = route.method_action ? this.methodActions.get(route.method_action) : undefined;
      const back = this.breadcrumbs.previous?.route;
      // The same view too: the list of an action is not its pivot or chart as it was left.
      const backIsOrigin = !!origin && !!back && !back.method_action && back.action === origin.origin.action && back.id === origin.origin.id
        && back.view_type === origin.origin.view_type;
      // A trail that already holds the source record (the list went on top of it) shows it there.
      const originInTrail = !!origin && this.breadcrumbs.items.some(item => item.id === crumbId(origin.origin, origin.origin.id ?? "new"));
      const previous = origin && !backIsOrigin && !originInTrail ? breadcrumbs.querySelector(".breadcrumb-previous") : null;
      if (origin && previous) {
        const divider = el("span", "›", "form-breadcrumb-divider"); divider.setAttribute("aria-hidden", "true");
        previous.prepend(this.originCrumb(origin), divider);
      } else if (origin && !backIsOrigin && !originInTrail) {
        // A record opened straight from a page that keeps no crumb of its own (a report line's journal entry) is the only
        // item of its trail: without this link nothing on the page leads back to where the user came from.
        const current = breadcrumbs.querySelector<HTMLElement>(".breadcrumb-current");
        if (current) {
          const trail = el("div", undefined, "form-breadcrumb-path"), divider = el("span", "›", "form-breadcrumb-divider");
          divider.setAttribute("aria-hidden", "true");
          current.replaceWith(trail);
          current.classList.add("form-breadcrumb-record");
          trail.append(this.originCrumb(origin), divider, current);
        }
      }
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
        } else if (this.listContext) {
          // A new record goes after the last record of its list: Previous saves a changed one and leads to that record.
          pagerCount.textContent = "New";
          prevBtn.disabled = this.listContext.total === 0;
          nextBtn.disabled = true;
          prevBtn.onclick = () => this.run(async () => {
            if (state.dirty && !await trySave()) return;
            await this.stepFromNew(route, state.id);
          });
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
    const syncDraftActions = (): void => {
      if (state.id && canEdit && !state.dirty && surface?.mode !== "read" && hasLocalEdit() !== (statusActions.style.display !== "none")) updateStatus();
    };
    viewport.addEventListener("input", syncDraftActions);
    viewport.addEventListener("keydown", event => { if (event.key === "Escape") syncDraftActions(); });
    panel.classList.toggle("has-chatter", hasChatter); panel.classList.toggle("has-aside", hasAside);
    viewport.append(headerHost);
    viewport.append(sheet);
    if (asideHost) viewport.append(asideHost);
    if (chatterHost) viewport.append(chatterHost);
    if (hasChatter) {
      const workspace = el("div", undefined, "record-form-workspace");
      workspace.append(viewport);
      staging.append(workspace);
    } else staging.append(viewport);

    // A declared title keeps its XML placement. A legacy form instead names the page in its breadcrumb.
    syncPageHeading = (): void => {
      // The addon's context line reads as helper text under the title, wherever a render put the title.
      const titleBlock = form.querySelector(".form-title-block");
      if (contextLine && titleBlock && titleBlock.nextElementSibling !== contextLine) titleBlock.after(contextLine);
      // A render without a title block took the line away with it: it returns to the top of the sheet.
      else if (contextLine && !titleBlock && !sheet.contains(contextLine)) sheet.insertBefore(contextLine, form);
      const current = breadcrumbs?.querySelector<HTMLElement>(".breadcrumb-current");
      if (!current) return;
      const heading = [...form.querySelectorAll("h1")].find(candidate => !candidate.closest("[hidden]"));
      if (heading?.classList.contains("form-title-heading") && current.textContent) {
        heading.setAttribute("aria-label", current.textContent);
      }
      retagCrumb(current, heading ? "span" : "h1");
    };
    headingObserver = new MutationObserver(() => syncPageHeading?.());
    headingObserver.observe(form, { childList: true, subtree: true, attributes: true, attributeFilter: ["hidden"] });

    const focusTarget = (element: Element | null): string | undefined => {
      if (element instanceof HTMLElement && headerHost.contains(element)) return element.dataset["focusKey"];
      return formFocus(form, element);
    };
    let releaseRebuiltButtons: (() => void) | undefined;
    const render = (focus = surface?.focus()): void => {
      if (disposed || this.lifetime.signal.aborted || this.expired) return;
      releaseRebuiltButtons?.(); releaseRebuiltButtons = undefined;
      surface!.render(focus); surface!.setDisabled(runningButton || this.uncertainMethod); updateStatus(); syncPageHeading?.();
      // Saving during an action can replace widgets and overflow controls while the action still runs.
      if (runningButton) releaseRebuiltButtons = lockButtonControls(surface!.controls().filter((control): control is HTMLButtonElement => control instanceof HTMLButtonElement));
      if (this.uncertainMethod) {
        surface!.setDisabled(true);
        for (const control of surface!.controls()) control.disabled = true;
        // The record menu lives in the breadcrumb in the default theme, outside `controls`.
        for (const control of [...controls.querySelectorAll<HTMLButtonElement>("button"), ...actionMenu.querySelectorAll<HTMLButtonElement>("button"), ...(recordMenu?.querySelectorAll<HTMLButtonElement>(".print-dropdown button") ?? [])]) if (!control.matches(".action-toggle-btn,.form-reload-btn")) control.disabled = true;
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
        // An action can hold the form through a draft commit. Validation must see
        // enabled inputs and be able to focus the first missing field.
        surface!.setDisabled(false);
        if (!surface!.valid()) { recordEvent({ type: "saveFailed" }); return false; }
        surface!.setDisabled(true);
        // A refusal that names fields is shown on those fields; only an error that names none reaches the error dialog.
        try { await state.save(); } catch (error) {
          const flagged = surface!.flagServerError(error);
          if (!this.lifetime.signal.aborted && binding === this.generation) recordEvent({ type: "saveFailed" });
          if (flagged) return false;
          throw error;
        }
        if (this.lifetime.signal.aborted || binding !== this.generation) return true;
        const toRead = readMode(true);
        render();
        published();
        updateStatus(); this.notice("Record saved.");
        recordEvent({ type: "saved", id: state.id! });
        if (toRead) recordEvent({ type: "modeChanged", mode: "read" });
        return true;
      } finally {
        this.busy = false;
        if (!this.lifetime.signal.aborted && binding === this.generation) surface!.setDisabled(runningButton || this.uncertainMethod);
      }
    };
    const save = async (): Promise<void> => { await trySave(); };
    /** The record was saved (here, by the leave prompt or by a breadcrumb's autosave): a new one gained its identity outside
     *  a navigation, so the page's address, the systray and the trail carry it, and a page stacked next leads back to it. */
    const published = (): void => {
      if (binding !== this.generation || this.expired || state.id === undefined) return;
      this.current = { ...route, id: String(state.id), view_type: "form" };
      this.currentUrl = routeUrl(this.current);
      this.syncSystrayRecord();
      // A step through the browser's history has already moved the browser to its entry: this one keeps its address.
      if (!this.traversalRunning) history.replaceState({ erpIndex: this.historyIndex }, "", this.currentUrl);
      updateBreadcrumbs();
      this.saveTrail();
    };
    this.savedRecords.set(state, published);
    /** Widgets hear of saves, discards and mode changes once the form has rendered the outcome. */
    const recordEvent = (event: RecordEvent): void => { if (!disposed && surface) surface.emitRecordEvent(event); };
    /** Switch a read-first form; false when nothing changed (an ordinary form, or already in that mode). */
    const readMode = (read: boolean): boolean => {
      if (!readFirst || !surface || state.id === undefined || surface.mode === (read ? "read" : "edit")) return false;
      surface.setReadMode(read);
      return true;
    };
    const edit = async (): Promise<void> => {
      refuseUncertain();
      if (!readMode(false)) return;
      render(); updateStatus();
      recordEvent({ type: "modeChanged", mode: "edit" });
      // The first editable control takes the focus the Edit button had.
      form.querySelector<HTMLElement>("input:not([disabled]):not([type=hidden]),textarea:not([disabled]),select:not([disabled])")?.focus();
    };

    const discard = async (): Promise<void> => {
      state.discard();
      if (!state.id) await this.navigate({ ...route, id: undefined, view_type: "list" });
      else {
        const toRead = readMode(true);
        render();
        updateStatus();
        recordEvent({ type: "discarded" });
        if (toRead) recordEvent({ type: "modeChanged", mode: "read" });
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
      control.disabled = control.disabled || this.uncertainMethod || failure !== undefined;
      if (failure !== undefined) control.title = `Unavailable: ${failure}`;
      parent.append(control);
      return control;
    };
    const clickButton = async (node: ButtonNode): Promise<void> => {
      if (runningButton || this.uncertainMethod || binding !== this.generation || this.expired) return;
      // An onchange may have hidden the button since its click was queued.
      if (buttonHidden(node, view.fields, state.values)) return;
      const focus = focusTarget(document.activeElement);
      let saveRefused = false;
      runningButton = true; this.actionPending = true; surface!.setDisabled(true);
      const invocation = { caller: state, generation: binding };
      this.pendingInvocation = invocation;
      const releaseButtons = lockButtonControls([
        ...surface!.controls().filter((control): control is HTMLButtonElement => control instanceof HTMLButtonElement),
        ...panel.querySelectorAll<HTMLButtonElement>("button")
      ]);
      try {
        await runObjectButton(node, {
          model: state.model, context: context,uid:this.userId, fields:view.fields,values:()=>state.values,id: () => state.id, dirty: () => state.dirty || hasLocalEdit(),
          block:()=>this.messages.block.begin(),
          action:(id,effective)=>this.declaredButtonAction(id,state,view.fields,effective,binding,async()=>{await state.load(state.id);if(binding===this.generation&&!this.expired)render(focus);}),
          url:href=>runUrlButton(href,{baseUrl:location.href,navigate:route=>this.navigate(route,false,false,invocation),openExternal:(url,target,features)=>{window.open(url,target,features);}}),
          save: async () => { const saved = await trySave(); saveRefused = !saved; return saved; },
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
          confirm: async (title, message, accept, cancel="Cancel") => await confirmChoice({title,message,accept,cancel,signal:this.lifetime.signal,service:this.messages.dialogs}) && binding === this.generation && !this.expired,
          prompt: async (prompt, title) => {
            const answer = await askButtonPrompt({ title, prompt, service: this.messages.dialogs, signal: this.lifetime.signal });
            return binding === this.generation && !this.expired ? answer : undefined;
          }
        });
      } finally {
        if (this.pendingInvocation === invocation) this.pendingInvocation = undefined;
        runningButton = false; this.actionPending = false;
        // End this invocation's transient lock. The render below reapplies uncertainty to writes,
        // while the persistent read-only Reload control remains available for recovery.
        releaseButtons();
        releaseRebuiltButtons?.(); releaseRebuiltButtons = undefined;
        if (binding !== this.generation || this.expired) return;
        if (!this.wizard) {
          // Validation already marked and focused the mounted fields. Rebuilding would lose that feedback.
          if (saveRefused) surface!.setDisabled(false);
          else render(focus);
          updateBreadcrumbs(); updateStatus();
        }
      }
    };

    controls.replaceChildren();
    const canCreate = view.canCreate !== false && !isFalsy(context["create"]);
    if (canCreate) {
      createButton = actionButton("New", () => runFormAction(() => this.navigate({ ...route, id: undefined, view_type: "form" })));
      controls.append(createButton);
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
    // Print is its own button before Actions (not mixed with record actions). A dirty or new record is saved first,
    // as the reference client does before a menu item runs.
    // A model without a report has no Print button.
    const print = printDropdown(this.rpc, state.model, report => runFormAction(async () => {
      refuseUncertain();
      if ((state.dirty || !state.id) && !await trySave()) return;
      if (!state.id) throw new ClientError("validation", "Save the record before printing it.");
      await printRecords({ rpc: this.rpc, dialogs: this.messages.dialogs, report: error => this.reporter.report("render", error) }, report, state.model, [state.id]);
    }), error => this.reportError(error), () => this.reportsOf(state.model));
    const recordMenus = el("div", undefined, "form-record-menus");
    recordMenus.append(print.node, actionDropdown);
    const actionMenuDropdown = attachMenuDropdown({ trigger: actionToggle, menu: actionMenu, mode: "class-open", onOpen: () => { duplicateBtn.hidden = !state.id || !canCreate; } });
    recordMenu = recordMenus; updateBreadcrumbs();
    if (smartHost) panel.append(smartHost);

    const stopActionMenu = (): void => { print.dispose(); actionMenuDropdown.dispose(); };

    form.addEventListener("submit", event => {
      event.preventDefault();
      runFormAction(save);
    });

    surface = new RecordForm({
      composeEmail: () => runFormAction(() => clickButton({ kind: "button", name: "action_open_external_email", label: "Email", variant: "secondary", confirm: null, invisible: null })),
      numericLocale: this.numericLocale, signal: this.lifetime.signal, form, state, view, headerHost, smartHost, asideHost, chatterHost, rpc: this.rpc, context: context,
      canEdit, recordEvents: true,
      ...(this.threadReconnect && { threadSocket: this.threadReconnect }),
      messagingStore: this.getMessagingStore(), uid: this.userId, userName: this.userName,
      run: this.run, report: error => this.reportError(error), messages: this.messages, editorLocale: this.editorLocale,
      notify: message => this.messages.notifications.add(message, { level: "warning" }),
      // A link clicked while a field's change is still on its way (a line dialog commits its line, then opens the link)
      // opens once that change has landed, so leaving the form asks about it; a change that failed reports itself and opens nothing.
      openRecord: async (model, id) => {
        if (scalarChange) { try { await scalarChange; } catch { return; } }
        await this.openRelatedRecord(model, id);
      }, views: this.viewLoader, button: renderButton, navigation: fieldNavigation,
      changeValues: (changes, focus) => this.run(async () => {
        if (this.uncertainMethod || this.expired) throw new ClientError("ambiguous", uncertainMethodMessage);
        const changeGeneration = this.generation;
        let changeOk = false;
        const work = (async () => {
          if (surface!.needsCommit()) await surface!.commit();
          if (this.lifetime.signal.aborted || changeGeneration !== this.generation) return;
          state.changeMany(changes); updateStatus(); this.busy = true; surface!.setDisabled(true);
          const locked: (HTMLInputElement | HTMLSelectElement | HTMLTextAreaElement)[] = [];
          for (const input of surface!.controls()) if (!(input instanceof HTMLButtonElement) && !input.disabled) { input.disabled = true; locked.push(input); }
          let warning: typeof state.warningDetails;
          try {
            await state.onchange(Object.keys(changes));
            warning = state.warningDetails;
            changeOk = true;
          }
          finally {
            this.busy = false;
            for (const input of locked) input.disabled = false;
            surface!.setDisabled(false);
            if (!this.expired) render(surface!.focus() || focus);
          }
          if (!this.expired && warning) this.messages.warning(warning);
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
        let warning: typeof state.warningDetails;
        try { await state.onchange(name); warning = state.warningDetails; }
        finally { if (!this.expired) render(surface!.focus() || focus); }
        if (!this.expired && warning) this.messages.warning(warning);
      },
      change: (name, value, focus) => this.run(() => {
        const changeGeneration = this.generation;
        let changeOk = false;
        const work = (async () => {
          if (this.uncertainMethod) throw new ClientError("ambiguous", uncertainMethodMessage);
          if (surface!.needsCommit()) await surface!.commit();
          if (this.lifetime.signal.aborted || changeGeneration !== this.generation) return;
          state.change(name, value); updateStatus(); this.busy = true; surface!.setDisabled(true);
          const locked: (HTMLInputElement | HTMLSelectElement | HTMLTextAreaElement)[] = [];
          for (const input of surface!.controls()) if (!(input instanceof HTMLButtonElement) && !input.disabled) { input.disabled = true; locked.push(input); }
          let warning: typeof state.warningDetails;
          try {
            await state.onchange(name);
            warning = state.warningDetails;
            changeOk = true;
          }
          finally {
            this.busy = false;
            for (const input of locked) input.disabled = false;
            if (!this.lifetime.signal.aborted) { surface!.setDisabled(false); render(surface!.focus() || focus); }
          }
          if (!this.lifetime.signal.aborted && !this.expired && warning) this.messages.warning(warning);
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
      if (readFirst && state.id !== undefined) surface.setReadMode(true);
      render();
      formHotkeys = createHotkeyController(document.body, {
        blocked: () => this.keyboardPending !== undefined || this.busy || this.actionPending || this.uncertainMethod || state.writeUncertain || this.messages.block.blocked,
        report: error => this.hotkeyError(error),
        bindings: (): HotkeyBinding[] => {
          if (surface!.mode === "read") return [
            // E edits a read-first record; typing in a field (the chatter composer, a search box) is never taken.
            { kind: "callback", chord: "e", target: editBtn, hint: "e", run: () => runFormAction(edit) },
          ];
          if (!canEdit && state.id) return [];
          // An editor can restore its draft with a non-bubbling input event during a render.
          if (statusActions.style.display === "none" && hasLocalEdit()) updateStatus();
          const modifier = actionHotkeyModifier(document);
          return [
            { kind: "callback", chord: modifier + "+s", target: saveBtn, allowEditable: true, run: async () => {
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
            { kind: "callback", chord: modifier + "+j", target: discardBtn, allowEditable: true, run: async () => { this.keyboardPending = "discard"; try { await discard(); } finally { this.keyboardPending = undefined; } } }
          ];
        }
      });
    }
    catch (error) { dispose(); stopActionMenu(); throw error; }
    return () => { dispose(); stopActionMenu(); };
  }
}
