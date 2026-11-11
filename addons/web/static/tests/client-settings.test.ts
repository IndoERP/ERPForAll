/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
import { afterEach, expect, test, vi } from "vitest";
import { settingsView } from "../src/client/services.js";
import { ClientError, RpcClient } from "../src/client/transport.js";
afterEach(() => document.body.replaceChildren());
function deferred() { let resolve!: () => void; const promise = new Promise<void>(done => { resolve = done; }); return { promise, resolve }; }
test("settings save isolates in-flight edits and retains the observed CAS revision", async () => {
  const gate = deferred(), calls: Record<string, unknown>[] = [];
  const rpc = new RpcClient(async (path, init) => {
    const args = JSON.parse(String(init?.body)) as Record<string, unknown>; calls.push(args);
    if (String(path).endsWith("erp_write_values")) await gate.promise;
    const response = String(path).endsWith("erp_describe") ? { wire_major: 1, registry_version: "r1", scope: args["scope"], declarations: [{ key: "notes.enabled", label: "Enabled", kind: "boolean", default: false, choices: null, writable: true }] }
      : { wire_major: 1, scope: args["scope"], revision: "opaque-r1", values: [{ key: "notes.enabled", value: false, source: "stored" }] };
    return new Response(JSON.stringify(response), { headers: { "Content-Type": "application/json" } });
  }, () => {});
  const host = document.createElement("div"); document.body.append(host);
  const controller = await settingsView(host, rpc, [{ id: 1, name: "One" }], job => { void job(); }, () => {}, {}, ["global", "company"]);
  const input = host.querySelector<HTMLInputElement>('input[type="checkbox"]')!;
  input.checked = true; input.dispatchEvent(new Event("change"));
  const saving = controller.save();
  await vi.waitFor(() => expect(calls.some(call => "expected_revision" in call)).toBe(true));
  expect(input.disabled).toBe(true); expect(host.querySelector("select")?.disabled).toBe(true);
  expect(calls.find(call => "expected_revision" in call)).toMatchObject({ expected_revision: "opaque-r1", changes: [{ key: "notes.enabled", value: true }] });
  gate.resolve(); await saving;
});
test("settings refuse values returned for a different company scope", async () => {
  const rpc = new RpcClient(async path => new Response(JSON.stringify(String(path).endsWith("erp_describe")
    ? { wire_major: 1, registry_version: "r1", scope: { kind: "company", company_id: 1 }, declarations: [{ key: "notes.enabled", label: "Enabled", kind: "boolean", default: false, choices: null, writable: true }] }
    : { wire_major: 1, scope: { kind: "company", company_id: 2 }, revision: "other", values: [{ key: "notes.enabled", value: true, source: "stored" }] }), { headers: { "Content-Type": "application/json" } }), () => {});
  const host = document.createElement("div"); document.body.append(host);
  await expect(settingsView(host, rpc, [{ id: 1, name: "One" }], job => { void job(); }, () => {}, {}, ["company"])).rejects.toThrow("Settings response scope does not match the request.");
});

test("malformed successful settings write cannot be replayed", async () => {
  let writes = 0;
  const rpc = new RpcClient(async (path, init) => {
    const args = JSON.parse(String(init?.body)) as Record<string, unknown>;
    const value = String(path).endsWith("erp_write_values") ? (++writes, null)
      : String(path).endsWith("erp_describe") ? { wire_major: 1, registry_version: "r1", scope: args["scope"], declarations: [{ key: "notes.enabled", label: "Enabled", kind: "boolean", default: false, choices: null, writable: true }] }
      : { wire_major: 1, scope: args["scope"], revision: "r1", values: [{ key: "notes.enabled", value: false, source: "stored" }] };
    return new Response(JSON.stringify(value), { headers: { "Content-Type": "application/json" } });
  }, () => {});
  const host = document.createElement("div"); document.body.append(host);
  const controller = await settingsView(host, rpc, [], job => { void job(); }, () => {}, {}, ["global"]);
  const input = host.querySelector<HTMLInputElement>("input")!; input.checked = true; input.dispatchEvent(new Event("change"));
  await expect(controller.save()).rejects.toMatchObject({ kind: "ambiguous" });
  await expect(controller.save()).rejects.toMatchObject({ kind: "ambiguous" });
  expect(writes).toBe(1); expect(controller.dirty()).toBe(true);
});

