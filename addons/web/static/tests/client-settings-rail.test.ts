/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
import { afterEach, expect, test } from "vitest";
import { readSettingsItems, readSettingsTarget, readMenuModule } from "../src/client/settings-items.js";
import { settingsView } from "../src/client/services.js";
import { RpcClient } from "../src/client/transport.js";
import type { SettingsRootMenu } from "../src/client/settings-items.js";
const item = { kind: "info", key: "notes.info", app: "notes", block: "General", label: "Visible", sequence: 0, value: "", detail: null };
afterEach(() => document.body.replaceChildren());
test.each([
  [{ ...item, value: null }, "Invalid setting item value."],
  [{ ...item, value: Infinity }, "Invalid setting item value."],
  [{ ...item, value: Number.MAX_SAFE_INTEGER + 1 }, "Invalid setting item value."],
  [{ ...item, sequence: -1 }, "Invalid setting item sequence."],
  [{ ...item, sequence: 0x1_0000_0000 }, "Invalid setting item sequence."],
  [{ ...item, app: "" }, "Invalid setting item app."],
  [{ ...item, label: "é".repeat(129) }, "Invalid setting item label."],
  [{ ...item, label: "Line\nBreak" }, "Invalid setting item label."],
  [{ ...item, detail: "é".repeat(513) }, "Invalid setting item detail."],
  [{ ...item, detail: "tab\t" }, "Invalid setting item detail."],
  [{ ...item, key: "wrong-key" }, "Invalid setting item key or app."],
])("invalid scalar or placement is rejected (%j)", (row, message) => {
  expect(() => readSettingsItems({ items: [row] }, [])).toThrowError(expect.objectContaining({ kind: "server", message }));
});
test("decoder accepts scalar boundaries and checks collisions against editable declarations", () => {
  for (const value of [false, 0, -1.5, Number.MAX_SAFE_INTEGER, "", "東京"]) expect(readSettingsItems({ items: [{ ...item, value, label: "é".repeat(128), detail: "é".repeat(512) }] }, [])[0]).toMatchObject({ value });
  expect(() => readSettingsItems({ items: [item] }, ["notes.info"])).toThrowError(expect.objectContaining({ kind: "server", message: "Duplicate settings item or declaration key: notes.info." }));
});
test.each([0, -1, 1.5, Number.MAX_SAFE_INTEGER + 1, "1"])("unsafe record target is refused (%j)", id => {
  expect(() => readSettingsTarget({ kind: "action", id: 7, res_id: id })).toThrowError(expect.objectContaining({ kind: "server", message: "Invalid setting target id." }));
});
test("menu module is required nullable provenance and cannot silently default", () => {
  expect(readMenuModule(null)).toBeNull(); expect(readMenuModule("notes")).toBe("notes");
  for (const raw of [undefined, 1, ""]) expect(() => readMenuModule(raw)).toThrowError(expect.objectContaining({ kind: "server", message: "Invalid menu module." }));
});
async function rail(roots: readonly SettingsRootMenu[]) {
  const rpc = new RpcClient(async (path, init) => {
    const args = JSON.parse(String(init?.body));
    return new Response(JSON.stringify(String(path).endsWith("erp_describe") ? { wire_major: 1, registry_version: "r1", scope: args.scope, declarations: [], items: [item] } : { wire_major: 1, scope: args.scope, revision: "r1", values: [] }), { headers: { "Content-Type": "application/json" } });
  }, () => {});
  const host = document.createElement("div"); document.body.append(host);
  await settingsView(host, rpc, [], job => { void job(); }, () => {}, {}, ["global"], "r1", undefined, undefined, null, { navigation: { roots, openTarget: async () => {} } });
  return host;
}
/// (partial: rail root identity and explicit fallback)
test("rail uses first matching root and preserves Unicode labels with the shell icon", async () => {
  const host = await rail([{ id: 1, module: null, label: "Notes", icon: null }, { id: 2, module: "notes", label: "東京 Notes", icon: "lucide:notebook-pen" }, { id: 3, module: "notes", label: "Later", icon: null }]);
  expect(host.querySelector(".settings-app-name")?.textContent).toBe("東京 Notes");
  expect(host.querySelector('.settings-app-icon [data-icon="notebook-pen"]')).not.toBeNull();
});
test("null ownership and label similarity do not replace the settings-only fallback", async () => {
  const host = await rail([{ id: 1, module: null, label: "Notes", icon: "lucide:settings" }, { id: 2, module: "other", label: "Notes", icon: null }]);
  expect(host.querySelector(".settings-app-name")?.textContent).toBe("Notes");
  expect(host.querySelector('.settings-app-icon[data-letter="N"]')).not.toBeNull();
  expect(host.querySelector(".settings-app-icon svg")).toBeNull();
});
