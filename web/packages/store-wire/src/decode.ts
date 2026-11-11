/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
import type { PayloadDoc } from "./generated/PayloadDoc.d";
import type { RecordDoc } from "./generated/RecordDoc.d";
import type { RecordRefDoc } from "./generated/RecordRefDoc.d";
import type { SchemaFieldDoc } from "./generated/SchemaFieldDoc.d";
import type { SchemaModelDoc } from "./generated/SchemaModelDoc.d";
import type { ValueDoc } from "./generated/ValueDoc.d";
import { WireError } from "./errors";
import {
  type Json,
  type JsonFloat,
  type JsonInt,
  type JsonObject,
  getMember,
  isArray,
  isNumber,
  isObject,
  parseJson,
} from "./json";
import { WireLimits } from "./limits";
import {
  MAX_SAFE,
  type StoreSchema,
  type SyncDecl,
  dict,
  finishSchema,
  isFieldKind,
  lookupField,
  lookupModel,
  makeField,
  utf8Length,
  validateFieldName,
  validateGeneration,
  validateModelName,
} from "./schema";
import { BusCursor, SnapshotDescriptor } from "./snapshot";
import { parseDecimal } from "./xid";

/** The wire major this build speaks. Field kinds and operations are part of it. */
export const WIRE_MAJOR = 1;

/**
 * Decide whether a client's announced wire major is served.
 *
 * @param client - the major the client announced
 * @returns the accepted major
 * @throws {@link WireError} `client_outdated` for any major other than the current one, older or newer
 */
export function acceptClientMajor(client: number): number {
  if (client !== WIRE_MAJOR) {
    throw new WireError("client_outdated", null, `client wire major ${client} is not supported (server speaks ${WIRE_MAJOR})`);
  }
  return WIRE_MAJOR;
}

/**
 * A decoded answer or frame in canonical order: upserts by (model, id), then deletes by (model, id). A consumer
 * applies every upsert before every delete.
 */
export interface DecodedPayload {
  /** The schema generation of the payload. */
  readonly generation: string;
  /** The snapshot descriptor. */
  readonly asof: SnapshotDescriptor;
  /** The bus cursor of a frame; `null` for an answer. */
  readonly cursor: BusCursor | null;
  /** The record entries that set fields, with fields ascending by name. */
  readonly upserts: readonly RecordDoc[];
  /** The records deleted by the payload. */
  readonly deletes: readonly RecordRefDoc[];
}

/** The result of a decode that did not fail. */
export type Outcome<T> =
  | { readonly outcome: "decoded"; readonly payload: T }
  | { readonly outcome: "needs_schema"; readonly generation: string };

/**
 * A decoded payload in the generated golden form (decimal-string xids), the shape the Rust fixtures record.
 *
 * @param payload - the decoded payload
 */
export function toPayloadDoc(payload: DecodedPayload): PayloadDoc {
  return {
    asof: payload.asof.toDoc(),
    cursor: payload.cursor === null ? null : payload.cursor.toDoc(),
    deletes: payload.deletes.map((d) => ({ id: d.id, model: d.model })),
    generation: payload.generation,
    upserts: payload.upserts.map((r) => ({ id: r.id, model: r.model, fields: r.fields.map((f) => ({ ...f })) })),
  };
}

function malformed(path: string, reason: string): WireError {
  return new WireError("malformed", path, reason);
}

function mismatch(path: string, expected: string): WireError {
  return new WireError("type_mismatch", path, `expected ${expected}`);
}

function notAllowed(path: string, reason: string): WireError {
  return new WireError("operation_not_allowed", path, reason);
}

function badSchemaAt(path: string, reason: string): WireError {
  return new WireError("bad_schema", path, reason);
}

function join(base: string, key: string): string {
  return base === "" ? key : `${base}.${key}`;
}

function member(doc: Json, base: string, key: string): Json {
  const found = getMember(doc, key);
  if (found === undefined) throw malformed(join(base, key), "missing member");
  return found;
}

function knownMembers(doc: Json, base: string, allowed: readonly string[]): void {
  if (!isObject(doc)) throw malformed(base, "expected an object");
  for (const [key] of doc.m) if (!allowed.includes(key)) throw malformed(join(base, key), "unknown member");
}

/** Like {@link knownMembers} for the schema, whose unknown members are schema errors. */
function knownSchemaMembers(doc: Json, base: string, allowed: readonly string[]): void {
  if (!isObject(doc)) throw badSchemaAt(base, "expected an object");
  for (const [key] of doc.m) if (!allowed.includes(key)) throw badSchemaAt(`${base}.${key}`, "unknown member");
}