test.each([false, true])("clean Settings saves issue no write (declarations=%s)", async populated => {
  let writes = 0;
  const rpc = new RpcClient(async (path, init) => {
    const args = JSON.parse(String(init?.body)) as Record<string, unknown>;
    if (String(path).endsWith("erp_write_values")) writes++;
    const response = String(path).endsWith("erp_describe")
      ? { wire_major: 1, registry_version: "r1", scope: args["scope"], declarations: populated ? [{ key: "notes.enabled", label: "Enabled", kind: "boolean", default: false, choices: null, writable: true }] : [] }
      : { wire_major: 1, scope: args["scope"], revision: "0", values: populated ? [{ key: "notes.enabled", value: false, source: "default" }] : [] };
    return new Response(JSON.stringify(response), { headers: { "Content-Type": "application/json" } });
  }, () => {});
  const host = document.createElement("div"); document.body.append(host);
  const controller = await settingsView(host, rpc, [], job => { void job(); }, () => {}, {}, ["global"]);
  if (populated) {
    const input = host.querySelector<HTMLInputElement>("input")!;
    input.checked = true; input.dispatchEvent(new Event("change"));
    input.checked = false; input.dispatchEvent(new Event("change"));
  }
  expect(controller.dirty()).toBe(false);
  await controller.save(); await controller.save();
  expect(writes).toBe(0);
});

test("settings page renders left sidebar with installed apps, titled sections, and aligned setting blocks", async () => {
  const declarations = [
    { key: "notes.enabled", label: "Enable shared notes", kind: "boolean", default: false, choices: null, writable: true, help: "Allow team members to collaborate on notes" },
    { key: "notes.max_items", label: "Max items per page", kind: "integer", default: 25, choices: null, writable: true, help: "Number of notes to display per page" },
    { key: "mail.server", label: "Outgoing mail host", kind: "string", default: "smtp.example.com", choices: null, writable: true },
    { key: "mail.encryption", label: "Encryption", kind: "selection", default: "tls", choices: [["none", "None"], ["tls", "STARTTLS"], ["ssl", "SSL/TLS"]], writable: true }
  ];
  const values = [
    { key: "notes.enabled", value: true, source: "stored" },
    { key: "notes.max_items", value: 50, source: "stored" },
    { key: "mail.server", value: "smtp.example.com", source: "default" },
    { key: "mail.encryption", value: "tls", source: "default" }
  ];
  const rpc = new RpcClient(async (path, init) => {
    const args = JSON.parse(String(init?.body)) as Record<string, unknown>;
    const response = String(path).endsWith("erp_describe")
      ? { wire_major: 1, registry_version: "r1", scope: args["scope"], declarations }
      : { wire_major: 1, scope: args["scope"], revision: "r1", values };
    return new Response(JSON.stringify(response), { headers: { "Content-Type": "application/json" } });
  }, () => {});

  const host = document.createElement("div");
  const panel = document.createElement("div");
  const controls = document.createElement("div");
  panel.innerHTML = '<div data-control-search></div>';
  controls.setAttribute("data-control-actions", "");
  document.body.append(panel, controls, host);

  await settingsView(host, rpc, [{ id: 1, name: "Company A" }], job => { void job(); }, () => {}, {}, ["global", "company"], undefined, controls, panel);

  // Control panel integration
  expect(controls.querySelector(".settings-toolbar")).not.toBeNull();
  expect(controls.querySelector("select")).not.toBeNull();
  const saveBtn = [...controls.querySelectorAll("button")].find(b => b.textContent === "Save");
  const discardBtn = [...controls.querySelectorAll("button")].find(b => b.textContent === "Discard");
  expect(saveBtn).toBeDefined();
  expect(discardBtn).toBeDefined();
  expect(panel.querySelector(".settings-search-box input")).not.toBeNull();

  // Sidebar app tabs
  const sidebar = host.querySelector(".settings-sidebar")!;
  expect(sidebar).not.toBeNull();
  const tabs = [...sidebar.querySelectorAll<HTMLButtonElement>(".settings-app-tab")];
  expect(tabs.map(t => t.textContent)).toEqual(["Notes", "Mail"]);
  expect(tabs[0]!.classList.contains("active")).toBe(true);

  // App sections
  const sections = [...host.querySelectorAll<HTMLElement>(".settings-section")];
  expect(sections).toHaveLength(2);
  expect(sections[0]!.getAttribute("data-app-section")).toBe("notes");
  expect(sections[1]!.getAttribute("data-app-section")).toBe("mail");

  // Setting blocks
  const booleanBlock = host.querySelector<HTMLElement>('[data-setting-key="notes.enabled"]')!;
  expect(booleanBlock).not.toBeNull();
  const settingLabel = booleanBlock.querySelector<HTMLLabelElement>("label.setting-label-text")!;
  const checkbox = booleanBlock.querySelector<HTMLInputElement>("input[type=checkbox]")!;
  expect(settingLabel.control).toBe(checkbox);
  const beforeClick = checkbox.checked;
  settingLabel.click();
  expect(checkbox.checked).toBe(!beforeClick);
  expect(booleanBlock.querySelector(".setting-left-pane input[type=checkbox]")).not.toBeNull();
  expect(booleanBlock.querySelector(".setting-right-pane .setting-label-text")?.textContent).toBe("Enable shared notes");
  expect(booleanBlock.querySelector(".setting-right-pane .setting-help")?.textContent).toBe("Allow team members to collaborate on notes");

  const intBlock = host.querySelector<HTMLElement>('[data-setting-key="notes.max_items"]')!;
  expect(intBlock.querySelector('.setting-right-pane input[type="number"]')).not.toBeNull();

  const selectBlock = host.querySelector<HTMLElement>('[data-setting-key="mail.encryption"]')!;
  expect(selectBlock.querySelector(".setting-right-pane select")).not.toBeNull();

  // Tab switching
  tabs[1]!.click();
  expect(tabs[0]!.classList.contains("active")).toBe(false);
  expect(tabs[1]!.classList.contains("active")).toBe(true);
});

