/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
import { expect, test } from "vitest";
import { loginReturnUrl, localReturnPath } from "../src/client/login-return.js";

test("the session-expired sign-in link keeps the local client route as its return path", () => {
  expect(loginReturnUrl({ pathname: "/erp/action-42", search: "?id=7&view_type=form" }))
    .toBe("/erp/login?redirect=%2Ferp%2Faction-42%3Fid%3D7%26view_type%3Dform");
  expect(loginReturnUrl({ pathname: "/erp", search: "" })).toBe("/erp/login?redirect=%2Ferp");
  expect(localReturnPath("/erp/action-42?id=7")).toBe("/erp/action-42?id=7");
  const href = loginReturnUrl({ pathname: "/erp/action-42", search: "?cids=1%2C2" });
  expect(new URL(href, "https://erp.example.test").searchParams.get("redirect")).toBe("/erp/action-42?cids=1%2C2");
});

test("unsafe return paths fall back to the plain sign-in page instead of a value the server refuses", () => {
  // Same refusals as the server's RedirectPolicy::normalize (crates/erp-server/tests/redirect.rs).
  for (const unsafe of [
    "https://evil.test/odoo", "//erp.example.test/odoo", "/%2f%2fevil.test", "/%2F%2Fevil.test",
    "/odoo%0d%0aX:y", "/odoo%5cfoo", "/odoo?x=%", "/odoo?x=%zz", "/odoo\\foo", "/odoo bar",
    "/odoo\u0085", "/odoo%c2%85", "/odoo\u007f", "", "erp", "/odoo%ff",
    `/erp?x=${"é".repeat(1024)}`,
    // each rule on its own: decoded `//` only, `%2f%2f` substring only, fragment, tab/newline, prefix backslash
    "/%2fevil.test", "/a%2f%2Fb", "/erp#x", "/erp?a=1#x", "/erp\tx", "/erp\nx", "/erp%09x", "/\\evil.test", "/%5cevil.test",
    // dot segments resolve to a scheme-relative path (`/.//evil.com` -> `//evil.com`)
    "/.//evil.com/x", "/erp/..//evil.com", "/%2e//evil.com", "/erp/%2e%2e//evil.com", "/erp/.", "/erp/..",
  ]) {
    expect(localReturnPath(unsafe), JSON.stringify(unsafe)).toBeNull();
  }
  expect(loginReturnUrl({ pathname: "//evil.test/x", search: "" })).toBe("/erp/login");
  expect(loginReturnUrl({ pathname: "/odoo", search: "?x=%zz" })).toBe("/erp/login");
  expect(localReturnPath(`/erp?x=${"a".repeat(2041)}`)).toHaveLength(2048);
  // Dots inside a segment and in the query are ordinary characters.
  expect(localReturnPath("/erp/file.v1?q=..")).toBe("/erp/file.v1?q=..");
});
