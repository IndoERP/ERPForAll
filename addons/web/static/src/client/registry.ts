/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
import { UIError, mount, unmount, type ComponentClass, type TemplateModule } from "@erpforall/elang";
import type { MessageServices } from "./messages.js";
import type { EditorLocale } from "./datetime-values.js";
import type { CurrencyDisplayCache } from "./field-currency.js";
import type { NumericLocale } from "./numeric-values.js";
import type { Field } from "./metadata.js";
import type { Company } from "../generated/human-ui.js";
import type { SettingsNavigation } from "./settings-items.js";
import type { ServiceController } from "./services.js";
import { ClientError, type RpcClient } from "./transport.js";
import type { Route } from "./route.js";
import type { CogMenuItem } from "./cog-menu.js";
import type { SystrayItem } from "./systray.js";
import type { CommandProvider, CommandCategory, CommandSetup } from "./command-palette.js";
import type { UserMenuItemFactory } from "./user-menu.js";
import { type DebugSection, type DebugItemProvider, DEFAULT_DEBUG_SECTIONS } from "./debug-menu.js";
export class Registry<T> {
  private readonly values = new Map<string, T>();
  constructor(private readonly category: string) {}
  add(key: string, value: T): void {
    if (!/^[a-zA-Z][a-zA-Z0-9_.]*$/.test(key)) throw new ClientError("validation", `Invalid ${this.category} identity.`);
    if (this.values.has(key)) throw new ClientError("validation", `Duplicate ${this.category}: ${key}.`);
    this.values.set(key, value);
  }
  get(key: string): T {
    const value = this.values.get(key);
    if (value === undefined) throw new ClientError("unsupported", `Unknown ${this.category}: ${key}.`);
    return value;
  }
  entries(): readonly (readonly [string, T])[] { return [...this.values.entries()]; }
  has(key: string): boolean { return this.values.has(key); }
  remove(key: string): boolean { return this.values.delete(key); }
}
export interface FieldWidgetProps { field: Field; value: unknown; change: (value: unknown) => void }
/**
 * What happened to the record a form shows, told to its widgets after the form has settled (rendered again):
 * a save that succeeded (with the record id) or failed, a discard, and a switch between read and edit mode.
 */
/** Loads the descriptions of `views` of `model` through the page's metadata cache, as `get_views` answers them. */
export type ViewLoader = (model: string, views: [number | null, string][], context: Record<string, unknown>) => Promise<unknown>;
export type RecordEvent = { readonly type: "saved"; readonly id: number } | { readonly type: "saveFailed" } | { readonly type: "discarded" } | { readonly type: "modeChanged"; readonly mode: "read" | "edit" };
/** A window action named by its XML id, opened as its list (or a new record in its form) with a concrete domain and context. */
export interface ActionRequest { xmlId: string; viewMode: "form" | "list" | null; domain: unknown[]; context: Record<string, unknown> }
/** The shell navigation handed to addon code. Both go through the shell's guards (an unsaved record is saved or the
 *  user is asked first). `navigate` keeps the page's companies unless the route names its own, and naming other
 *  companies reloads the page with them (the server checks them). A route with a `method_action` is refused: that
 *  identity belongs to the page. `openAction` opens a window action by XML id as its list (or a new record in its
 *  form) with the request's domain and `default_<field>` context, and a breadcrumb back to the page it was called
 *  from. Once the owner that received them is gone (the page was left), both do nothing. */
