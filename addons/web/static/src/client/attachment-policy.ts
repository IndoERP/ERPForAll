/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
import { ClientError, type RpcClient } from "./transport.js";

export interface AttachmentPolicy {
  readonly max_file_bytes: number;
  readonly inline_max_bytes: number;
}
const policies = new WeakMap<RpcClient, AttachmentPolicy>();
const requestCaps = new WeakMap<RpcClient, number>();

/** Strict DTO decoding; bounds mirror and the base attachment runtime. */
export function attachmentPolicy(value: unknown): AttachmentPolicy {
  if (value === null || typeof value !== "object" || Array.isArray(value)) throw invalid();
  const raw = value as Record<string, unknown>;
  const max = raw["max_file_bytes"], inline = raw["inline_max_bytes"];
  if (Object.keys(raw).length !== 2 || typeof max !== "number" || !Number.isSafeInteger(max) || max < 1 || max > 2147483647
      || typeof inline !== "number" || !Number.isSafeInteger(inline) || inline < 0 || inline > Math.min(max, 65536)) throw invalid();
  return Object.freeze({ max_file_bytes: max, inline_max_bytes: inline });
}
function invalid(): ClientError { return new ClientError("server", "The server returned an invalid attachment size policy. Reload the application."); }

/** Bootstrap may explicitly have no provider; selecting a file must still fetch an authoritative policy. */
export function initializeAttachmentPolicy(rpc: RpcClient, value: unknown, requestCap?: unknown): void {
  if (value !== null && value !== undefined) policies.set(rpc, attachmentPolicy(value));
  if (requestCap !== null && requestCap !== undefined) {
    if (typeof requestCap !== "number" || !Number.isSafeInteger(requestCap) || requestCap < 65536 || requestCap > 134217728) throw invalid();
    requestCaps.set(rpc, requestCap);
  }
}

/** A standalone editor may have no node transport hint; it must not invent one. */
export function checkAttachmentRequest(rpc: RpcClient, args: Record<string, unknown>): void {
  const cap = requestCaps.get(rpc);
  if (cap !== undefined && new TextEncoder().encode(JSON.stringify(args)).length > cap) {
    throw new ClientError("validation", `The encoded attachment request exceeds this server's ${humanBytes(cap)} request limit.`);
  }
}

function humanBytes(bytes: number): string {
  if (bytes >= 1048576 && bytes % 1048576 === 0) return `${bytes / 1048576} MiB`;
  if (bytes >= 1024 && bytes % 1024 === 0) return `${bytes / 1024} KiB`;
  return `${bytes} bytes`;
}

/** The limit in user words, from the boot snapshot; selecting a file still checks the current limit before upload. */
export function attachmentLimitHint(rpc: RpcClient): string | undefined {
  const policy = policies.get(rpc);
  return policy === undefined ? undefined : `Each file can be up to ${humanBytes(policy.inline_max_bytes)}.`;
}

/** Refresh even when an older policy would reject: an administrator may have raised the limit. */
export async function checkAttachmentFile(rpc: RpcClient, file: File): Promise<void> {
  const policy = attachmentPolicy(await rpc.call("ir.attachment", "erp_upload_limits", {}));
  policies.set(rpc, policy);
  if (!Number.isSafeInteger(file.size) || file.size < 0) throw new ClientError("validation", "The browser reported an unusable size for this file.");
  if (file.size > policy.inline_max_bytes) throw new ClientError("validation", `"${file.name}" exceeds the current browser upload limit of ${humanBytes(policy.inline_max_bytes)}.`);
}