test("settings search filters blocks and highlights matching text in DOM safely", async () => {
  const declarations = [
    { key: "notes.enabled", label: "Enable shared notes", kind: "boolean", default: false, choices: null, writable: true, help: "Allow team members to collaborate on notes" },
    { key: "notes.max_items", label: "Max items per page", kind: "integer", default: 25, choices: null, writable: true, help: "Number of notes per batch" },
    { key: "mail.server", label: "Outgoing mail host", kind: "string", default: "smtp.example.com", choices: null, writable: true, help: "SMTP server address" }
  ];
  const values = [
    { key: "notes.enabled", value: true, source: "stored" },
    { key: "notes.max_items", value: 25, source: "default" },
    { key: "mail.server", value: "smtp.example.com", source: "default" }
  ];
  const rpc = new RpcClient(async (path, init) => {
    const args = JSON.parse(String(init?.body)) as Record<string, unknown>;
    const response = String(path).endsWith("erp_describe")
      ? { wire_major: 1, registry_version: "r1", scope: args["scope"], declarations }
      : { wire_major: 1, scope: args["scope"], revision: "r1", values };
    return new Response(JSON.stringify(response), { headers: { "Content-Type": "application/json" } });
  }, () => {});

  const host = document.createElement("div");
  const panel = document.createElement("div");
  panel.innerHTML = '<div data-control-search></div>';
  document.body.append(panel, host);

  await settingsView(host, rpc, [], job => { void job(); }, () => {}, {}, ["global"], undefined, undefined, panel);

  const searchInput = panel.querySelector<HTMLInputElement>(".settings-search-box input")!;
  expect(searchInput).not.toBeNull();

  // Filter for "SMTP"
  searchInput.value = "SMTP";
  searchInput.dispatchEvent(new Event("input"));

  const notesSection = host.querySelector<HTMLElement>('[data-app-section="notes"]')!;
  const mailSection = host.querySelector<HTMLElement>('[data-app-section="mail"]')!;
  expect(notesSection.style.display).toBe("none");
  expect(mailSection.style.display).toBe("");

  const mailBlock = host.querySelector<HTMLElement>('[data-setting-key="mail.server"]')!;
  expect(mailBlock.style.display).toBe("");
  // Check highlight mark
  const highlight = mailBlock.querySelector("mark.highlight");
  expect(highlight).not.toBeNull();
  expect(highlight?.textContent?.toLowerCase()).toBe("smtp");

  // Search non-existent
  searchInput.value = "xyz_no_match";
  searchInput.dispatchEvent(new Event("input"));
  expect(notesSection.style.display).toBe("none");
  expect(mailSection.style.display).toBe("none");
  const noResults = host.querySelector<HTMLElement>(".settings-no-results")!;
  expect(noResults.style.display).not.toBe("none");

  // Clear search: back to the chosen app only (one app at a time).
  searchInput.value = "";
  searchInput.dispatchEvent(new Event("input"));
  expect(notesSection.style.display).toBe("");
  expect(mailSection.style.display).toBe("none");
  expect(noResults.style.display).toBe("none");
  expect(host.querySelector("mark.highlight")).toBeNull();
});

