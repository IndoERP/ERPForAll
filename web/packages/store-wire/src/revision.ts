/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
import type { SnapshotDescriptor } from "./snapshot";

/** `Rd(A)`: a value taken from answer `A` that `A` did not mark as written. */
export interface ReadRevision {
  readonly kind: "read";
  /** The answer's descriptor. */
  readonly asof: SnapshotDescriptor;
}

/**
 * `Wr(x, D)`: a value written by transaction `x`, carried by `x`'s bus event or by `x`'s own answer through its
 * `written` marker. `D` is `x`'s snapshot with `own = x`.
 */
export interface WriteRevision {
  readonly kind: "write";
  /** The writing transaction. */
  readonly xid: bigint;
  /** The writer's descriptor. */
  readonly asof: SnapshotDescriptor;
  /** The bus event id; `null` when the revision comes from an answer's `written` marker. */
  readonly eventId: bigint | null;
}

/** The revision of one coordinate (spec §3). */
export type Revision = ReadRevision | WriteRevision;

/**
 * `Rd(A)` for an answer descriptor.
 *
 * @param asof - the answer's descriptor
 */
export function readRevision(asof: SnapshotDescriptor): ReadRevision {
  return Object.freeze({ kind: "read", asof });
}

/**
 * `Wr(x, D)`.
 *
 * @param xid - the writing transaction
 * @param asof - the writer's descriptor
 * @param eventId - the bus event id, or null for a `written` marker of an answer
 */
export function writeRevision(xid: bigint, asof: SnapshotDescriptor, eventId: bigint | null): WriteRevision {
  return Object.freeze({ kind: "write", xid, asof, eventId });
}

/**
 * The state table of spec §4.4: does an incoming revision replace the held one? No held revision is handled by the
 * caller (the incoming value is applied). Ties keep the held value: equal completed counts between reads, the same
 * xid without a higher event id between writes.
 *
 * | held \ incoming | `Rd(A')`               | `Wr(y, D')`                                  |
 * |-----------------|------------------------|----------------------------------------------|
 * | `Rd(A)`         | `n(A') > n(A)`         | `y` not in `C(A)`                            |
 * | `Wr(x, D)`      | `x` in `C(A')`         | `x` in `K(D')`; same xid: higher event id    |
 *
 * `C` is the completed set, `K` without the snapshot's own transaction: an answer's own unmarked write and the same
 * transaction's event are one writer, so neither replaces the other. For the same xid a revision without an event id (an answer's `written` marker) is lower than any event id.
 *
 * @param held - the held revision
 * @param incoming - the incoming revision
 */
export function replacesState(held: Revision, incoming: Revision): boolean {
  if (held.kind === "read") {
    return incoming.kind === "read"
      ? incoming.asof.completedCount() > held.asof.completedCount()
      : !held.asof.completes(incoming.xid);
  }
  if (incoming.kind === "read") return incoming.asof.completes(held.xid);
  if (incoming.xid === held.xid) {
    return incoming.eventId !== null && (held.eventId === null || incoming.eventId > held.eventId);
  }
  return incoming.asof.reflects(held.xid);
}

/**
 * Whether an incoming answer may replace a counter's base answer (spec §4.4, decidable form): the incoming known
 * set contains the held one and the two are known to differ.
 *
 * `n(A) <= n(A')`, and (`own(A)` is null or in `K(A')`), and (`n(A') > n(A)` or (`own(A')` is not null and not
 * in `K(A)`)).
 *
 * @param held - the base answer's descriptor
 * @param incoming - the incoming answer's descriptor
 */
export function supersedesCounterBase(held: SnapshotDescriptor, incoming: SnapshotDescriptor): boolean {
  const nHeld = held.completedCount();
  const nIncoming = incoming.completedCount();
  if (nHeld > nIncoming) return false;
  if (held.own !== null && !incoming.reflects(held.own)) return false;
  return nIncoming > nHeld || (incoming.own !== null && !held.reflects(incoming.own));
}

/**
 * A canonical text of a revision, equal exactly when two revisions are the same value.
 *
 * @param revision - the revision
 */
export function revisionKey(revision: Revision): string {
  const d = revision.asof;
  const asof = `${d.xmin}:${d.xmax}:${d.xip.join(",")}:${d.own ?? ""}`;
  return revision.kind === "read" ? `R[${asof}]` : `W[${revision.xid}|${revision.eventId ?? ""}|${asof}]`;
}
