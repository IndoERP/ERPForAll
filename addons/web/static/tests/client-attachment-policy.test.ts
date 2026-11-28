/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
import { expect, test } from "vitest";
import { attachmentLimitHint, attachmentPolicy, attachmentUploadCsrf, checkAttachmentFile, checkAttachmentRequest, initializeAttachmentPolicy } from "../src/client/attachment-policy.js";
import { RpcClient } from "../src/client/transport.js";

/// (partial: live policy increases/decreases override stale bootstrap preflight)
test("selection refreshes policy after an administrator raises or lowers the setting", async () => {
  let limit = 4, reads = 0;
  const rpc = new RpcClient(async (path, init) => {
    expect(String(path)).toBe("/erp/json/2/ir.attachment/erp_upload_limits");
    expect(JSON.parse(String(init?.body))).toEqual({});
    reads++;
    return new Response(JSON.stringify({ max_file_bytes: limit, inline_max_bytes: limit }), { headers: { "Content-Type": "application/json" } });
  }, () => {});
  initializeAttachmentPolicy(rpc, { max_file_bytes: 4, inline_max_bytes: 4 });
  const file = new File(["123456"], "six.txt");
  await expect(checkAttachmentFile(rpc, file)).rejects.toMatchObject({ kind: "validation", message: '"six.txt" exceeds the current browser upload limit of 4 bytes.' });
  limit = 8;
  await checkAttachmentFile(rpc, file);
  limit = 2;
  await expect(checkAttachmentFile(rpc, file)).rejects.toMatchObject({ kind: "validation", message: '"six.txt" exceeds the current browser upload limit of 2 bytes.' });
  expect(reads).toBe(3);
});

test("malformed policy fails explicitly and zero inline permits only empty files", async () => {
  for (const raw of [undefined, null, {}, { max_file_bytes: 0, inline_max_bytes: 0 }, { max_file_bytes: 4, inline_max_bytes: 5 }, { max_file_bytes: 4, inline_max_bytes: 4, unknown: true }, { max_file_bytes: "4", inline_max_bytes: 4 }]) {
    try { attachmentPolicy(raw); throw new Error("expected policy refusal"); }
    catch (error) { expect(error).toMatchObject({ kind: "server", message: "The server returned an invalid attachment size policy. Reload the application." }); }
  }
  const rpc = new RpcClient(async () => new Response(JSON.stringify({ max_file_bytes: 4, inline_max_bytes: 0 }), { headers: { "Content-Type": "application/json" } }), () => {});
  await checkAttachmentFile(rpc, new File([], "empty.txt"));
  await expect(checkAttachmentFile(rpc, new File(["x"], "one.txt"))).rejects.toMatchObject({ kind: "validation", message: '"one.txt" exceeds the current browser upload limit of 0 bytes.' });
});

test("request admission counts the serialized UTF-8 envelope separately from decoded content", () => {
  const rpc = new RpcClient(async () => { throw new Error("no network expected"); }, () => {});
  initializeAttachmentPolicy(rpc, { max_file_bytes: 65536, inline_max_bytes: 65536 }, 65536);
  checkAttachmentRequest(rpc, { data: "a".repeat(65525) });
  for (const args of [{ data: "a".repeat(65526) }, { name: "é".repeat(32768), data: "" }]) {
    try { checkAttachmentRequest(rpc, args); throw new Error("expected request refusal"); }
    catch (error) { expect(error).toMatchObject({ kind: "validation", message: "The encoded attachment request exceeds this server's 64 KiB request limit." }); }
  }
});

/// (partial: the limit hint is plain words and has no implementation wording)
test("the limit hint states the limit in user words", () => {
  const rpc = new RpcClient(async () => new Response("{}"), () => {});
  expect(attachmentLimitHint(rpc)).toBeUndefined();
  initializeAttachmentPolicy(rpc, { max_file_bytes: 2048, inline_max_bytes: 2048 });
  expect(attachmentLimitHint(rpc)).toBe("Each file can be up to 2 KiB.");
});

test("streaming HTTP upload mode enforces max_file_bytes rather than inline_max_bytes", async () => {
  const rpc = new RpcClient(async () => new Response(JSON.stringify({ max_file_bytes: 134217728, inline_max_bytes: 2048 }), { headers: { "Content-Type": "application/json" } }), () => {});
  expect(attachmentUploadCsrf(rpc)).toBeUndefined();
  // Without a CSRF token, inline_max_bytes still bounds uploads even when max_file_bytes is larger.
  initializeAttachmentPolicy(rpc, { max_file_bytes: 134217728, inline_max_bytes: 2048 });
  expect(attachmentLimitHint(rpc)).toBe("Each file can be up to 2 KiB.");
  const photo = new File([new Uint8Array(16 * 1024)], "IMG_8068.JPG", { type: "image/jpeg" });
  await expect(checkAttachmentFile(rpc, photo)).rejects.toMatchObject({ kind: "validation", message: '"IMG_8068.JPG" exceeds the current browser upload limit of 2 KiB.' });

  // Invalid CSRF tokens fail explicitly.
  for (const badCsrf of ["", 123, {}]) {
    expect(() => initializeAttachmentPolicy(rpc, undefined, undefined, badCsrf)).toThrow("invalid upload token");
  }

  // With a valid session CSRF token, max_file_bytes is enforced instead.
  initializeAttachmentPolicy(rpc, { max_file_bytes: 134217728, inline_max_bytes: 2048 }, undefined, "csrf-token");
  expect(attachmentUploadCsrf(rpc)).toBe("csrf-token");
  expect(attachmentLimitHint(rpc)).toBe("Each file can be up to 128 MiB.");
  await checkAttachmentFile(rpc, photo);
  const oversized = new File(["x"], "huge.bin");
  Object.defineProperty(oversized, "size", { value: 134217729 });
  await expect(checkAttachmentFile(rpc, oversized)).rejects.toMatchObject({ kind: "validation", message: '"huge.bin" exceeds the current browser upload limit of 128 MiB.' });
});
