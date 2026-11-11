/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
// The client messaging store: typed records on keyed tables, delegation,
// local records, bounded message windows, and application of wire answers and bus frames.
export { DEFAULT_CONFIG, resolveConfig } from "./config";
export type { MessagingConfig } from "./config";
export { MessagingStoreError } from "./errors";
export type { MessagingStoreErrorCode } from "./errors";
export { RecordHandle } from "./handle";
export { MessagingStore } from "./store";
export type {
  ApplyResult,
  CollectReport,
  DelegationRule,
  Lease,
  LocalModel,
  MessagingStoreOptions,
  SchemaResult,
} from "./store";
export type {
  Counter,
  CounterKeys,
  CounterView,
  FieldValue,
  FieldsOf,
  Many,
  ManyKeys,
  ModelName,
  One,
  OneKeys,
  RecordId,
  RecordRef,
  Reference,
  ResyncReason,
  ResyncRequest,
  StoreModels,
  TargetOf,
} from "./types";
export { ThreadWindows } from "./windows";
export type { LiveOutcome, ThreadRef, WindowView } from "./windows";
