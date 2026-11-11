/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
import { KeyedRecordStore, batch, untrack } from "@erpforall/elang";
import {
  Decoder,
  StoreTable,
  WireError,
  lookupField,
  lookupModel,
  type ApplyOutcome,
  type DecodedPayload,
  type Outcome,
  type StoreSchema,
} from "@erpforall/store-wire";
import { type MessagingConfig, resolveConfig } from "./config";
import { MessagingStoreError } from "./errors";
import { type HandleHost, RecordHandle } from "./handle";
import type { CounterView, ModelName, RecordId, ResyncRequest } from "./types";
import { type ThreadRef, ThreadWindows } from "./windows";

/** The result of applying one answer or frame. */
export type ApplyResult =
  | {
      /** The payload was applied; `resync` lists counters that can no longer be shown. */
      readonly status: "applied";
      readonly resync: readonly ResyncRequest[];
    }
  | {
      /** The payload is for a schema generation the store does not hold: fetch it, `setSchema`, apply again. */
      readonly status: "needs_schema";
      readonly generation: string;
    }
  | {
      /** The decoder refused the payload; nothing was applied. The error was reported. */
      readonly status: "refused";
      readonly error: WireError;
      readonly resync: readonly ResyncRequest[];
    };

/** What {@link MessagingStore.setSchema} did. */
export interface SchemaResult {
  /** Whether the generation differs from the one held before (all wire records were dropped). */
  readonly changed: boolean;
  /** `schema_changed` when records were dropped, so surfaces refetch. */
  readonly resync: readonly ResyncRequest[];
}

/** A hold on a record by a displayed surface: while held, collection never drops it. */
export interface Lease {
  readonly model: string;
  readonly id: number;
  /** Ends the hold; calling it again does nothing. */
  readonly release: () => void;
}

/** What one {@link MessagingStore.collect} pass removed. */
export interface CollectReport {
  /** Windows dropped (least recently used beyond `maxThreads`). */
  readonly windows: number;
  /** Records evicted, by model. */
  readonly evicted: Readonly<Record<string, number>>;
}

/** The construction options of {@link MessagingStore}. */
export interface MessagingStoreOptions {
  /**
   * Receives every refusal the store handles instead of throwing (the browser error report in the app).
   * Required: there is no silent default.
   */
  readonly reportError: (error: MessagingStoreError | WireError) => void;
  /** A schema to hold at once (text or UTF-8 bytes), as `setSchema` would. */
  readonly schema?: string | Uint8Array;
  /** Caps and names; see {@link MessagingConfig}. */
  readonly config?: Partial<MessagingConfig>;
}

/** A delegation rule: reads a model lacks fall through to the record its many2one `via` field names. */
export interface DelegationRule {
  readonly via: string;
}

/** The local-record API of one local model. */
export interface LocalModel {
  readonly model: string;
  /** Creates a record. @throws {@link MessagingStoreError} `bad_id` or `bad_local` (it exists) or `unknown_field` */
  create(id: string, values?: Readonly<Record<string, unknown>>): void;
  /** Reads a field (tracked in a reactive scope); `undefined` for a missing record or an unset field. */
  get(id: string, field: string): unknown;
  /** Writes fields. @throws {@link MessagingStoreError} `bad_local` (no such record) or `unknown_field` */
  set(id: string, values: Readonly<Record<string, unknown>>): void;
  /** Removes the record; an absent record is an error (`bad_local`). */
  delete(id: string): void;
  /** Whether the record exists. */
  has(id: string): boolean;
  /** The record ids. */
  ids(): string[];
}

interface LocalDefinition {
  readonly fields: readonly string[];
}

const RELATION_KINDS = new Set(["many2one", "x2many", "reference"]);

function sameArray(a: readonly number[], b: readonly number[]): boolean {
  return a.length === b.length && a.every((v, i) => v === b[i]);
}

function sameValue(a: unknown, b: unknown): boolean {
  if (Object.is(a, b)) return true;
  if (Array.isArray(a) && Array.isArray(b)) return sameArray(a as number[], b as number[]);
  if (typeof a === "object" && typeof b === "object" && a !== null && b !== null) {
    const x = a as Record<string, unknown>;
    const y = b as Record<string, unknown>;
    const keys = Object.keys(x);
    return keys.length === Object.keys(y).length && keys.every((k) => Object.is(x[k], y[k]));
  }
  return false;
}