export interface ShellNavigation {
  navigate(route: Route): Promise<void>;
  openAction(request: ActionRequest): Promise<void>;
  /** Open the native route of a visible menu by its installed window action XML id. */
  openMenuAction?(xmlId: string): Promise<void>;
}
export interface WidgetEnvironment extends FieldWidgetProps {
  /** Signed-in user and owning form view for local table preferences. */
  columnScope?: string | undefined;
  numericLocale?: NumericLocale;
  currencies?: CurrencyDisplayCache;
  values?: Readonly<Record<string, unknown>>;
  /** The model of the record `values` belongs to. */
  model?: string;
  /** The record holds an unsaved change of this field. */
  changed?: boolean;
  fields?: readonly Field[];
  changeValues?(changes: Readonly<Record<string, unknown>>): void;
  viewContext?: "form" | "list" | "card";
  /** The record form's mode: `read` while a read-first form (`edit_mode="read"`) shows a saved record. */
  mode?: "read" | "edit";
  /**
   * Listen to the record's save, save failure, discard and mode changes (main record forms only). Returns the
   * unsubscribe function; the form also unsubscribes when the widget is disposed. A throwing listener is reported
   * and never stops the save or the other listeners.
   */
  onRecordEvent?(listener: (event: RecordEvent) => void): () => void;
  /** Draft editors stage synchronously; their owner commits onchange on blur or Save. */
  draft?: { stage(value: unknown): void; commit(): void };
  messages?: MessageServices;
  /** Validated bootstrap display settings; kept separate from the RPC context. */
  editorLocale?: EditorLocale;
  /** Owner-scope disposal; stop reads and remove any protected child overlays when aborted. */
  signal?: AbortSignal;
  openRecord?: (model: string, id: number) => Promise<void>;
  /** The page's cached view loader; absent, a widget that opens a form in a dialog asks the server itself. */
  views?: ViewLoader;
  /** Opens a window action by XML id ({@link ShellNavigation.openAction}); present on the main form's fields, list cells and kanban cards (not in dialogs or one2many lines). */
  openAction?: (request: ActionRequest) => Promise<void>;
  /** Goes to a route through the shell ({@link ShellNavigation.navigate}); present where `openAction` is. */
  navigate?: (route: Route) => Promise<void>;
  rpc: RpcClient;
  context: Record<string, unknown>;
  run: (job: () => Promise<void>) => void;
  report: (error: unknown) => void;
}
/** Optional retained draft lifecycle. No engine implementation objects escape. */
export interface DraftWidgetLifecycle {
  /** Validate a replacement without changing the editor or its history. */
  admit(value: unknown): void;
  flush(): void;
  update(value: unknown, field: Field): void;
  validate(): boolean;
  focus(): void;
  setDisabled(value: boolean): void;
}
/** Editor-local text/error snapshot; owners may restore it across an unrelated rerender. */
export interface WidgetDraft { readonly value: string; readonly message: string }
export interface WidgetHandle { node: HTMLElement; dispose(): void; captureDraft?(): WidgetDraft | undefined; restoreDraft?(draft: WidgetDraft): void; draft?: DraftWidgetLifecycle }
export interface FieldWidget { isEmpty?(value: unknown): boolean; isValid?(value: unknown): boolean; kinds: readonly Field["kind"][]; render: (environment: WidgetEnvironment) => WidgetHandle }
export interface ServiceEnvironment extends Partial<ShellNavigation> { /** Authorized Settings presentation and guarded navigation. */ settingsNavigation?: SettingsNavigation; messages?: MessageServices; editorLocale?: EditorLocale; host: HTMLElement; panel?: HTMLElement; controls?: HTMLElement; rpc: RpcClient; companies: readonly Company[]; run: (job: () => Promise<void>) => void; notice: (message: string) => void; context: Record<string, unknown>; registry: string; options: Record<string, unknown> }
export interface ServiceFactory { model: string; validateOptions(options: Record<string, unknown>): void; create(environment: ServiceEnvironment): Promise<ServiceController> }
/** The decoded `ir.actions.client` a screen is mounted. */
export interface ClientActionDescriptor { readonly id: number | null; readonly name: string; readonly tag: string; readonly params: Readonly<Record<string, unknown>> }
/** What a client action screen receives: the host area, the services a screen needs, its action and the state it left. */
export interface ClientActionEnvironment extends Partial<ShellNavigation> {
  host: HTMLElement; panel: HTMLElement; controls: HTMLElement; rpc: RpcClient; companies: readonly Company[];
  run: (job: () => Promise<void>) => void; notice: (message: string) => void; messages: MessageServices; editorLocale: EditorLocale;
  /** Current address when mounted by the shell, including bookmarkable list and record selections. */
  readonly route?: Readonly<Route>;
  /** Mount the current session's Discuss controller into its installed workspace action. */
  mountDiscuss?: (host: HTMLElement) => () => void;
  /** Active interactive identity for first-party, session-bound client actions. */
  readonly userId?: number; readonly currentCompanyId?: number; readonly csrfToken?: string;
  action: ClientActionDescriptor;
  /** What `captureState` returned when the user left this action, or `null` on a first opening. */
  restored: unknown;
  /** Run an action result (an existing record form, a related list or another client action) through the normal navigation; the screen stays in the breadcrumb. */
  open(result: unknown): Promise<void>;
  /** Open an authorized modal action by XML id; resolves true on a completed action and false on dismissal. */
  openWizard?(xmlId: string): Promise<boolean>;
  /** Open an unsaved form through its installed window action, after shell navigation guards. */
  openDraft?(xmlId: string, defaults: Readonly<Record<string, string>>): Promise<void>;
}
/** `dispose` is synchronous and idempotent and the screen ignores late rendering after it; `captureState` returns JSON of at most 16 KiB. */
export interface ClientActionHandle {
  dispose(): void;
  captureState?(): unknown;
  /** Editable client actions participate in the shell's normal unsaved-change and navigation guards. */
  edits?: Pick<ServiceController, "dirty" | "busy" | "save" | "discard">;
}
export interface ClientActionFactory { create(environment: ClientActionEnvironment): Promise<ClientActionHandle> }
export const clientActions = new Registry<ClientActionFactory>("client action");
export interface ClientActionRedirectEnvironment {
  readonly route: Readonly<Route>;
  readonly rpc: RpcClient;
  readonly context: Readonly<Record<string, unknown>>;
  readonly signal: AbortSignal;
}
export const clientActionRedirects = new Registry<(environment: ClientActionRedirectEnvironment) => Promise<Route>>("client action redirect");
function checkClientActionTag(tag: string): void {
  if (!/^[a-z0-9_]+\.[a-z][a-z0-9_]*$/.test(tag) || tag.length > 64) throw new ClientError("validation", `Invalid client action tag: ${tag.slice(0, 64)}.`);
}
/** Bind a client action tag (`<addon>.<name>`, declared on the server by the same addon) to its screen. A second registration of one tag fails. */
export function registerClientAction(tag: string, factory: ClientActionFactory): void {
  checkClientActionTag(tag);
  clientActions.add(tag, factory);
}
/** Preserve bookmarks to a retired, authorized client action by resolving its current window route before mounting. */
export function registerClientActionRedirect(tag: string, redirect: (environment: ClientActionRedirectEnvironment) => Promise<Route>): void {
  checkClientActionTag(tag);
  clientActionRedirects.add(tag, redirect);
}
export interface ControlPanelExtension { model: string; component: ComponentClass }
/** What a list header may use: the list's own data access, read as the signed-in user. */
export interface ListHeaderEnvironment { model: string; title: string; rpc: RpcClient; context: Record<string, unknown> }
/** A header above an action's list view (not kanban, pivot or other views). `refresh` runs on every load with its own
 * copy of the domain the rows were read with; `signal` aborts when a newer load starts or the list closes, and a
 * failure after that is not reported. */
