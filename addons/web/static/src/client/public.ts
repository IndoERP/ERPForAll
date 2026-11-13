/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
// The trusted addon entry surface. Registration precedes shell boot; identities are unique.
export { Component, signal, onMounted, onWillDestroy } from "@erpforall/elang";
export {
  registerFieldWidget, resolveFieldWidget, registerServiceController, registerControlPanelExtension, elangFieldWidget,
  registerCogMenuItem, registerSystrayItem, registerCommandProvider, registerCommandCategory, registerCommandSetup,
  registerUserMenuItem, registerDebugSection, registerDebugItemProvider
} from "./registry.js";
export type { FieldWidgetProps, WidgetEnvironment, WidgetHandle, WidgetDraft, FieldWidget, ServiceEnvironment, ServiceFactory } from "./registry.js";
export type { CogMenuItem, CogMenuItemEnvironment } from "./cog-menu.js";
export { prepareCogMenu, renderCogMenu, getPrintItemAriaLabel } from "./cog-menu.js";
export type { SystrayItem, SystrayEnvironment, SystrayHandle } from "./systray.js";
export { prepareSystrayItems, renderSystray } from "./systray.js";
export type { Command, CommandProvider, CommandCategory, CommandSetup, CommandPaletteConfig, CommandPaletteHandle } from "./command-palette.js";
export { openCommandPalette, getActiveCommandPalette } from "./command-palette.js";
export type { UserMenuItem, UserMenuItemType, UserMenuItemFactory, UserMenuEnvironment } from "./user-menu.js";
export { composeUserMenuItems, renderUserMenuItems } from "./user-menu.js";
export type { DebugItem, DebugSection, DebugItemProvider, DebugEnvironment, GroupedDebugSection } from "./debug-menu.js";
export { loadGroupedDebugItems, createDebugCommandProvider, renderDebugMenu, DEFAULT_DEBUG_SECTIONS } from "./debug-menu.js";
export type { ControlPanelProps } from "./control-panel.js";

export { MessageServices } from "./messages.js";
export type { NotificationLevel, NotificationOptions, DialogHandle, DialogOptions, ConfirmationOptions, ConfirmationHandle } from "./message-types.js";
export { selectCreateDialog } from "./select-create-dialog.js";
export { exportDialog } from "./export-dialog.js";
export type { SelectCreateDialogOptions } from "./select-create-dialog.js";
export type { ExportDialogOptions } from "./export-dialog.js";
export { createDateTimeEditor, createDateTimeRangeEditor } from "./datetime-editor.js";
export type { DateTimeEditorOptions, DateTimeEditorHandle, DateTimeRangeEditorOptions, DateTimeRangeEditorHandle } from "./datetime-editor.js";
export { createTimeEditor } from "./time-editor.js";
export type { TimeEditorOptions, TimeEditorHandle } from "./time-editor.js";
export { editorLocale } from "./datetime-values.js";
export type { EditorLocale, EditorValue, DateKind, DateParts, ParseResult } from "./datetime-values.js";

