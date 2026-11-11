/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
// The trusted addon entry surface. Registration precedes shell boot; identities are unique.
export { Component, signal, onMounted, onWillDestroy } from "@erpforall/elang";
export { registerFieldWidget, resolveFieldWidget, registerServiceController, registerControlPanelExtension, elangFieldWidget } from "./registry.js";
export type { FieldWidgetProps, WidgetEnvironment, WidgetHandle, WidgetDraft, FieldWidget, ServiceEnvironment, ServiceFactory } from "./registry.js";
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
