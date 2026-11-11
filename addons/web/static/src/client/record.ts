/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
import { notificationClasses } from './notification-classes.js';
import { combineFields, array, integer, object, text, type Field, type Subview } from "./metadata.js";
import { effectiveNodes, occurrenceFields } from "./field-modifiers.js";
import { constantRelationContext } from "./field-relations.js";
import { passwordModels } from "./password-flow.js";
import { ClientError, RpcClient } from "./transport.js";

export function specification(fields: readonly Field[]): Record<string, unknown> {
  return Object.fromEntries(fields.map(field => [field.name, fieldSpecification(field)]));
}
function fieldSpecification(field: Field): Record<string, unknown> {
  // A one2many reads every field its embedded list and line form show, so a line opens without
  // another round trip; without a subview it reads only the lines' names.
  if (field.kind === "one2many" && field.subview) return { fields: { display_name: {}, ...specification(lineFields(field.subview)) } };
  if (!field.relation) return {};
  const context = constantRelationContext(field);
  return { fields: { display_name: {}, ...(field.options?.color_field && field.options.color_field.model === field.relation ? { [field.options.color_field.name]: {} } : {}) }, ...(context ? { context } : {}) };
}
/** The fields of a one2many line: its list columns, then the line form's fields. */
export function lineFields(subview: Subview): Field[] {
  // Form policy governs line editing/writes; list-only fields retain their list policy.
  const form = subview.form?.fields ?? [];
  const formNames = new Set(form.map(field => field.name));
  return combineFields([...subview.list.filter(field => !formNames.has(field.name)), ...form]);
}

/** A one2many line as the form holds it: the line record's values (with `id` once it is stored),
 *  plus `__key` for a line not created yet and `__changed` for the fields edited on a stored line. */
