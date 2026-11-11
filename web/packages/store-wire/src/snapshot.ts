/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
import type { AsofDoc } from "./generated/AsofDoc.d";
import type { CursorDoc } from "./generated/CursorDoc.d";
import { WireError } from "./errors";
import { WireLimits } from "./limits";
import { formatXid, parseDecimal, parseXid } from "./xid";

function bad(path: string, reason: string): WireError {
  return new WireError("bad_descriptor", path, reason);
}

/**
 * The snapshot descriptor of one transaction, `{xmin, xmax, xip, own}` (4): what that transaction's
 * snapshot knew when it answered or emitted. Immutable; every constructor enforces the invariants
 * `xmin <= xmax`, `xip` strictly ascending inside `[xmin, xmax)`, `own` null or at least `xmax` (A1) and not in `xip`, and never truncates the list.
 */
export class SnapshotDescriptor {
  /** The lowest transaction id still active when the snapshot was taken. */
  readonly xmin: bigint;
  /** One past the highest completed transaction id. */
  readonly xmax: bigint;
  /** The in-progress transactions in `[xmin, xmax)`, strictly ascending. */
  readonly xip: readonly bigint[];
  /** The answering transaction's own xid, when it had one. */
  readonly own: bigint | null;

  private constructor(xmin: bigint, xmax: bigint, xip: readonly bigint[], own: bigint | null) {
    this.xmin = xmin;
    this.xmax = xmax;
    this.xip = Object.freeze([...xip]);
    this.own = own;
    Object.freeze(this);
  }

  /**
   * Build a descriptor from numbers.
   *
   * @param xmin - lowest active transaction id
   * @param xmax - one past the highest completed id
   * @param xip - in-progress ids, strictly ascending inside `[xmin, xmax)`
   * @param own - the transaction's own xid, or null
   * @param limits - the host's limits
   * @throws {@link WireError} `bad_descriptor` for a broken invariant, `limit_exceeded` for a long `xip`
   */
  static create(
    xmin: bigint,
    xmax: bigint,
    xip: readonly bigint[],
    own: bigint | null,
    limits: WireLimits = WireLimits.defaults(),
  ): SnapshotDescriptor {
    limits.check("xip", xip.length, "asof.xip");
    if (xmin > xmax) throw bad("asof", "xmin is above xmax");
    for (const [i, entry] of xip.entries()) {
      if (entry < xmin || entry >= xmax) throw bad(`asof.xip[${i}]`, "xip entry is outside [xmin, xmax)");
      const previous = xip[i - 1];
      if (previous !== undefined && previous >= entry) throw bad(`asof.xip[${i}]`, "xip is not strictly ascending");
    }
    if (own !== null && xip.includes(own)) throw bad("asof.own", "own is in xip");
    // A1: the snapshot is fixed before the transaction's xid is assigned, so own is never below xmax. C(D) and n(D)
    // agree with each other only under this invariant.
    if (own !== null && own < xmax) throw bad("asof.own", "own is below xmax");
    return new SnapshotDescriptor(xmin, xmax, xip, own);
  }

  /**
   * Build from the wire strings, each a canonical xid8.
   *
   * @param doc - the `asof` object as written on the wire
   * @param limits - the host's limits
   * @throws {@link WireError} `bad_descriptor` with the path of the offending member (`asof.xmin`, `asof.xip[2]`,
   * ...) or `limit_exceeded`
   */
  static fromWire(doc: AsofDoc, limits: WireLimits = WireLimits.defaults()): SnapshotDescriptor {
    // The length is checked before any entry is parsed, so an oversize list costs nothing.
    limits.check("xip", doc.xip.length, "asof.xip");
    const xmin = parseXid(doc.xmin, "asof.xmin");
    const xmax = parseXid(doc.xmax, "asof.xmax");
    const xip = doc.xip.map((text, i) => parseXid(text, `asof.xip[${i}]`));
    const own = doc.own === null ? null : parseXid(doc.own, "asof.own");
    return SnapshotDescriptor.create(xmin, xmax, xip, own, limits);
  }

  /**
   * The reflection predicate.5: is a transaction known to this snapshot? True below `xmin`, inside
   * `[xmin, xmax)` and not in progress, and for the snapshot's own transaction.
   *
   * @param xid - the transaction to test
   */
  reflects(xid: bigint): boolean {
    if (xid < this.xmin) return true;
    if (xid < this.xmax && !this.xip.includes(xid)) return true;
    return this.own === xid;
  }

  /**
   * Whether a transaction is in the completed set `C(D)`: the reflection predicate without the snapshot's own
   * transaction. Revision comparisons between answers and events use this, so an answer's own unmarked writes and
   * the same transaction's event compare as the same writer.
   *
   * @param xid - the transaction to test
   */
  completes(xid: bigint): boolean {
    return xid < this.xmin || (xid < this.xmax && !this.xip.includes(xid));
  }

  /** The completed count `xmax - |xip|`, the order key between two answers. */
  completedCount(): bigint {
    return this.xmax - BigInt(this.xip.length);
  }

  /** The descriptor as it travels in `asof`. */
  toDoc(): AsofDoc {
    return {
      own: this.own === null ? null : formatXid(this.own),
      xip: this.xip.map(formatXid),
      xmax: formatXid(this.xmax),
      xmin: formatXid(this.xmin),
    };
  }
}

/**
 * Descriptors in ascending order of their completed count, stable on ties: the order that decides which of two
 * reads is newer.
 *
 * @param descriptors - the descriptors
 * @returns indices into `descriptors`
 */
export function readOrder(descriptors: readonly SnapshotDescriptor[]): number[] {
  return descriptors
    .map((d, i) => ({ i, n: d.completedCount() }))
    .sort((a, b) => (a.n < b.n ? -1 : a.n > b.n ? 1 : a.i - b.i))
    .map((entry) => entry.i);
}

/**
 * The position of a bus frame: the emitting transaction's xid and the event id. Ordered numerically by `xid`,
 * then `id`, never as strings (`"9" < "10"`).
 */
export class BusCursor {
  /** The emitting transaction's xid. */
  readonly xid: bigint;
  /** The event id, 1..2^63-1. */
  readonly id: bigint;

  private constructor(xid: bigint, id: bigint) {
    this.xid = xid;
    this.id = id;
    Object.freeze(this);
  }

  /**
   * Build from the wire strings.
   *
   * @param doc - the `cursor` object as written on the wire
   * @throws {@link WireError} `bad_descriptor` with path `cursor.xid` or `cursor.id`
   */
  static fromWire(doc: CursorDoc): BusCursor {
    const xid = parseXid(doc.xid, "cursor.xid");
    const id = parseDecimal(doc.id);
    if (typeof id === "string") throw bad("cursor.id", id);
    if (id === 0n || id > (1n << 63n) - 1n) throw bad("cursor.id", "event id is outside 1..=2^63-1");
    return new BusCursor(xid, id);
  }

  /** The cursor as it travels in a frame. */
  toDoc(): CursorDoc {
    return { id: formatXid(this.id), xid: formatXid(this.xid) };
  }
}

/**
 * Numeric order of two cursors: negative, zero or positive.
 *
 * @param a - first cursor
 * @param b - second cursor
 */
export function compareCursors(a: BusCursor, b: BusCursor): number {
  if (a.xid !== b.xid) return a.xid < b.xid ? -1 : 1;
  if (a.id !== b.id) return a.id < b.id ? -1 : 1;
  return 0;
}
