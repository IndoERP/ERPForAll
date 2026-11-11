/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
import type { RecordDoc } from "./generated/RecordDoc.d";
import type { ValueDoc } from "./generated/ValueDoc.d";
import { ApplyError } from "./errors";
import type { DecodedPayload } from "./decode";
import {
  type Revision,
  readRevision,
  replacesState,
  revisionKey,
  supersedesCounterBase,
  writeRevision,
} from "./revision";
import { type StoreSchema, lookupField, lookupModel } from "./schema";
import type { SnapshotDescriptor } from "./snapshot";

/** The default bound of a counter's delta log. */
export const DEFAULT_COUNTER_LOG_BOUND = 1024;

/** Why a counter coordinate can no longer be shown. */
export type ResyncReason = "log_overflow" | "saturated_decrement";

/** The client must fetch a fresh answer for this counter: it holds no value it can vouch for. */
export interface ResyncNeeded {
  readonly model: string;
  readonly id: number;
  readonly field: string;
  readonly reason: ResyncReason;
}

/** What applying one payload asks of the host. */
export interface ApplyOutcome {
  /** Counter coordinates that went stale, or were still stale when another delta arrived. */
  readonly resync: readonly ResyncNeeded[];
}

/** The shown state of a counter coordinate. */
export interface CounterView {
  /** The value, or `null` before any base answer and while the coordinate is stale. */
  readonly value: number | null;
  /** True when the value is "the cap or more" (a capped counter at its cap). */
  readonly saturated: boolean;
  /** True after a `ResyncNeeded` that no later answer has cleared. */
  readonly stale: boolean;
}

interface StateCoord {
  readonly kind: "state";
  value: ValueDoc;
  rev: Revision;
}

interface MemberState {
  readonly member: boolean;
  readonly rev: Revision;
}

interface MembershipCoord {
  readonly kind: "x2many";
  readonly target: string;
  /** A replace: a member set with one shared revision. */
  base: { members: Set<number>; rev: Revision } | null;
  /** Per-element revisions that differ from what the base implies. */
  overrides: Map<number, MemberState>;
}

interface LogEntry {
  readonly xid: bigint;
  readonly id: bigint;
  readonly delta: number;
}

interface CounterCoord {
  readonly kind: "counter";
  readonly cap: number | null;
  base: { value: number; asof: SnapshotDescriptor } | null;
  log: LogEntry[];
  /** The requests outstanding when the coordinate last went stale; null while it is not stale. */
  stale: Set<number> | null;
  staleReason: ResyncReason | null;
}

type Coord = StateCoord | MembershipCoord | CounterCoord;

interface RecordState {
  exists: Revision;
  readonly fields: Map<string, Coord>;
  /** The requests outstanding when the record was evicted; `null` while the record is held. */
  retired: Set<number> | null;
}

interface Tombstone {
  rev: Revision;
  readonly outstanding: Set<number>;
  readonly touched: { coord: MembershipCoord; id: number; rev: Revision }[];
}

interface Context {
  readonly resync: ResyncNeeded[];
  readonly request: number | undefined;
  /** Whether a field of the record being applied changed or confirmed a coordinate (revives a retired record). */
  revived: boolean;
}

function effective(coord: MembershipCoord, id: number): MemberState | null {
  const override = coord.overrides.get(id);
  if (override !== undefined) return override;
  return coord.base === null ? null : { member: coord.base.members.has(id), rev: coord.base.rev };
}

function byNumber(a: number, b: number): number {
  return a - b;
}

/**
 * The reference coordinate table of spec §4.4, which wraps. It holds, per (model, id, field):
 *
 * - state coordinates: a value and the revision it came with, replaced by the state table;
 * - counters: a base answer plus a log of deltas the base does not reflect;
 * - x2many fields: membership per element (a replaced base set plus per-element overrides);
 * - record existence, with tombstones that outlive a delete until every request outstanding at that time settles.
 *
 * Within one payload every upsert applies before every delete. Re-applying a payload changes nothing. Applying
 * is in-memory and synchronous, and decoded payloads are already validated, so a payload either applies whole or
 * the host called the table against its contract ({@link ApplyError}).
 */
