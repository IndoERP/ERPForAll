/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
import { array, object, text } from "./metadata.js";
import { ClientError, type RpcClient } from "./transport.js";

const stylesheetId = "database-appearance-stylesheet";

async function loadThemeStylesheet(): Promise<void> {
  const link = document.createElement("link");
  link.id = stylesheetId;
  link.rel = "stylesheet";
  // Tenant-published fixed static alias; the server resolves only the current admitted bundle.
  link.href = "/theme_office_blue/static/src/theme.css";
  await new Promise<void>((resolve, reject) => {
    const fail = (): void => {
      clearTimeout(timer);
      link.remove();
      reject(new ClientError("server", "Office Blue stylesheet could not be loaded. Reload the application."));
    };
    const timer = setTimeout(fail, 15000);
    link.onload = () => { clearTimeout(timer); resolve(); };
    link.onerror = fail;
    document.head.append(link);
  });
}

const appearanceKey = "theme_office_blue.appearance";

/** Resolve only this database's installed declaration. Never persist appearance in browser storage.
 * An absent declaration is the specified uninstall/default path; transport and corrupt values fail.
 */
export async function loadDatabaseTheme(rpc: RpcClient, registry: string, context: Record<string, unknown>, loadStylesheet: () => Promise<void> = loadThemeStylesheet): Promise<void> {
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
  const matches = array(description["declarations"], "settings declarations")
    .map(value => object(value, "setting declaration")).filter(value => value["key"] === appearanceKey);
  if (!matches.length) return;
  if (matches.length !== 1 || matches[0]?.["kind"] !== "selection") {
    throw new ClientError("server", "Invalid database appearance declaration.");
  }
  const response = object(await rpc.call("ir.config.setting", "erp_read_values", { scope, keys: [appearanceKey], context }));
  if (response["wire_major"] !== 1) throw new ClientError("unsupported", "Unsupported appearance response.");
  checkScope(response);
  const values = array(response["values"], "appearance values");
  if (values.length !== 1) throw new ClientError("server", "Invalid database appearance setting.");
  const entry = object(values[0], "appearance value"), value = entry["value"];
  if (entry["key"] !== appearanceKey || (value !== "default" && value !== "office-blue")) {
    throw new ClientError("server", "Invalid database appearance setting.");
  }
  // Explicit selection wins over device preference; the addon stylesheet supplies all tokens.
  if (value === "office-blue") {
    await loadStylesheet();
    document.documentElement.dataset["officeTheme"] = text(value, "appearance");
  }
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
