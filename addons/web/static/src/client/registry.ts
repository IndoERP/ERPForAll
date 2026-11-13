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
export interface WidgetEnvironment extends FieldWidgetProps {
  numericLocale?: NumericLocale;
  currencies?: CurrencyDisplayCache;
  values?: Readonly<Record<string, unknown>>;
  fields?: readonly Field[];
  changeValues?(changes: Readonly<Record<string, unknown>>): void;
  viewContext?: "form" | "list" | "card";
  /** Draft editors stage synchronously; their owner commits onchange on blur or Save. */
  draft?: { stage(value: unknown): void; commit(): void };
  messages?: MessageServices;
  /** Validated bootstrap display settings; kept separate from the RPC context. */
  editorLocale?: EditorLocale;
  /** Owner-scope disposal; stop reads and remove any protected child overlays when aborted. */
  signal?: AbortSignal;
  openRecord?: (model: string, id: number) => Promise<void>;
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
export interface ServiceEnvironment { /** Authorized Settings presentation and guarded navigation. */ settingsNavigation?: SettingsNavigation; messages?: MessageServices; editorLocale?: EditorLocale; host: HTMLElement; panel?: HTMLElement; controls?: HTMLElement; rpc: RpcClient; companies: readonly Company[]; run: (job: () => Promise<void>) => void; notice: (message: string) => void; context: Record<string, unknown>; registry: string; options: Record<string, unknown> }
export interface ServiceFactory { model: string; validateOptions(options: Record<string, unknown>): void; create(environment: ServiceEnvironment): Promise<ServiceController> }
export interface ControlPanelExtension { model: string; component: ComponentClass }
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
