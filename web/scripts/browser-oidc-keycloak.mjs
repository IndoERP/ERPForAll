/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
// Run against a disposable loopback ERP and Keycloak realm. Pass the fixture JSON on stdin.
// The realm needs a confidential ERP client, an enabled user, and the exact ERP callback.
import assert from "node:assert/strict";
import { createHash, randomUUID, X509Certificate } from "node:crypto";
import { readFile } from "node:fs/promises";
import { request } from "node:https";
import { join } from "node:path";
import { chromium } from "playwright";

let raw = "";
for await (const part of process.stdin) raw += part;
const cfg = JSON.parse(raw);
const erp = new URL(cfg.origin), issuer = new URL(cfg.issuer);
assert(["127.0.0.1", "localhost"].includes(erp.hostname) && ["127.0.0.1", "localhost"].includes(issuer.hostname));
assert.equal(issuer.protocol, "https:");
const realm = issuer.pathname.match(/^\/realms\/([A-Za-z0-9_-]+)$/)?.[1];
assert(realm && realm !== "master", "Use a separate test realm");
const ca = await readFile(cfg.caFile);
const server = new X509Certificate(await readFile(cfg.serverCertFile));
const spki = createHash("sha256").update(server.publicKey.export({ type: "spki", format: "der" })).digest("base64");
const adminPath = "/admin/realms/" + realm;
async function keycloak(method, path, options = {}) {
  const body = options.form ? new URLSearchParams(options.form).toString()
    : options.json ? JSON.stringify(options.json) : undefined;
  return new Promise((resolve, reject) => {
    const req = request(new URL(path, issuer.origin), {
      ca, method, headers: {
        ...(options.token ? { Authorization: "Bearer " + options.token } : {}),
        ...(body ? { "Content-Type": options.form ? "application/x-www-form-urlencoded" : "application/json" } : {}),
      },
    }, async res => {
      try {
        const parts = [];
        for await (const chunk of res) parts.push(chunk);
        if (res.statusCode < 200 || res.statusCode >= 300) throw new Error("Keycloak administration returned HTTP " + res.statusCode);
        const text = Buffer.concat(parts).toString();
        resolve({ value: text ? JSON.parse(text) : null, location: res.headers.location });
      } catch (err) { reject(err); }
    });
    req.on("error", reject);
    req.end(body);
  });
}
async function adminToken() {
  const { value } = await keycloak("POST", "/realms/master/protocol/openid-connect/token", {
    form: { grant_type: "password", client_id: "admin-cli", username: cfg.adminLogin, password: cfg.adminPassword },
  });
  assert(value.access_token);
  return value.access_token;
}
async function activeKey(token) {
  const { value } = await keycloak("GET", adminPath + "/keys", { token });
  assert.equal(typeof value.active.RS256, "string");
  return value.active.RS256;
}
const browser = await chromium.launch({
  executablePath: process.env.CHROME_BIN ?? (process.platform === "darwin"
    ? "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome" : "/usr/bin/google-chrome"),
  args: ["--ignore-certificate-errors-spki-list=" + spki],
});
let phase = "setup", generatedKey, operator, restoreProvider = false;
const pass = label => { console.log("PASS " + label); };
async function page() {
  const context = await browser.newContext();
  context.setDefaultTimeout(20000);
  return context.newPage();
}
async function passwordLogin(tab) {
  await tab.goto(erp.origin + "/erp/login");
  await tab.locator('input[name="login"]').fill(cfg.erpLogin);
  await tab.locator('input[name="password"]').fill(cfg.erpPassword);
  await tab.getByRole("button", { name: "Sign in", exact: true }).click();
  await tab.locator(".user-button").waitFor();
}
const uid = tab => tab.locator("#erp-bootstrap").evaluate(node => JSON.parse(node.textContent).identity.uid);
async function external(tab, link = false, denied = false) {
  await tab.goto(erp.origin + "/erp/login");
  const auth = tab.waitForRequest(req => {
    const u = new URL(req.url());
    return u.origin === issuer.origin && u.pathname === issuer.pathname + "/protocol/openid-connect/auth";
  });
  await tab.getByRole("button", { name: (link ? "Connect " : "Sign in with ") + cfg.label, exact: true }).click();
  const query = new URL((await auth).url()).searchParams;
  assert.equal(query.get("client_id"), cfg.clientId);
  assert.equal(query.get("redirect_uri"), erp.origin + "/erp/login/external/callback");
  assert.equal(query.get("response_type"), "code");
  assert.equal(query.get("code_challenge_method"), "S256");
  assert(/^[A-Za-z0-9_-]{43}$/.test(query.get("code_challenge")));
  assert(query.get("nonce") && query.get("state"));
  await tab.locator('#username').fill(cfg.externalLogin);
  await tab.locator('#password').fill(cfg.externalPassword);
  const callback = tab.waitForResponse(response => {
    const url = new URL(response.url());
    return url.origin === erp.origin && url.pathname === "/erp/login/external/callback";
  });
  await tab.locator('#kc-login').click();
  const response = await callback;
  if (response.status() !== 303) throw new Error("ERP callback returned HTTP " + response.status());
  const refused = new URL(response.headers().location, erp.origin).searchParams.has("external_error");
  if (denied !== refused) throw new Error(denied ? "Disconnected account was accepted" : "External authorization was refused");
  if (denied) await tab.waitForURL(u => u.origin === erp.origin && u.searchParams.has("external_error"));
  else await tab.locator(".user-button").waitFor();
}
async function operatorScreen(tab) {
  const reply = await tab.request.post(erp.origin + "/erp/json/2/ir.ui.menu/erp_load_web_menus", {
    data: {}, headers: { Origin: erp.origin, "Sec-Fetch-Site": "same-origin" },
  });
  assert.equal(reply.status(), 200);
  const action = (await reply.json()).nodes.find(item => item.label === "Single Sign-On" && item.action_kind === "client");
  assert(action, "Single Sign-On settings menu is missing");
  await tab.goto(erp.origin + "/erp/action-" + action.action_id + "?menu=" + action.id);
  await tab.getByRole("button", { name: "Refresh status" }).waitFor();
}
async function revoked(tab) {
  await tab.goto(erp.origin + "/erp");
  await tab.getByRole("button", { name: "Sign in", exact: true }).waitFor();
}
try {
  phase = "native operator session";
  operator = await page();
  await passwordLogin(operator);
  const expectedUid = await uid(operator);
  phase = "account link and PKCE";
  const linked = await page();
  await passwordLogin(linked);
  await external(linked, true);
  assert.equal(await uid(linked), expectedUid);
  pass("account link by fresh native and Keycloak sign-in, S256, state and nonce");
  phase = "external sign-in";
  const initial = await page();
  await external(initial);
  assert.equal(await uid(initial), expectedUid);
  pass("linked account opened a new ERP session through Keycloak");

  phase = "operator diagnostics";
  await operatorScreen(operator);
  await operator.getByText(issuer.href, { exact: true }).waitFor();
  await operator.getByText("HTTPS key set", { exact: true }).waitFor();
  await operator.getByText("Ready", { exact: true }).waitFor();
  const cached = operator.getByText("Cached signing keys", { exact: true }).locator("xpath=following-sibling::dd[1]");
  const before = Number(await cached.innerText());
  assert(before > 0);
  phase = "key rotation";
  const token = await adminToken();
  const previousKey = await activeKey(token);
  const { value: realmInfo } = await keycloak("GET", adminPath, { token });
  const { location } = await keycloak("POST", adminPath + "/components", { token, json: {
    name: "erp-acceptance-" + randomUUID(), providerType: "org.keycloak.keys.KeyProvider", providerId: "rsa-generated",
    parentId: realmInfo.id, config: { priority: ["9999"], algorithm: ["RS256"], keySize: ["2048"], active: ["true"], enabled: ["true"] },
  }});
  generatedKey = location?.split("/").at(-1);
  assert(generatedKey && /^[a-zA-Z0-9-]+$/.test(generatedKey), "Keycloak did not identify its new signing key component");
  const newKey = await activeKey(token);
  assert.notEqual(newKey, previousKey);
  const published = (await keycloak("GET", issuer.pathname + "/protocol/openid-connect/certs")).value.keys;
  const signing = published.filter(key => key.use === "sig" && ["RS256", "EdDSA"].includes(key.alg));
  assert(signing.some(key => key.kid === newKey) && signing.some(key => key.kid === previousKey));
  const rotated = await page();
  await external(rotated);
  assert.equal(await uid(rotated), expectedUid);
  await operator.getByRole("button", { name: "Refresh status" }).click();
  await operator.waitForFunction(count => {
    const item = Array.from(document.querySelectorAll("dt")).find(e => e.textContent === "Cached signing keys");
    return Number(item?.nextElementSibling?.textContent) === count;
  }, signing.length);
  pass("Keycloak selected a new RS256 key and ERP refreshed JWKS without a restart");
  if (process.env.OIDC_OUTPUT) await operator.screenshot({ path: join(process.env.OIDC_OUTPUT, "oidc-provider.png") });
  phase = "provider disable and native fallback";
  await operator.getByRole("button", { name: "Disable provider", exact: true }).click();
  restoreProvider = true;
  await operator.locator("dialog[open]").getByRole("button", { name: "Disable provider" }).click();
  await operator.getByRole("button", { name: "Enable provider" }).waitFor();
  await revoked(rotated);
  await revoked(initial);
  const native = await page();
  await passwordLogin(native);
  assert.equal(await uid(native), expectedUid);
  pass("disabling the provider revoked issued sessions while native sign-in succeeded");
  phase = "account disconnect";
  await operator.getByRole("button", { name: "Enable provider" }).click();
  await operator.getByRole("button", { name: "Disable provider" }).waitFor();
  restoreProvider = false;
  const restored = await page();
  await external(restored);
  await native.goto(erp.origin + "/erp/login");
  await native.getByRole("button", { name: "Disconnect " + cfg.label }).click();
  await revoked(restored);
  await external(await page(), false, true);
  pass("disconnect revoked the linked session and prevented fresh external sign-in");
} catch (e) {
  // Login requests, redirects and locator diagnostics can include short-lived authentication material.
  const safe = /^(?:(ERP callback|Keycloak administration) returned HTTP \d{3}|Disconnected account was accepted|External authorization was refused)$/.test(e?.message);
  const status = safe ? ": " + e.message : "";
  console.error("FAIL " + phase + " (" + (e?.name ?? "Error") + ")" + status);
  process.exitCode = 1;
} finally {
  try {
    if (restoreProvider) {
      await operatorScreen(operator);
      const enable = operator.getByRole("button", { name: "Enable provider" });
      if (await enable.isVisible()) {
        await enable.click();
        await operator.getByRole("button", { name: "Disable provider" }).waitFor();
      }
    }
  } catch {
    console.error("Could not restore the fixture's provider policy");
    process.exitCode = 1;
  } finally {
    await browser.close();
  }
  if (generatedKey) await keycloak("DELETE", adminPath + "/components/" + generatedKey, { token: await adminToken() });
}
