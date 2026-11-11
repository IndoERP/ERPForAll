/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
// The in-test server model and its independent oracle.
//
// The model plays a small PostgreSQL-like server and the client's bus and fetch plumbing in one loop. A script of
// random actions starts transactions (each takes its snapshot when it starts, REPEATABLE READ), lets them write
// under the producer obligations A1-A5 of spec §4.3, commits or aborts them, turns every commit into bus frames
// (A4), turns every answering transaction into an answer, and delivers frames and answers to the reference
// `StoreTable` through the strict decoder, in random interleavings.
//
// The oracle never looks at the table or at `apply.ts`: it replays the committed transactions' operations in commit
// order over the seed data and yields the server state the client must converge to. Expected values in the
// property test come from here only.
//
// Producer obligations as modelled:
// - A1: an answer is the transaction's snapshot plus its own writes, with the descriptor of that snapshot.
// - A2: snapshots are taken from one timeline, so their completed sets are nested.
// - A3: a state or membership coordinate can only be written when no other writer holds it uncommitted and every
//   committed writer is in the writer's snapshot (otherwise the write is skipped, as PostgreSQL would refuse it).
// - A4: a commit emits events for exactly the coordinates it wrote (final value / net delta / add-remove / delete).
// - A5: record ids are never reused.
import { Harness, asof as asofDoc } from "../support/wire";

/** One scripted action; the fields are plain integers so that a failing script shrinks and replays. */
export interface Action {
  readonly kind: number;
  readonly a: number;
  readonly b: number;
  readonly c: number;
}

/**
 * How frames are delivered.
 *
 * - `ordered`: `(xid, id)` order behind the delivery horizon (a frame waits for lower in-progress transactions),
 *   with record deletes. This is the order the bus gives (5).
 * - `arbitrary`: any order of committed frames. Record deletes are left out: the tombstone rule of spec §4.4
 *   relies on bus order, and spec §4.4 claims order independence for state and counter rules.
 */
export type Mode = "ordered" | "arbitrary";

/** Model switches. */
export interface ModelOptions {
  readonly mode: Mode;
  /** Break obligation A4: a commit also emits an event for a title it did not write. */
  readonly breakA4?: boolean;
}

const THREAD = "a.thread";
const MESSAGE = "b.message";
const CAP = 5;

type Json = null | boolean | number | string | Json[] | { [key: string]: Json };

/** The seed data every run starts from, committed by the pseudo transaction 0. */
export const INITIAL = {
  threads: [
    { id: 1, title: "t1", stats: "s1", unread: 1, capped: 2, members: [1, 2] },
    { id: 2, title: "t2", stats: "s2", unread: 2, capped: 3, members: [2, 3] },
    { id: 3, title: "t3", stats: "s3", unread: 3, capped: 4, members: [] as number[] },
  ],
  messages: [
    { id: 1, text: "m1" },
    { id: 2, text: "m2" },
    { id: 3, text: "m3" },
    { id: 4, text: "m4" },
  ],
} as const;

/** A small deterministic generator for the incidental choices inside one action. */
export class Rng {
  #state: number;

  constructor(seed: number) {
    this.#state = seed >>> 0;
  }

  next(): number {
    this.#state = (this.#state + 0x6d2b79f5) >>> 0;
    let t = this.#state;
    t = Math.imul(t ^ (t >>> 15), t | 1);
    t ^= t + Math.imul(t ^ (t >>> 7), t | 61);
    return ((t ^ (t >>> 14)) >>> 0) / 4294967296;
  }

  int(n: number): number {
    return Math.floor(this.next() * n);
  }

  chance(p: number): boolean {
    return this.next() < p;
  }
}

function must<T>(value: T | undefined, what: string): T {
  if (value === undefined) throw new Error(`model bug: ${what}`);
  return value;
}

type StateField = "title" | "text" | "stats";
type CounterField = "unread" | "capped";

type Op =
  | { readonly t: "set"; readonly model: string; readonly id: number; readonly field: StateField; readonly value: string }
  | { readonly t: "member"; readonly thread: number; readonly elem: number; readonly on: boolean }
  | { readonly t: "inc"; readonly thread: number; readonly field: CounterField; readonly delta: number }
  | { readonly t: "create"; readonly model: string; readonly id: number }
  | { readonly t: "delete"; readonly model: string; readonly id: number };

interface Tx {
  readonly idx: number;
  readonly answer: boolean;
  readonly forced: boolean;
  readonly seed: number;
  readonly request: number | null;
  readonly visible: ReadonlySet<number>;
  readonly snap: { readonly xmin: number; readonly xmax: number; readonly xip: readonly number[] };
  xid: number | null;
  status: "active" | "committed" | "aborted";
  readonly ops: Op[];
}

interface Rec {
  readonly createdBy: number;
  deletedBy: number | null;
}

interface Version<V> {
  readonly tx: number;
  readonly seq: number;
  readonly value: V;
}

interface Delta {
  readonly tx: number;
  readonly delta: number;
}

