/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
import type { FieldKindDoc } from "./generated/FieldKindDoc.d";
import type { SchemaDoc } from "./generated/SchemaDoc.d";
import type { SchemaFieldDoc } from "./generated/SchemaFieldDoc.d";
import type { SchemaModelDoc } from "./generated/SchemaModelDoc.d";
import type { SyncClassDoc } from "./generated/SyncClassDoc.d";
import { WireError } from "./errors";
import type { WireLimits } from "./limits";

/** A validated store schema as the decoder holds it (the generated `SchemaDoc`). */
export type StoreSchema = SchemaDoc;

/** The closed set of field kinds. Adding one bumps the wire major. */
export const FIELD_KINDS: readonly FieldKindDoc[] = [
  "bool", "int", "float", "string", "html", "date", "datetime", "selection", "many2one", "reference", "x2many", "counter",
];

/** The largest integer a JavaScript number holds exactly. */
export const MAX_SAFE = Number.MAX_SAFE_INTEGER;

const MAX_NAME_BYTES = 128;

/**
 * Whether a string is a field kind.
 *
 * @param name - the wire name
 */
export function isFieldKind(name: string): name is FieldKindDoc {
  return (FIELD_KINDS as readonly string[]).includes(name);
}

/**
 * The size of a string in UTF-8 bytes, without allocating.
 *
 * @param s - a well-formed string
 */
export function utf8Length(s: string): number {
  let n = 0;
  for (const ch of s) {
    const c = ch.codePointAt(0) ?? 0;
    n += c < 0x80 ? 1 : c < 0x800 ? 2 : c < 0x10000 ? 3 : 4;
  }
  return n;
}

function badName(path: string, reason: string): WireError {
  return new WireError("bad_name", path, reason);
}

function badSchema(path: string, reason: string): WireError {
  return new WireError("bad_schema", path, reason);
}

/** The own entry of a dictionary; a name such as `constructor` is never found on the prototype chain. */
function own<T>(dict: { [key in string]?: T }, name: string): T | undefined {
  return Object.hasOwn(dict, name) ? dict[name] : undefined;
}

/**
 * The model of a schema.
 *
 * @param schema - the schema
 * @param name - the model name
 */
export function lookupModel(schema: StoreSchema, name: string): SchemaModelDoc | undefined {
  return own(schema.models, name);
}

/**
 * The field of a model.
 *
 * @param model - the model
 * @param name - the field name
 */
export function lookupField(model: SchemaModelDoc, name: string): SchemaFieldDoc | undefined {
  return own(model.fields, name);
}

/**
 * Validate a model name: lowercase letters, digits, `_` and `.`, starting with a letter, at most 128 bytes.
 *
 * @param name - the name
 * @param path - the path reported by a refusal
 * @throws {@link WireError} `bad_name`
 */
export function validateModelName(name: string, path: string): void {
  if (name.length > MAX_NAME_BYTES || !/^[a-z][a-z0-9_.]*$/.test(name)) {
    throw badName(path, "model name must be lowercase letters, digits, '_' and '.', starting with a letter");
  }
}

/**
 * Validate a field name: lowercase letters, digits and `_`, starting with a letter or `_`, not `id`, not `delete`.
 *
 * @param name - the name
 * @param path - the path reported by a refusal
 * @throws {@link WireError} `bad_name`
 */
export function validateFieldName(name: string, path: string): void {
  if (name.length > MAX_NAME_BYTES || !/^[a-z_][a-z0-9_]*$/.test(name)) {
    throw badName(path, "field name must be lowercase letters, digits and '_', starting with a letter or '_'");
  }
  if (name === "id" || name === "delete") throw badName(path, "field name is reserved");
}

/**
 * Validate a schema generation token: ASCII letters and digits, `.`, `_`, `-`, `:`, from 1 byte up to the
 * `schema_token_bytes` limit.
 *
 * @param token - the token
 * @param limits - the host's limits
 * @throws {@link WireError} `bad_name` (path `schema`) or `limit_exceeded`
 */
export function validateGeneration(token: string, limits: WireLimits): void {
  if (token === "") throw badName("schema", "empty generation token");
  limits.check("schema_token_bytes", utf8Length(token), "schema");
  if (!/^[A-Za-z0-9._:-]+$/.test(token)) {
    throw badName("schema", "generation token has a character outside A-Z a-z 0-9 . _ - :");
  }
}

/** The sync declaration as written, before it is checked against the kind. */
export type SyncDecl = "unset" | "state" | "answer_only";

/**
 * Check one field declaration against the schema rules (spec §4.2).
 *
 * @param path - `models.<m>.fields.<f>`
 * @param kind - the field kind
 * @param target - the declared target model, or null
 * @param sync - the declared sync class
 * @param cap - the declared saturation cap, or null
 * @throws {@link WireError} `bad_schema` or `bad_name` with the path of the offending member
 */
export function makeField(
  path: string,
  kind: FieldKindDoc,
  target: string | null,
  sync: SyncDecl,
  cap: number | null,
): SchemaFieldDoc {
  const relation = kind === "many2one" || kind === "x2many";
  if (relation && target === null) throw badSchema(`${path}.target`, `${kind} needs a target model`);
  if (!relation && target !== null) throw badSchema(`${path}.target`, "target is only allowed on many2one and x2many");
  if (target !== null) validateModelName(target, `${path}.target`);
  if (cap !== null) {
    if (kind !== "counter") throw badSchema(`${path}.cap`, "cap is only allowed on counter");
    if (cap === 0 || cap > MAX_SAFE) throw badSchema(`${path}.cap`, "cap must be 1..=2^53-1");
  }
  let syncClass: SyncClassDoc;
  if (kind === "counter") {
    if (sync !== "unset") throw badSchema(`${path}.sync`, "sync is not allowed on counter");
    syncClass = "counter";
  } else {
    syncClass = sync === "answer_only" ? "answer_only" : "state";
  }
  return { cap, kind, sync: syncClass, target };
}

/**
 * Assemble a schema from validated models, checking that every relation names a model of the schema.
 *
 * @param generation - the generation token
 * @param models - the validated models (a null-prototype dictionary)
 * @throws {@link WireError} `bad_schema` for a target that is not a model
 */
export function finishSchema(generation: string, models: { [key in string]?: SchemaModelDoc }): StoreSchema {
  for (const [model, body] of Object.entries(models)) {
    for (const [field, spec] of Object.entries(body?.fields ?? {})) {
      if (spec?.target != null && !Object.hasOwn(models, spec.target)) {
        throw badSchema(`models.${model}.fields.${field}.target`, `target ${spec.target} is not a model of the schema`);
      }
    }
  }
  return { generation, models };
}

/** A dictionary with no prototype, so any wire-supplied key (`__proto__`, `constructor`) is an ordinary key. */
export function dict<T>(): { [key in string]?: T } {
  return Object.create(null) as { [key in string]?: T };
}
