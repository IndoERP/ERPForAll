/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
/**
  mirror the arguments built by chatter, from addons/mail/src/thread_rpc.rs
 * thread_upload/thread_attach and addons/base/src/attachments/service.rs prepare. Access and
 * store effects remain the native fixture's responsibility; this fake validates shape and bytes.
 */
export function refuseInvalidAttachmentRequest(args: Record<string, unknown>, limit: number): Response | undefined {
  const fail = (message: string, name = "erp.exceptions.ValidationError"): Response => new Response(JSON.stringify({ name, message }), { status: 422, headers: { "Content-Type": "application/json" } });
  if (Object.keys(args).some(key => !["ids", "name", "mimetype", "data"].includes(key))
      || !Array.isArray(args["ids"]) || args["ids"].length !== 1 || !Number.isSafeInteger(args["ids"][0]) || args["ids"][0] <= 0
      || typeof args["name"] !== "string" || args["name"].length === 0 || new TextEncoder().encode(args["name"]).length > 255 || /[\r\n\0]/u.test(args["name"])
      || typeof args["data"] !== "string" || args["mimetype"] !== undefined && typeof args["mimetype"] !== "string") return fail("Invalid attachment upload arguments.");
  let bytes: string;
  try { bytes = atob(args["data"]); }
  catch { return fail("The attachment data is not valid base64.", "erp.exceptions.UserError"); }
  if (btoa(bytes) !== args["data"]) return fail("The attachment data is not valid base64.", "erp.exceptions.UserError");
  if (bytes.length > limit) return fail(`The attachment is larger than the limit of ${limit} bytes (base.attachment_max_bytes).`);
  return undefined;
}
