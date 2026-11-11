/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
import type { CounterView } from "@erpforall/store-wire";

/** A polymorphic (model, id) reference value (the `reference` field kind). */
export interface RecordRef {
  readonly model: string;
  readonly id: number;
}

/** The shown state of a counter field: value, saturation and staleness (from the wire applier). */
export type { CounterView };

/** Declares a many2one field in {@link StoreModels}: the projected value is the target id or `null`. */
export interface One<M extends string> {
  readonly __messagingStoreOne: M;
}

/** Declares an x2many field in {@link StoreModels}: the projected value is the ascending list of target ids. */
export interface Many<M extends string> {
  readonly __messagingStoreMany: M;
}

/** Declares a counter field in {@link StoreModels}. */
export interface Counter {
  readonly __messagingStoreCounter: true;
}

/** Declares a polymorphic reference field in {@link StoreModels}. */
export interface Reference {
  readonly __messagingStoreReference: true;
}

/**
 * The declared record shapes, by model name. The Rust generator emits one entry per schema model; addons add the
 * fields (and models) they bring by augmenting it:
 *
 * ```ts
 * declare module "@erpforall/mail-store" {
 *   interface StoreModels {
 *     "mail.message": { starred: boolean };
 *   }
 * }
 * ```
 *
 * A field is a plain TS type for a scalar, or one of {@link One}, {@link Many}, {@link Counter}, {@link Reference}.
 * Declarations are compile-time only: the runtime checks every name against the schema.
 */
export interface StoreModels {}

/** The model names a handle may be typed by: the declared ones, or any string when nothing is declared. */
export type ModelName = keyof StoreModels extends never ? string : (keyof StoreModels & string) | (string & {});

/** The declared fields of a model; an undeclared model is dynamically typed. */
export type FieldsOf<M extends string> = M extends keyof StoreModels ? StoreModels[M] : Record<string, unknown>;

/** The value a handle reads for a declared field. */
export type FieldValue<F> = F extends One<string>
  ? number | null
  : F extends Many<string>
    ? readonly number[]
    : F extends Counter
      ? CounterView
      : F extends Reference
        ? RecordRef | null
        : F;

/** The names of a model's many2one fields. */
export type OneKeys<M extends string> = M extends keyof StoreModels
  ? { [K in keyof StoreModels[M] & string]: StoreModels[M][K] extends One<string> ? K : never }[keyof StoreModels[M] & string]
  : string;

/** The names of a model's x2many fields. */
export type ManyKeys<M extends string> = M extends keyof StoreModels
  ? { [K in keyof StoreModels[M] & string]: StoreModels[M][K] extends Many<string> ? K : never }[keyof StoreModels[M] & string]
  : string;

/** The names of a model's counter fields. */
export type CounterKeys<M extends string> = M extends keyof StoreModels
  ? { [K in keyof StoreModels[M] & string]: StoreModels[M][K] extends Counter ? K : never }[keyof StoreModels[M] & string]
  : string;

/** The target model a declared relation field points at. */
export type TargetOf<F> = F extends One<infer M> ? M : F extends Many<infer M> ? M : string;

/** A record id: a safe integer from the server, or a non-empty string for a local record. */
export type RecordId = number | string;

/** Why the store asks the host for a fresh answer. */
export type ResyncReason =
  | "unknown_model"
  | "unknown_field"
  | "payload_refused"
  | "counter_stale"
  | "schema_changed";

/**
 * A request for the host to fetch fresh data. It names only server records and server fields: a local record never
 * appears in one.
 */
export interface ResyncRequest {
  readonly reason: ResyncReason;
  /** The model concerned, when the reason names one. */
  readonly model?: string;
  /** The record concerned (counters). */
  readonly id?: number;
  /** The field concerned. */
  readonly field?: string;
  /** For a refusal, the decoder's path into the payload (`WireError.path`). */
  readonly path?: string | null;
  /** For a refusal, the decoder's message. */
  readonly detail?: string;
  /** For `counter_stale`, why the counter went stale. */
  readonly counterReason?: "log_overflow" | "saturated_decrement";
}
