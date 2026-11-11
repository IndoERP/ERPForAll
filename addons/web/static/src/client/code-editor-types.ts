/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
import type { ClientError } from "./transport.js";

/** Language presentation only: no parsing, execution or authorization is implied. */
export type CodeEditorMode = "text" | "javascript" | "xml" | "qweb" | "scss" | "python" | "json" | "bash";
export type CodeEditorTheme = "auto" | "light" | "dark";
export type CodeSessionId = string | number;

/** Positive browser-local safety limits; callers may lower the documented hard maxima. */
export interface CodeEditorLimits {
  readonly maxDocumentBytes: number;
  readonly maxSessions: number;
  readonly maxTotalDocumentBytes: number;
  readonly maxHistoryBytes: number;
  readonly maxHistoryGroups: number;
}
export interface CodeEditorPresentation {
  readonly theme?: CodeEditorTheme;
  readonly lineWrapping?: boolean;
  readonly showLineNumbers?: boolean;
  readonly maxLines?: number;
  readonly required?: boolean;
  /** Empty text clears an owner-supplied validation message. */
  readonly invalid?: string;
}
export interface CodeEditorSessionOptions {
  readonly sessionId: CodeSessionId;
  readonly value: string;
  readonly mode?: CodeEditorMode;
}
export interface CodeEditorChange { readonly sessionId: CodeSessionId; readonly value: string }
export interface CodeEditorOptions extends CodeEditorPresentation, CodeEditorSessionOptions {
  readonly label: string;
  readonly readonly?: boolean;
  readonly disabled?: boolean;
  readonly limits?: Partial<CodeEditorLimits>;
  readonly signal?: AbortSignal;
  readonly onChange: (change: CodeEditorChange) => void;
  readonly report: (error: unknown) => void;
  readonly onBlur?: () => void;
}
/** Availability follows the active editable session. Subscription does not confer storage authority. */
export interface CodeEditorHistory {
  readonly canUndo: boolean;
  readonly canRedo: boolean;
  /** False when empty or superseded by a reentrant owner update; readonly/disposed commands refuse. */
  undo(): boolean;
  /** False when empty or superseded by a reentrant owner update; readonly/disposed commands refuse. */
  redo(): boolean;
  subscribe(listener: () => void): () => void;
}
export interface CodeEditorController {
  readonly history: CodeEditorHistory;
  getValue(): string;
  getSessionId(): CodeSessionId;
  setValue(value: string): void;
  setSession(options: CodeEditorSessionOptions): void;
  setReadonly(value: boolean): void;
  setDisabled(value: boolean): void;
  setPresentation(options: CodeEditorPresentation): void;
  focus(): void;
  releaseSession(id: CodeSessionId): void;
  dispose(): void;
}
/** Disposable trusted-addon component; no engine implementation objects escape. */
export interface CodeEditorHandle extends CodeEditorController { readonly node: HTMLElement }

/** Private, copied engine state. Public subscribers can reenter; each notification precedes them. */
export interface CodeEngineState {
  readonly readonly: boolean;
  readonly disabled: boolean;
  readonly presentation: Readonly<Required<CodeEditorPresentation>>;
}

/** Private adapter notifications. The surrounding component owns presentation. */
export interface CodeEngineCallbacks {
  state(snapshot: CodeEngineState): void;
  invalid(error: ClientError): void;
  status(message: string): void;
}