interface FrameMsg {
  readonly xid: number;
  readonly id: number;
  readonly desc: { readonly xmin: number; readonly xmax: number; readonly xip: readonly number[] };
  readonly records: Record<string, readonly Json[]>;
}

interface AnswerMsg {
  readonly request: number;
  readonly asof: ReturnType<typeof asofDoc>;
  readonly records: Record<string, readonly Json[]>;
  readonly written: Record<string, Record<string, string[]>> | undefined;
}

interface Item {
  readonly model: string;
  readonly id: number;
  readonly frame: number;
  readonly field: string | null;
  readonly value: Json;
}

const sk = (model: string, id: number, field: string): string => `${model}#${id}.${field}`;
const rk = (model: string, id: number): string => `${model}#${id}`;
const mk = (thread: number, elem: number): string => `${THREAD}#${thread}.messages#${elem}`;
const sortedNumbers = (values: Iterable<number>): number[] => [...values].sort((x, y) => x - y);

/** What the run told the client, used to decide which coordinates the client must hold at the end. */
interface Mentions {
  readonly records: Set<string>;
  readonly state: Set<string>;
  readonly counterAnswered: Set<string>;
  readonly counterEvents: Set<string>;
  readonly memberAnswered: Set<number>;
  readonly memberEvents: Map<number, Set<number>>;
  readonly stats: { key: string; n: number; value: string }[];
}

/** The independent oracle result. */
interface Oracle {
  readonly alive: ReadonlySet<string>;
  readonly state: ReadonlyMap<string, string>;
  readonly members: ReadonlyMap<number, readonly number[]>;
  readonly counters: ReadonlyMap<string, number>;
}