function stringOf(value: Json, path: string): string {
  if (typeof value !== "string") throw malformed(path, "expected a string");
  return value;
}

/** An integer literal as `u64`, or null for a float, a negative or a literal above `u64`. */
function asU64(n: JsonInt | JsonFloat): bigint | null {
  return n.t === "int" && n.v >= 0n ? n.v : null;
}

const I64_MAX = (1n << 63n) - 1n;
const I64_MIN = -(1n << 63n);

/** An integer literal as `i64`, or null for a float or a literal outside `i64`. */
function asI64(n: JsonInt | JsonFloat): bigint | null {
  return n.t === "int" && n.v >= I64_MIN && n.v <= I64_MAX ? n.v : null;
}

/** Check `wire` first (a foreign major is refused before any content error), then `schema`. */
function header(doc: Json, limits: WireLimits): string {
  if (!isObject(doc)) throw malformed("", "expected an object");
  const wire = member(doc, "", "wire");
  if (!isNumber(wire)) throw malformed("wire", "expected an integer");
  const found = asU64(wire);
  if (found === null) throw malformed("wire", "expected an integer");
  if (found !== BigInt(WIRE_MAJOR)) throw new WireError("wire_major_unsupported", null, `unsupported wire major ${found}`);
  const token = stringOf(member(doc, "", "schema"), "schema");
  validateGeneration(token, limits);
  return token;
}

function decodeAsof(value: Json, limits: WireLimits): SnapshotDescriptor {
  knownMembers(value, "asof", ["own", "xip", "xmax", "xmin"]);
  const xmin = stringOf(member(value, "asof", "xmin"), "asof.xmin");
  const xmax = stringOf(member(value, "asof", "xmax"), "asof.xmax");
  const list = member(value, "asof", "xip");
  if (!isArray(list)) throw malformed("asof.xip", "expected an array");
  limits.check("xip", list.length, "asof.xip");
  const xip = list.map((item, i) => stringOf(item, `asof.xip[${i}]`));
  const ownValue = member(value, "asof", "own");
  const own = ownValue === null ? null : stringOf(ownValue, "asof.own");
  return SnapshotDescriptor.fromWire({ own, xip, xmax, xmin }, limits);
}

function decodeCursor(value: Json): BusCursor {
  knownMembers(value, "cursor", ["id", "xid"]);
  const xid = stringOf(member(value, "cursor", "xid"), "cursor.xid");
  const id = stringOf(member(value, "cursor", "id"), "cursor.id");
  return BusCursor.fromWire({ id, xid });
}

function checkId(id: bigint, path: string): number {
  if (id === 0n || id > BigInt(MAX_SAFE)) throw new WireError("bad_id", path, `bad record id ${id}`);
  return Number(id);
}

function decodeId(value: Json, path: string): number {
  if (!isNumber(value)) throw malformed(path, "expected an integer");
  const id = asU64(value);
  if (id === null) throw new WireError("bad_id", path, `bad record id ${value.v}`);
  return checkId(id, path);
}

function checkSafeInt(n: bigint, path: string): number {
  if (n < -BigInt(MAX_SAFE) || n > BigInt(MAX_SAFE)) {
    throw new WireError("value_out_of_range", path, "integer is outside the safe range +-(2^53-1)");
  }
  return Number(n);
}

function checkString(s: string, limits: WireLimits, path: string): string {
  limits.check("string_bytes", utf8Length(s), path);
  return s;
}

function digits(s: string): number | null {
  return /^[0-9]+$/.test(s) ? Number.parseInt(s, 10) : null;
}

function validDate(s: string): boolean {
  if (s.length !== 10 || s[4] !== "-" || s[7] !== "-") return false;
  const y = digits(s.slice(0, 4));
  const m = digits(s.slice(5, 7));
  const d = digits(s.slice(8, 10));
  if (y === null || m === null || d === null) return false;
  const leap = (y % 4 === 0 && y % 100 !== 0) || y % 400 === 0;
  const days = [1, 3, 5, 7, 8, 10, 12].includes(m) ? 31 : [4, 6, 9, 11].includes(m) ? 30 : m === 2 ? (leap ? 29 : 28) : 0;
  return days !== 0 && y >= 1 && d >= 1 && d <= days;
}

function checkDate(s: string, path: string): string {
  if (!validDate(s)) throw new WireError("value_out_of_range", path, "not a valid YYYY-MM-DD date");
  return s;
}

