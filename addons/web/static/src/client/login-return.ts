/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
/**
 * Return path for the session-expired sign-in link (threat model T-SHELL.8).
 *
 * The client applies the same refusals as the server's `RedirectPolicy::normalize`
 * (`crates/erp-server/src/redirect.rs`) to a path-only value, so the link never carries a
 * `redirect` the login route would answer with 400 "unsafe redirect". It is stricter than the server:
 * it also refuses `#` and dot segments. A refused value drops the return path and links to the plain
 * sign-in page.
 */

const LOGIN = "/erp/login";
/** Same bound as the server's `SHELL_URI_BYTES`. */
const RETURN_PATH_BYTES = 2048;
/** Unicode general category Cc, the set Rust's `char::is_control` tests (U+0000–U+001F, U+007F–U+009F). */
const CONTROL = /[\u0000-\u001f\u007f-\u009f]/;

/** Strict percent decoding: a malformed escape or invalid UTF-8 is a refusal, as on the server. */
function decode(value: string): string | null {
  if (/%(?![0-9a-fA-F]{2})/.test(value)) return null;
  try { return decodeURIComponent(value); } catch { return null; }
}

/** The value itself when it is a safe same-origin path (with optional query), otherwise `null`. */
export function localReturnPath(value: string): string | null {
  if (!value || new TextEncoder().encode(value).length > RETURN_PATH_BYTES) return null;
  if (CONTROL.test(value) || value.includes("\\") || value.includes(" ") || value.includes("#")) return null;
  if (!value.startsWith("/") || value.startsWith("//")) return null;
  const decoded = decode(value);
  if (decoded === null || decoded.startsWith("//") || CONTROL.test(decoded) || decoded.includes("\\")) return null;
  if (value.toLowerCase().includes("%2f%2f")) return null;
  // Dot segments are refused outright: URL resolution would turn `/.//host` or `/a/..//host` into a
  // scheme-relative `//host` path after every check above.
  const path = decoded.split("?", 1)[0]!;
  if (path.split("/").some(segment => segment === "." || segment === "..")) return null;
  if (new URL(value, "https://erp.invalid").pathname.startsWith("//")) return null;
  return value;
}

/** The sign-in URL that brings the user back to `location` (path and query; the fragment is dropped). */
export function loginReturnUrl(location: { readonly pathname: string; readonly search: string }): string {
  const target = localReturnPath(location.pathname + location.search);
  return target === null ? LOGIN : `${LOGIN}?${new URLSearchParams({ redirect: target })}`;
}