test("search highlights hostile label, help and markup queries as text only", async () => {
  const hostile = "<img src=x onerror=alert(1)>";
  const declarations = [{ key: "mail.server", label: `Host ${hostile}`, kind: "string", default: "a", choices: null, writable: true, help: `Help ${hostile} end` }];
  const rpc = new RpcClient(async (path, init) => {
    const args = JSON.parse(String(init?.body)) as Record<string, unknown>;
    const response = String(path).endsWith("erp_describe")
      ? { wire_major: 1, registry_version: "r1", scope: args["scope"], declarations }
      : { wire_major: 1, scope: args["scope"], revision: "r1", values: [{ key: "mail.server", value: "a", source: "default" }] };
    return new Response(JSON.stringify(response), { headers: { "Content-Type": "application/json" } });
  }, () => {});
  const host = document.createElement("div"), panel = document.createElement("div");
  panel.innerHTML = "<div data-control-search></div>";
  document.body.append(panel, host);
  await settingsView(host, rpc, [], job => { void job(); }, () => {}, {}, ["global"], undefined, undefined, panel);
  const input = panel.querySelector<HTMLInputElement>(".settings-search-box input")!;
  expect(host.querySelector("img")).toBeNull();
  input.value = "<img"; input.dispatchEvent(new Event("input"));
  const block = host.querySelector<HTMLElement>('[data-setting-key="mail.server"]')!;
  expect(document.querySelector("img")).toBeNull();
  expect(block.textContent).toContain(`Host ${hostile}`);
  expect(block.textContent).toContain(`Help ${hostile} end`);
  const marks = [...block.querySelectorAll("mark")];
  expect(marks.length).toBeGreaterThanOrEqual(2);
  expect(marks.every(mark => mark.textContent === "<img")).toBe(true);
});

test("save clears an active search filter so an invalid input is visible, and Escape clears the filter", async () => {
  const declarations = [
    { key: "notes.max_items", label: "Max items", kind: "integer", default: 25, choices: null, writable: true },
    { key: "mail.server", label: "Outgoing host", kind: "string", default: "a", choices: null, writable: true },
  ];
  const rpc = new RpcClient(async (path, init) => {
    const args = JSON.parse(String(init?.body)) as Record<string, unknown>;
    const response = String(path).endsWith("erp_describe")
      ? { wire_major: 1, registry_version: "r1", scope: args["scope"], declarations }
      : { wire_major: 1, scope: args["scope"], revision: "r1", values: [{ key: "notes.max_items", value: 25, source: "default" }, { key: "mail.server", value: "a", source: "default" }] };
    return new Response(JSON.stringify(response), { headers: { "Content-Type": "application/json" } });
  }, () => {});
  const host = document.createElement("div"), panel = document.createElement("div");
  panel.innerHTML = "<div data-control-search></div>";
  document.body.append(panel, host);
  const controller = await settingsView(host, rpc, [], job => { void job(); }, () => {}, {}, ["global"], undefined, undefined, panel);
  const search = panel.querySelector<HTMLInputElement>(".settings-search-box input")!;
  const notes = host.querySelector<HTMLElement>('[data-app-section="notes"]')!;
  const mail = host.querySelector<HTMLElement>('[data-app-section="mail"]')!;
  const number = host.querySelector<HTMLInputElement>('[data-setting-key="notes.max_items"] input')!;
  number.required = true; number.value = ""; number.dispatchEvent(new Event("input")); number.dispatchEvent(new Event("change"));
  search.value = "outgoing"; search.dispatchEvent(new Event("input"));
  expect(notes.style.display).toBe("none");
  const error = await controller.save().then(() => undefined, (caught: unknown) => caught);
  expect(error).toBeInstanceOf(ClientError);
  expect((error as ClientError).kind).toBe("validation");
  expect((error as ClientError).message).toBe("Correct the invalid settings before saving.");
  expect(search.value).toBe("");
  expect(notes.style.display).toBe("");
  expect(mail.style.display).toBe("none");
  // The invalid setting's app opens even when another app was chosen (one app at a time).
  host.querySelector<HTMLButtonElement>('.settings-app-tab[data-app="mail"]')!.click();
  expect(notes.style.display).toBe("none");
  await expect(controller.save()).rejects.toMatchObject({ kind: "validation" });
  expect(notes.style.display).toBe(""); expect(mail.style.display).toBe("none");
  expect(host.querySelector(".settings-app-tab.active")?.getAttribute("data-app")).toBe("notes");
  search.value = "outgoing"; search.dispatchEvent(new Event("input"));
  expect(notes.style.display).toBe("none");
  search.dispatchEvent(new KeyboardEvent("keydown", { key: "Escape" }));
  expect(search.value).toBe(""); expect(notes.style.display).toBe("");
});