function checkDatetime(s: string, path: string): string {
  const h = digits(s.slice(11, 13));
  const mi = digits(s.slice(14, 16));
  const se = digits(s.slice(17, 19));
  const ok =
    s.length === 19 && validDate(s.slice(0, 10)) && s[10] === " " && s[13] === ":" && s[16] === ":" &&
    h !== null && h < 24 && mi !== null && mi < 60 && se !== null && se < 60;
  if (!ok) throw new WireError("value_out_of_range", path, "not a valid YYYY-MM-DD HH:MM:SS datetime");
  return s;
}

const OPS: readonly string[] = ["add", "remove", "inc"];

/**
 * Refuse an operation object on a kind that has no handling of its own: an unknown key is `unknown_operation`; a
 * known one is `operation_not_allowed` for this kind.
 */
function refuseObject(object: JsonObject, kind: string, path: string): WireError {
  const unknown = object.m.find(([k]) => !OPS.includes(k));
  if (unknown !== undefined) return new WireError("unknown_operation", path, `unknown operation ${unknown[0]}`);
  const first = object.m[0];
  if (first === undefined) return mismatch(path, kind);
  if (first[0] === "inc") return notAllowed(path, "inc is only allowed on a counter field");
  return notAllowed(path, "add and remove are only allowed on an x2many field");
}

function decodeIdList(value: Json, limits: WireLimits, path: string, nonEmpty: boolean): number[] {
  if (!isArray(value)) throw mismatch(path, "x2many");
  limits.check("ids_per_list", value.length, path);
  if (nonEmpty && value.length === 0) throw new WireError("empty_operation", path, "empty operation");
  const ids = value.map((item, j) => decodeId(item, `${path}[${j}]`));
  const sorted = [...ids].sort((a, b) => a - b);
  for (let i = 1; i < sorted.length; i += 1) {
    if (sorted[i - 1] === sorted[i]) throw new WireError("duplicate_id", path, `duplicate id ${sorted[i]}`);
  }
  return sorted;
}

function decodeCounterObject(object: JsonObject, frame: boolean, path: string): ValueDoc {
  const unknown = object.m.find(([k]) => !OPS.includes(k));
  if (unknown !== undefined) throw new WireError("unknown_operation", path, `unknown operation ${unknown[0]}`);
  if (object.m.some(([k]) => k !== "inc")) throw notAllowed(path, "add and remove are only allowed on an x2many field");
  if (!frame) throw notAllowed(path, "inc is only allowed in a bus frame");
  const only = object.m[0];
  if (only === undefined || !isNumber(only[1])) throw mismatch(path, "counter");
  const n = asI64(only[1]);
  if (n === null) throw mismatch(path, "counter");
  if (n === 0n) throw new WireError("zero_delta", path, "zero counter delta");
  return { kind: "inc", value: checkSafeInt(n, path) };
}

function decodeX2ManyOps(object: JsonObject, limits: WireLimits, path: string): ValueDoc {
  const unknown = object.m.find(([k]) => !OPS.includes(k));
  if (unknown !== undefined) throw new WireError("unknown_operation", path, `unknown operation ${unknown[0]}`);
  if (object.m.some(([k]) => k === "inc")) throw notAllowed(path, "inc is only allowed on a counter field");
  if (object.m.length === 0) throw new WireError("empty_operation", path, "empty operation");
  const list = (key: string): number[] => {
    const entry = object.m.find(([k]) => k === key);
    return entry === undefined ? [] : decodeIdList(entry[1], limits, `${path}.${key}`, true);
  };
  const add = list("add");
  const remove = list("remove");
  const overlap = add.find((id) => remove.includes(id));
  if (overlap !== undefined) {
    throw new WireError("add_remove_overlap", path, `id ${overlap} is in both add and remove`);
  }
  return { kind: "x2many_delta", value: { add, remove } };
}