export class StoreTable {
  readonly #schema: StoreSchema;
  readonly #logBound: number;
  readonly #records = new Map<string, Map<number, RecordState>>();
  readonly #tombstones = new Map<string, Map<number, Tombstone>>();
  readonly #retired = new Map<string, Map<number, RecordState>>();
  readonly #requests = new Set<number>();
  #nextRequest = 1;

  /**
   * @param schema - the schema the payloads are decoded against
   * @param logBound - the most deltas one counter logs before it goes stale (default {@link DEFAULT_COUNTER_LOG_BOUND})
   * @throws {@link ApplyError} for a bound that is not a positive safe integer
   */
  constructor(schema: StoreSchema, logBound: number = DEFAULT_COUNTER_LOG_BOUND) {
    if (!Number.isSafeInteger(logBound) || logBound < 1) throw new ApplyError(`counter log bound ${logBound} is not a positive integer`);
    this.#schema = schema;
    this.#logBound = logBound;
  }

  /**
   * Note that a request for an answer was sent. A tombstone made while it is outstanding is kept until it settles.
   *
   * @returns the request id to pass to {@link StoreTable.applyAnswer} and {@link StoreTable.settleRequest}
   */
  beginRequest(): number {
    const id = this.#nextRequest;
    this.#nextRequest += 1;
    this.#requests.add(id);
    return id;
  }

  /**
   * Note that a request is finished (its answer applied or given up), then release the tombstones that no longer
   * wait for any request.
   *
   * @param request - an id from {@link StoreTable.beginRequest}
   * @throws {@link ApplyError} for an id that is not outstanding
   */
  settleRequest(request: number): void {
    if (!this.#requests.delete(request)) throw new ApplyError(`request ${request} is not outstanding`);
    for (const [model, byId] of this.#tombstones) {
      for (const [id, tombstone] of byId) {
        tombstone.outstanding.delete(request);
        if (tombstone.outstanding.size === 0) this.#release(model, id, tombstone);
      }
    }
    for (const [model, byId] of this.#retired) {
      for (const [id, state] of byId) {
        state.retired?.delete(request);
        if (state.retired?.size === 0) this.#drop(model, id);
      }
    }
  }

  /**
   * Evict a record from the table (the client store's cache policy,.4). With no request outstanding the
   * record is dropped at once. Otherwise it is retired: it reads as absent, but its coordinates and revisions are
   * kept and keep following frames, so an answer that was already on its way cannot overwrite a newer event with older
   * data (D-6); it is released when every request outstanding now has settled. A payload revives it when
   * any of its fields changes a coordinate (a state value that replaces the held one, or an equal revision that
   * confirms it, or any counter or x2many field); a payload whose state fields are all older leaves it retired.
   * Evicting an unknown record does nothing.
   *
   * @param model - the model
   * @param id - the record id
   */
  evictRecord(model: string, id: number): void {
    const state = this.#records.get(model)?.get(id);
    if (state === undefined || state.retired !== null) return;
    if (this.#requests.size === 0) {
      this.#records.get(model)?.delete(id);
      return;
    }
    state.retired = new Set(this.#requests);
    const bucket = this.#retired.get(model) ?? new Map<number, RecordState>();
    this.#retired.set(model, bucket);
    bucket.set(id, state);
  }

  /**
   * Whether an evicted record's revisions are still held for outstanding requests.
   *
   * @param model - the model
   * @param id - the record id
   */
  isRetired(model: string, id: number): boolean {
    return this.#retired.get(model)?.has(id) ?? false;
  }

  #unretire(model: string, id: number): void {
    const bucket = this.#retired.get(model);
    if (bucket?.delete(id) === true && bucket.size === 0) this.#retired.delete(model);
  }

  #drop(model: string, id: number): void {
    this.#unretire(model, id);
    this.#records.get(model)?.delete(id);
  }

