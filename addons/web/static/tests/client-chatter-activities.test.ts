/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
import { afterEach, beforeEach, expect, test } from "vitest";
import { RpcClient } from "../src/client/transport.js";
import { Activities, defaultDeadline, relativeDay } from "../src/client/chatter-activities.js";

const json = (value: unknown): Response => new Response(JSON.stringify(value), { headers: { "Content-Type": "application/json" } });
const settle = (): Promise<void> => new Promise((resolve) => setTimeout(resolve, 20));
const NOW = new Date(2026, 10, 11, 9, 0, 0);

interface Call { readonly path: string; readonly body: Record<string, any> }
const row = (id: number, summary: string, state: string, deadline: string, can = true): Record<string, unknown> => ({
  id, summary, note: "", date_deadline: deadline, state, can_write: can,
  user_id: { id: 2, display_name: "Alice Adams" }, activity_type_id: { id: 4, display_name: "Call", icon: "phone" },
});
const TYPES = { length: 2, records: [
  { id: 4, name: "Call", summary: "Call", delay_count: 2, delay_unit: "days", default_user_id: null },
  { id: 2, name: "To-Do", summary: "To-Do", delay_count: 1, delay_unit: "weeks", default_user_id: null },
] };

let host: HTMLElement;
beforeEach(() => { host = document.createElement("div"); document.body.append(host); });
afterEach(() => host.remove());

function mount(rows: Record<string, unknown>[], extra: (call: Call) => Response | undefined = () => undefined): { a: Activities; calls: Call[]; rows: Record<string, unknown>[] } {
  const calls: Call[] = [];
  const state = { rows };
  const rpc = new RpcClient(async (path, init) => {
    const call = { path: String(path), body: JSON.parse(String(init?.body)) as Record<string, any> };
    calls.push(call);
    const custom = extra(call);
    if (custom) return custom;
    if (call.path.endsWith("/mail.activity/web_search_read")) return json({ length: state.rows.length, records: state.rows });
    if (call.path.endsWith("/mail.activity.type/web_search_read")) return json(TYPES);
    if (call.path.endsWith("/res.users/web_name_search")) return json([{ id: 2, display_name: "Alice Adams" }, { id: 3, display_name: "Bob Brown" }]);
    if (call.path.endsWith("/mail.activity/unlink") || call.path.endsWith("/mail.activity/create") || call.path.endsWith("/mail.activity/write")) { state.rows = []; return json(true); }
    if (call.path.endsWith("/mail.activity/action_feedback")) { state.rows = []; return json(101); }
    throw new Error(`Unexpected path: ${path}`);
  }, () => {});
  const a = new Activities({ rpc, model: "res.partner", recordId: 7, uid: 2, now: () => NOW, onLogged: async () => { calls.push({ path: "logged", body: {} }); } });
  host.append(a.element, a.button);
  return { a, calls, rows: state.rows };
}

/// (partial: activities card grouping, states, relative due text and empty state)
test("planned activities are grouped overdue, today, planned with relative due text and an absolute title", async () => {
  const { a } = mount([row(1, "Planned thing", "planned", "2026-11-15"), row(2, "Late thing", "overdue", "2026-11-08"), row(3, "Now thing", "today", "2026-11-11")]);
  await a.load();
  const groups = [...host.querySelectorAll<HTMLElement>(".erp-mail-activity-group")];
  expect(groups.map((g) => g.dataset["state"])).toEqual(["overdue", "today", "planned"]);
  expect(host.querySelector(".is-overdue .erp-mail-activity-due")?.textContent).toBe("3 days ago");
  expect(host.querySelector(".is-overdue .erp-mail-activity-due")?.getAttribute("title")).toBe("2026-11-08");
  expect(host.querySelector(".is-today .erp-mail-activity-due")?.textContent).toBe("Today");
  expect(host.querySelector(".is-planned .erp-mail-activity-due")?.textContent).toBe("In 4 days");
  expect(host.querySelector(".erp-mail-activities-count")?.textContent).toBe("3");
  expect(host.querySelector(".erp-mail-activity-user")?.textContent).toBe("Alice Adams");
  a.dispose();
});

/// (partial: an empty activities list renders nothing but the header button)
test("no activities renders no card, only the header button", async () => {
  const { a } = mount([]);
  await a.load();
  expect(a.element.hidden).toBe(true);
  expect(a.button.textContent).toBe("Activity");
  expect(a.button.getAttribute("aria-label")).toBe("Activity");
  a.dispose();
});