function decodeValue(
  schema: StoreSchema,
  limits: WireLimits,
  frame: boolean,
  field: SchemaFieldDoc,
  value: Json,
  path: string,
): ValueDoc {
  if (frame && field.sync === "answer_only") throw notAllowed(path, "answer_only field is not allowed in a frame");
  const kind = field.kind;
  // Operation objects first, for the kinds that do not take an object as their plain value.
  if (isObject(value)) {
    if (kind === "x2many") return decodeX2ManyOps(value, limits, path);
    if (kind === "counter") return decodeCounterObject(value, frame, path);
    if (kind === "reference") {
      if (value.m.some(([k]) => OPS.includes(k))) throw refuseObject(value, kind, path);
    } else {
      throw refuseObject(value, kind, path);
    }
  }
  switch (kind) {
    case "bool":
      if (typeof value === "boolean") return { kind, value };
      break;
    case "int":
      if (isNumber(value)) {
        const n = asI64(value);
        if (n !== null) return { kind, value: checkSafeInt(n, path) };
        if (value.t === "int") {
          throw new WireError("value_out_of_range", path, "integer is outside the safe range +-(2^53-1)");
        }
      }
      break;
    case "float":
      if (isNumber(value)) {
        const f = value.t === "int" ? Number(value.v) : value.v;
        if (Number.isFinite(f)) return { kind, value: f };
      }
      break;
    case "string":
    case "html":
    case "selection":
      if (typeof value === "string") return { kind, value: checkString(value, limits, path) };
      break;
    case "date":
      if (typeof value === "string") return { kind, value: checkDate(value, path) };
      break;
    case "datetime":
      if (typeof value === "string") return { kind, value: checkDatetime(value, path) };
      break;
    case "many2one":
      if (value === null) return { kind, value: null };
      if (isNumber(value)) return { kind, value: decodeId(value, path) };
      break;
    case "reference":
      if (value === null) return { kind, value: null };
      if (isObject(value)) return { kind, value: decodeReference(schema, value, path) };
      break;
    case "x2many":
      if (isArray(value)) {
        if (frame) throw notAllowed(path, "x2many replace is not allowed in a frame");
        return { kind: "x2many_replace", value: decodeIdList(value, limits, path, false) };
      }
      break;
    case "counter":
      if (isNumber(value)) {
        if (frame) throw notAllowed(path, "a frame carries counters as inc deltas");
        const n = asI64(value);
        if (n === null) throw mismatch(path, "counter");
        const safe = checkSafeInt(n, path);
        if (field.cap !== null && safe >= 0 && safe > field.cap) {
          throw new WireError("value_out_of_range", path, `counter value is above its cap ${field.cap}`);
        }
        return { kind, value: safe };
      }
      break;
  }
  throw mismatch(path, kind);
}

function decodeReference(schema: StoreSchema, value: JsonObject, path: string): { model: string; id: number } | null {
  const model = getMember(value, "model");
  const id = getMember(value, "id");
  if (value.m.length !== 2 || model === undefined || id === undefined) throw mismatch(path, "reference");
  if (typeof model !== "string") throw mismatch(path, "reference");
  if (lookupModel(schema, model) === undefined) {
    throw new WireError("unknown_model", `${path}.model`, `unknown model ${model}`);
  }
  const decodedId = decodeId(id, `${path}.id`);
  validateModelName(model, `${path}.model`);
  return { id: decodedId, model };
}

function decodeFieldSpec(spec: Json, path: string): SchemaFieldDoc {
  knownSchemaMembers(spec, path, ["cap", "kind", "sync", "target"]);
  const kindValue = getMember(spec, "kind");
  if (kindValue === undefined) throw badSchemaAt(`${path}.kind`, "kind is required");
  if (typeof kindValue !== "string") throw badSchemaAt(`${path}.kind`, "kind must be a string");
  if (!isFieldKind(kindValue)) throw badSchemaAt(`${path}.kind`, `unknown kind ${kindValue}`);
  const targetValue = getMember(spec, "target");
  let target: string | null = null;
  if (targetValue !== undefined) {
    if (typeof targetValue !== "string") throw badSchemaAt(`${path}.target`, "target must be a string");
    target = targetValue;
  }
  const capValue = getMember(spec, "cap");
  let cap: number | null = null;
  if (capValue !== undefined) {
    if (!isNumber(capValue)) throw badSchemaAt(`${path}.cap`, "cap must be an integer");
    const n = asU64(capValue);
    if (n === null) throw badSchemaAt(`${path}.cap`, "cap must be 1..=2^53-1");
    // A value above the safe range is refused by makeField as a cap error; keep it distinguishable.
    cap = n > BigInt(MAX_SAFE) ? MAX_SAFE + 1 : Number(n);
  }
  const syncValue = getMember(spec, "sync");
  let sync: SyncDecl = "unset";
  if (syncValue !== undefined) {
    if (syncValue === "state") sync = "state";
    else if (syncValue === "answer_only") sync = "answer_only";
    else throw badSchemaAt(`${path}.sync`, "sync must be state or answer_only");
  }
  return makeField(path, kindValue, target, sync, cap);
}