/** The model of one run. */
export class Model {
  readonly h = new Harness();
  readonly #mode: Mode;
  readonly #breakA4: boolean;
  readonly #txs: Tx[] = [];
  readonly #recs = new Map<string, Rec>();
  readonly #ids = new Map<string, number[]>([[THREAD, []], [MESSAGE, []]]);
  readonly #versions = new Map<string, Version<string | boolean>[]>();
  readonly #deltas = new Map<string, Delta[]>();
  readonly #pendingWriters = new Map<string, number>();
  readonly #touchers = new Map<string, Set<number>>();
  readonly #commitOrder: number[] = [];
  readonly #frames: FrameMsg[] = [];
  readonly #answers: AnswerMsg[] = [];
  readonly #mentions: Mentions = {
    records: new Set(),
    state: new Set(),
    counterAnswered: new Set(),
    counterEvents: new Set(),
    memberAnswered: new Set(),
    memberEvents: new Map(),
    stats: [],
  };
  readonly #resync = new Set<string>();
  #seq = 0;
  #answersApplied = 0;
  #writtenMarkers = 0;
  #writingAnswers = 0;
  #nextXid = 0;
  #nextEvent = 0;
  #nextValue = 0;
  #nextId = new Map<string, number>([[THREAD, 4], [MESSAGE, 5]]);

  constructor(options: ModelOptions) {
    this.#mode = options.mode;
    this.#breakA4 = options.breakA4 === true;
    // Transaction 0 committed the seed data before everything else.
    this.#txs.push({
      idx: 0, answer: false, forced: false, seed: 0, request: null, visible: new Set(), snap: { xmin: 1, xmax: 1, xip: [] },
      xid: null, status: "committed", ops: [],
    });
    for (const t of INITIAL.threads) {
      this.#seedRecord(THREAD, t.id);
      this.#seed(sk(THREAD, t.id, "title"), t.title);
      this.#seed(sk(THREAD, t.id, "stats"), t.stats);
      this.#deltas.set(sk(THREAD, t.id, "unread"), [{ tx: 0, delta: t.unread }]);
      this.#deltas.set(sk(THREAD, t.id, "capped"), [{ tx: 0, delta: t.capped }]);
      for (const e of t.members) this.#seed(mk(t.id, e), true);
    }
    for (const m of INITIAL.messages) {
      this.#seedRecord(MESSAGE, m.id);
      this.#seed(sk(MESSAGE, m.id, "text"), m.text);
    }
  }

  #seedRecord(model: string, id: number): void {
    this.#recs.set(rk(model, id), { createdBy: 0, deletedBy: null });
    must(this.#ids.get(model), "model ids").push(id);
  }

  #seed(key: string, value: string | boolean): void {
    this.#versions.set(key, [{ tx: 0, seq: 0, value }]);
  }

  // ---- script ---------------------------------------------------------------------------------------------

  /**
   * Force the pattern: T1 writes a counter and stays in progress, T2 (a higher xid) writes it
   * and commits, then an answer starts that knows T2 but not T1 and carries that counter.
   */
  forcePattern(): void {
    const t1 = this.#start(false, 1, false);
    this.#tryInc(t1, 1, "unread", 1);
    const t2 = this.#start(false, 2, false);
    this.#tryInc(t2, 1, "unread", 1);
    this.#commit(t2);
    this.#start(true, 3, true);
  }

  /** Run one scripted action. */
  step(action: Action, index: number): void {
    const r = new Rng(Math.imul(action.a + 1, 7919) ^ Math.imul(action.b + 1, 104729) ^ Math.imul(action.c + 1, 1299709) ^ index);
    const active = this.#txs.filter((t) => t.idx > 0 && t.status === "active");
    const pick = active.length === 0 ? undefined : active[action.a % active.length];
    switch (action.kind) {
      case 0:
      case 1:
        if (active.length < 5) this.#start(r.chance(0.5), r.int(1 << 30), false);
        return;
      case 2: case 3: case 4: case 5: case 6: case 7:
        if (pick !== undefined) this.#writeOp(pick, action, r);
        return;
      case 8:
      case 9:
        if (pick !== undefined) this.#commit(pick);
        return;
      case 10:
        if (pick !== undefined) {
          if (r.chance(0.5)) this.#abort(pick);
          else this.#commit(pick);
        }
        return;
      case 11:
      case 12:
        this.#deliverFrame(action.b);
        return;
      default:
        this.#deliverAnswer(action.b);
    }
  }

  /** End the run: commit everything still open, then deliver every frame and every answer. */
  finish(): void {
    for (let guard = 0; ; guard += 1) {
      const active = this.#txs.filter((t) => t.idx > 0 && t.status === "active");
      const t = active[(guard * 7 + 1) % Math.max(active.length, 1)];
      if (t === undefined) break;
      this.#commit(t);
    }
    let i = 0;
    while (this.#frames.length > 0) this.#deliverFrame(i++ * 3);
    while (this.#answers.length > 0) this.#deliverAnswer(i++ * 5);
  }

  // ---- transactions ---------------------------------------------------------------------------------------

  #start(answer: boolean, seed: number, forced: boolean): Tx {
    const finished = this.#txs.filter((t) => t.idx > 0 && t.xid !== null && t.status !== "active");
    const xmax = finished.reduce((m, t) => Math.max(m, must(t.xid ?? undefined, "xid") + 1), 1);
    const xip = sortedNumbers(
      this.#txs.filter((t) => t.idx > 0 && t.status === "active" && t.xid !== null && t.xid < xmax).map((t) => must(t.xid ?? undefined, "xid")),
    );
    const visible = new Set(this.#txs.filter((t) => t.status === "committed").map((t) => t.idx));
    const tx: Tx = {
      idx: this.#txs.length, answer, forced, seed, request: answer ? this.h.table.beginRequest() : null, visible,
      snap: { xmin: xip[0] ?? xmax, xmax, xip }, xid: null, status: "active", ops: [],
    };
    this.#txs.push(tx);
    this.#checkSnapshot(tx);
    return tx;
  }

  /** The descriptor's known set must agree with the timeline: the model's own consistency check. */
  #checkSnapshot(t: Tx): void {
    for (const other of this.#txs) {
      if (other.idx === 0 || other.idx === t.idx || other.xid === null || other.status === "aborted") continue;
      const inK = other.xid < t.snap.xmin || (other.xid < t.snap.xmax && !t.snap.xip.includes(other.xid));
      if (inK !== t.visible.has(other.idx)) throw new Error(`model bug: descriptor of tx ${t.idx} disagrees on tx ${other.idx}`);
    }
  }

  #visibleTo(t: Tx, idx: number): boolean {
    return idx === 0 || idx === t.idx || t.visible.has(idx);
  }

  #alive(t: Tx, key: string): boolean {
    const r = this.#recs.get(key);
    return r !== undefined && this.#visibleTo(t, r.createdBy) && r.deletedBy === null;
  }

  #aliveAt(t: Tx, key: string): boolean {
    const r = this.#recs.get(key);
    return r !== undefined && this.#visibleTo(t, r.createdBy) && (r.deletedBy === null || !this.#visibleTo(t, r.deletedBy));
  }

  #canWriteState(t: Tx, key: string): boolean {
    const holder = this.#pendingWriters.get(key);
    if (holder !== undefined && holder !== t.idx) return false;
    return (this.#versions.get(key) ?? []).every((v) => this.#visibleTo(t, v.tx));
  }

  #touch(t: Tx, key: string): void {
    const set = this.#touchers.get(key) ?? new Set<number>();
    set.add(t.idx);
    this.#touchers.set(key, set);
  }

  #ensureXid(t: Tx): void {
    if (t.xid === null) {
      this.#nextXid += 1;
      t.xid = this.#nextXid;
    }
  }

  #writeOp(t: Tx, action: Action, r: Rng): void {
    const pickId = (model: string): number | undefined => {
      const ids = must(this.#ids.get(model), "ids");
      return ids[action.c % Math.max(ids.length, 1)];
    };
    const thread = pickId(THREAD);
    const message = pickId(MESSAGE);
    switch (action.b % 9) {
      case 0:
        if (thread !== undefined) this.#trySet(t, THREAD, thread, "title");
        return;
      case 1:
        if (message !== undefined) this.#trySet(t, MESSAGE, message, "text");
        return;
      case 2:
      case 3:
        if (thread !== undefined && message !== undefined) this.#tryMember(t, thread, message, action.b % 9 === 2);
        return;
      case 4:
      case 5: {
        if (thread === undefined) return;
        const delta = (r.chance(0.35) ? -1 : 1) * (1 + r.int(3));
        this.#tryInc(t, thread, action.b % 9 === 4 ? "unread" : "capped", delta);
        return;
      }
      case 6:
        this.#create(t, action.c % 2 === 0 ? THREAD : MESSAGE);
        return;
      case 7:
        if (this.#mode === "ordered") {
          const model = action.c % 2 === 0 ? THREAD : MESSAGE;
          const id = pickId(model);
          if (id !== undefined) this.#tryDelete(t, model, id);
        }
        return;
      default:
        if (thread !== undefined && !t.answer) this.#trySet(t, THREAD, thread, "stats");
    }
  }

  #trySet(t: Tx, model: string, id: number, field: StateField): void {
    const key = sk(model, id, field);
    if (!this.#alive(t, rk(model, id)) || !this.#canWriteState(t, key)) return;
    this.#ensureXid(t);
    this.#nextValue += 1;
    t.ops.push({ t: "set", model, id, field, value: `${field}${this.#nextValue}` });
    this.#pendingWriters.set(key, t.idx);
    this.#touch(t, rk(model, id));
  }

  #tryMember(t: Tx, thread: number, elem: number, on: boolean): void {
    const key = mk(thread, elem);
    if (!this.#alive(t, rk(THREAD, thread)) || !this.#alive(t, rk(MESSAGE, elem)) || !this.#canWriteState(t, key)) return;
    this.#ensureXid(t);
    t.ops.push({ t: "member", thread, elem, on });
    this.#pendingWriters.set(key, t.idx);
    this.#touch(t, rk(THREAD, thread));
    this.#touch(t, rk(MESSAGE, elem));
  }

  #tryInc(t: Tx, thread: number, field: CounterField, delta: number): void {
    if (!this.#alive(t, rk(THREAD, thread))) return;
    if (delta < 0) {
      // Counters count rows, so they never go below zero: a decrement needs room even if every other open
      // transaction decrements as much as it has so far.
      const key = sk(THREAD, thread, field);
      let floor = (this.#deltas.get(key) ?? []).reduce((s, d) => s + d.delta, 0);
      for (const other of this.#txs) {
        if (other.status !== "active") continue;
        floor += Math.min(0, other.ops.reduce((s, op) => (op.t === "inc" && op.thread === thread && op.field === field ? s + op.delta : s), 0));
      }
      if (floor + delta < 0) return;
    }
    this.#ensureXid(t);
    t.ops.push({ t: "inc", thread, field, delta });
    this.#touch(t, rk(THREAD, thread));
  }

  #create(t: Tx, model: string): void {
    const id = must(this.#nextId.get(model), "next id");
    this.#nextId.set(model, id + 1);
    this.#ensureXid(t);
    this.#recs.set(rk(model, id), { createdBy: t.idx, deletedBy: null });
    must(this.#ids.get(model), "ids").push(id);
    this.#nextValue += 1;
    const field: StateField = model === THREAD ? "title" : "text";
    t.ops.push({ t: "create", model, id });
    t.ops.push({ t: "set", model, id, field, value: `${field}${this.#nextValue}` });
    this.#pendingWriters.set(sk(model, id, field), t.idx);
    this.#touch(t, rk(model, id));
  }

  #tryDelete(t: Tx, model: string, id: number): void {
    const key = rk(model, id);
    const rec = this.#recs.get(key);
    if (rec === undefined || !this.#alive(t, key) || rec.createdBy === t.idx) return;
    for (const other of this.#touchers.get(key) ?? []) {
      const o = must(this.#txs[other], "tx");
      if (other === t.idx || o.status === "aborted") continue;
      if (o.status === "active" || !this.#visibleTo(t, other)) return;
    }
    this.#ensureXid(t);
    rec.deletedBy = t.idx;
    t.ops.push({ t: "delete", model, id });
    this.#touch(t, key);
  }

  #abort(t: Tx): void {
    t.status = "aborted";
    this.#release(t);
    for (const op of t.ops) {
      if (op.t === "delete") {
        const rec = this.#recs.get(rk(op.model, op.id));
        if (rec !== undefined && rec.deletedBy === t.idx) rec.deletedBy = null;
      }
    }
    // A request whose transaction never answers is given up.
    if (t.request !== null) this.h.table.settleRequest(t.request);
  }

  #release(t: Tx): void {
    for (const [key, holder] of [...this.#pendingWriters]) if (holder === t.idx) this.#pendingWriters.delete(key);
  }

  #commit(t: Tx): void {
    // The answer is read before the commit, from the snapshot plus the transaction's own writes (A1).
    const answer = t.answer ? this.#composeAnswer(t) : null;
    t.status = "committed";
    this.#seq += 1;
    const seq = this.#seq;
    this.#commitOrder.push(t.idx);
    this.#release(t);
    const finals = new Map<string, string | boolean>();
    for (const op of t.ops) {
      if (op.t === "set") finals.set(sk(op.model, op.id, op.field), op.value);
      else if (op.t === "member") finals.set(mk(op.thread, op.elem), op.on);
      else if (op.t === "inc") {
        const key = sk(THREAD, op.thread, op.field);
        this.#deltas.set(key, [...(this.#deltas.get(key) ?? []), { tx: t.idx, delta: op.delta }]);
      }
    }
    for (const [key, value] of finals) this.#versions.set(key, [...(this.#versions.get(key) ?? []), { tx: t.idx, seq, value }]);
    this.#emitFrames(t);
    if (t.request !== null) {
      if (answer === null) this.h.table.settleRequest(t.request);
      else this.#answers.push({ ...answer, request: t.request });
    }
  }

  // ---- reads at a snapshot --------------------------------------------------------------------------------

  #stateAt(t: Tx, key: string): string | undefined {
    for (let i = t.ops.length - 1; i >= 0; i -= 1) {
      const op = must(t.ops[i], "op");
      if (op.t === "set" && sk(op.model, op.id, op.field) === key) return op.value;
    }
    let best: Version<string | boolean> | undefined;
    for (const v of this.#versions.get(key) ?? []) {
      if (this.#visibleTo(t, v.tx) && (best === undefined || v.seq > best.seq)) best = v;
    }
    return typeof best?.value === "string" ? best.value : undefined;
  }

  #memberAt(t: Tx, thread: number, elem: number): boolean {
    if (!this.#aliveAt(t, rk(MESSAGE, elem))) return false;
    for (let i = t.ops.length - 1; i >= 0; i -= 1) {
      const op = must(t.ops[i], "op");
      if (op.t === "member" && op.thread === thread && op.elem === elem) return op.on;
    }
    let best: Version<string | boolean> | undefined;
    for (const v of this.#versions.get(mk(thread, elem)) ?? []) {
      if (this.#visibleTo(t, v.tx) && (best === undefined || v.seq > best.seq)) best = v;
    }
    return best?.value === true;
  }

  #counterAt(t: Tx, thread: number, field: CounterField): number {
    const key = sk(THREAD, thread, field);
    let sum = 0;
    for (const d of this.#deltas.get(key) ?? []) if (this.#visibleTo(t, d.tx)) sum += d.delta;
    for (const op of t.ops) if (op.t === "inc" && op.thread === thread && op.field === field) sum += op.delta;
    return sum;
  }

  #asofOf(t: Tx, own: number | null): ReturnType<typeof asofDoc> {
    return asofDoc(t.snap.xmin, t.snap.xmax, t.snap.xip, own);
  }

  // ---- answers --------------------------------------------------------------------------------------------

  #composeAnswer(t: Tx): Omit<AnswerMsg, "request"> | null {
    const r = new Rng(t.seed);
    const asof = this.#asofOf(t, t.xid);
    const n = t.snap.xmax - t.snap.xip.length;
    const records: Record<string, Json[]> = {};
    const written: Record<string, Record<string, string[]>> = {};
    const owned = (model: string, id: number, field: string): boolean =>
      t.ops.some((op) => op.t === "set" && op.model === model && op.id === id && op.field === field);
    for (const model of [THREAD, MESSAGE]) {
      for (const id of sortedNumbers(must(this.#ids.get(model), "ids"))) {
        if (!this.#aliveAt(t, rk(model, id))) continue;
        const force = t.forced && model === THREAD && id === 1;
        if (!force && !r.chance(0.6)) continue;
        const entry: { [key: string]: Json } = { id };
        const names: string[] = [];
        const mark = (field: string): void => {
          if (t.xid !== null && field !== "stats" && owned(model, id, field) && r.chance(0.5)) names.push(field);
        };
        if (model === THREAD) {
          if (r.chance(0.7)) {
            entry["title"] = must(this.#stateAt(t, sk(THREAD, id, "title")), "title");
            this.#mentions.state.add(sk(THREAD, id, "title"));
            mark("title");
          }
          if (r.chance(0.6)) {
            const members = sortedNumbers(must(this.#ids.get(MESSAGE), "ids")).filter((e) => this.#memberAt(t, id, e));
            entry["messages"] = members;
            this.#mentions.memberAnswered.add(id);
          }
          if (force || r.chance(0.6)) {
            entry["unread"] = this.#counterAt(t, id, "unread");
            this.#mentions.counterAnswered.add(sk(THREAD, id, "unread"));
          }
          if (r.chance(0.6)) {
            entry["capped"] = Math.min(this.#counterAt(t, id, "capped"), CAP);
            this.#mentions.counterAnswered.add(sk(THREAD, id, "capped"));
          }
          const stats = this.#stateAt(t, sk(THREAD, id, "stats"));
          if (stats !== undefined && r.chance(0.5)) {
            entry["stats"] = stats;
            this.#mentions.stats.push({ key: sk(THREAD, id, "stats"), n, value: stats });
          }
        } else if (r.chance(0.85)) {
          entry["text"] = must(this.#stateAt(t, sk(MESSAGE, id, "text")), "text");
          this.#mentions.state.add(sk(MESSAGE, id, "text"));
          mark("text");
        }
        if (Object.keys(entry).length === 1) continue;
        (records[model] ??= []).push(entry);
        this.#mentions.records.add(rk(model, id));
        if (names.length > 0) ((written[model] ??= {})[String(id)] = names);
      }
    }
    if (Object.keys(records).length === 0) return null;
    return { asof, records, written: Object.keys(written).length === 0 ? undefined : written };
  }

  // ---- frames ---------------------------------------------------------------------------------------------

  #emitFrames(t: Tx): void {
    if (t.xid === null) return;
    const deleted = new Set(t.ops.filter((op) => op.t === "delete").map((op) => (op.t === "delete" ? rk(op.model, op.id) : "")));
    const r = new Rng(t.seed ^ 0x5bd1e995);
    const nFrames = 1 + r.int(2);
    const items: Item[] = [];
    const states = new Map<string, Op & { t: "set" }>();
    const members = new Map<string, Op & { t: "member" }>();
    const incs = new Map<string, number>();
    for (const op of t.ops) {
      if (op.t === "set") states.set(sk(op.model, op.id, op.field), op);
      else if (op.t === "member") members.set(mk(op.thread, op.elem), op);
      else if (op.t === "inc") incs.set(sk(THREAD, op.thread, op.field), (incs.get(sk(THREAD, op.thread, op.field)) ?? 0) + op.delta);
    }
    let n = 0;
    for (const op of states.values()) {
      // `stats` is answer_only: no event carries it. Records this transaction deleted carry the marker alone.
      if (op.field === "stats" || deleted.has(rk(op.model, op.id))) continue;
      items.push({ model: op.model, id: op.id, frame: n++ % nFrames, field: op.field, value: op.value });
    }
    for (const op of members.values()) {
      if (deleted.has(rk(THREAD, op.thread)) || deleted.has(rk(MESSAGE, op.elem))) continue;
      items.push({ model: THREAD, id: op.thread, frame: n++ % nFrames, field: "messages", value: { elem: op.elem, on: op.on } });
    }
    for (const [key, net] of incs) {
      const m = /^a\.thread#(\d+)\.(\w+)$/.exec(key);
      const thread = Number(must(m?.[1], "key"));
      if (net === 0 || deleted.has(rk(THREAD, thread))) continue;
      const field = must(m?.[2], "field");
      if (nFrames === 2 && Math.abs(net) >= 2) {
        const first = Math.trunc(net / 2);
        items.push({ model: THREAD, id: thread, frame: 0, field, value: { inc: first } });
        items.push({ model: THREAD, id: thread, frame: 1, field, value: { inc: net - first } });
      } else {
        items.push({ model: THREAD, id: thread, frame: n++ % nFrames, field, value: { inc: net } });
      }
    }
    for (const key of deleted) {
      const m = /^(.+)#(\d+)$/.exec(key);
      items.push({ model: must(m?.[1], "model"), id: Number(must(m?.[2], "id")), frame: n++ % nFrames, field: null, value: null });
    }
    if (this.#breakA4) {
      // Negative control: an event for a coordinate this transaction never wrote (breaks A4).
      const target = sortedNumbers(must(this.#ids.get(THREAD), "ids")).find(
        (id) => this.#alive(t, rk(THREAD, id)) && !states.has(sk(THREAD, id, "title")),
      );
      if (target !== undefined && items.length > 0) items.push({ model: THREAD, id: target, frame: 0, field: "title", value: "bogus" });
    }
    const desc = t.snap;
    for (let f = 0; f < nFrames; f += 1) {
      const mine = items.filter((item) => item.frame === f);
      if (mine.length === 0) continue;
      const byRecord = new Map<string, { model: string; id: number; fields: { [key: string]: Json }; del: boolean }>();
      for (const item of mine) {
        const key = rk(item.model, item.id);
        const entry = byRecord.get(key) ?? { model: item.model, id: item.id, fields: {}, del: false };
        byRecord.set(key, entry);
        this.#mentions.records.add(key);
        if (item.field === null) {
          entry.del = true;
        } else if (item.field === "messages") {
          const v = item.value as { elem: number; on: boolean };
          const delta = (entry.fields["messages"] ?? {}) as { add?: number[]; remove?: number[] };
          const list = (v.on ? (delta.add ??= []) : (delta.remove ??= []));
          list.push(v.elem);
          list.sort((x, y) => x - y);
          entry.fields["messages"] = delta;
          const seen = this.#mentions.memberEvents.get(item.id) ?? new Set<number>();
          seen.add(v.elem);
          this.#mentions.memberEvents.set(item.id, seen);
        } else {
          entry.fields[item.field] = item.value;
          if (item.field === "unread" || item.field === "capped") this.#mentions.counterEvents.add(sk(item.model, item.id, item.field));
          else this.#mentions.state.add(sk(item.model, item.id, item.field));
        }
      }
      const records: Record<string, Json[]> = {};
      for (const entry of [...byRecord.values()].sort((x, y) => (x.model < y.model ? -1 : x.model > y.model ? 1 : x.id - y.id))) {
        (records[entry.model] ??= []).push(entry.del ? { id: entry.id, delete: true } : { id: entry.id, ...entry.fields });
      }
      this.#nextEvent += 1;
      this.#frames.push({ xid: t.xid, id: this.#nextEvent, desc, records });
    }
  }

  // ---- delivery -------------------------------------------------------------------------------------------

  #deliverFrame(pick: number): void {
    if (this.#frames.length === 0) return;
    let index: number;
    if (this.#mode === "arbitrary") {
      index = pick % this.#frames.length;
    } else {
      index = 0;
      this.#frames.forEach((f, i) => {
        const best = must(this.#frames[index], "frame");
        if (f.xid < best.xid || (f.xid === best.xid && f.id < best.id)) index = i;
      });
      const next = must(this.#frames[index], "frame");
      // The delivery horizon: nothing is delivered past a lower transaction that is still in progress.
      if (this.#txs.some((t) => t.status === "active" && t.xid !== null && t.xid < next.xid)) return;
    }
    const [frame] = this.#frames.splice(index, 1);
    const f = must(frame, "frame");
    const out = this.h.frame(f.xid, f.id, asofDoc(f.desc.xmin, f.desc.xmax, f.desc.xip), f.records);
    for (const r of out.resync) this.#resync.add(sk(r.model, r.id, r.field));
  }

  #deliverAnswer(pick: number): void {
    if (this.#answers.length === 0) return;
    const [answer] = this.#answers.splice(pick % this.#answers.length, 1);
    const a = must(answer, "answer");
    const out = this.h.answer(a.asof, a.records, a.request, a.written);
    this.#answersApplied += 1;
    if (a.asof.own !== null) this.#writingAnswers += 1;
    if (a.written !== undefined) this.#writtenMarkers += 1;
    for (const r of out.resync) this.#resync.add(sk(r.model, r.id, r.field));
    this.h.table.settleRequest(a.request);
  }

  // ---- oracle and comparison ------------------------------------------------------------------------------

  /** The server state after the committed transactions, replayed in commit order over the seed data. */
  oracle(): Oracle {
    const alive = new Set<string>();
    const state = new Map<string, string>();
    const member = new Map<number, Map<number, boolean>>();
    const counters = new Map<string, number>();
    for (const t of INITIAL.threads) {
      alive.add(rk(THREAD, t.id));
      state.set(sk(THREAD, t.id, "title"), t.title);
      state.set(sk(THREAD, t.id, "stats"), t.stats);
      counters.set(sk(THREAD, t.id, "unread"), t.unread);
      counters.set(sk(THREAD, t.id, "capped"), t.capped);
      member.set(t.id, new Map(t.members.map((e) => [e, true] as const)));
    }
    for (const m of INITIAL.messages) {
      alive.add(rk(MESSAGE, m.id));
      state.set(sk(MESSAGE, m.id, "text"), m.text);
    }
    for (const idx of this.#commitOrder) {
      for (const op of must(this.#txs[idx], "tx").ops) {
        if (op.t === "create") {
          alive.add(rk(op.model, op.id));
          if (op.model === THREAD) {
            member.set(op.id, new Map());
            counters.set(sk(THREAD, op.id, "unread"), 0);
            counters.set(sk(THREAD, op.id, "capped"), 0);
          }
        } else if (op.t === "set") state.set(sk(op.model, op.id, op.field), op.value);
        else if (op.t === "member") must(member.get(op.thread), "thread members").set(op.elem, op.on);
        else if (op.t === "inc") {
          const key = sk(THREAD, op.thread, op.field);
          counters.set(key, (counters.get(key) ?? 0) + op.delta);
        } else alive.delete(rk(op.model, op.id));
      }
    }
    const members = new Map<number, readonly number[]>();
    for (const [thread, map] of member) {
      members.set(thread, sortedNumbers([...map].filter(([e, on]) => on && alive.has(rk(MESSAGE, e))).map(([e]) => e)));
    }
    return { alive, state, members, counters };
  }

  /** Compare every coordinate the client holds with the oracle. An empty list means the run converged. */
  mismatches(): string[] {
    const bad: string[] = [];
    const o = this.oracle();
    const table = this.h.table;
    const parse = (key: string): { model: string; id: number; field: string } => {
      const m = /^(.+)#(\d+)\.(.+)$/.exec(key);
      return { model: must(m?.[1], "model"), id: Number(must(m?.[2], "id")), field: must(m?.[3], "field") };
    };
    for (const key of this.#mentions.records) {
      const { model, id } = parse(`${key}.x`);
      if (table.hasRecord(model, id) !== o.alive.has(key)) bad.push(`record ${key}: held=${table.hasRecord(model, id)} server=${o.alive.has(key)}`);
    }
    for (const key of this.#mentions.state) {
      const { model, id, field } = parse(key);
      const held = table.value(model, id, field);
      if (!o.alive.has(rk(model, id))) {
        if (held !== undefined) bad.push(`state ${key}: held after delete`);
        continue;
      }
      const want = { kind: "string", value: o.state.get(key) };
      if (JSON.stringify(held) !== JSON.stringify(want)) bad.push(`state ${key}: held=${JSON.stringify(held)} server=${JSON.stringify(want)}`);
    }
    const bestStats = new Map<string, { n: number; values: Set<string> }>();
    for (const s of this.#mentions.stats) {
      const best = bestStats.get(s.key);
      if (best === undefined || s.n > best.n) bestStats.set(s.key, { n: s.n, values: new Set([s.value]) });
      else if (s.n === best.n) best.values.add(s.value);
    }
    for (const [key, best] of bestStats) {
      const { model, id, field } = parse(key);
      if (!o.alive.has(rk(model, id))) continue;
      const held = table.value(model, id, field);
      const heldValue = held?.kind === "string" ? held.value : undefined;
      if (heldValue === undefined || !best.values.has(heldValue)) {
        bad.push(`answer-only ${key}: held=${JSON.stringify(held)} newest answer values=${[...best.values].join("|")}`);
      }
    }
    const threads = new Set<number>([...this.#mentions.memberAnswered, ...this.#mentions.memberEvents.keys()]);
    for (const id of threads) {
      const held = table.members(THREAD, id, "messages");
      if (!o.alive.has(rk(THREAD, id))) {
        if (held !== undefined) bad.push(`members ${THREAD}#${id}: held after delete`);
        continue;
      }
      const server = must(o.members.get(id), "oracle members");
      const want = this.#mentions.memberAnswered.has(id)
        ? server
        : server.filter((e) => this.#mentions.memberEvents.get(id)?.has(e) === true);
      if (JSON.stringify(held) !== JSON.stringify(want)) bad.push(`members ${THREAD}#${id}: held=${JSON.stringify(held)} server=${JSON.stringify(want)}`);
    }
    const counterKeys = new Set([...this.#mentions.counterAnswered, ...this.#mentions.counterEvents]);
    for (const key of counterKeys) {
      const { id, field } = parse(key);
      const view = table.counter(THREAD, id, field);
      if (!o.alive.has(rk(THREAD, id))) {
        if (view !== undefined) bad.push(`counter ${key}: held after delete`);
        continue;
      }
      const total = must(o.counters.get(key), "oracle counter");
      const capped = field === "capped";
      const got = JSON.stringify(view);
      if (view === undefined) {
        bad.push(`counter ${key}: not held`);
      } else if (view.stale) {
        if (!capped) bad.push(`counter ${key}: uncapped counter is stale`);
        else if (!this.#resync.has(key)) bad.push(`counter ${key}: stale without a ResyncNeeded`);
      } else if (!this.#mentions.counterAnswered.has(key)) {
        if (view.value !== null) bad.push(`counter ${key}: value without any answer ${got}`);
      } else {
        const want = JSON.stringify({ value: capped ? Math.min(total, CAP) : total, saturated: capped && total >= CAP, stale: false });
        if (got !== want) bad.push(`counter ${key}: held=${got} server=${want}`);
      }
    }
    return bad;
  }

  /** How much of each kind of thing the run exercised; the coverage check of the property test sums these. */
  coverage(): Record<string, number> {
    const committed = this.#txs.filter((t) => t.status === "committed" && t.idx > 0);
    const count = (kind: Op["t"]): number => committed.reduce((s, t) => s + t.ops.filter((op) => op.t === kind).length, 0);
    return {
      deletes: count("delete"),
      creates: count("create"),
      sets: count("set"),
      memberOps: count("member"),
      incs: count("inc"),
      aborted: this.#txs.filter((t) => t.status === "aborted").length,
      frames: this.#nextEvent,
      answers: this.#answersApplied,
      writtenMarkers: this.#writtenMarkers,
      writingAnswers: this.#writingAnswers,
      resyncs: this.#resync.size,
    };
  }

  /** What the run did, for the failure artifact. */
  summary(): { transactions: number; committed: number; aborted: number; frames: number } {
    const done = this.#txs.slice(1);
    return {
      transactions: done.length,
      committed: done.filter((t) => t.status === "committed").length,
      aborted: done.filter((t) => t.status === "aborted").length,
      frames: this.#nextEvent,
    };
  }
}

/**
 * Run a script against a fresh model and return the model after everything was delivered.
 *
 * @param script - the scripted actions
 * @param options - the mode and switches
 */
export function runScript(script: readonly Action[], options: ModelOptions): Model {
  const model = new Model(options);
  model.forcePattern();
  script.forEach((action, i) => model.step(action, i));
  model.finish();
  return model;
}