/// (partial: schedule form defaults and create call)
test("the schedule form defaults the due date from the type and the assignee to the current user, then creates", async () => {
  const { a, calls } = mount([]);
  await a.load();
  a.button.click();
  await settle();
  const date = host.querySelector<HTMLInputElement>(".erp-mail-activity-date")!;
  const who = host.querySelector<HTMLSelectElement>(".erp-mail-activity-user-select")!;
  expect(date.value).toBe("2026-11-13");
  expect(who.value).toBe("2");
  host.querySelector<HTMLSelectElement>(".erp-mail-activity-type")!.value = "2";
  host.querySelector<HTMLSelectElement>(".erp-mail-activity-type")!.dispatchEvent(new Event("change"));
  expect(date.value).toBe("2026-11-18");
  host.querySelector<HTMLInputElement>(".erp-mail-activity-summary-input")!.value = "Prepare invoice";
  host.querySelector<HTMLFormElement>(".erp-mail-activity-form")!.dispatchEvent(new Event("submit", { cancelable: true }));
  await settle();
  const create = calls.find((c) => c.path.endsWith("/mail.activity/create"))!;
  expect(create.body["vals_list"]).toEqual([{ summary: "Prepare invoice", date_deadline: "2026-11-18", user_id: 2, note: null, res_model: "res.partner", res_id: 7, activity_type_id: 2 }]);
  expect(host.querySelector(".erp-mail-activity-form")).toBeNull();
  a.dispose();
});

/// (partial: mark done posts feedback through action_feedback and reloads the thread)
test("mark done sends the optional feedback and asks the thread to reload", async () => {
  const { a, calls } = mount([row(1, "Call Bob", "today", "2026-11-11")]);
  await a.load();
  host.querySelector<HTMLButtonElement>('[data-action="done"]')!.click();
  host.querySelector<HTMLTextAreaElement>(".erp-mail-activity-feedback")!.value = "Spoke to him";
  host.querySelector<HTMLFormElement>(".erp-mail-activity-done")!.dispatchEvent(new Event("submit", { cancelable: true }));
  await settle();
  const done = calls.find((c) => c.path.endsWith("/action_feedback"))!;
  expect(done.body).toMatchObject({ ids: [1], feedback: "Spoke to him", attachment_ids: [] });
  expect(calls.some((c) => c.path === "logged")).toBe(true);
  expect(host.querySelector(".erp-mail-activity")).toBeNull();
  a.dispose();
});

/// (partial: edit writes the changed values, cancel unlinks, rows without write access have no actions)
test("edit writes the values, cancel unlinks, and a read-only activity shows no actions", async () => {
  const { a, calls } = mount([row(1, "Call Bob", "planned", "2026-11-20"), row(2, "Not mine", "planned", "2026-11-21", false)]);
  await a.load();
  expect(host.querySelectorAll(".erp-mail-activity")[1]?.querySelector(".erp-mail-activity-actions")).toBeNull();
  host.querySelector<HTMLButtonElement>('[data-action="edit"]')!.click();
  await settle();
  host.querySelector<HTMLInputElement>(".erp-mail-activity-summary-input")!.value = "Call Bob again";
  host.querySelector<HTMLFormElement>(".erp-mail-activity-form")!.dispatchEvent(new Event("submit", { cancelable: true }));
  await settle();
  expect(calls.find((c) => c.path.endsWith("/mail.activity/write"))!.body).toMatchObject({ ids: [1], vals: { summary: "Call Bob again", date_deadline: "2026-11-20", user_id: 2 } });
  a.dispose();
  const second = mount([row(5, "Drop me", "planned", "2026-11-20")]);
  await second.a.load();
  host.querySelector<HTMLButtonElement>('[data-action="cancel"]')!.click();
  await settle();
  expect(second.calls.find((c) => c.path.endsWith("/unlink"))!.body).toMatchObject({ ids: [5] });
  second.a.dispose();
});

