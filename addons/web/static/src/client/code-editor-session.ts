/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
import { ClientError } from "./transport.js";
import type { CodeEditorLimits, CodeEditorMode, CodeEditorSessionOptions, CodeSessionId } from "./code-editor-types.js";

export interface CodeSelection { readonly row: number; readonly column: number; readonly anchorRow?: number; readonly anchorColumn?: number }
interface Snapshot { readonly value: string; readonly selection: CodeSelection }
interface HistoryGroup { readonly before: Snapshot; readonly after: Snapshot; readonly serial: number; readonly bytes: number }
interface Session { value: string; bytes: number; mode: CodeEditorMode; selection: CodeSelection; undo: HistoryGroup[]; redo: HistoryGroup[] }
const encoder = new TextEncoder();
export const CODE_EDITOR_HARD_LIMITS: Readonly<CodeEditorLimits> = Object.freeze({ maxDocumentBytes: 1048576, maxSessions: 8, maxTotalDocumentBytes: 4194304, maxHistoryBytes: 8388608, maxHistoryGroups: 100 });
const modes: readonly CodeEditorMode[] = ["text", "javascript", "xml", "qweb", "scss", "python", "json", "bash"];
const failure = (message: string): never => { throw new ClientError("validation", message); };
export function codePlainOptions(value: unknown, keys: readonly string[], label: string): void {
  if (!value || typeof value !== "object" || ![Object.prototype, null].includes(Object.getPrototypeOf(value))) return failure(`${label} must be a plain object.`);
  for (const key of Reflect.ownKeys(value)) if (typeof key !== "string" || !keys.includes(key)) failure(`Unknown ${label.toLowerCase()}: ${String(key)}.`);
}
export function codeEditorText(value: unknown, maxBytes: number): string {
  if (typeof value !== "string") return failure("Code editor value must be primitive text.");
  if (value.length > maxBytes || encoder.encode(value).byteLength > maxBytes) return failure(`Code editor value must be at most ${maxBytes} UTF-8 bytes.`);
  return value;
}
export function codeSessionId(value: unknown): CodeSessionId {
  if (typeof value === "string" && value.length <= 256 && encoder.encode(value).byteLength <= 256 || typeof value === "number" && Number.isSafeInteger(value)) return value;
  return failure("Code editor sessionId must be bounded text or a safe integer.");
}
export function codeEditorMode(value: unknown): CodeEditorMode {
  if (typeof value === "string" && modes.includes(value as CodeEditorMode)) return value as CodeEditorMode;
  return failure("Code editor mode is not supported.");
}
export function codeEditorLimits(value: Partial<CodeEditorLimits> = {}): CodeEditorLimits {
  codePlainOptions(value, Object.keys(CODE_EDITOR_HARD_LIMITS), "Code editor limit");
  const result = { ...CODE_EDITOR_HARD_LIMITS };
  for (const key of Object.keys(result) as (keyof CodeEditorLimits)[]) {
    if (value[key] === undefined) continue;
    const selected = value[key];
    if (!Number.isSafeInteger(selected) || selected <= 0 || selected > result[key]) failure(`Code editor ${key} must be a positive safe integer no greater than ${result[key]}.`);
    result[key] = selected;
  }
  return result;
}
function clamp(value: string, selection: CodeSelection): CodeSelection {
  const lines = value.split("\n"), row = Math.max(0, Math.min(selection.row, lines.length - 1));
  const column = Math.max(0, Math.min(selection.column, lines[row]!.length));
  const anchorRow = Math.max(0, Math.min(selection.anchorRow ?? row, lines.length - 1));
  const anchorColumn = Math.max(0, Math.min(selection.anchorColumn ?? column, lines[anchorRow]!.length));
  return selection.anchorRow === undefined ? { row, column } : { row, column, anchorRow, anchorColumn };
}

/** Browser-local source/history ownership. Snapshots include both stacks and bounded selection metadata.
 * Ace's native recorder is detached by the engine: there is no second hidden delta history. */
