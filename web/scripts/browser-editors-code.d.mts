/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
/** Applicable code-editor scenes independently required by browser receipt admission. */
export const CODE_EDITOR_STATES: readonly [
  "default", "empty", "hover", "focus", "edited", "undo", "redo", "readonly",
  "disabled", "required", "invalid", "error",
];
/** Returns only the pinned, immediate Playwright UtilityScript eval caller signature. */
export function classifyPlaywrightEvalCaller(stack: unknown): "UtilityScript.evaluate (<anonymous>:290:30)" | null;
/** Validates an explicit route; only an absent value defaults to Moli. */
export function codeEditorBrowserRoute(value: unknown): "moli" | "chromium";
/** Refuses incomplete, unpinned or failed compiled-browser evidence. */
export function validateCodeEditorManifest(manifest: unknown): void;

/** A scoped contrast measurement or an explicit refusal to establish coverage. */
export interface CodeEditorContrastCoverage {
  readonly target: readonly string[];
  readonly inTextLayer: boolean;
  readonly reason?: string;
  readonly ratio?: number;
}
/** Runs the exact browser DOM measurement; callers supply DOM and Range geometry. */
export function measureCodeEditorContrastCoverage(findings: readonly { readonly target: readonly string[] }[]): CodeEditorContrastCoverage[];

/** Explicit partial scope is allowed only with the Moli route. */
export function codeEditorBrowserScope(value: unknown, route: "moli" | "chromium"): "full" | "moli-native-selection-excluded";
/** Retains unrelated checks while refusing full acceptance for scoped Moli evidence. */
export function validateCodeEditorPartialManifest(manifest: unknown): void;
/** Checks complete Chromium and partial Moli receipts against identical frozen inputs. */
export function validateCodeEditorRouteComparison(native: unknown, partial: unknown): void;