/// (partial: server errors render inline in the card, with Retry on load)
test("a failed load and a failed mark done render inline alerts", async () => {
  let fail = true;
  const { a } = mount([row(1, "Call Bob", "today", "2026-11-11")], (call) => {
    if (fail && call.path.endsWith("/mail.activity/web_search_read")) return new Response(JSON.stringify({ name: "AccessError", message: "No access to activities" }), { status: 403, headers: { "Content-Type": "application/json" } });
    if (call.path.endsWith("/action_feedback")) return new Response(JSON.stringify({ name: "ValidationError", message: "Feedback too long" }), { status: 422, headers: { "Content-Type": "application/json" } });
    return undefined;
  });
  await a.load();
  expect(host.querySelector(".erp-mail-activities-error")?.getAttribute("role")).toBe("alert");
  expect(host.querySelector(".erp-mail-activities-error")?.textContent).toContain("No access");
  fail = false;
  host.querySelector<HTMLButtonElement>(".erp-mail-activities-error .erp-mail-retry")!.click();
  await settle();
  expect(host.querySelector(".erp-mail-activities-error")).toBeNull();
  host.querySelector<HTMLButtonElement>('[data-action="done"]')!.click();
  host.querySelector<HTMLFormElement>(".erp-mail-activity-done")!.dispatchEvent(new Event("submit", { cancelable: true }));
  await settle();
  expect(host.querySelector(".erp-mail-activities-error")?.textContent).toContain("Feedback too long");
  a.dispose();
});

test("date helpers: deadline from a type's delay and relative text", () => {
  expect(defaultDeadline({ delayCount: 2, delayUnit: "days" }, NOW)).toBe("2026-11-13");
  expect(defaultDeadline({ delayCount: 1, delayUnit: "months" }, NOW)).toBe("2026-12-11");
  expect(relativeDay("2026-11-12", NOW)).toBe("Tomorrow");
  expect(relativeDay("2026-11-10", NOW)).toBe("Yesterday");
});

const alertText = (): string => host.querySelector(".erp-mail-activities-error")?.textContent ?? "";
const submitForm = (): void => { host.querySelector<HTMLFormElement>(".erp-mail-activity-form")!.dispatchEvent(new Event("submit", { cancelable: true })); };

/// (partial: an edited activity keeps its assignee even when the user list does not include them)
test("editing an activity assigned to a user outside the listed users keeps that user", async () => {
  const far = { ...row(1, "Far away", "planned", "2026-11-20"), user_id: { id: 9, display_name: "Zed Zimmer" } };
  const { a, calls } = mount([far]);
  await a.load();
  host.querySelector<HTMLButtonElement>('[data-action="edit"]')!.click();
  await settle();
  const who = host.querySelector<HTMLSelectElement>(".erp-mail-activity-user-select")!;
  expect(who.value).toBe("9");
  submitForm();
  await settle();
  expect(calls.find((c) => c.path.endsWith("/mail.activity/write"))!.body["vals"]).toMatchObject({ user_id: 9 });
  a.dispose();
});

/// (partial: a submit without a valid assignee or type is refused instead of sending 0)
test("scheduling without an activity type is refused with a message and sends nothing", async () => {
  const { a, calls } = mount([], (call) => (call.path.endsWith("/mail.activity.type/web_search_read") ? json({ length: 0, records: [] }) : undefined));
  await a.load();
  a.button.click();
  await settle();
  submitForm();
  await settle();
  expect(alertText()).toContain("activity type");
  expect(calls.some((c) => c.path.endsWith("/mail.activity/create"))).toBe(false);
  a.dispose();
});

/// (partial: the assignee is found by a name search on the server)
test("typing in the assignee search asks the server and keeps the chosen user", async () => {
  const { a, calls } = mount([], (call) => (call.path.endsWith("/res.users/web_name_search") && call.body["name"] === "zed" ? json([{ id: 9, display_name: "Zed Zimmer" }]) : undefined));
  await a.load();
  a.button.click();
  await settle();
  const find = host.querySelector<HTMLInputElement>(".erp-mail-activity-user-search")!;
  find.value = "zed";
  find.dispatchEvent(new Event("input"));
  await settle();
  expect(calls.some((c) => c.path.endsWith("/res.users/web_name_search") && c.body["name"] === "zed")).toBe(true);
  const options = [...host.querySelectorAll<HTMLOptionElement>(".erp-mail-activity-user-select option")].map((o) => o.value);
  expect(options).toEqual(["2", "9"]);
  a.dispose();
});

