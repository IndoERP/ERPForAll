/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
// @erpforall/elang: the Elang runtime's public API. Compiled
// templates use `@erpforall/elang/abi` instead.

export {
  Component,
  ErrorBoundary,
  Pending,
  Portal,
  VariableList,
  Virtual,
  mount,
  unmount,
  onError,
  onFlushed,
  onMounted,
  onWillDestroy,
  onWillStart,
  onWillUnmount,
  onWillUpdateProps,
  useEffect,
} from "./runtime/component.js";
export type { ComponentClass, MountOptions, PropsOf, Slot, TemplateModule } from "./runtime/component.js";
export type { VariableListObserve, VariableListProps, VirtualProps, VirtualRow } from "./runtime/component.js";
export type { Row } from "./runtime/control.js";
export { batch, computed, effect, flushSync, signal, untrack } from "./runtime/reactive.js";
export type { Signal } from "./runtime/reactive.js";
export {
  ColumnarRecordStore,
  KeyedRecordStore,
  createSelector,
  raw,
  store,
  toRaw,
} from "./runtime/store.js";
export type { ModelTable, RecordId } from "./runtime/store.js";
export type { Selector } from "./runtime/store.js";
export { isPropsSchema, props, types, validateAndApplyPropsDefaults } from "./runtime/props.js";
export type { PropsSchema, TypeDef, ValidationContext, Validator } from "./runtime/props.js";
export { resource } from "./runtime/resource.js";
export type { Resource, ResourceOptions } from "./runtime/resource.js";
export { Markup, htmlEscape, isMarkup, markup } from "./runtime/markup.js";
export { ReactiveLoopError, UIError, UnsafeAttributeError } from "./runtime/errors.js";
export type { ElangError, UIErrorBaseCode, UIErrorCode, UIErrorName } from "./runtime/errors.js";

export { useEnv, useSubEnv, useService } from "./runtime/component.js";
export { createEnv } from "./runtime/env.js";
export type { Env } from "./runtime/env.js";
export { ExtensionError } from "./runtime/extension-error.js";
export type { ExtensionErrorCode } from "./runtime/extension-error.js";
export { Registry, RegistryHub } from "./runtime/registry.js";
export type { Registries, RegistrySchema, RegistryOptions } from "./runtime/registry.js";
export { ServiceContainer } from "./runtime/services.js";
export type { Services, ServiceClass, ServiceResolver } from "./runtime/services.js";
export { patch, patchStatic, PatchSet } from "./runtime/patch.js";
export type { PatchFactory, Unpatch, PatchInfo } from "./runtime/patch.js";
export { registerAddons, AddonRegistrationError } from "./runtime/addons.js";
export type { RegistrationContext, AddonRegistration } from "./runtime/addons.js";

export { createFocusTrap, createRovingTabindex, LiveRegion } from "./runtime/accessibility.js";
export type { FocusTrapOptions, RovingOptions } from "./runtime/accessibility.js";
export { createTranslator } from "./runtime/translation.js";
export type { TranslationCatalogue, TranslationOptions, Translate } from "./runtime/translation.js";

export { createHotkeyController, createHotkeyBoundary } from "./runtime/hotkeys.js";
export type { HotkeyBinding, HotkeyControllerOptions, HotkeyBoundaryOptions } from "./runtime/hotkeys.js";