export class CodeSessions {
  readonly #sessions = new Map<CodeSessionId, Session>();
  readonly #limits: CodeEditorLimits;
  readonly #status: (message: string) => void;
  #id: CodeSessionId | undefined;
  #serial = 0;
  #dead = false;
  constructor(limits: CodeEditorLimits, status: (message: string) => void) { this.#limits = codeEditorLimits(limits); this.#status = status; }
  #alive(): void { if (this.#dead) failure("Code editor is disposed."); }
  #active(): Session { this.#alive(); const session = this.#id === undefined ? undefined : this.#sessions.get(this.#id); if (!session) return failure("Code editor has no active session."); return session; }
  get id(): CodeSessionId { this.#active(); return this.#id!; }
  get value(): string { return this.#active().value; }
  get mode(): CodeEditorMode { return this.#active().mode; }
  get selection(): CodeSelection { return { ...this.#active().selection }; }
  set selection(value: CodeSelection) { const session = this.#active(); session.selection = clamp(session.value, value); }
  get canUndo(): boolean { return !this.#dead && this.#id !== undefined && this.#active().undo.length > 0; }
  get canRedo(): boolean { return !this.#dead && this.#id !== undefined && this.#active().redo.length > 0; }
  get historyBytes(): number { let bytes = 0; for (const session of this.#sessions.values()) for (const group of [...session.undo, ...session.redo]) bytes += group.bytes; return bytes; }
  #admit(value: unknown, replacing?: Session): { value: string; bytes: number } {
    const text = codeEditorText(value, this.#limits.maxDocumentBytes), bytes = encoder.encode(text).byteLength;
    let total = bytes; for (const session of this.#sessions.values()) if (session !== replacing) total += session.bytes;
    if (total > this.#limits.maxTotalDocumentBytes) failure(`Code editor documents must total at most ${this.#limits.maxTotalDocumentBytes} UTF-8 bytes.`);
    return { value: text, bytes };
  }
  activate(options: CodeEditorSessionOptions): void {
    this.#alive(); codePlainOptions(options, ["sessionId", "value", "mode"], "Code editor session option");
    const id = codeSessionId(options.sessionId), existing = this.#sessions.get(id);
    const mode = options.mode === undefined ? existing?.mode ?? "text" : codeEditorMode(options.mode);
    const admitted = this.#admit(options.value, existing);
    if (!existing && this.#sessions.size >= this.#limits.maxSessions) failure(`Code editor retains at most ${this.#limits.maxSessions} sessions; release an inactive session first.`);
    if (existing) {
      if (existing.value !== admitted.value) { Object.assign(existing, admitted); existing.selection = clamp(existing.value, existing.selection); existing.undo = []; existing.redo = []; }
      existing.mode = mode;
    } else this.#sessions.set(id, { ...admitted, mode, selection: { row: 0, column: 0 }, undo: [], redo: [] });
    this.#id = id;
  }
  replace(value: string): void {
    const session = this.#active(), admitted = this.#admit(value, session);
    if (session.value === admitted.value) return;
    Object.assign(session, admitted); session.selection = clamp(session.value, session.selection); session.undo = []; session.redo = [];
  }
  /** Preflight native insertions before the text engine builds a replacement document. */
  admitInsertion(inserted: string, removed: string): void {
    const session = this.#active();
    const text = codeEditorText(inserted, this.#limits.maxDocumentBytes);
    const bytes = session.bytes - encoder.encode(removed).byteLength + encoder.encode(text).byteLength;
    if (bytes > this.#limits.maxDocumentBytes) failure(`Code editor value must be at most ${this.#limits.maxDocumentBytes} UTF-8 bytes.`);
    let total = bytes; for (const other of this.#sessions.values()) if (other !== session) total += other.bytes;
    if (total > this.#limits.maxTotalDocumentBytes) failure(`Code editor documents must total at most ${this.#limits.maxTotalDocumentBytes} UTF-8 bytes.`);
  }
  edit(value: string, selection: CodeSelection): boolean {
    const session = this.#active(), admitted = this.#admit(value, session);
    if (session.value === admitted.value) { session.selection = clamp(value, selection); return false; }
    const before = { value: session.value, selection: { ...session.selection } }, after = { value: admitted.value, selection: clamp(value, selection) };
    // 128 bytes conservatively charges the fixed two selection records and group metadata in
    // addition to both UTF-8 payloads. This is admission accounting, not a heap measurement.
    const bytes = Math.max(session.bytes, session.value.length * 2) + Math.max(admitted.bytes, admitted.value.length * 2) + 128;
    Object.assign(session, admitted); session.selection = after.selection; session.redo = [];
    session.undo.push({ before, after, bytes, serial: ++this.#serial });
    this.#trim(); return true;
  }
  #trim(): void {
    let pruned = false;
    for (const session of this.#sessions.values()) while (session.undo.length + session.redo.length > this.#limits.maxHistoryGroups) { this.#removeOldest(session); pruned = true; }
    while (this.historyBytes > this.#limits.maxHistoryBytes) {
      let oldest: Session | undefined, serial = Infinity;
      for (const session of this.#sessions.values()) for (const group of [...session.undo, ...session.redo]) if (group.serial < serial) { serial = group.serial; oldest = session; }
      if (!oldest) return failure("Code editor history accounting is inconsistent.");
      this.#removeOldest(oldest); pruned = true;
    }
    if (pruned) this.#status("Some undo and redo history was removed to stay within the editor's safety limits.");
  }
  #removeOldest(session: Session): void {
    const undo = session.undo.reduce((index, group, candidate, list) => group.serial < list[index]!.serial ? candidate : index, 0);
    const redo = session.redo.reduce((index, group, candidate, list) => group.serial < list[index]!.serial ? candidate : index, 0);
    if (session.undo.length && (!session.redo.length || session.undo[undo]!.serial < session.redo[redo]!.serial)) session.undo.splice(undo, 1);
    // The oldest redo transition is the next step from the current document. Its later
    // transitions depend on it, so removing only that group would let redo jump across a gap.
    // Clear the dependent redo chain while leaving the current source and undo prefix intact.
    else if (session.redo.length) session.redo = [];
    else failure("Code editor history accounting is inconsistent.");
  }
  undo(): boolean { const session = this.#active(), group = session.undo.at(-1); if (!group) return false; const admitted = this.#admit(group.before.value, session); session.undo.pop(); session.redo.push(group); Object.assign(session, admitted); session.selection = { ...group.before.selection }; return true; }
  redo(): boolean { const session = this.#active(), group = session.redo.at(-1); if (!group) return false; const admitted = this.#admit(group.after.value, session); session.redo.pop(); session.undo.push(group); Object.assign(session, admitted); session.selection = { ...group.after.selection }; return true; }
  release(value: CodeSessionId): void { this.#alive(); const id = codeSessionId(value); if (id === this.#id) failure("The active code editor session cannot be released."); if (!this.#sessions.delete(id)) failure("Code editor session does not exist."); }
  dispose(): void { this.#dead = true; this.#sessions.clear(); this.#id = undefined; }
}
