/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
import type { MessageServices } from "./messages.js";
import { ClientError } from "./transport.js";

/** Owner-scoped presentation capabilities. These handles confer no server authority. */
export interface EditorEnvironment {
  messages: Pick<MessageServices, "popovers" | "dialogs">;
  report(error: unknown): void;
  signal?: AbortSignal;
}
export interface EditorHandle { node: HTMLElement; dispose(): void }
export const EDITOR_TEXT_BYTES = 4096;
export const EDITOR_OPTIONS_LIMIT = 1024;
export const EDITOR_OPTIONS_BYTES = 262144;
export function editorText(value: unknown, label: string, maxBytes = EDITOR_TEXT_BYTES): asserts value is string {
  if (!Number.isSafeInteger(maxBytes) || maxBytes < 0 || maxBytes > EDITOR_OPTIONS_BYTES) throw new ClientError("validation", "Editor text limit must be an integer from 0 to 262144.");
  if (typeof value !== "string" || value.length > maxBytes || new TextEncoder().encode(value).length > maxBytes) throw new ClientError("validation", `${label} must be text of at most ${maxBytes} bytes.`);
}
export function editorOptions(value: unknown, keys: readonly string[], label: string): void {
  if (value === null || typeof value !== "object" || (Object.getPrototypeOf(value) !== Object.prototype && Object.getPrototypeOf(value) !== null)) throw new ClientError("validation", `${label} options must be a plain object.`);
  for (const key of Reflect.ownKeys(value)) if (typeof key !== "string" || !keys.includes(key)) throw new ClientError("validation", `Unknown ${label.toLowerCase()} option: ${String(key)}.`);
}
export function editorEnvironment(value: EditorEnvironment): void {
  editorOptions(value, ["messages", "report", "signal"], "Editor environment");
  if (!value.messages || typeof value.messages.popovers?.open !== "function" || typeof value.messages.dialogs?.open !== "function" || typeof value.report !== "function" || value.signal !== undefined && !(value.signal instanceof AbortSignal)) throw new ClientError("validation", "Editor environment requires message services, a reporter and an optional AbortSignal.");
}
