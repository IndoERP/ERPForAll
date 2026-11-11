/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
import type { CounterKeys, CounterView, FieldValue, FieldsOf, ManyKeys, OneKeys, RecordId, TargetOf } from "./types";

/** @internal What a {@link RecordHandle} asks of its store. */
export interface HandleHost {
  /** Reads a coordinate (tracked inside a reactive scope), following delegation; validates model and field. */
  read(model: string, id: RecordId, field: string): unknown;
  /** The relation value of a many2one or x2many field and its target model, following delegation. */
  relation(model: string, id: RecordId, field: string, kind: "many2one" | "x2many"): { readonly target: string; readonly ids: readonly number[] };
  /** Writes a field of a local record; refuses server records. */
  write(model: string, id: RecordId, field: string, value: unknown): void;
  /** Whether the record is held (an untracked read). */
  exists(model: string, id: RecordId): boolean;
  /** Reads a counter field (tracked), following delegation; refuses other kinds. */
  counter(model: string, id: RecordId, field: string): CounterView | undefined;
}

/**
 * A typed, read-only view of one record of the messaging store, identified by (model, id). Handles are cheap values:
 * `store.record(model, id)` makes a new one per call and two handles of one record read the same coordinates.
 * Reads inside a reactive scope are tracked per (record, field).
 *
 * A field the model does not have falls through to the record named by the model's delegation rule (a channel reads
 * its thread's fields). A name that no model of the chain declares throws a `MessagingStoreError` (`unknown_field`).
 *
 * The class is the extension seam for read helpers: an addon adds a typed getter with `patch(RecordHandle, ...)` and
 * declares it with `declare module "@erpforall/mail-store" { interface RecordHandle<M extends string> { ... } }`.
 */
export class RecordHandle<M extends string = string> {
  readonly model: M;
  readonly id: RecordId;
  readonly #host: HandleHost;

  /** @internal Created by {@link MessagingStore.record}. */
  constructor(host: HandleHost, model: M, id: RecordId) {
    this.#host = host;
    this.model = model;
    this.id = id;
  }

  /** Whether the record is held. Not tracked: read a field to react to changes. */
  get exists(): boolean {
    return this.#host.exists(this.model, this.id);
  }

  /**
   * The value of a field, or `undefined` while the record holds none (not loaded, evicted, deleted).
   *
   * @param field - a field of the model or of a record it delegates to
   * @throws {@link MessagingStoreError} `unknown_model` or `unknown_field`
   */
  get<K extends keyof FieldsOf<M> & string>(field: K): FieldValue<FieldsOf<M>[K]> | undefined {
    return this.#host.read(this.model, this.id, field) as FieldValue<FieldsOf<M>[K]> | undefined;
  }

  /**
   * Writes a field of a local record. Server records, including fields they delegate to another record, are
   * written by payloads only: this refuses them.
   *
   * @param field - a field of the local model
   * @param value - the new value
   * @throws {@link MessagingStoreError} `read_only` for a server record, `unknown_field`, `bad_local`
   */
  set<K extends keyof FieldsOf<M> & string>(field: K, value: FieldValue<FieldsOf<M>[K]>): void {
    this.#host.write(this.model, this.id, field, value);
  }

  /**
   * The record a many2one field points at, or `undefined` when the field is empty or not loaded.
   *
   * @param field - a many2one field
   * @throws {@link MessagingStoreError} `field_kind` for a field of another kind
   */
  one<K extends OneKeys<M>>(field: K): RecordHandle<TargetOf<FieldsOf<M>[K & keyof FieldsOf<M>]>> | undefined {
    const { target, ids } = this.#host.relation(this.model, this.id, field, "many2one");
    const id = ids[0];
    return id === undefined ? undefined : new RecordHandle(this.#host, target as TargetOf<FieldsOf<M>[K & keyof FieldsOf<M>]>, id);
  }

  /**
   * The records an x2many field lists, in ascending id order (empty while the field is not loaded).
   *
   * @param field - an x2many field
   * @throws {@link MessagingStoreError} `field_kind` for a field of another kind
   */
  many<K extends ManyKeys<M>>(field: K): RecordHandle<TargetOf<FieldsOf<M>[K & keyof FieldsOf<M>]>>[] {
    const { target, ids } = this.#host.relation(this.model, this.id, field, "x2many");
    return ids.map((id) => new RecordHandle(this.#host, target as TargetOf<FieldsOf<M>[K & keyof FieldsOf<M>]>, id));
  }

  /**
   * A counter field's shown state, or `undefined` before the record holds it.
   *
   * @param field - a counter field
   * @throws {@link MessagingStoreError} `field_kind` for a field of another kind
   */
  counter<K extends CounterKeys<M>>(field: K): CounterView | undefined {
    return this.#host.counter(this.model, this.id, field);
  }

  /** Whether another handle names the same record. */
  is(other: RecordHandle): boolean {
    return other.model === this.model && other.id === this.id;
  }
}
