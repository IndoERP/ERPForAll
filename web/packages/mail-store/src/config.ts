/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
import { MessagingStoreError } from "./errors";

/**
 * The store's caps and names (4: "caps are configuration whose defaults the slice sets"). Every cap
 * bounds memory; the effect of the defaults is measured at milestone end.
 */
export interface MessagingConfig {
  readonly pageSize: number;
  /** Pages one thread's window may hold; the window holds at most `pageSize * maxWindowPages` messages. */
  readonly maxWindowPages: number;
  /** Windows kept for threads no surface holds; the least recently used beyond this are dropped. */
  readonly maxThreads: number;
  /** The model whose records the windows list. */
  readonly messageModel: string;
  /** Models swept when nothing retained references a record (partners, attachments). */
  readonly collectModels: readonly string[];
  /** Run {@link MessagingStore.collect} whenever a request settles. */
  readonly collectOnSettle: boolean;
}

/** The defaults of {@link MessagingConfig}. */
export const DEFAULT_CONFIG: MessagingConfig = Object.freeze({
  pageSize: 30,
  maxWindowPages: 5,
  maxThreads: 50,
  messageModel: "mail.message",
  collectModels: Object.freeze(["res.partner", "ir.attachment"]),
  collectOnSettle: true,
});

function positive(name: string, value: unknown): number {
  if (typeof value !== "number" || !Number.isSafeInteger(value) || value < 1) {
    throw new MessagingStoreError("bad_config", `config.${name} must be a positive integer, got ${String(value)}`);
  }
  return value;
}

/**
 * Fill the defaults and validate.
 *
 * @param given - the caps to change
 * @throws {@link MessagingStoreError} `bad_config` for a cap that is not a positive integer, a model name that is not
 *   a non-empty string, a collected model equal to the message model, or an option this version does not know
 */
export function resolveConfig(given: Partial<MessagingConfig> = {}): MessagingConfig {
  for (const key of Object.keys(given)) {
    if (!Object.hasOwn(DEFAULT_CONFIG, key)) {
      throw new MessagingStoreError("bad_config", `config.${key} is not a known option`);
    }
  }
  const merged = { ...DEFAULT_CONFIG, ...given };
  positive("pageSize", merged.pageSize);
  positive("maxWindowPages", merged.maxWindowPages);
  positive("maxThreads", merged.maxThreads);
  if (typeof merged.messageModel !== "string" || merged.messageModel === "") {
    throw new MessagingStoreError("bad_config", `config.messageModel must be a non-empty string, got ${String(merged.messageModel)}`);
  }
  if (!Array.isArray(merged.collectModels) || merged.collectModels.some((m) => typeof m !== "string" || m === "")) {
    throw new MessagingStoreError("bad_config", "config.collectModels must be a list of non-empty model names");
  }
  if (merged.collectModels.includes(merged.messageModel)) {
    throw new MessagingStoreError(
      "bad_config",
      `config.collectModels must not contain the message model ${merged.messageModel}: windows own messages`,
    );
  }
  if (typeof merged.collectOnSettle !== "boolean") {
    throw new MessagingStoreError("bad_config", `config.collectOnSettle must be a boolean, got ${String(merged.collectOnSettle)}`);
  }
  return Object.freeze({ ...merged, collectModels: Object.freeze([...merged.collectModels]) });
}
