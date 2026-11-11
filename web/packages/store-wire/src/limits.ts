/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
import type { LimitDoc } from "./generated/LimitDoc.d";
import { WireError } from "./errors";

/** Every bounded quantity of a payload, in the order the Rust crate declares them. */
export const LIMITS: readonly LimitDoc[] = [
  "bytes",
  "depth",
  "models",
  "records",
  "ids_per_list",
  "xip",
  "string_bytes",
  "schema_token_bytes",
  "schema_fields",
];

const MAXIMUMS: Readonly<Record<LimitDoc, number>> = {
  bytes: 8 * 1024 * 1024,
  depth: 16,
  models: 256,
  records: 50_000,
  ids_per_list: 50_000,
  xip: 65_536,
  string_bytes: 1024 * 1024,
  schema_token_bytes: 128,
  schema_fields: 16_384,
};

/**
 * The compiled-in maximum of a limit. It is also the default, and no host setting can exceed it.
 *
 * @param limit - which limit
 */
export function limitMaximum(limit: LimitDoc): number {
  return MAXIMUMS[limit];
}

/**
 * The limits in force for one decoder. Immutable: a host lowers a limit with {@link WireLimits.lowered}, which
 * refuses zero and anything above the compiled-in maximum, so no configuration switches a bound off or widens it.
 */
export class WireLimits {
  readonly #values: Readonly<Record<LimitDoc, number>>;

  private constructor(values: Readonly<Record<LimitDoc, number>>) {
    this.#values = values;
  }

  /** The limits at their compiled-in maxima. */
  static defaults(): WireLimits {
    return new WireLimits({ ...MAXIMUMS });
  }

  /**
   * The value in force for a limit.
   *
   * @param limit - which limit
   */
  get(limit: LimitDoc): number {
    return this.#values[limit];
  }

  /**
   * A copy with one limit lowered.
   *
   * @param limit - which limit
   * @param value - the new value, 1 up to the compiled-in maximum
   * @throws {@link WireError} `bad_limit` for zero, a non-integer or a value above the maximum
   */
  lowered(limit: LimitDoc, value: number): WireLimits {
    if (!Number.isSafeInteger(value) || value === 0) {
      throw new WireError("bad_limit", null, `${limit}: zero is not a limit`);
    }
    if (value < 0 || value > MAXIMUMS[limit]) {
      throw new WireError("bad_limit", null, `${limit}: ${value} is outside 1..=${MAXIMUMS[limit]}`);
    }
    return new WireLimits({ ...this.#values, [limit]: value });
  }

  /**
   * Refuse an input of `size` against a limit.
   *
   * @param limit - which limit
   * @param size - the size of the input
   * @param path - where the input is
   * @throws {@link WireError} `limit_exceeded` naming the limit
   */
  check(limit: LimitDoc, size: number, path: string): void {
    if (size > this.#values[limit]) {
      throw new WireError("limit_exceeded", path, `limit ${limit} exceeded (maximum ${this.#values[limit]})`, limit);
    }
  }
}