export interface ListHeaderHandle { element: HTMLElement; refresh(domain: unknown[], options: { signal: AbortSignal }): Promise<void>; dispose(): void }
export interface ListHeaderExtension { model: string; create(environment: ListHeaderEnvironment): ListHeaderHandle }
export const listHeaderExtensions = new Registry<ListHeaderExtension>("list header extension");
export function registerListHeaderExtension(key: string, extension: ListHeaderExtension): void { listHeaderExtensions.add(key, extension); }
export const controlPanelExtensions = new Registry<ControlPanelExtension>("control panel extension");
export function registerControlPanelExtension(key: string, extension: ControlPanelExtension): void { controlPanelExtensions.add(key, extension); }
export const fieldWidgets = new Registry<FieldWidget>("field widget");
export const serviceControllers = new Registry<ServiceFactory>("service controller");
export function registerFieldWidget(key: string, widget: FieldWidget): void { fieldWidgets.add(key, widget); }
export function registerServiceController(key: string, controller: ServiceFactory): void { serviceControllers.add(key, controller); }

export const cogMenuItems = new Registry<CogMenuItem>("cog menu item");
export function registerCogMenuItem(key: string, item: CogMenuItem): void { cogMenuItems.add(key, item); }

export const systrayItems = new Registry<SystrayItem>("systray item");
export function registerSystrayItem(key: string, item: SystrayItem): void { systrayItems.add(key, item); }

