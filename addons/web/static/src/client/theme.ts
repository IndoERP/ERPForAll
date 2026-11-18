/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
import { array, object, text } from "./metadata.js";
import { ClientError, type RpcClient } from "./transport.js";

const stylesheetId = "database-appearance-stylesheet";
// `<addon>.appearance`: the settings registry binds a key's prefix to its installed owner addon.
const appearanceKey = /^([a-z][a-z0-9_]*)\.appearance$/;
// A selected value becomes the `data-office-theme` attribute that the owner's stylesheet scopes to.
const appearanceToken = /^[a-z][a-z0-9-]{0,63}$/;

async function loadThemeStylesheet(href: string): Promise<void> {
  const link = document.createElement("link");
  link.id = stylesheetId;
  link.rel = "stylesheet";
  // Tenant-published fixed static alias; the server resolves only the current admitted bundle.
  link.href = href;
  await new Promise<void>((resolve, reject) => {
    const fail = (): void => {
      clearTimeout(timer);
      link.remove();
      reject(new ClientError("server", "The database appearance stylesheet could not be loaded. Reload the application."));
    };
    const timer = setTimeout(fail, 15000);
    link.onload = () => { clearTimeout(timer); resolve(); };
    link.onerror = fail;
    document.head.append(link);
  });
}

/** Whether a database appearance is applied; every appearance styles the sidebar (office) shell. */
export function officeShell(): boolean {
  return document.documentElement.hasAttribute("data-office-theme");
}

interface Appearance { key: string; owner: string; choices: ReadonlySet<string> }

function appearanceOf(declaration: Record<string, unknown>): Appearance | undefined {
  const key = declaration["key"];
  const owner = typeof key === "string" ? appearanceKey.exec(key)?.[1] : undefined;
  if (owner === undefined || typeof key !== "string") return undefined;
  const invalid = (): ClientError => new ClientError("server", "Invalid database appearance declaration.");
  const raw = declaration["choices"];
  if (declaration["kind"] !== "selection" || !Array.isArray(raw)) throw invalid();
  const choices = new Set<string>();
  for (const pair of raw) {
    const value: unknown = Array.isArray(pair) ? pair[0] : undefined;
    if (typeof value !== "string" || (value !== "default" && !appearanceToken.test(value)) || choices.has(value)) throw invalid();
    choices.add(value);
  }
  if (!choices.has("default")) throw invalid();
  return { key, owner, choices };
}

/** Resolve only this database's installed appearance declarations. Never persist appearance in browser storage.
 * Each `<addon>.appearance` selection belongs to one installed addon whose static `src/theme.css` scopes its
 * palette to the selected value. No declaration is the default path; more than one selected value, transport
 * and corrupt values fail.
 */
export async function loadDatabaseTheme(rpc: RpcClient, registry: string, context: Record<string, unknown>, loadStylesheet: (href: string) => Promise<void> = loadThemeStylesheet): Promise<void> {
  document.documentElement.removeAttribute("data-office-theme");
  document.getElementById(stylesheetId)?.remove();
  const scope = { kind: "global" };
  const description = object(await rpc.call("ir.config.setting", "erp_describe", { scope, context }));
  if (description["wire_major"] !== 1 || description["registry_version"] !== registry) {
    throw new ClientError("unsupported", "Settings changed. Reload the application.");
  }
  const checkScope = (response: Record<string, unknown>): void => {
    const actual = object(response["scope"], "appearance scope");
    if (actual["kind"] !== "global" || Object.keys(actual).length !== 1) {
      throw new ClientError("server", "Invalid database appearance scope.");
    }
  };
  checkScope(description);
  const appearances = array(description["declarations"], "settings declarations")
    .map(value => appearanceOf(object(value, "setting declaration")))
    .filter((value): value is Appearance => value !== undefined);
  if (!appearances.length) return;
  const keys = appearances.map(item => item.key);
  if (new Set(keys).size !== keys.length) throw new ClientError("server", "Invalid database appearance declaration.");
  const response = object(await rpc.call("ir.config.setting", "erp_read_values", { scope, keys, context }));
  if (response["wire_major"] !== 1) throw new ClientError("unsupported", "Unsupported appearance response.");
  checkScope(response);
  const values = array(response["values"], "appearance values").map(value => object(value, "appearance value"));
  if (values.length !== appearances.length) throw new ClientError("server", "Invalid database appearance setting.");
  const selected = appearances.flatMap((item, index) => {
    const entry = values[index]!, value = entry["value"];
    if (entry["key"] !== item.key || typeof value !== "string" || !item.choices.has(value)) {
      throw new ClientError("server", "Invalid database appearance setting.");
    }
    return value === "default" ? [] : [{ owner: item.owner, value }];
  });
  if (selected.length > 1) {
    throw new ClientError("server", "More than one database appearance is selected. Choose one in Settings.");
  }
  const chosen = selected[0];
  if (!chosen) return;
  // Explicit selection wins over device preference; the owner's stylesheet supplies all tokens.
  await loadStylesheet(`/${chosen.owner}/static/src/theme.css`);
  document.documentElement.dataset["officeTheme"] = text(chosen.value, "appearance");
}

/**
 *  attribute); `light` and `dark` set the explicit `data-theme` that client.css already honours.
 *  `false` is a row that holds no value (also `system`). Any other value is a server fault and fails loudly. 
 */
export function applyColorScheme(value: unknown): void {
  if (value === "system" || value === false) delete document.documentElement.dataset["theme"];
  else if (value === "light" || value === "dark") document.documentElement.dataset["theme"] = value;
  else throw new ClientError("server", "Invalid colour scheme.");
}

/** Read the signed-in user's own `res.users.color_scheme` and apply it. */
export async function loadUserColorScheme(rpc: RpcClient, uid: number, context: Record<string, unknown>): Promise<void> {
  const rows = array(await rpc.call("res.users", "read", { ids: [uid], fields: ["color_scheme"], context }), "user rows");
  if (rows.length !== 1) throw new ClientError("server", "Invalid colour scheme response.");
  applyColorScheme(object(rows[0], "user row")["color_scheme"]);
}