export { createFileEditor } from "./file-editor.js";
export type { FileEditorOptions, FileEditorHandle } from "./file-editor.js";
export { createDropzone } from "./dropzone.js";
export type { DropzoneOptions, DropzoneHandle } from "./dropzone.js";
export type { UploadContext, UploadTransport } from "./file-upload.js";
export { createColorPalette } from "./color-palette.js";
export { createTagsList } from "./tags-list.js";
export { createEmojiPicker } from "./emoji-picker.js";
export type { ColorPaletteOptions } from "./color-palette.js";
export type { TagsListOptions, TagPresentation } from "./tags-list.js";
export type { EmojiPickerOptions } from "./emoji-picker.js";
export { createModelSelector } from "./model-selector.js";
export { createModelFieldSelector } from "./model-field-selector.js";
export { createSelectMenu } from "./select-menu.js";
export { createSignaturePad } from "./signature-pad.js";
export { createAutocomplete } from "./autocomplete.js";
export {
  currencyExchangeRateControl, currencyDatePickerControl, refreshCurrencyRateControl,
  invertRate, formatRateDisplay, parseRateInput, getCurrencySwapPreference, setCurrencySwapPreference,
  getRatesListColumns, resolveExchangeAccount,
} from "./account-currency-rate.js";
export type {
  CurrencyExchangeRateHandle, CurrencyDatePickerHandle, RefreshCurrencyRateHandle,
  RatesListConfig, ExchangeSettings, ExchangeDiffKind, CurrencyAccountOverride,
} from "./account-currency-rate.js";
export type { EditorEnvironment, EditorHandle } from "./editor-types.js";
export type { ModelChoice, ModelSelectorOptions, ModelSelectorHandle } from "./model-selector.js";
export type { ModelFieldSelectorOptions, ModelFieldSelectorHandle } from "./model-field-selector.js";
export type { ChoiceValue, SelectChoice, SelectGroup, SelectMenuOptions } from "./select-menu.js";
export type { AutocompleteOptions, AutocompleteSource, AutocompleteOption, AutocompleteHandle } from "./autocomplete.js";
export type { SignatureMode, SignatureState, SignaturePadOptions, SignaturePadHandle } from "./signature-pad.js";
export { createTreeEditor } from './tree-editor.js';
export type { TreeEditorOptions, TreeEditorHandle } from './tree-editor.js';
export type { TreeEditorProvider, TreeEditorOperator, TreeValueEditorOptions } from './tree-editor-fields.js';
export { createDomainEditor } from './domain-editor.js';
export type { DomainEditorOptions, DomainEditorHandle } from './domain-editor.js';
export { createExpressionEditor } from './expression-editor.js';
export type { ExpressionEditorOptions, ExpressionEditorHandle, ExpressionField } from './expression-editor.js';
export type { ConditionLiteral, ConditionNode, TreeLimits, CodecResult } from './condition-tree.js';
export { createHtmlEditor } from "./html-editor.js";
export type { HtmlEditorOptions, HtmlEditorHandle } from "./html-editor.js";
export type { HtmlEditorLimits } from "./html-editor-document.js";


export { createCodeEditor } from "./code-editor.js";
export type { CodeEditorOptions, CodeEditorHandle, CodeEditorChange, CodeEditorHistory, CodeEditorMode, CodeEditorTheme, CodeEditorLimits, CodeEditorPresentation, CodeEditorSessionOptions, CodeSessionId } from "./code-editor-types.js";

export { parseNumeric, formatNumeric } from "./numeric-values.js";
export type { NumericLocale, NumericKind } from "./numeric-values.js";

export { Chatter } from "./chatter.js";
export type { ChatterOptions } from "./chatter.js";

export { ControlPanel, mountControlPanel } from "./control-panel.js";
export type { ControlPanelAdapter, ControlPanelDisplayFlags, ControlPanelHandle, ControlPanelMountOptions } from "./control-panel.js";
export { Pager, parsePagerRange } from "./pager.js";
export type { PagerProps } from "./pager.js";
export { ViewSwitcher } from "./view-switcher.js";
export type { ViewSwitcherEntry, ViewSwitcherProps } from "./view-switcher.js";
export { ActionMenus, executeActionMenuItem, loadAvailablePrintItems } from "./action-menus.js";
export type { ActionMenuItem, ActionMenuEnvironment, ActionMenuOptions, PrintMenuLoadOptions } from "./action-menus.js";

export { BottomSheet, getActiveBottomSheetCount } from "./bottom-sheet.js";
export type { BottomSheetOptions } from "./bottom-sheet.js";
export { HomeMenu } from "./home-menu.js";
export type { HomeMenuApp, HomeMenuOptions } from "./home-menu.js";
export { NavbarOverflow } from "./navbar-overflow.js";
export type { NavbarOverflowOptions, NavbarSectionItem } from "./navbar-overflow.js";
export { MobileBurgerMenu, isDisplayStandalone } from "./burger-menu.js";
export type { BurgerUser, BurgerCompany, BurgerUserMenuItem, MobileBurgerMenuOptions } from "./burger-menu.js";

export { activitySystrayItem, renderActivityMenu, parseActivityMenuData, isUnknownActivityModel, getModelIconName } from "./activity-menu.js";
export type { ActivityGroupData, ActivityMenuData, ActivityMenuEnvironment, ActivityMenuHandle } from "./activity-menu.js";