export const commandProviders = new Registry<CommandProvider>("command provider");
export function registerCommandProvider(key: string, provider: CommandProvider): void { commandProviders.add(key, provider); }

export const commandCategories = new Registry<CommandCategory>("command category");
export function registerCommandCategory(key: string, category: CommandCategory): void { commandCategories.add(key, category); }

export const commandSetups = new Registry<CommandSetup>("command setup");
export function registerCommandSetup(key: string, setup: CommandSetup): void { commandSetups.add(key, setup); }

export const userMenuItems = new Registry<UserMenuItemFactory>("user menu item");
export function registerUserMenuItem(key: string, factory: UserMenuItemFactory): void { userMenuItems.add(key, factory); }

export const debugSections = new Registry<DebugSection>("debug section");
export function registerDebugSection(key: string, section: DebugSection): void { debugSections.add(key, section); }
for (const [key, section] of Object.entries(DEFAULT_DEBUG_SECTIONS)) {
  debugSections.add(key, section);
}

export const debugItemProviders = new Registry<DebugItemProvider>("debug item provider");
export function registerDebugItemProvider(key: string, provider: DebugItemProvider): void { debugItemProviders.add(key, provider); }
let templates: readonly TemplateModule[] | undefined;
export function configureClientTemplates(value: readonly TemplateModule[]): void { templates = value; }
export function clientTemplates(): readonly TemplateModule[] { if (!templates) throw new ClientError("server", "Compiled client templates have not been installed."); return templates; }
/** Mount a trusted compiled addon widget under the form controller's lifecycle. */
export function elangFieldWidget(component: ComponentClass): FieldWidget["render"] {
  return environment => {
    const node = document.createElement("div"); let disposed = false;
    void mount(component, node, { templates: clientTemplates(), props: { field: environment.field, value: environment.value, change: environment.change }, dev: false, onError: environment.report })
      .catch(error => {
        // mount registers synchronously. Our unmount already cancels an unfinished mount;
        // do not unmount it again when the promise settles, or hide any other failure.
        if (disposed && error instanceof UIError && error.code === "usage" && error.message === "unmount: the app was unmounted before it finished mounting") return;
        environment.report(error);
      });
    return { node, dispose: () => { if (disposed) return; disposed = true; unmount(node); } };
  };
}

/** Resolve an admitted identity with explicit context precedence; unknown widgets never fall back by kind. */
export function resolveFieldWidget(key: string, context: "form" | "list" | "card", jsClass?: string): FieldWidget {
  if (jsClass && fieldWidgets.has(`${jsClass}.${key}`)) return fieldWidgets.get(`${jsClass}.${key}`);
  if (fieldWidgets.has(`${context}.${key}`)) return fieldWidgets.get(`${context}.${key}`);
  return fieldWidgets.get(key);
}