interface MutableField {
  name: string;
  value: ValueDoc;
  written: boolean;
}

interface MutableRecord {
  id: number;
  model: string;
  fields: MutableField[];
}

function applyWritten(upserts: MutableRecord[], written: Json, hasOwn: boolean): void {
  const bad = (path: string, reason: string): WireError => new WireError("bad_written", path, `bad written marker: ${reason}`);
  if (!hasOwn) throw bad("written", "answer has no own xid");
  if (!isObject(written)) throw malformed("written", "expected an object");
  for (const [model, ids] of written.m) {
    const mpath = `written.${model}`;
    if (!upserts.some((r) => r.model === model)) throw bad(mpath, "model is not in the payload");
    if (!isObject(ids)) throw malformed(mpath, "expected an object");
    for (const [idText, names] of ids.m) {
      const ipath = `${mpath}.${idText}`;
      const parsed = parseDecimal(idText);
      if (typeof parsed === "string" || parsed < 1n || parsed > BigInt(MAX_SAFE)) throw bad(ipath, "not a record id");
      const record = upserts.find((r) => r.model === model && r.id === Number(parsed));
      if (record === undefined) throw bad(ipath, "record is not in the payload");
      if (!isArray(names)) throw malformed(ipath, "expected an array");
      const seen = new Set<string>();
      for (const [j, nameValue] of names.entries()) {
        const npath = `${ipath}[${j}]`;
        const name = stringOf(nameValue, npath);
        if (seen.has(name)) throw new WireError("duplicate_field", npath, "duplicate field");
        seen.add(name);
        const field = record.fields.find((f) => f.name === name);
        if (field === undefined) throw bad(npath, "field is not set by the entry");
        if (field.value.kind === "counter" || field.value.kind === "inc") {
          throw bad(npath, "counter fields cannot be written markers");
        }
        field.written = true;
      }
    }
  }
}

function byModelThenId(a: { model: string; id: number }, b: { model: string; id: number }): number {
  if (a.model !== b.model) return a.model < b.model ? -1 : 1;
  return a.id - b.id;
}

/**
 * The strict decoder: payloads are checked against the schema it holds and the limits it was given, and refused
 * exactly like the Rust decoder (same code, same path). Nothing is returned for a refused payload, so nothing is
 * applied partially. Duplicate object keys are refused, never resolved last-one-wins.
 */
export class Decoder {
  #schema: StoreSchema | null = null;
  readonly #limits: WireLimits;

  /**
   * @param limits - the host's limits (default: the compiled-in maxima)
   */
  constructor(limits: WireLimits = WireLimits.defaults()) {
    this.#limits = limits;
  }

  /**
   * Hold a schema.
   *
   * @param schema - a schema from {@link Decoder.decodeSchema}
   */
  setSchema(schema: StoreSchema): void {
    this.#schema = schema;
  }

  /** The schema generation held, or `null`. */
  get generation(): string | null {
    return this.#schema === null ? null : this.#schema.generation;
  }

  /**
   * Decode a schema payload.
   *
   * @param input - the document as text or UTF-8 bytes
   * @throws {@link WireError} `wire_major_unsupported`, `malformed`, `bad_schema`, `bad_name` or `limit_exceeded`
   */
  decodeSchema(input: string | Uint8Array): StoreSchema {
    const limits = this.#limits;
    const doc = parseJson(input, limits);
    const generation = header(doc, limits);
    knownMembers(doc, "", ["models", "schema", "wire"]);
    const models = member(doc, "", "models");
    if (!isObject(models)) throw malformed("models", "expected an object");
    limits.check("models", models.m.length, "models");
    const out = dict<SchemaModelDoc>();
    let totalFields = 0;
    for (const [model, body] of models.m) {
      const mpath = `models.${model}`;
      validateModelName(model, mpath);
      knownSchemaMembers(body, mpath, ["fields"]);
      const fieldsPath = `${mpath}.fields`;
      const fields = member(body, mpath, "fields");
      if (!isObject(fields)) throw malformed(fieldsPath, "expected an object");
      totalFields += fields.m.length;
      limits.check("schema_fields", totalFields, fieldsPath);
      const decoded = dict<SchemaFieldDoc>();
      for (const [field, spec] of fields.m) {
        const fpath = `${fieldsPath}.${field}`;
        validateFieldName(field, fpath);
        decoded[field] = decodeFieldSpec(spec, fpath);
      }
      out[model] = { fields: decoded };
    }
    return finishSchema(generation, out);
  }