function settingsRpc(declarations: Record<string, unknown>[]) {
  return new RpcClient(async (path, init) => {
    const args = JSON.parse(String(init?.body)) as Record<string, unknown>;
    const response = String(path).endsWith("erp_describe")
      ? { wire_major: 1, registry_version: "r1", scope: args["scope"], declarations }
      : { wire_major: 1, scope: args["scope"], revision: "r1", values: declarations.map(d => ({ key: d["key"], value: d["default"], source: "default" })) };
    return new Response(JSON.stringify(response), { headers: { "Content-Type": "application/json" } });
  }, () => {});
}
const twoApps = [
  { key: "notes.enabled", label: "Enable shared notes", kind: "boolean", default: false, choices: null, writable: true },
  { key: "mail_server.host", label: "Outgoing mail host", kind: "string", default: "smtp.example.com", choices: null, writable: true }
];

test("settings show one app at a time, each app with its addon tile, and a search reaches every app", async () => {
  const host = document.createElement("div"), panel = document.createElement("div");
  panel.innerHTML = '<div data-control-search></div>';
  document.body.append(panel, host);
  await settingsView(host, settingsRpc(twoApps), [], job => { void job(); }, () => {}, {}, ["global"], undefined, undefined, panel);
  const section = (app: string) => host.querySelector<HTMLElement>(`[data-app-section="${app}"]`)!;
  const tabs = [...host.querySelectorAll<HTMLButtonElement>(".settings-app-tab")];
  // The first app opens; the other app's settings stay out of the way until it is chosen.
  expect([section("notes").style.display, section("mail_server").style.display]).toEqual(["", "none"]);
  expect(tabs.map(tab => tab.getAttribute("aria-selected"))).toEqual(["true", "false"]);
  // Every app carries its addon tile (initial on a tone) in the sidebar and in its heading.
  expect(tabs.map(tab => tab.querySelector<HTMLElement>(".settings-app-icon")?.dataset["letter"])).toEqual(["N", "M"]);
  expect(section("mail_server").querySelector(".settings-section-title .settings-app-icon")).not.toBeNull();
  expect(tabs.map(tab => tab.textContent)).toEqual(["Notes", "Mail Server"]);
  tabs[1]!.click();
  expect([section("notes").style.display, section("mail_server").style.display]).toEqual(["none", ""]);
  expect(tabs.map(tab => tab.getAttribute("aria-selected"))).toEqual(["false", "true"]);
  // A search shows matches from every app; clearing it returns to the chosen app.
  const search = panel.querySelector<HTMLInputElement>(".settings-search-box input")!;
  search.value = "e"; search.dispatchEvent(new Event("input"));
  expect([section("notes").style.display, section("mail_server").style.display]).toEqual(["", ""]);
  search.value = ""; search.dispatchEvent(new Event("input"));
  expect([section("notes").style.display, section("mail_server").style.display]).toEqual(["none", ""]);
});

test("an addon's own settings menu opens on its app, and an app without settings is refused", async () => {
  const host = document.createElement("div"); document.body.append(host);
  await settingsView(host, settingsRpc(twoApps), [], job => { void job(); }, () => {}, {}, ["global"], undefined, undefined, undefined, "mail_server");
  expect(host.querySelector<HTMLElement>('[data-app-section="mail_server"]')!.style.display).toBe("");
  expect(host.querySelector<HTMLElement>('[data-app-section="notes"]')!.style.display).toBe("none");
  expect(host.querySelector(".settings-app-tab.active")?.getAttribute("data-app")).toBe("mail_server");
  const other = document.createElement("div"); document.body.append(other);
  await expect(settingsView(other, settingsRpc(twoApps), [], job => { void job(); }, () => {}, {}, ["global"], undefined, undefined, undefined, "sale"))
    .rejects.toThrow("The addon 'sale' declares no settings for this page.");
});
