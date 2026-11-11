/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
import type { ErrorCodeDoc } from "./generated/ErrorCodeDoc.d";
import type { LimitDoc } from "./generated/LimitDoc.d";

/**
 * A refusal by the decoder or a host-limit setter. The `code` and `path` are the contract shared with the Rust
 * decoder: every refusal carries the same stable machine code and the same path into the
 * input. The message is for people and is not compared.
 */
export class WireError extends Error {
  /** The stable machine code of the refusal. */
  readonly code: ErrorCodeDoc;
  /**
   * The path into the input. An empty string means the whole document; `null` is for refusals that have no
   * document position (`wire_major_unsupported`, `client_outdated`, `bad_limit`).
   */
  readonly path: string | null;
  /** The limit that was exceeded, for `limit_exceeded`; `null` otherwise. */
  readonly limit: LimitDoc | null;

  constructor(code: ErrorCodeDoc, path: string | null, reason: string, limit: LimitDoc | null = null) {
    super(path === null ? `${code}: ${reason}` : `${code} at ${path === "" ? "<document>" : path}: ${reason}`);
    this.name = "WireError";
    this.code = code;
    this.path = path;
    this.limit = limit;
  }
}

/**
 * The reference applier was used against its own contract: a payload for another schema generation, a request
 * that was never begun, an invalid bound. It is a programming error of the host, never a wire refusal.
 */
export class ApplyError extends Error {
  constructor(message: string) {
    super(message);
    this.name = "ApplyError";
  }
}