function coordKey(model: string, id: number, field: string): string {
  return `${model}\0${id}\0${field}`;
}

function refKey(model: string, id: number): string {
  return `${model}\0${id}`;
}

/**
 * The messaging store of one browser tab. It serves every messaging surface from one set of records
 * keyed by (model, id), applies wire payloads (answers and bus frames) through the reference applier of
 * `@erpforall/store-wire`, and keeps its memory bounded.
 *
 * Values reach the reactive tables only by projection from the applier after a payload, so a value the applier
 * refused as stale never shows..
 */
export class MessagingStore {
  /** The message windows of the store's threads. */
  readonly windows: ThreadWindows;
  /** The resolved configuration. */
  readonly config: MessagingConfig;

  readonly #reportError: (error: MessagingStoreError | WireError) => void;
  readonly #host: HandleHost = {
    read: (model, id, field) => this.#read(model, id, field),
    relation: (model, id, field, kind) => this.#relation(model, id, field, kind),
    counter: (model, id, field) => this.#counterOf(model, id, field),
    exists: (model, id) => this.#exists(model, id),
    write: (model, id, field, value) => this.#write(model, id, field, value),
  };
  readonly #keyed = new KeyedRecordStore();
  readonly #decoder = new Decoder();
  readonly #locals = new Map<string, LocalDefinition>();
  readonly #delegations = new Map<string, { readonly via: string; readonly target: string }>();
  readonly #leases = new Map<string, number>();
  /** x2many target -> the coordinates (`model\0id\0field`) that list it. */
  readonly #referrers = new Map<string, Set<string>>();
  readonly #requests = new Map<number, { readonly table: StoreTable; readonly inner: number }>();
  #nextRequest = 1;
  #schema: StoreSchema | null = null;
  #table: StoreTable | null = null;

  constructor(options: MessagingStoreOptions) {
    if (typeof options.reportError !== "function") {
      throw new MessagingStoreError("bad_config", "options.reportError is required: the store never swallows an error");
    }
    this.#reportError = options.reportError;
    this.config = resolveConfig(options.config);
    this.windows = new ThreadWindows(this.#keyed, this.config.pageSize * this.config.maxWindowPages);
    if (options.schema !== undefined) this.setSchema(options.schema);
  }

  /** The schema generation held, or `null` before `setSchema`. */
  get generation(): string | null {
    return this.#schema === null ? null : this.#schema.generation;
  }

  /**
   * Hold a schema. The same generation changes nothing. A different generation drops every wire record and window
   * (local records stay) and asks the host to refetch.
   *
   * @param input - the schema document, as text or UTF-8 bytes
   * @throws {@link WireError} for a schema the decoder refuses
   * @throws {@link MessagingStoreError} `local_model_clash` when the schema declares a local model's name;
   *   `delegation` when a registered delegation no longer holds in it
   */
  setSchema(input: string | Uint8Array): SchemaResult {
    const schema = this.#decoder.decodeSchema(input);
    for (const name of this.#locals.keys()) {
      if (lookupModel(schema, name) !== undefined) {
        throw new MessagingStoreError("local_model_clash", `schema ${schema.generation} declares ${name}, which is a local model of this store`);
      }
    }
    const rules = [...this.#delegations].map(([model, rule]) => ({ model, via: rule.via }));
    const targets = new Map<string, string>();
    for (const { model, via } of rules) targets.set(model, this.#checkRule(schema, model, via));
    for (const start of targets.keys()) {
      let at = targets.get(start);
      for (let hops = 0; at !== undefined && hops <= targets.size; hops++) {
        if (at === start) throw new MessagingStoreError("delegation", `schema ${schema.generation} makes the delegation of ${start} a cycle`);
        at = targets.get(at);
      }
    }
    if (this.#schema !== null && this.#schema.generation === schema.generation) return { changed: false, resync: [] };
    const hadSchema = this.#schema !== null;
    batch(() => {
      this.#dropWire();
      this.#decoder.setSchema(schema);
      this.#schema = schema;
      this.#table = new StoreTable(schema);
      for (const [model, target] of targets) this.#delegations.set(model, { via: this.#delegations.get(model)?.via ?? "", target });
    });
    return { changed: hadSchema, resync: hadSchema ? [{ reason: "schema_changed" }] : [] };
  }

  /**
   * Note that a request for an answer was sent. Revisions and tombstones made while it is outstanding are kept until
   * it settles (D-6).
   *
   * @returns the id for {@link MessagingStore.applyAnswer} and {@link MessagingStore.settleRequest}
   * @throws {@link MessagingStoreError} `no_schema`
   */
  beginRequest(): number {
    const table = this.#requireTable("beginRequest");
    const id = this.#nextRequest++;
    this.#requests.set(id, { table, inner: table.beginRequest() });
    return id;
  }

  /**
   * Note that a request is finished (its answer applied or given up). Releases the tombstones and retired records
   * that waited for it, then collects when `collectOnSettle` is on.
   *
   * @param request - an id from {@link MessagingStore.beginRequest}
   * @throws {@link MessagingStoreError} `request` for an id that is not outstanding
   */
  settleRequest(request: number): void {
    const held = this.#requests.get(request);
    if (held === undefined) throw new MessagingStoreError("request", `request ${request} is not outstanding`);
    this.#requests.delete(request);
    if (held.table === this.#table) held.table.settleRequest(held.inner);
    if (this.config.collectOnSettle) this.collect();
  }

  /**
   * Decode and apply an RPC answer.
   *
   * @param input - the answer document, as text or UTF-8 bytes
   * @param request - the request that fetched it, when the host tracks requests
   * @throws {@link MessagingStoreError} `no_schema`, or `request` for an id that is not outstanding
   */
  applyAnswer(input: string | Uint8Array, request?: number): ApplyResult {
    const table = this.#requireTable("applyAnswer");
    let inner: number | undefined;
    if (request !== undefined) {
      const held = this.#requests.get(request);
      if (held === undefined) throw new MessagingStoreError("request", `request ${request} is not outstanding`);
      inner = held.table === table ? held.inner : undefined;
    }
    return this.#apply(() => this.#decoder.decodeAnswer(input), (payload) => table.applyAnswer(payload, inner));
  }

  /**
   * Decode and apply a bus frame, through the applier's frame entry. Duplicate frames are harmless (the applier is
   * idempotent); ordering and replay belong to the bus client.
   *
   * @param input - the frame document, as text or UTF-8 bytes
   * @throws {@link MessagingStoreError} `no_schema`
   */
  applyFrame(input: string | Uint8Array): ApplyResult {
    const table = this.#requireTable("applyFrame");
    return this.#apply(() => this.#decoder.decodeFrame(input), (payload) => table.applyFrame(payload));
  }

  /**
   * A handle on a record. The model must be in the schema or be a local model; a wire id is a safe integer, a local
   * id a non-empty string.
   *
   * @param model - the model name
   * @param id - the record id
   * @throws {@link MessagingStoreError} `unknown_model` or `bad_id`
   */
  record<M extends ModelName>(model: M, id: RecordId): RecordHandle<M> {
    this.#checkRecord("record", model, id);
    return new RecordHandle(this.#host, model, id);
  }

  /**
   * Chains are followed.
   *
   * @param model - a schema model
   * @param rule - the many2one field naming the parent record
   * @throws {@link MessagingStoreError} `no_schema`, `unknown_model`, or `delegation` for a missing or non-many2one
   *   field, a model that already delegates, or a cycle
   */
  delegate(model: string, rule: DelegationRule): void {
    const schema = this.#schema;
    if (schema === null) throw new MessagingStoreError("no_schema", `delegate(${model}): set a schema first`);
    if (this.#delegations.has(model)) {
      throw new MessagingStoreError("delegation", `${model} already delegates through ${this.#delegations.get(model)?.via ?? ""}`);
    }
    const target = this.#checkRule(schema, model, rule.via);
    let at = target;
    while (at !== model) {
      const next = this.#delegations.get(at);
      if (next === undefined) {
        this.#delegations.set(model, { via: rule.via, target });
        return;
      }
      at = next.target;
    }
    throw new MessagingStoreError("delegation", `delegating ${model} through ${rule.via} to ${target} would make a cycle`);
  }

  /**
   * Declares a local model: UI records of this tab that are never sent to the server (3).
   *
   * @param model - the model name; not a schema model
   * @param fields - the field names a record may hold
   * @throws {@link MessagingStoreError} `local_model_clash` for a schema model name, `bad_local` for a bad or
   *   repeated definition
   */
  defineLocalModel(model: string, fields: readonly string[]): LocalModel {
    if (typeof model !== "string" || model === "" || model.includes("$")) {
      throw new MessagingStoreError("bad_local", `local model name must be a non-empty string without "$", got ${JSON.stringify(model)}`);
    }
    if (this.#locals.has(model)) throw new MessagingStoreError("bad_local", `local model ${model} is already defined`);
    if (this.#schema !== null && lookupModel(this.#schema, model) !== undefined) {
      throw new MessagingStoreError("local_model_clash", `${model} is a schema model; a local model needs its own name`);
    }
    if (fields.length === 0 || fields.some((f) => typeof f !== "string" || f === "") || new Set(fields).size !== fields.length) {
      throw new MessagingStoreError("bad_local", `fields of local model ${model} must be a non-empty list of unique non-empty strings`);
    }
    this.#locals.set(model, { fields: [...fields] });
    return this.local(model);
  }

  /**
   * The API of a defined local model.
   *
   * @param model - a local model
   * @throws {@link MessagingStoreError} `unknown_model` for a model that is not local
   */
  local(model: string): LocalModel {
    const def = this.#locals.get(model);
    if (def === undefined) throw new MessagingStoreError("unknown_model", `${model} is not a local model of this store`);
    const table = this.#keyed.table(model);
    const checkId = (method: string, id: unknown): string => {
      if (typeof id !== "string" || id === "") {
        throw new MessagingStoreError("bad_id", `${model}.${method}: a local record id must be a non-empty string, got ${String(id)}`);
      }
      return id;
    };
    const checkFields = (method: string, values: Readonly<Record<string, unknown>>): void => {
      for (const field of Object.keys(values)) {
        if (!def.fields.includes(field)) throw new MessagingStoreError("unknown_field", `${model}.${method}: ${field} is not a field of local model ${model}`);
      }
    };
    const missing = (method: string, id: string): MessagingStoreError =>
      new MessagingStoreError("bad_local", `${model}.${method}: no local record ${JSON.stringify(id)}`);
    return {
      model,
      create: (id, values = {}) => {
        checkId("create", id);
        checkFields("create", values);
        if (untrack(() => table.hasRecord(id))) {
          throw new MessagingStoreError("bad_local", `${model}.create: local record ${JSON.stringify(id)} already exists`);
        }
        batch(() => {
          // A record holds a value for every declared field so that it exists even when created empty.
          for (const field of def.fields) table.set(id, field, Object.hasOwn(values, field) ? values[field] : undefined);
        });
      },
      get: (id, field) => {
        checkId("get", id);
        if (!def.fields.includes(field)) throw new MessagingStoreError("unknown_field", `${model}.get: ${field} is not a field of local model ${model}`);
        return table.get(id, field);
      },
      set: (id, values) => {
        checkId("set", id);
        checkFields("set", values);
        if (!untrack(() => table.hasRecord(id))) throw missing("set", id);
        batch(() => {
          for (const [field, value] of Object.entries(values)) table.set(id, field, value);
        });
      },
      delete: (id) => {
        checkId("delete", id);
        if (!untrack(() => table.hasRecord(id))) throw missing("delete", id);
        table.deleteRecord(id);
      },
      has: (id) => untrack(() => table.hasRecord(checkId("has", id))),
      ids: () => untrack(() => [...table.recordIds()].filter((id): id is string => typeof id === "string")),
    };
  }

  /**
   * The local records of one model as plain data, for the owner's own persistence (localStorage, IndexedDB; D7). This
   * is the only export of local data, and a wire model is refused, so server data is never written out through it.
   *
   * @param model - a local model
   * @throws {@link MessagingStoreError} `not_local` for a schema model, `unknown_model` for an unknown name
   */
  persistable(model: string): { id: string; values: Record<string, unknown> }[] {
    if (!this.#locals.has(model)) {
      if (this.#schema !== null && lookupModel(this.#schema, model) !== undefined) {
        throw new MessagingStoreError("not_local", `${model} is a server model; only local models can be persisted by the client`);
      }
      throw new MessagingStoreError("unknown_model", `${model} is not a local model of this store`);
    }
    const def = this.#locals.get(model);
    const table = this.#keyed.table(model);
    return untrack(() =>
      [...table.recordIds()].map((id) => ({
        id: String(id),
        values: Object.fromEntries((def?.fields ?? []).map((f) => [f, table.get(id, f)])),
      })),
    );
  }

  /**
   * Marks a record as held by a displayed surface. While any lease on it is held, {@link MessagingStore.collect}
   * keeps the record, a held thread's window and everything the window's messages reference.
   *
   * @param model - a schema model
   * @param id - a server record id
   * @throws {@link MessagingStoreError} `unknown_model` (local models are never collected) or `bad_id`
   */
  hold(model: string, id: number): Lease {
    if (this.#locals.has(model)) {
      throw new MessagingStoreError("unknown_model", `hold(${model}): local records are never collected, hold server records only`);
    }
    this.#checkRecord("hold", model, id);
    const key = refKey(model, id);
    this.#leases.set(key, (this.#leases.get(key) ?? 0) + 1);
    this.windows.touch({ model, id });
    let released = false;
    return {
      model,
      id,
      release: () => {
        if (released) return;
        released = true;
        const count = (this.#leases.get(key) ?? 1) - 1;
        if (count <= 0) this.#leases.delete(key);
        else this.#leases.set(key, count);
        this.windows.touch({ model, id });
      },
    };
  }

  /**
   * Bounds memory (4). Drops the windows of threads no surface holds beyond `maxThreads`, least recently
   * used first; then evicts the messages no kept window or lease holds, and the collectable records nothing retained
   * references. Relations of retained records are followed through the schema into collectable models only; models
   * that are neither messages nor collectable are roots and are never swept here.
   *
   * Call it when a fetch has finished and its windows are updated (`settleRequest` does when `collectOnSettle` is on):
   * records an answer brought are unreferenced until the window update names them.
   */
  collect(): CollectReport {
    const schema = this.#schema;
    const table = this.#table;
    const evicted: Record<string, number> = {};
    let windows = 0;
    if (schema === null || table === null) return { windows, evicted };
    const held = (ref: ThreadRef): boolean => this.#leases.has(refKey(ref.model, ref.id));
    const candidates = this.windows.threads().filter((ref) => !held(ref));
    candidates.sort((a, b) => this.windows.lastUsed(b) - this.windows.lastUsed(a));
    for (const ref of candidates.slice(this.config.maxThreads)) {
      this.windows.drop(ref);
      windows++;
    }

    const message = this.config.messageModel;
    const collectable = new Set(this.config.collectModels);
    const retained = new Map<string, Set<number>>();
    const queue: { model: string; id: number }[] = [];
    const retain = (model: string, id: number): void => {
      let set = retained.get(model);
      if (set === undefined) retained.set(model, (set = new Set()));
      if (set.has(id)) return;
      set.add(id);
      queue.push({ model, id });
    };
    for (const key of this.#leases.keys()) {
      const [model, id] = key.split("\0");
      if (model !== undefined && id !== undefined) retain(model, Number(id));
    }
    for (const id of this.windows.messageIds()) retain(message, id);
    for (const model of this.#wireModels()) {
      if (model === message || collectable.has(model)) continue;
      for (const id of untrack(() => [...this.#keyed.table(model).recordIds()])) {
        if (typeof id === "number") retain(model, id);
      }
    }
    while (queue.length > 0) {
      const next = queue.pop();
      if (next === undefined) break;
      const modelSchema = lookupModel(schema, next.model);
      if (modelSchema === undefined) continue;
      for (const [name, spec] of Object.entries(modelSchema.fields)) {
        if (spec === undefined || !RELATION_KINDS.has(spec.kind)) continue;
        const value = untrack(() => this.#keyed.table(next.model).get(next.id, name));
        for (const ref of this.#targets(spec.kind, spec.target, value)) {
          if (collectable.has(ref.model)) retain(ref.model, ref.id);
        }
      }
    }
    for (const model of [message, ...collectable]) {
      const keep = retained.get(model);
      for (const id of untrack(() => [...this.#keyed.table(model).recordIds()])) {
        if (typeof id !== "number" || keep?.has(id) === true) continue;
        this.#evict(model, id);
        evicted[model] = (evicted[model] ?? 0) + 1;
      }
    }
    return { windows, evicted };
  }

  #read(model: string, id: RecordId, field: string): unknown {
    const at = this.#locate(model, id, field);
    return at === undefined ? undefined : this.#keyed.table(at.model).get(at.id, field);
  }

  #relation(model: string, id: RecordId, field: string, kind: "many2one" | "x2many"): { target: string; ids: readonly number[] } {
    const spec = this.#specOf(model, field);
    if (spec.kind !== kind || spec.target === null) {
      throw new MessagingStoreError("field_kind", `${model}.${field} is a ${spec.kind} field, not ${kind === "x2many" ? "an x2many" : "a many2one"} field`);
    }
    const at = this.#locate(model, id, field);
    const value = at === undefined ? undefined : this.#keyed.table(at.model).get(at.id, field);
    if (kind === "many2one") return { target: spec.target, ids: typeof value === "number" ? [value] : [] };
    return { target: spec.target, ids: Array.isArray(value) ? (value as number[]) : [] };
  }

  #counterOf(model: string, id: RecordId, field: string): CounterView | undefined {
    const spec = this.#specOf(model, field);
    if (spec.kind !== "counter") throw new MessagingStoreError("field_kind", `${model}.${field} is a ${spec.kind} field, not a counter field`);
    return this.#read(model, id, field) as CounterView | undefined;
  }

  #write(model: string, id: RecordId, field: string, value: unknown): void {
    this.#checkRecord("set", model, id);
    if (this.#locals.has(model)) {
      this.local(model).set(String(id), { [field]: value });
      return;
    }
    this.#specOf(model, field);
    let at = model;
    while (!this.#declares(at, field)) at = this.#delegations.get(at)?.target ?? at;
    const why = at === model ? "" : `it is delegated to ${at} and `;
    throw new MessagingStoreError("read_only", `${model}.${field} is read-only: ${why}server records are written by payloads only`);
  }

  #exists(model: string, id: RecordId): boolean {
    if (this.#locals.has(model)) return untrack(() => this.#keyed.table(model).hasRecord(id));
    return typeof id === "number" && this.#table !== null && this.#table.hasRecord(model, id);
  }

  // ---- applying

  #apply(decode: () => Outcome<DecodedPayload>, run: (payload: DecodedPayload) => ApplyOutcome): ApplyResult {
    let outcome: Outcome<DecodedPayload>;
    try {
      outcome = decode();
    } catch (error) {
      if (!(error instanceof WireError)) throw error;
      return this.#refuse(error);
    }
    if (outcome.outcome === "needs_schema") return { status: "needs_schema", generation: outcome.generation };
    const payload = outcome.payload;
    let applied: ApplyOutcome;
    try {
      applied = run(payload);
    } finally {
      // A host-contract error can leave a payload partly applied: the tables still follow what the applier holds.
      batch(() => this.#project(payload));
    }
    const resync: ResyncRequest[] = applied.resync.map((r) => ({
      reason: "counter_stale",
      model: r.model,
      id: r.id,
      field: r.field,
      counterReason: r.reason,
    }));
    return { status: "applied", resync };
  }

  #refuse(error: WireError): ApplyResult {
    this.#reportError(error);
    const reason = error.code === "unknown_model" ? "unknown_model" : error.code === "unknown_field" ? "unknown_field" : "payload_refused";
    return { status: "refused", error, resync: [{ reason, path: error.path, detail: error.message }] };
  }

  #project(payload: DecodedPayload): void {
    const schema = this.#schema;
    const table = this.#table;
    if (schema === null || table === null) return;
    for (const record of payload.upserts) {
      const modelSchema = lookupModel(schema, record.model);
      if (modelSchema === undefined) continue;
      // A record with no reactive row is new, or was evicted and has just been revived by this payload: the
      // applier may hold coordinates the payload did not mention, so project every field the schema declares.
      const keyed = this.#keyed.table(record.model);
      const names = untrack(() => keyed.hasRecord(record.id))
        ? record.fields.map((f) => f.name)
        : Object.keys(modelSchema.fields);
      for (const name of names) this.#projectField(table, record.model, record.id, name, modelSchema);
    }
    for (const ref of payload.deletes) {
      if (table.hasRecord(ref.model, ref.id)) continue;
      this.#removeRow(ref.model, ref.id);
      if (ref.model === this.config.messageModel) this.windows.remove(ref.id);
      for (const key of [...(this.#referrers.get(refKey(ref.model, ref.id)) ?? [])]) {
        const [model, id, field] = key.split("\0");
        const targetSchema = model === undefined ? undefined : lookupModel(schema, model);
        if (model !== undefined && id !== undefined && field !== undefined && targetSchema !== undefined) {
          this.#projectField(table, model, Number(id), field, targetSchema);
        }
      }
    }
  }

  #projectField(table: StoreTable, model: string, id: number, name: string, modelSchema: NonNullable<ReturnType<typeof lookupModel>>): void {
    const spec = lookupField(modelSchema, name);
    if (spec === undefined) return;
    let value: unknown;
    switch (spec.kind) {
      case "x2many": {
        const members = table.members(model, id, name);
        if (members === undefined) return;
        this.#reindex(model, id, name, spec.target ?? "", members);
        value = Object.freeze(members);
        break;
      }
      case "counter": {
        value = table.counter(model, id, name);
        if (value === undefined) return;
        break;
      }
      default: {
        const held = table.value(model, id, name);
        if (held === undefined) return;
        value = (held as { value: unknown }).value;
      }
    }
    const keyed = this.#keyed.table(model);
    if (untrack(() => keyed.has(id, name) && sameValue(keyed.get(id, name), value))) return;
    keyed.set(id, name, value);
  }

  #reindex(model: string, id: number, field: string, target: string, next: readonly number[]): void {
    const keyed = this.#keyed.table(model);
    const before = untrack(() => keyed.get(id, field));
    const prev = Array.isArray(before) ? (before as number[]) : [];
    const key = coordKey(model, id, field);
    for (const m of prev) {
      if (next.includes(m)) continue;
      const set = this.#referrers.get(refKey(target, m));
      set?.delete(key);
      if (set?.size === 0) this.#referrers.delete(refKey(target, m));
    }
    for (const m of next) {
      const rk = refKey(target, m);
      let set = this.#referrers.get(rk);
      if (set === undefined) this.#referrers.set(rk, (set = new Set()));
      set.add(key);
    }
  }

  /** Drops a record's projected row and its entries in the referrer index. */
  #removeRow(model: string, id: number): void {
    const schema = this.#schema;
    const modelSchema = schema === null ? undefined : lookupModel(schema, model);
    const keyed = this.#keyed.table(model);
    for (const [name, spec] of Object.entries(modelSchema?.fields ?? {})) {
      if (spec?.kind !== "x2many") continue;
      const before = untrack(() => keyed.get(id, name));
      if (!Array.isArray(before)) continue;
      const key = coordKey(model, id, name);
      for (const m of before as number[]) {
        const set = this.#referrers.get(refKey(spec.target ?? "", m));
        set?.delete(key);
        if (set?.size === 0) this.#referrers.delete(refKey(spec.target ?? "", m));
      }
    }
    keyed.deleteRecord(id);
  }

  #evict(model: string, id: number): void {
    this.#removeRow(model, id);
    this.#table?.evictRecord(model, id);
  }

  #dropWire(): void {
    const schema = this.#schema;
    if (schema !== null) {
      for (const model of Object.keys(schema.models)) {
        const keyed = this.#keyed.peekTable(model);
        if (keyed === undefined) continue;
        for (const id of untrack(() => [...keyed.recordIds()])) keyed.deleteRecord(id);
      }
    }
    this.windows.clear();
    this.#referrers.clear();
  }

  #wireModels(): string[] {
    return this.#schema === null ? [] : Object.keys(this.#schema.models);
  }

  #targets(kind: string, target: string | null, value: unknown): { model: string; id: number }[] {
    if (kind === "many2one") return typeof value === "number" && target !== null ? [{ model: target, id: value }] : [];
    if (kind === "x2many") return Array.isArray(value) && target !== null ? (value as number[]).map((id) => ({ model: target, id })) : [];
    if (typeof value === "object" && value !== null && "model" in value && "id" in value) {
      const ref = value as { model: string; id: number };
      return [{ model: ref.model, id: ref.id }];
    }
    return [];
  }

  // ---- reading

  #requireTable(method: string): StoreTable {
    if (this.#table === null) throw new MessagingStoreError("no_schema", `${method}: set a schema first`);
    return this.#table;
  }

  #checkRecord(method: string, model: unknown, id: unknown): void {
    if (typeof model !== "string" || model === "") {
      throw new MessagingStoreError("unknown_model", `${method}: model must be a non-empty string, got ${String(model)}`);
    }
    if (this.#locals.has(model)) {
      if (typeof id !== "string" || id === "") {
        throw new MessagingStoreError("bad_id", `${method}(${model}): a local record id must be a non-empty string, got ${String(id)}`);
      }
      return;
    }
    if (this.#schema === null || lookupModel(this.#schema, model) === undefined) {
      throw new MessagingStoreError("unknown_model", `${method}: ${model} is neither a model of schema ${this.generation ?? "(none)"} nor a local model`);
    }
    if (typeof id !== "number" || !Number.isSafeInteger(id)) {
      throw new MessagingStoreError("bad_id", `${method}(${model}): a server record id must be a safe integer, got ${String(id)}`);
    }
  }

  /** The declared kind of a field of a model or, through delegation, of a model it delegates to. */
  #specOf(model: string, field: string): { readonly kind: string; readonly target: string | null } {
    if (this.#locals.has(model)) {
      if (!this.#locals.get(model)?.fields.includes(field)) {
        throw new MessagingStoreError("unknown_field", `${model}.${field} is not a field of local model ${model}`);
      }
      return { kind: "local", target: null };
    }
    let at = model;
    for (;;) {
      const modelSchema = this.#schema === null ? undefined : lookupModel(this.#schema, at);
      if (modelSchema === undefined) throw new MessagingStoreError("unknown_model", `${at} is not a model of schema ${this.generation ?? "(none)"}`);
      const spec = lookupField(modelSchema, field);
      if (spec !== undefined) return spec;
      const rule = this.#delegations.get(at);
      if (rule === undefined) {
        throw new MessagingStoreError("unknown_field", `${model}.${field} is not a field of ${model}${at === model ? "" : ` or of ${at}`}`);
      }
      at = rule.target;
    }
  }

  /** The record whose table holds the field for (model, id): the record itself, or the parent along delegation. */
  #locate(model: string, id: RecordId, field: string): { model: string; id: RecordId } | undefined {
    this.#checkRecord("read", model, id);
    this.#specOf(model, field);
    let at = model;
    let rid: RecordId = id;
    while (!this.#declares(at, field)) {
      const rule = this.#delegations.get(at);
      if (rule === undefined) return undefined;
      const next = this.#keyed.table(at).get(rid, rule.via);
      at = rule.target;
      if (typeof next !== "number") return undefined;
      rid = next;
    }
    return { model: at, id: rid };
  }

  #declares(model: string, field: string): boolean {
    if (this.#locals.has(model)) return true;
    const modelSchema = this.#schema === null ? undefined : lookupModel(this.#schema, model);
    return modelSchema !== undefined && lookupField(modelSchema, field) !== undefined;
  }

  #checkRule(schema: StoreSchema, model: string, via: string): string {
    const modelSchema = lookupModel(schema, model);
    if (modelSchema === undefined) throw new MessagingStoreError("unknown_model", `delegate(${model}): ${model} is not a model of schema ${schema.generation}`);
    const spec = lookupField(modelSchema, via);
    if (spec === undefined || spec.kind !== "many2one" || spec.target === null) {
      throw new MessagingStoreError("delegation", `delegate(${model}): ${via} must be a many2one field of ${model} in schema ${schema.generation}`);
    }
    return spec.target;
  }
}
