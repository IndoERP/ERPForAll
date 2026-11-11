/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
import { htmlEditorLimits, type HtmlEditorLimits } from "./html-editor-document.js";
import { ClientError } from "./transport.js";

interface Entry<T> { readonly value: T; readonly bytes: number }
/** Isolated, bounded snapshots; callers count the complete serialized value including selection. */
export class HtmlEditorHistory<T> {
  private entries: Entry<T>[] = [];
  private index = -1;
  private readonly limits: Readonly<HtmlEditorLimits>;
  constructor(initial: T, limits: HtmlEditorLimits, private readonly bytes: (value: T) => number) {
    this.limits = htmlEditorLimits(limits);
    this.reset(initial);
  }
  get canUndo(): boolean { return this.index > 0; }
  get canRedo(): boolean { return this.index >= 0 && this.index < this.entries.length - 1; }
  private admit(value: T): Entry<T> {
    let copy: T;
    try { copy = structuredClone(value); }
    catch { throw new ClientError("validation", "HTML editor snapshot must be cloneable."); }
    const size = this.bytes(copy);
    if (!Number.isSafeInteger(size) || size < 0) throw new ClientError("validation", "HTML editor snapshot bytes must be a nonnegative safe integer.");
    if (size > this.limits.historyBytes) throw new ClientError("validation", "HTML editor snapshot exceeds historyBytes limit.");
    return { value: copy, bytes: size };
  }
  /** Add one transition, dropping redo and oldest undo prefixes only after successful admission. */
  commit(value: T): { shortened: boolean } {
    const entry = this.admit(value);
    const retained = this.entries.slice(0, this.index + 1);
    retained.push(entry);
    let size = retained.reduce((total, item) => total + item.bytes, 0);
    let removed = 0;
    while (retained.length - removed > this.limits.historyEntries || size > this.limits.historyBytes) {
      size -= retained[removed]!.bytes;
      removed++;
    }
    this.entries = retained.slice(removed);
    this.index = this.entries.length - 1;
    return { shortened: removed > 0 };
  }
  /** Stage a current selection without changing the reachable undo/redo chain. Refuses atomically. */
  replaceCurrent(value: T): void {
    const entry = this.admit(value);
    if (this.index < 0) throw new ClientError("validation", "HTML editor history has no current snapshot.");
    const size = this.entries.reduce((total, item, index) => total + (index === this.index ? entry.bytes : item.bytes), 0);
    if (size > this.limits.historyBytes) throw new ClientError("validation", "HTML editor history exceeds historyBytes limit.");
    this.entries[this.index] = entry;
  }
  /** Read the previous snapshot without moving, so callers can move only after presenting it. */
  peekUndo(): T | undefined { return this.canUndo ? structuredClone(this.entries[this.index - 1]!.value) : undefined; }
  peekRedo(): T | undefined { return this.canRedo ? structuredClone(this.entries[this.index + 1]!.value) : undefined; }
  undo(): T | undefined {
    if (!this.canUndo) return undefined;
    const value = structuredClone(this.entries[this.index - 1]!.value);
    this.index--;
    return value;
  }
  redo(): T | undefined {
    if (!this.canRedo) return undefined;
    const value = structuredClone(this.entries[this.index + 1]!.value);
    this.index++;
    return value;
  }
  reset(value: T): void {
    const entry = this.admit(value);
    this.entries = [entry];
    this.index = 0;
  }
  clear(): void { this.entries = []; this.index = -1; }
}