// Malformed server answers are errors, never defaults (a parameterised test carries no SPEC tag).
test.each([
  ["no records list", (call: Call) => (call.path.endsWith("/mail.activity/web_search_read") ? json({ length: 0 }) : undefined), "records"],
  ["an unknown state", (call: Call) => (call.path.endsWith("/mail.activity/web_search_read") ? json({ records: [row(1, "x", "someday", "2026-11-20")] }) : undefined), "unknown state"],
  ["a missing id", (call: Call) => (call.path.endsWith("/mail.activity/web_search_read") ? json({ records: [{ ...row(1, "x", "planned", "2026-11-20"), id: "abc" }] }) : undefined), "valid id"],
])("a malformed activities answer (%s) shows an inline error", async (_name, extra, text) => {
  const { a } = mount([], extra);
  await a.load();
  expect(alertText()).toContain(text);
  a.dispose();
});

test.each([
  ["types without delay_count", (call: Call) => (call.path.endsWith("/mail.activity.type/web_search_read") ? json({ records: [{ id: 4, name: "Call", summary: "", delay_unit: "days" }] }) : undefined), "delay_count"],
  ["types without delay_unit", (call: Call) => (call.path.endsWith("/mail.activity.type/web_search_read") ? json({ records: [{ id: 4, name: "Call", summary: "", delay_count: 1 }] }) : undefined), "delay_unit"],
  ["a user search that is not a list", (call: Call) => (call.path.endsWith("/res.users/web_name_search") ? json({ oops: true }) : undefined), "not a list"],
  ["an unreadable own user", (call: Call) => (call.path.endsWith("/res.users/web_name_search") ? json([{ id: 3, display_name: "Bob Brown" }]) : call.path.endsWith("/res.users/web_read") ? json([]) : undefined), "signed-in user"],
])("a malformed answer when the form opens (%s) shows an inline error and no form", async (_name, extra, text) => {
  const { a } = mount([], extra);
  await a.load();
  a.button.click();
  await settle();
  expect(alertText()).toContain(text);
  expect(host.querySelector(".erp-mail-activity-form")).toBeNull();
  expect(host.textContent).not.toContain("Me");
  a.dispose();
});

/// (partial: a double click opens one form and a double cancel sends one unlink)
test("a double click on Activities opens one form and a double click on Cancel unlinks once", async () => {
  const { a, calls } = mount([row(1, "Drop me", "planned", "2026-11-20")]);
  await a.load();
  a.button.click();
  a.button.click();
  await settle();
  expect(host.querySelectorAll(".erp-mail-activity-form")).toHaveLength(1);
  expect(calls.filter((c) => c.path.endsWith("/mail.activity.type/web_search_read"))).toHaveLength(1);
  const cancel = host.querySelector<HTMLButtonElement>('[data-action="cancel"]')!;
  cancel.click();
  cancel.click();
  await settle();
  expect(calls.filter((c) => c.path.endsWith("/unlink"))).toHaveLength(1);
  a.dispose();
});

/// (partial: keyboard, Escape closes the forms and focus returns to a control that exists)
test("Escape closes the schedule and done forms, the toggle names its form, and focus returns", async () => {
  const { a } = mount([row(1, "Call Bob", "today", "2026-11-11")]);
  await a.load();
  expect(a.button.getAttribute("aria-controls")).toBe(host.querySelector(".erp-mail-activities-form-host")!.id);
  a.button.click();
  await settle();
  host.querySelector<HTMLElement>(".erp-mail-activity-type")!.dispatchEvent(new KeyboardEvent("keydown", { key: "Escape", bubbles: true }));
  expect(host.querySelector(".erp-mail-activity-form")).toBeNull();
  expect(a.button.getAttribute("aria-expanded")).toBe("false");
  expect(document.activeElement).toBe(a.button);
  host.querySelector<HTMLButtonElement>('[data-action="done"]')!.click();
  host.querySelector<HTMLElement>(".erp-mail-activity-feedback")!.dispatchEvent(new KeyboardEvent("keydown", { key: "Escape", bubbles: true }));
  expect(host.querySelector(".erp-mail-activity-done")).toBeNull();
  expect(document.activeElement).toBe(host.querySelector('[data-action="done"]'));
  a.dispose();
});

test("a month delay from Jan 31 lands on the last day of the shorter month", () => {
  expect(defaultDeadline({ delayCount: 1, delayUnit: "months" }, new Date(2026, 0, 31))).toBe("2026-02-28");
});