export type Line = Record<string, unknown>;
let nextLine = 0;
export function newLineKey(): string { return `line_${++nextLine}`; }
function lines(value: unknown, label: string): Line[] {
  return value === undefined || value === false ? [] : array(value, label).map(line => object(line, "one2many line"));
}
/** Nested one2many lines are never written from a line. */
export function lineCommands(field: Field, value: unknown, initial: unknown): unknown[] {
  const subview = field.subview, commands: unknown[] = [], kept = new Set<number>();
  for (const line of lines(value, "one2many lines")) {
    if (line["id"] === undefined) {
      if (!subview) throw new ClientError("unsupported", `${field.label}: lines cannot be created without the line views.`);
      commands.push([0, 0, lineVals(subview, line, null)]);
      continue;
    }
    const id = integer(line["id"], "line id");
    kept.add(id);
    if (line["__changed"] === undefined) continue;
    if (!subview) throw new ClientError("unsupported", `${field.label}: lines cannot be edited without the line views.`);
    const changed = array(line["__changed"], "changed line fields").map(name => text(name, "changed line field"));
    if (changed.length) commands.push([1, id, lineVals(subview, line, changed)]);
  }
  for (const line of lines(initial, "stored one2many lines")) {
    const id = integer(line["id"], "line id");
    if (!kept.has(id)) commands.push([2, id, 0]);
  }
  return commands;
}
/** Apply occurrence modifiers in the line's own value scope before validation or parent writes. */
export function linePolicy(subview: Subview, line: Line): Field[] {
  if (!subview.form) return lineFields(subview);
  const occurrences = occurrenceFields(effectiveNodes(subview.form.nodes, lineFields(subview), line));
  const present = new Set(occurrences.map(field => field.name));
  return combineFields([...occurrences, ...lineFields(subview).filter(field => !present.has(field.name))]);
}
function lineVals(subview: Subview, line: Line, changed: readonly string[] | null): Record<string, unknown> {
  const vals: Record<string, unknown> = {};
  for (const field of linePolicy(subview, line)) {
    if (field.required && !field.readonly && !field.invisible && field.kind !== "boolean" && (line[field.name] === false || line[field.name] === null || line[field.name] === undefined || line[field.name] === "" || Array.isArray(line[field.name]) && (line[field.name] as unknown[]).length === 0)) throw new ClientError("validation", `${field.label} is required.`);
    if (field.name === "id" || field.readonly && !field.forceSave || field.kind === "one2many" || !Object.hasOwn(line, field.name)) continue;
    if (changed && !changed.includes(field.name)) continue;
    vals[field.name] = writableValue(field, line[field.name]);
  }
  return vals;
}
/** Apply an onchange's one2many commands to the lines the form holds. */
function lineDiff(previous: unknown, update: unknown): Line[] {
  let values = lines(previous, "previous lines");
  for (const raw of array(update, "line commands")) {
    const command = array(raw, "line command"), code = command[0];
    if (code === 5) { values = []; continue; }
    if (code === 0) { values.push({ ...object(command[2], "new line"), __key: newLineKey() }); continue; }
    if (typeof command[1] !== "number") throw new ClientError("unsupported", "Onchange returned a line command for a line that is not stored yet.");
    const id = integer(command[1], "line id");
    if (code === 2 || code === 3) values = values.filter(line => line["id"] !== id);
    else if (code === 4) { if (!values.some(line => line["id"] === id)) values.push({ ...(command[2] && typeof command[2] === "object" ? object(command[2], "linked line") : {}), id }); }
    else if (code === 1) {
      const index = values.findIndex(line => line["id"] === id);
      if (index < 0) throw new ClientError("server", "Onchange updated an unknown line.");
      values[index] = { ...values[index], ...object(command[2], "line update"), id };
    } else throw new ClientError("unsupported", "Unsupported line command.");
  }
  return values;
}
function writableValue(field: Field, value: unknown, initial?: unknown): unknown {
  if (field.kind === "one2many") return lineCommands(field, value, initial);
  // RPC distinguishes JSON null (the ORM unset sentinel) from a boolean false.
  // Empty widgets/onchange values use false locally; retain it only for Boolean fields.
  if (value === false && field.kind !== "boolean") return field.kind === "many2many" ? [[6, 0, []]] : null;
  if (field.kind === "many2one") return value && typeof value === "object" ? integer(object(value)["id"], "relation id") : value;
  if (field.kind === "many2many") {
    return [[6, 0, array(value, "relation values").map(item => typeof item === "number" ? integer(item, "relation id") : integer(object(item)["id"], "relation id"))]];
  }
  return value;
}
// Onchange returns x2many command diffs; web_read returns projected records. Never send
// display labels back as writes, or treat a command tuple as a persisted identifier.
function relationDiff(previous: unknown, update: unknown, defaults = false): unknown[] {
  let values = previous === undefined || previous === false ? [] : [...array(previous, "previous relations")];
  const idOf = (value: unknown): number => typeof value === "number" ? integer(value, "relation id") : integer(object(value)["id"], "relation id");
  for (const raw of array(update, "relation commands")) {
    const command = array(raw, "relation command"), code = command[0];
    if (command.length !== 3) throw new ClientError("server", "Invalid relation command.");
    if (code === 5) { values = []; continue; }
    if (code === 6) { values = array(command[2], "relation IDs").map(id => integer(id, "relation id")); continue; }
    if (code === 0) throw new ClientError("unsupported", "Inline creation of related records requires the relational subform slice.");
    const id = integer(command[1], "relation id");
    if (code === 2 || code === 3) values = values.filter(value => idOf(value) !== id);
    else if (code === 4) { if (!values.some(value => idOf(value) === id)) values.push(defaults && command[2] === 0 ? id : { ...object(command[2], "linked record"), id }); }
    else if (code === 1) {
      const index = values.findIndex(value => idOf(value) === id);
      if (index < 0) throw new ClientError("server", "Onchange updated an unknown related record.");
      const old = values[index]; values[index] = { ...(typeof old === "number" ? { id: old } : object(old)), ...object(command[2], "relation update"), id };
    } else throw new ClientError("unsupported", "Unsupported relation command.");
  }
  return values;
}
export interface OnchangeWarning { readonly title: string; readonly message: string; readonly type: string; readonly className?: string; readonly sticky?: boolean }
/** A single form owner flushes local drafts before any save path and resets them on authoritative changes. */
export interface RecordEditorOwner {
  admit(values: Record<string, unknown>): void;
  /** Reconcile admitted authoritative onchange values without dropping staged field commits. */
  onchangeAccepted(values: Record<string, unknown>): void;
  beforeSave(): void | Promise<void>;
  setSaving(value: boolean): void;
  reset(): void;
}
export class RecordState {
  /** Changes when an explicit discard invalidates editor-local drafts. */
  draftVersion = 0;
  private editorOwner: RecordEditorOwner | undefined;
  private resetIdentity = {};
  /** Attach the mounted form; the returned disposer cannot detach a replacement owner. */
  bindEditorOwner(owner: RecordEditorOwner): () => void {
    if (this.editorOwner) throw new ClientError("validation", "This record already has an editor owner.");
    this.editorOwner = owner;
    return () => { if (this.editorOwner === owner) { this.editorOwner = undefined; this.resetIdentity = {}; } };
  }
  values: Record<string, unknown> = {};
  private initial: Record<string, unknown> = {};
  private changes = new Set<string>();
  private policyFields: readonly Field[] | undefined;
  private policyFailure: Error | undefined;
  private unknownWrite = false;
  private pending = false;
  id: number | undefined;
  warning = "";
  warningDetails: OnchangeWarning | undefined;
  constructor(private readonly rpc: RpcClient, readonly model: string, readonly fields: readonly Field[], private readonly context: Record<string, unknown>) {}
  /** Publish effective occurrence policy atomically before a save; expression failure remains blocking. */
  updatePolicy(fields: readonly Field[], failure?: Error): void {
    if (fields.some(field => !this.fields.some(item => item.name === field.name))) throw new ClientError("server", "Effective policy names an unrequested field.");
    this.policyFields = fields; this.policyFailure = failure;
  }
  private get effectiveFields(): readonly Field[] { return this.policyFields ?? this.fields; }
  /** Effective presentation/save policy from the last successful form evaluation. */
  get currentFields(): readonly Field[] { return this.effectiveFields; }
  get dirty(): boolean { return this.changes.size !== 0; }
  get busy(): boolean { return this.pending; }
  /** A sent save requires authoritative readback before another write. */
  get writeUncertain(): boolean { return this.unknownWrite; }
  private args(extra: Record<string, unknown>): Record<string, unknown> { return { ...extra, context: this.context }; }
  async load(id?: number): Promise<void> {
    if (this.pending) throw new ClientError("validation", "Wait for the current operation to complete.");
    const previous = {
      id: this.id,
      values: this.values,
      initial: this.initial,
      changes: this.changes,
      unknownWrite: this.unknownWrite,
      warning: this.warning,
      warningDetails: this.warningDetails,
    };
    this.pending = true;
    try {
      if (this.unknownWrite && this.id !== undefined && id !== this.id) {
        throw new ClientError("ambiguous", "Reload the same record to verify the previous change before navigating away.");
      }
      // A default_get response has no identity and cannot establish whether an uncertain
      // create committed. Preserve the draft and keep the write guard even when it succeeds.
      if (this.unknownWrite && this.id === undefined && id === undefined) {
        object(await this.rpc.call(this.model, "default_get", this.args({ fields: this.fields.map(field => field.name) })), "defaults");
        throw new ClientError("ambiguous", "Defaults cannot verify whether this record was created. Find it before continuing.");
      }
      if (id === undefined) {
        this.id = undefined;
        this.values = object(await this.rpc.call(this.model, "default_get", this.args({ fields: this.fields.map(field => field.name) })), "defaults");
        // default_get uses ORM commands, whereas widgets and vals() use IDs/projected
        // records. Normalize once at this boundary, before initial onchange serializes them.
        for (const field of this.fields) {
          if (field.kind === "one2many" && Object.hasOwn(this.values, field.name)) {
            const value = this.values[field.name];
            const items = value === false ? [] : array(value, "line defaults");
            const normalized = items.some(Array.isArray) ? lineDiff(undefined, items) : items.map(id => ({ id: integer(id, "default line id") }));
            this.values = { ...this.values, [field.name]: normalized };
            continue;
          }
          if (field.kind !== "many2many" || !Object.hasOwn(this.values, field.name)) continue;
          const value = this.values[field.name];
          const items = value === false ? [] : array(value, "relation defaults");
          const normalized = items.some(Array.isArray)
            ? relationDiff(undefined, items, true)
            : items.map(id => integer(id, "default relation id"));
          this.values = { ...this.values, [field.name]: normalized };
        }
        await this.applyOnchange("");
      } else {
        const records = array(await this.rpc.call(this.model, "web_read", this.args({ ids: [id], specification: specification(this.fields) })), "records");
        if (records.length !== 1 || object(records[0])["id"] !== id) throw new ClientError("server", "The record is unavailable or the response did not match it.");
        this.id = id;
        this.values = object(records[0]);
      }
      this.editorOwner?.admit(this.values);
      this.resetIdentity = {};
      this.initial = structuredClone(this.values);
      this.changes.clear();
      this.unknownWrite = false;
      this.draftVersion += 1;
      this.editorOwner?.reset();
    } catch (error) {
      this.id = previous.id;
      this.values = previous.values;
      this.initial = previous.initial;
      this.changes = previous.changes;
      this.unknownWrite = previous.unknownWrite;
      this.warning = previous.warning;
      this.warningDetails = previous.warningDetails;
      throw error;
    } finally { this.pending = false; }
  }
  /** Stage all related endpoint changes after validating the complete set; never publish half a range. */
  changeMany(changes: Readonly<Record<string, unknown>>): void {
    const entries = Object.entries(changes);
    if (!entries.length || entries.length > this.fields.length) throw new ClientError("validation", "Invalid field change set.");
    if (this.pending) throw new ClientError("validation", "Wait for the current operation to complete.");
    for (const [name] of entries) {
      const field = this.effectiveFields.find(candidate => candidate.name === name);
      if (!field || field.readonly || name === "id") throw new ClientError("validation", "This field cannot be edited.");
    }
    const values = { ...this.values }, changed = new Set(this.changes);
    for (const [name, value] of entries) {
      values[name] = value;
      if (JSON.stringify(value) === JSON.stringify(this.initial[name])) changed.delete(name); else changed.add(name);
    }
    this.values = values; this.changes = changed;
  }
  change(name: string, value: unknown): void { this.changeMany({ [name]: value }); }
  discard(): void { this.draftVersion += 1; this.resetIdentity = {}; this.values = structuredClone(this.initial); this.changes.clear(); this.warning = ""; this.warningDetails = undefined; this.editorOwner?.reset(); }
  private vals(all: boolean, withReadonly = false): Record<string, unknown> {
    return Object.fromEntries(this.effectiveFields.filter(field => field.name !== "id" && (withReadonly || !field.readonly || field.forceSave) && (all || this.changes.has(field.name)) && Object.hasOwn(this.values, field.name)).map(field => [field.name, writableValue(field, this.values[field.name], this.initial[field.name])]));
  }
  private async applyOnchange(name: string | readonly string[]): Promise<void> {
    const identity = this.resetIdentity;
    const response = await this.rpc.call(this.model, "onchange", this.args({ ids: this.id === undefined ? [] : [this.id], values: this.vals(true, true), field_names: typeof name === "string" ? name ? [name] : [] : [...name], fields_spec: specification(this.fields) }));
    if (identity !== this.resetIdentity) throw new ClientError("validation", "Onchange was superseded by a record reset.");
    const result = object(response, "onchange response");
    const update = object(result["value"], "onchange values");
    // Draft admission precedes mutation so a malformed source cannot destroy an accepted draft.
    this.editorOwner?.admit({ ...this.values, ...update });
    for (const [key, value] of Object.entries(update)) {
      if (!this.fields.some(field => field.name === key)) throw new ClientError("server", "Onchange returned an unrequested field.");
      const field = this.fields.find(candidate => candidate.name === key)!;
      const resolved = field.kind === "many2many" ? relationDiff(this.values[key], value) : field.kind === "one2many" ? lineDiff(this.values[key], value) : value;
      this.values = { ...this.values, [key]: resolved };
      if (name) this.changes.add(key);
    }
    this.editorOwner?.onchangeAccepted(this.values);
    if (result["warning"] !== undefined) {
      // Ordinary onchange retains its updated values when warning presentation is refused.
      // Clear older feedback and report the boundary instead of publishing stale or truncated text.
      this.warning = ""; this.warningDetails = undefined;
      const warning = object(result["warning"], "onchange warning");
      for (const key of Object.keys(warning)) if (!["title", "message", "type", "className", "sticky"].includes(key)) throw new ClientError("server", `Unknown onchange warning option: ${key}.`);
      const className = Object.hasOwn(warning, "className") ? notificationClasses(warning["className"], "onchange warning") : undefined;
      if (Object.hasOwn(warning, "sticky") && typeof warning["sticky"] !== "boolean") throw new ClientError("server", "Onchange warning sticky must be boolean.");
      if (typeof warning["message"] !== "string" || warning["title"] !== undefined && typeof warning["title"] !== "string" || warning["type"] !== undefined && typeof warning["type"] !== "string") throw new ClientError("server", "Invalid onchange warning.");
      const title = warning["title"] === undefined ? "Warning" : warning["title"];
      const type = warning["type"] === undefined ? "dialog" : warning["type"];
      // MessageServices forwards the body unchanged to DialogService, whose body cap is 8192 bytes.
      if (new TextEncoder().encode(warning["message"]).length > 8192) throw new ClientError("server", "Onchange warning message must be text of at most 8192 bytes.");
      if (new TextEncoder().encode(title).length > 4096 || type.length > 256) throw new ClientError("server", "Onchange warning exceeds its text limit.");
      this.warning = warning["message"];
      this.warningDetails = { title, message: this.warning, type, ...(className === undefined ? {} : { className }), ...(Object.hasOwn(warning, "sticky") ? { sticky: warning["sticky"] as boolean } : {}) };
    } else { this.warning = ""; this.warningDetails = undefined; }
  }
  async onchange(name: string | readonly string[]): Promise<void> {
    if (this.pending) throw new ClientError("validation", "Wait for the current operation to complete.");
    // Typed secrets of the managed password forms travel only in their operation (item 7). Those
    // transient models declare no onchange, so an edit stays in the browser.
    if (passwordModels.has(this.model)) return;
    this.pending = true;
    try { await this.applyOnchange(name); }
    finally { this.pending = false; }
  }
  async save(): Promise<void> {
    if (passwordModels.has(this.model)) throw new ClientError("validation", "A password form is submitted with its button; it is never saved.");
    if (this.policyFailure) throw this.policyFailure;
    if (this.unknownWrite) throw new ClientError("ambiguous", "Reload the record before another write; the previous change may already be saved.");
    const owner = this.editorOwner, identity = this.resetIdentity;
    if (owner) {
      const admission = owner.beforeSave();
      if (admission !== undefined) await admission;
      if (this.editorOwner !== owner) throw new ClientError("validation", "The record editor owner changed before saving.");
      if (this.resetIdentity !== identity) throw new ClientError("validation", "Save was superseded by a record reset.");
    }
    // A second save may have entered while an asynchronous draft commit was pending.
    if (this.pending) throw new ClientError("validation", "Wait for the current operation to complete.");
    if (this.unknownWrite) throw new ClientError("ambiguous", "Reload the record before another write; the previous change may already be saved.");
    for (const field of this.effectiveFields) {
      const value = this.values[field.name];
      if (field.required && !field.readonly && !field.invisible && (value === undefined || value === null || value === "" || field.kind !== "boolean" && value === false
        || (field.kind === "one2many" || field.kind === "many2many") && Array.isArray(value) && value.length === 0)) throw new ClientError("validation", `${field.label} is required.`);
    }
    this.pending = true;
    let received = false;
    try {
      owner?.setSaving(true);
      const response = await this.rpc.call(this.model, "web_save", this.args({ ids: this.id === undefined ? [] : [this.id], vals: this.vals(this.id === undefined), specification: specification(this.fields) }), { write: true });
      received = true;
      const rows = array(response, "saved records");
      if (rows.length !== 1) { this.unknownWrite = true; throw new ClientError("ambiguous", "Unexpected save response. Check whether the record was saved before continuing."); }
      const saved = object(rows[0]), id = integer(saved["id"], "saved record id");
      if (!id || this.id !== undefined && id !== this.id) { this.unknownWrite = true; throw new ClientError("ambiguous", "Save response did not match the record. Reload before continuing."); }
      this.editorOwner?.admit(saved);
      this.id = id; this.values = saved; this.initial = structuredClone(saved); this.changes.clear(); this.editorOwner?.reset();
    } catch (error) {
      if (received || error instanceof ClientError && (error.kind === "ambiguous" || error.status >= 500)) this.unknownWrite = true;
      if (received && !(error instanceof ClientError && error.kind === "ambiguous")) throw new ClientError("ambiguous", "The save response was invalid. Reload the record before another write.");
      throw error;
    } finally { this.pending = false; owner?.setSaving(false); }
  }
}
