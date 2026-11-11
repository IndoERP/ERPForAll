/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
// The store delta wire contract in TypeScript: a strict decoder
// that refuses exactly like the Rust decoder, and the reference coordinate table of spec §4.4. The wire types come
// from the Rust-generated declarations in ./generated; nothing here redefines them.
export { StoreTable, DEFAULT_COUNTER_LOG_BOUND } from "./apply";
export type { ApplyOutcome, CounterView, ResyncNeeded, ResyncReason } from "./apply";
export { Decoder, WIRE_MAJOR, acceptClientMajor, toPayloadDoc } from "./decode";
export type { DecodedPayload, Outcome } from "./decode";
export { ApplyError, WireError } from "./errors";
export { LIMITS, WireLimits, limitMaximum } from "./limits";
export { readRevision, replacesState, revisionKey, supersedesCounterBase, writeRevision } from "./revision";
export type { ReadRevision, Revision, WriteRevision } from "./revision";
export { FIELD_KINDS, lookupField, lookupModel } from "./schema";
export type { StoreSchema } from "./schema";
export { BusCursor, SnapshotDescriptor, compareCursors, readOrder } from "./snapshot";
export { formatXid, parseXid } from "./xid";