  /**
   * Decode an answer payload.
   *
   * @param input - the document as text or UTF-8 bytes
   * @throws {@link WireError} any refusal of the decode contract
   */
  decodeAnswer(input: string | Uint8Array): Outcome<DecodedPayload> {
    return this.#decodePayload(input, false);
  }

  /**
   * Decode a bus frame.
   *
   * @param input - the document as text or UTF-8 bytes
   * @throws {@link WireError} any refusal of the decode contract
   */
  decodeFrame(input: string | Uint8Array): Outcome<DecodedPayload> {
    return this.#decodePayload(input, true);
  }

  #decodePayload(input: string | Uint8Array, frame: boolean): Outcome<DecodedPayload> {
    const limits = this.#limits;
    const doc = parseJson(input, limits);
    const generation = header(doc, limits);
    const schema = this.#schema;
    if (schema === null || schema.generation !== generation) return { outcome: "needs_schema", generation };
    knownMembers(
      doc,
      "",
      frame ? ["asof", "cursor", "records", "schema", "wire"] : ["asof", "records", "schema", "wire", "written"],
    );
    const asof = decodeAsof(member(doc, "", "asof"), limits);
    let cursor: BusCursor | null = null;
    if (frame) {
      cursor = decodeCursor(member(doc, "", "cursor"));
      if (asof.own !== cursor.xid) {
        throw new WireError("bad_descriptor", "asof.own", "bad descriptor: own xid must equal the cursor xid");
      }
    }
    const { upserts, deletes } = this.#decodeRecords(schema, member(doc, "", "records"), frame);
    const written = getMember(doc, "written");
    if (written !== undefined) applyWritten(upserts, written, asof.own !== null);
    upserts.sort(byModelThenId);
    deletes.sort(byModelThenId);
    return {
      outcome: "decoded",
      payload: Object.freeze({
        generation,
        asof,
        cursor,
        upserts: Object.freeze(upserts.map((r) => Object.freeze({ id: r.id, model: r.model, fields: Object.freeze(r.fields.map((f) => Object.freeze(f))) }))) as readonly RecordDoc[],
        deletes: Object.freeze(deletes) as readonly RecordRefDoc[],
      }),
    };
  }

  #decodeRecords(schema: StoreSchema, records: Json, frame: boolean): { upserts: MutableRecord[]; deletes: RecordRefDoc[] } {
    const limits = this.#limits;
    if (!isObject(records)) throw malformed("records", "expected an object");
    limits.check("models", records.m.length, "records");
    let total = 0;
    for (const [, list] of records.m) if (isArray(list)) total += list.length;
    limits.check("records", total, "records");
    const upserts: MutableRecord[] = [];
    const deletes: RecordRefDoc[] = [];
    for (const [model, list] of records.m) {
      const mpath = `records.${model}`;
      const modelSchema: SchemaModelDoc | undefined = lookupModel(schema, model);
      if (modelSchema === undefined) throw new WireError("unknown_model", mpath, `unknown model ${model}`);
      validateModelName(model, mpath);
      if (!isArray(list)) throw malformed(mpath, "expected an array");
      const seen = new Set<number>();
      for (const [i, entry] of list.entries()) {
        const epath = `${mpath}[${i}]`;
        if (!isObject(entry)) throw malformed(epath, "expected an object");
        const id = decodeId(member(entry, epath, "id"), `${epath}.id`);
        if (seen.has(id)) throw new WireError("duplicate_id", `${epath}.id`, `duplicate id ${id}`);
        seen.add(id);
        const marker = getMember(entry, "delete");
        if (marker !== undefined) {
          if (marker !== true) throw malformed(`${epath}.delete`, "expected true");
          if (entry.m.length > 2) throw new WireError("delete_with_fields", epath, "delete marker together with fields");
          deletes.push({ id, model });
          continue;
        }
        const fields: MutableField[] = [];
        for (const [key, value] of entry.m) {
          if (key === "id") continue;
          const fpath = `${epath}.${key}`;
          const fieldSchema = lookupField(modelSchema, key);
          if (fieldSchema === undefined) throw new WireError("unknown_field", fpath, `unknown field ${key}`);
          fields.push({ name: key, value: decodeValue(schema, limits, frame, fieldSchema, value, fpath), written: false });
        }
        fields.sort((a, b) => (a.name < b.name ? -1 : a.name > b.name ? 1 : 0));
        upserts.push({ id, model, fields });
      }
    }
    return { upserts, deletes };
  }
}