  #held(model: string, id: number): RecordState | undefined {
    const state = this.#records.get(model)?.get(id);
    return state === undefined || state.retired !== null ? undefined : state;
  }

  /**
   * Apply a decoded answer.
   *
   * @param payload - an answer (no cursor) decoded against this table's schema generation
   * @param request - the request that fetched it, when the host tracks requests
   * @throws {@link ApplyError} for a frame, another schema generation, or a request that is not outstanding
   */
  applyAnswer(payload: DecodedPayload, request?: number): ApplyOutcome {
    if (payload.cursor !== null) throw new ApplyError("applyAnswer was given a frame");
    if (request !== undefined && !this.#requests.has(request)) throw new ApplyError(`request ${request} is not outstanding`);
    return this.#apply(payload, request);
  }

  /**
   * Apply a decoded bus frame.
   *
   * @param payload - a frame (with cursor) decoded against this table's schema generation
   * @throws {@link ApplyError} for an answer or another schema generation
   */
  applyFrame(payload: DecodedPayload): ApplyOutcome {
    if (payload.cursor === null) throw new ApplyError("applyFrame was given an answer");
    return this.#apply(payload, undefined);
  }

  /**
   * Whether a record is held.
   *
   * @param model - the model
   * @param id - the record id
   */
  hasRecord(model: string, id: number): boolean {
    return this.#held(model, id) !== undefined;
  }

  /**
   * Whether a deleted record's tombstone is still held.
   *
   * @param model - the model
   * @param id - the record id
   */
  hasTombstone(model: string, id: number): boolean {
    return this.#tombstones.get(model)?.has(id) ?? false;
  }

  /**
   * The value of a state coordinate (any kind except x2many and counter).
   *
   * @param model - the model
   * @param id - the record id
   * @param field - the field name
   * @returns the decoded value, or `undefined` when nothing is held
   */
  value(model: string, id: number, field: string): ValueDoc | undefined {
    const coord = this.#held(model, id)?.fields.get(field);
    return coord?.kind === "state" ? coord.value : undefined;
  }

  /**
   * The members of an x2many field, ascending.
   *
   * @param model - the model
   * @param id - the record id
   * @param field - the field name
   * @returns the ids, or `undefined` when the field is not held
   */
  members(model: string, id: number, field: string): number[] | undefined {
    const coord = this.#held(model, id)?.fields.get(field);
    if (coord?.kind !== "x2many") return undefined;
    const out = new Set(coord.base?.members);
    for (const [element, state] of coord.overrides) {
      if (state.member) out.add(element);
      else out.delete(element);
    }
    return [...out].sort(byNumber);
  }

  /**
   * The shown state of a counter: base plus the deltas the base does not reflect.
   *
   * @param model - the model
   * @param id - the record id
   * @param field - the field name
   * @returns the view, or `undefined` when the coordinate is not held
   */
  counter(model: string, id: number, field: string): CounterView | undefined {
    const coord = this.#held(model, id)?.fields.get(field);
    if (coord?.kind !== "counter") return undefined;
    if (coord.stale !== null) return { value: null, saturated: false, stale: true };
    if (coord.base === null) return { value: null, saturated: false, stale: false };
    if (coord.cap !== null && coord.base.value === coord.cap) return { value: coord.cap, saturated: true, stale: false };
    let sum = coord.base.value;
    for (const entry of coord.log) sum += entry.delta;
    if (!Number.isSafeInteger(sum)) throw new ApplyError(`counter ${model}#${id}.${field} left the safe integer range`);
    if (coord.cap !== null && sum >= coord.cap) return { value: coord.cap, saturated: true, stale: false };
    return { value: sum, saturated: false, stale: false };
  }

  /**
   * Everything held, as plain data with canonical revision texts, for comparing two tables or one table before and
   * after a payload is applied again. Keys and lists are sorted.
   */
  snapshot(): unknown {
    const records: Record<string, Record<string, unknown>> = {};
    for (const [model, byId] of [...this.#records].sort(([a], [b]) => (a < b ? -1 : 1))) {
      const out: Record<string, unknown> = {};
      for (const [id, record] of [...byId].sort(([a], [b]) => a - b)) {
        const fields: Record<string, unknown> = {};
        for (const [name, coord] of [...record.fields].sort(([a], [b]) => (a < b ? -1 : 1))) {
          fields[name] = this.#describe(coord);
        }
        out[String(id)] = {
          exists: revisionKey(record.exists),
          fields,
          ...(record.retired === null ? {} : { retired: [...record.retired].sort(byNumber) }),
        };
      }
      records[model] = out;
    }
    const tombstones: Record<string, Record<string, unknown>> = {};
    for (const [model, byId] of [...this.#tombstones].sort(([a], [b]) => (a < b ? -1 : 1))) {
      const out: Record<string, unknown> = {};
      for (const [id, t] of [...byId].sort(([a], [b]) => a - b)) {
        out[String(id)] = { rev: revisionKey(t.rev), outstanding: [...t.outstanding].sort(byNumber) };
      }
      tombstones[model] = out;
    }
    return { records, tombstones };
  }

  #describe(coord: Coord): unknown {
    switch (coord.kind) {
      case "state":
        return { value: coord.value, rev: revisionKey(coord.rev) };
      case "x2many":
        return {
          base: coord.base === null ? null : { members: [...coord.base.members].sort(byNumber), rev: revisionKey(coord.base.rev) },
          overrides: [...coord.overrides].sort(([a], [b]) => a - b).map(([id, s]) => [id, s.member, revisionKey(s.rev)]),
        };
      case "counter":
        return {
          base: coord.base === null ? null : { value: coord.base.value, asof: revisionKey(readRevision(coord.base.asof)) },
          log: coord.log.map((e) => `${e.xid}/${e.id}/${e.delta}`),
          stale: coord.stale === null ? null : [...coord.stale].sort(byNumber),
        };
    }
  }

  #apply(payload: DecodedPayload, request: number | undefined): ApplyOutcome {
    if (payload.generation !== this.#schema.generation) {
      throw new ApplyError(`payload generation ${payload.generation} is not the table's ${this.#schema.generation}`);
    }
    const frame = payload.cursor !== null;
    const read = frame ? null : readRevision(payload.asof);
    const write =
      payload.cursor !== null
        ? writeRevision(payload.cursor.xid, payload.asof, payload.cursor.id)
        : payload.asof.own === null
          ? null
          : writeRevision(payload.asof.own, payload.asof, null);
    const plain = frame ? write : read;
    if (plain === null) throw new ApplyError("a frame needs its write revision");
    const ctx: Context = { resync: [], request, revived: false };
    for (const record of payload.upserts) this.#upsert(ctx, record, plain, write);
    for (const ref of payload.deletes) this.#delete(ref.model, ref.id, plain);
    return { resync: ctx.resync };
  }

  #upsert(ctx: Context, record: RecordDoc, plain: Revision, write: Revision | null): void {
    const modelSchema = lookupModel(this.#schema, record.model);
    if (modelSchema === undefined) throw new ApplyError(`model ${record.model} is not in the table's schema`);
    const byId = this.#records.get(record.model) ?? new Map<number, RecordState>();
    this.#records.set(record.model, byId);
    const tombstone = this.#tombstones.get(record.model)?.get(record.id);
    let state = byId.get(record.id);
    if (tombstone !== undefined) {
      // The existence coordinate holds `exists = false`. A stale answer does not bring the record back.
      if (!replacesState(tombstone.rev, plain)) return;
      this.#release(record.model, record.id, tombstone);
    }
    if (state === undefined) {
      state = { exists: plain, fields: new Map(), retired: null };
      byId.set(record.id, state);
    } else if (replacesState(state.exists, plain)) {
      state.exists = plain;
    }
    ctx.revived = false;
    for (const field of record.fields) {
      const spec = lookupField(modelSchema, field.name);
      if (spec === undefined) throw new ApplyError(`field ${record.model}.${field.name} is not in the table's schema`);
      let rev = plain;
      if (field.written) {
        if (write === null) throw new ApplyError("a written marker needs an own xid");
        rev = write;
      }
      this.#field(ctx, state, record, field.name, spec.cap, spec.target, field.value, rev);
    }
    // A retired record comes back when a field changed or confirmed a coordinate; a payload whose state fields are all older leaves it retired.
    if (state.retired !== null && ctx.revived) {
      state.retired = null;
      this.#unretire(record.model, record.id);
    }
  }

  #field(
    ctx: Context,
    state: RecordState,
    record: RecordDoc,
    name: string,
    cap: number | null,
    target: string | null,
    value: ValueDoc,
    rev: Revision,
  ): void {
    const held = state.fields.get(name);
    switch (value.kind) {
      case "counter": {
        const coord = this.#counterCoord(state, name, held, cap);
        ctx.revived = true;
        this.#counterAnswer(ctx, coord, record, name, value.value, rev);
        return;
      }
      case "inc": {
        const coord = this.#counterCoord(state, name, held, cap);
        ctx.revived = true;
        if (rev.kind !== "write") throw new ApplyError("a counter delta needs a write revision");
        this.#counterInc(ctx, coord, record, name, rev.xid, rev.eventId, value.value);
        return;
      }
      case "x2many_replace":
      case "x2many_delta": {
        if (target === null) throw new ApplyError(`x2many ${record.model}.${name} has no target`);
        let coord: MembershipCoord;
        if (held?.kind === "x2many") coord = held;
        else {
          coord = { kind: "x2many", target, base: null, overrides: new Map() };
          state.fields.set(name, coord);
        }
        ctx.revived = true;
        if (value.kind === "x2many_replace") this.#replaceMembers(coord, value.value, rev);
        else this.#deltaMembers(coord, value.value.add, value.value.remove, rev);
        return;
      }
      default: {
        if (held?.kind !== "state" || replacesState(held.rev, rev)) {
          state.fields.set(name, { kind: "state", value, rev });
          ctx.revived = true;
        } else if (revisionKey(held.rev) === revisionKey(rev)) {
          // The same revision is the same value: the payload confirms what is held.
          ctx.revived = true;
        }
      }
    }
  }

  #counterCoord(state: RecordState, name: string, held: Coord | undefined, cap: number | null): CounterCoord {
    if (held?.kind === "counter") return held;
    const coord: CounterCoord = { kind: "counter", cap, base: null, log: [], stale: null, staleReason: null };
    state.fields.set(name, coord);
    return coord;
  }

  #markStale(ctx: Context, coord: CounterCoord, record: RecordDoc, field: string, reason: ResyncReason): void {
    coord.stale = new Set(this.#requests);
    coord.staleReason = reason;
    ctx.resync.push({ model: record.model, id: record.id, field, reason });
  }

  #counterAnswer(ctx: Context, coord: CounterCoord, record: RecordDoc, field: string, value: number, rev: Revision): void {
    const asof = rev.asof;
    if (coord.stale !== null) {
      // Only an answer from a request sent after the last dropped delta reflects every delta that was dropped.
      if (ctx.request !== undefined && coord.stale.has(ctx.request)) return;
      coord.stale = null;
      coord.staleReason = null;
    } else if (coord.base !== null && !supersedesCounterBase(coord.base.asof, asof)) {
      return;
    }
    coord.base = { value, asof };
    coord.log = coord.log.filter((entry) => !asof.reflects(entry.xid));
    if (coord.cap !== null && value === coord.cap && coord.log.some((entry) => entry.delta < 0)) {
      this.#markStale(ctx, coord, record, field, "saturated_decrement");
    }
  }

  #counterInc(
    ctx: Context,
    coord: CounterCoord,
    record: RecordDoc,
    field: string,
    xid: bigint,
    eventId: bigint | null,
    delta: number,
  ): void {
    if (eventId === null) throw new ApplyError("a counter delta needs an event id");
    if (coord.stale !== null) {
      // The delta cannot be logged: a later answer must come from a request sent after this one.
      coord.stale = new Set(this.#requests);
      ctx.resync.push({ model: record.model, id: record.id, field, reason: coord.staleReason ?? "log_overflow" });
      return;
    }
    if (coord.base?.asof.reflects(xid) === true) return;
    if (coord.log.some((entry) => entry.xid === xid && entry.id === eventId)) return;
    if (coord.cap !== null && coord.base?.value === coord.cap && delta < 0) {
      this.#markStale(ctx, coord, record, field, "saturated_decrement");
      return;
    }
    if (coord.log.length >= this.#logBound) {
      this.#markStale(ctx, coord, record, field, "log_overflow");
      return;
    }
    coord.log.push({ xid, id: eventId, delta });
  }

  #tombstoned(model: string, id: number): boolean {
    return this.#tombstones.get(model)?.has(id) ?? false;
  }

  #replaceMembers(coord: MembershipCoord, ids: readonly number[], rev: Revision): void {
    const set = new Set(ids.filter((id) => !this.#tombstoned(coord.target, id)));
    const universe = new Set<number>([...(coord.base?.members ?? []), ...coord.overrides.keys(), ...set]);
    const othersReplace = coord.base === null || replacesState(coord.base.rev, rev);
    const base = othersReplace ? { members: set, rev } : coord.base;
    const overrides = new Map<number, MemberState>();
    for (const id of universe) {
      const held = effective(coord, id);
      const state: MemberState = held === null || replacesState(held.rev, rev) ? { member: set.has(id), rev } : held;
      const implied = base === null ? null : { member: base.members.has(id), rev: base.rev };
      if (implied === null || implied.member !== state.member || revisionKey(implied.rev) !== revisionKey(state.rev)) {
        overrides.set(id, state);
      }
    }
    coord.base = base;
    coord.overrides = overrides;
  }

  #deltaMembers(coord: MembershipCoord, add: readonly number[], remove: readonly number[], rev: Revision): void {
    for (const [ids, member] of [[add, true], [remove, false]] as const) {
      for (const id of ids) {
        if (this.#tombstoned(coord.target, id)) continue;
        const held = effective(coord, id);
        if (held === null || replacesState(held.rev, rev)) coord.overrides.set(id, { member, rev });
      }
    }
  }

  #delete(model: string, id: number, rev: Revision): void {
    const byId = this.#records.get(model);
    const live = byId?.get(id);
    const held = this.#tombstones.get(model)?.get(id);
    if (live !== undefined) {
      if (!replacesState(live.exists, rev)) return;
      this.#unretire(model, id);
      byId?.delete(id);
    } else if (held !== undefined) {
      if (replacesState(held.rev, rev)) held.rev = rev;
      return;
    }
    const tombstone: Tombstone = { rev, outstanding: new Set(this.#requests), touched: [] };
    const bucket = this.#tombstones.get(model) ?? new Map<number, Tombstone>();
    this.#tombstones.set(model, bucket);
    bucket.set(id, tombstone);
    // The id leaves every held x2many whose target is this model.
    for (const records of this.#records.values()) {
      for (const record of records.values()) {
        for (const coord of record.fields.values()) {
          if (coord.kind !== "x2many" || coord.target !== model) continue;
          if (effective(coord, id)?.member !== true) continue;
          coord.base?.members.delete(id);
          coord.overrides.set(id, { member: false, rev });
          tombstone.touched.push({ coord, id, rev });
        }
      }
    }
    if (tombstone.outstanding.size === 0) this.#release(model, id, tombstone);
  }

  #release(model: string, id: number, tombstone: Tombstone): void {
    this.#tombstones.get(model)?.delete(id);
    for (const { coord, id: element, rev } of tombstone.touched) {
      const override = coord.overrides.get(element);
      if (override !== undefined && !override.member && revisionKey(override.rev) === revisionKey(rev)) {
        coord.overrides.delete(element);
      }
    }
  }
}
