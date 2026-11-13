/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
import { describe, expect, test, vi, beforeEach, afterEach } from "vitest";
import {
  activitySystrayItem,
  getModelIconName,
  isUnknownActivityModel,
  parseActivityGroupAction,
  parseActivityMenuData,
  registerBuiltinSystrayItems,
  renderActivityMenu
} from "../src/client/activity-menu.js";
import { Registry, systrayItems } from "../src/client/registry.js";
import type { SystrayItem } from "../src/client/systray.js";
import { renderSystray, type SystrayEnvironment } from "../src/client/systray.js";
import { ClientError, type RpcClient } from "../src/client/transport.js";

const partner = {
  model: "res.partner", name: "Contact", addon: "base", overdue_count: 2, today_count: 0, planned_count: 1,
  total_count: 2, activity_ids: [10, 11, 12]
};
const other = {
  model: "mail.activity", name: "Other Activities", addon: false, overdue_count: 0, today_count: 4, planned_count: 0,
  total_count: 4, activity_ids: [20]
};
const answer = (groups: object[], counter: number): unknown => ({ activity_counter: counter, groups });

function unknownModel(): ClientError {
  return new ClientError("validation", "the model 'mail.activity' does not exist", 404, {
    endpoint: "/erp/json/2/mail.activity/erp_activity_groups", status: 404, name: "werkzeug.exceptions.NotFound"
  });
}

describe("Activity menu client", () => {
  let container: HTMLElement;
  let reported: unknown[];
  let opened: unknown[];
  beforeEach(() => {
    opened = [];
    container = document.createElement("div");
    document.body.append(container);
    reported = [];
  });
  afterEach(() => { container.remove(); document.body.replaceChildren(); });

  function rpcReturning(...results: unknown[]): { rpc: RpcClient; call: ReturnType<typeof vi.fn> } {
    const call = vi.fn();
    for (const result of results) {
      if (result instanceof Error) call.mockRejectedValueOnce(result); else call.mockResolvedValueOnce(result);
    }
    return { rpc: { call } as unknown as RpcClient, call };
  }
  const envFor = (rpc: RpcClient): SystrayEnvironment => ({
    rpc, context: { lang: "en_US" }, report: error => { reported.push(error); },
    openActivities: async action => { opened.push(action); }
  });
  const trigger = (node: HTMLElement): HTMLButtonElement => node.querySelector<HTMLButtonElement>(".activity-menu-trigger")!;
  const badge = (node: HTMLElement): HTMLElement | null => node.querySelector<HTMLElement>(".activity-menu-counter");

  /// (partial: the built-in registration function adds the entry once under mail.activity_menu; the boot call itself is not tested)
  test("registerBuiltinSystrayItems registers the activity menu once", () => {
    const registry = new Registry<SystrayItem>("systray item");
    registerBuiltinSystrayItems(registry);
    registerBuiltinSystrayItems(registry);
    expect(registry.entries().map(([key]) => key)).toEqual(["mail.activity_menu"]);
    expect(registry.get("mail.activity_menu")).toBe(activitySystrayItem);
    expect(activitySystrayItem.sequence).toBe(20);
  });

  /// (partial: importing the registry module does not register the entry; only the production boot does)
  test("the shared systray registry is empty until boot registers the entry", () => {
    expect(systrayItems.has("mail.activity_menu")).toBe(false);
  });

  /// (partial: the registered entry renders in a systray through renderSystray and reads once at start)
  test("renderSystray shows the registered entry with its badge from one initial call", async () => {
    const { rpc, call } = rpcReturning(answer([partner, other], 6));
    const registry = new Registry<SystrayItem>("systray item");
    registerBuiltinSystrayItems(registry);
    const handle = await renderSystray(container, registry.entries(), envFor(rpc));
    expect(call).toHaveBeenCalledTimes(1);
    expect(container.querySelector(".activity-menu-trigger")).not.toBeNull();
    expect(badge(container)?.textContent).toBe("6");
    expect(reported).toEqual([]);
    handle.dispose();
  });

  /// (partial: badge hidden at 0 and shown after the initial fetch without opening the panel)
  test("the badge follows the counter from the first fetch, without opening", async () => {
    const zero = rpcReturning(answer([], 0));
    const quiet = renderActivityMenu(envFor(zero.rpc));
    container.append(quiet.node);
    await vi.waitFor(() => expect(zero.call).toHaveBeenCalledTimes(1));
    await vi.waitFor(() => expect(quiet.node.querySelector(".activity-menu-empty")?.textContent).toBe("No activities to do."));
    expect(badge(quiet.node)).toBeNull();
    expect(trigger(quiet.node).getAttribute("aria-label")).toBe("Activities");
    quiet.dispose();

    const busy = rpcReturning(answer([partner, other], 6));
    const handle = renderActivityMenu(envFor(busy.rpc));
    container.append(handle.node);
    await vi.waitFor(() => expect(badge(handle.node)?.textContent).toBe("6"));
    expect(busy.call).toHaveBeenCalledWith("mail.activity", "erp_activity_groups", { context: { lang: "en_US" } }, { background: true });
    expect(handle.node.querySelector(".activity-menu-dropdown")?.classList.contains("open")).toBe(false);
    expect(trigger(handle.node).getAttribute("aria-label")).toContain("6");
    handle.dispose();
  });

  /// (partial: opening the panel fetches again and repaints badge and rows)
  test("opening the panel fetches again", async () => {
    const { rpc, call } = rpcReturning(answer([partner], 2), answer([other], 4));
    const handle = renderActivityMenu(envFor(rpc));
    container.append(handle.node);
    await vi.waitFor(() => expect(badge(handle.node)?.textContent).toBe("2"));
    trigger(handle.node).click();
    await vi.waitFor(() => expect(badge(handle.node)?.textContent).toBe("4"));
    expect(call).toHaveBeenCalledTimes(2);
    expect(handle.node.querySelector<HTMLElement>(".activity-group-item")?.dataset["modelName"]).toBe("mail.activity");
    handle.dispose();
  });

  /// (partial: a refresh already running is not started twice)
  test("a refresh in flight is not duplicated", async () => {
    const { rpc, call } = rpcReturning(answer([], 0));
    const handle = renderActivityMenu(envFor(rpc), parseActivityMenuData(answer([], 0)));
    container.append(handle.node);
    await Promise.all([handle.refresh(), handle.refresh()]);
    expect(call).toHaveBeenCalledTimes(1);
    handle.dispose();
  });

  /// (partial: empty state wording)
  test("an empty answer shows the empty state", () => {
    const { rpc } = rpcReturning();
    const handle = renderActivityMenu(envFor(rpc), parseActivityMenuData(answer([], 0)));
    container.append(handle.node);
    expect(handle.node.querySelector(".activity-menu-empty")?.textContent).toBe("No activities to do.");
    expect(handle.node.querySelector(".activity-group-item")).toBeNull();
    handle.dispose();
  });

  /// (partial: rows with icon, name and three counts, muted at zero)
  test("rows show icon, name and three counts, muted when zero", () => {
    const { rpc } = rpcReturning();
    const data = parseActivityMenuData(answer([partner, other], 6));
    const handle = renderActivityMenu(envFor(rpc), data);
    container.append(handle.node);
    const rows = [...handle.node.querySelectorAll<HTMLElement>(".activity-group-item")];
    expect(rows.map(row => row.dataset["modelName"])).toEqual(data.groups.map(group => group.model));
    const first = rows[0]!;
    expect(first.querySelector(".activity-group-title")?.textContent).toBe(data.groups[0]!.name);
    expect(first.querySelector(".activity-group-icon svg.erp-icon")).not.toBeNull();
    const counts = [...first.querySelectorAll<HTMLElement>(".activity-count")];
    expect(counts.map(node => node.textContent)).toEqual([`${data.groups[0]!.overdue_count} late`, `${data.groups[0]!.today_count} today`, `${data.groups[0]!.planned_count} future`]);
    expect(counts.map(node => node.classList.contains("is-zero"))).toEqual([false, true, false]);
    // Only a count above zero is a control; a zero stays text.
    expect([...first.querySelectorAll(".activity-count")].map(node => node.tagName)).toEqual(["BUTTON", "SPAN", "BUTTON"]);
    handle.dispose();
  });

  /// (partial: a malformed answer is reported with the field named and is never rendered as counts)
  test.each([
    ["not an object", [], "the answer", "an object"],
    ["counter negative", { activity_counter: -1, groups: [] }, "activity_counter", "a non-negative safe integer"],
    ["counter fractional", { activity_counter: 1.5, groups: [] }, "activity_counter", "a non-negative safe integer"],
    ["counter unsafe", { activity_counter: 2 ** 53, groups: [] }, "activity_counter", "a non-negative safe integer"],
    ["counter missing", { groups: [] }, "activity_counter", "a non-negative safe integer"],
    ["groups not a list", { activity_counter: 0, groups: {} }, "groups", "a list"],
    ["group not an object", { activity_counter: 0, groups: ["x"] }, "groups[0]", "an object"],
    ["model missing", { activity_counter: 0, groups: [{ ...partner, model: undefined }] }, "groups[0].model", "a string"],
    ["name wrong type", { activity_counter: 0, groups: [{ ...partner, name: 5 }] }, "groups[0].name", "a string"],
    ["addon wrong type", { activity_counter: 0, groups: [{ ...partner, addon: null }] }, "groups[0].addon", "a string or false"],
    ["overdue count string", { activity_counter: 0, groups: [{ ...partner, overdue_count: "2" }] }, "groups[0].overdue_count", "a non-negative safe integer"],
    ["today count negative", { activity_counter: 0, groups: [{ ...partner, today_count: -2 }] }, "groups[0].today_count", "a non-negative safe integer"],
    ["planned count fractional", { activity_counter: 0, groups: [{ ...partner, planned_count: 0.5 }] }, "groups[0].planned_count", "a non-negative safe integer"],
    ["total count missing", { activity_counter: 0, groups: [partner, { ...other, total_count: undefined }] }, "groups[1].total_count", "a non-negative safe integer"],
    ["ids not integers", { activity_counter: 0, groups: [{ ...partner, activity_ids: [1, "2"] }] }, "groups[0].activity_ids", "a list of positive safe integers"],
    ["ids not positive", { activity_counter: 0, groups: [{ ...partner, activity_ids: [0] }] }, "groups[0].activity_ids", "a list of positive safe integers"]
  ])("malformed answer (%s) is reported and shows no counts", async (_label, malformed, field, expected) => {
    const { rpc } = rpcReturning(malformed);
    const handle = renderActivityMenu(envFor(rpc));
    container.append(handle.node);
    await vi.waitFor(() => expect(reported.length).toBe(1));
    const error = reported[0] as ClientError;
    expect(error).toBeInstanceOf(ClientError);
    expect(error.kind).toBe("server");
    expect(error.message).toBe(`The activity menu answer is invalid: ${field} must be ${expected}.`);
    await vi.waitFor(() => expect(handle.node.querySelector(".activity-menu-failed")?.textContent).toBe("Activities could not be loaded."));
    expect(handle.node.querySelector(".activity-count")).toBeNull();
    expect(badge(handle.node)).toBeNull();
    expect(trigger(handle.node).classList.contains("is-failed")).toBe(true);
    expect(trigger(handle.node).getAttribute("aria-label")).toBe("Activities, could not be loaded");
    handle.dispose();
  });

  /// (partial: a failed call on open replaces older counts with the failure, not with zero)
  test("a failed refresh on open is reported and the old counts are not kept as current", async () => {
    const failure = new ClientError("network", "The network is down.");
    const { rpc } = rpcReturning(answer([partner], 2), failure);
    const handle = renderActivityMenu(envFor(rpc));
    container.append(handle.node);
    await vi.waitFor(() => expect(badge(handle.node)?.textContent).toBe("2"));
    trigger(handle.node).click();
    await vi.waitFor(() => expect(reported).toEqual([failure]));
    await vi.waitFor(() => expect(handle.node.querySelector(".activity-menu-failed")).not.toBeNull());
    expect(badge(handle.node)).toBeNull();
    expect(trigger(handle.node).classList.contains("is-failed")).toBe(true);
    handle.dispose();
  });

  /// (partial: a failure is announced once per run of failures, as a notification when the app has one, and the next open retries)
  test("a failure is announced once, uses notifications when present, and opening retries", async () => {
    const failure = new ClientError("network", "The network is down.");
    const { rpc, call } = rpcReturning(failure, failure, answer([partner], 2));
    const add = vi.fn();
    const env = { ...envFor(rpc), messages: { notifications: { add } } } as unknown as SystrayEnvironment;
    const handle = renderActivityMenu(env);
    container.append(handle.node);
    await vi.waitFor(() => expect(trigger(handle.node).classList.contains("is-failed")).toBe(true));
    await handle.refresh();
    expect(call).toHaveBeenCalledTimes(2);
    expect(add).toHaveBeenCalledTimes(1);
    expect(add).toHaveBeenCalledWith(`Activities could not be loaded. ${failure.message}`, { level: "danger" });
    expect(reported).toEqual([]);
    trigger(handle.node).click();
    await vi.waitFor(() => expect(badge(handle.node)?.textContent).toBe("2"));
    expect(trigger(handle.node).classList.contains("is-failed")).toBe(false);
    handle.dispose();
  });

  /// (partial: disposing before the answer leaves the DOM untouched and reports nothing)
  test("dispose during an in-flight fetch does not touch the DOM or report", async () => {
    for (const settle of [(resolve: (value: unknown) => void, _reject: (error: unknown) => void) => resolve(answer([partner], 2)),
      (_resolve: (value: unknown) => void, reject: (error: unknown) => void) => reject(new ClientError("network", "offline"))]) {
      let finish!: () => void;
      const call = vi.fn(() => new Promise<unknown>((resolve, reject) => { finish = () => settle(resolve, reject); }));
      const handle = renderActivityMenu(envFor({ call } as unknown as RpcClient));
      container.append(handle.node);
      const body = handle.node.querySelector(".activity-menu-body")!;
      const before = body.innerHTML;
      handle.dispose();
      finish();
      await new Promise(done => setTimeout(done, 0));
      expect(body.innerHTML).toBe(before);
      expect(trigger(handle.node).classList.contains("is-failed")).toBe(false);
      expect(badge(handle.node)).toBeNull();
      expect(reported).toEqual([]);
    }
  });

  /// (partial: Esc closes the panel and returns focus to the trigger; the panel is a labelled region, not a menu)
  test("Escape closes the panel and focus returns to the trigger", () => {
    const { rpc } = rpcReturning(answer([], 0));
    const handle = renderActivityMenu(envFor(rpc), parseActivityMenuData(answer([partner], 2)));
    container.append(handle.node);
    const button = trigger(handle.node);
    const panel = handle.node.querySelector<HTMLElement>(".activity-menu-dropdown")!;
    expect(panel.getAttribute("role")).toBe("region");
    expect(panel.getAttribute("aria-label")).toBe("Activities");
    expect(button.hasAttribute("aria-haspopup")).toBe(false);
    expect(button.getAttribute("aria-controls")).toBe(panel.id);
    button.focus();
    button.click();
    expect(button.getAttribute("aria-expanded")).toBe("true");
    expect(panel.classList.contains("open")).toBe(true);
    button.dispatchEvent(new KeyboardEvent("keydown", { key: "Escape", bubbles: true, cancelable: true }));
    expect(button.getAttribute("aria-expanded")).toBe("false");
    expect(panel.classList.contains("open")).toBe(false);
    expect(document.activeElement).toBe(button);
    handle.dispose();
  });

  /// (partial: the entry is hidden only for a deployment without the mail.activity model; every other first-read failure keeps it in a failed state)
  test("isDisplayed is false only for the unknown mail.activity model", async () => {
    expect(isUnknownActivityModel(unknownModel())).toBe(true);

    const hidden = rpcReturning(unknownModel());
    expect(await activitySystrayItem.isDisplayed?.(envFor(hidden.rpc))).toBe(false);
    expect(reported).toEqual([]);

    // Same status and kind, but the method is unknown: a different failure.
    const method = new ClientError("validation", "The method 'mail.activity.erp_activity_groups' does not exist", 404, {
      endpoint: "/erp/json/2/mail.activity/erp_activity_groups", status: 404, name: "werkzeug.exceptions.NotFound"
    });
    const denied = new ClientError("permission", "Access denied.", 403);
    for (const failure of [method, denied, new ClientError("network", "offline")]) {
      expect(isUnknownActivityModel(failure)).toBe(false);
      reported = [];
      const env = envFor(rpcReturning(failure).rpc);
      expect(await activitySystrayItem.isDisplayed?.(env)).toBe(true);
      const handle = activitySystrayItem.render!(env);
      container.append(handle.node);
      // render shows the failed state and announces the failure once.
      expect(reported).toEqual([failure]);
      expect(trigger(handle.node).classList.contains("is-failed")).toBe(true);
      handle.dispose();
    }

    // A malformed answer keeps the entry too.
    reported = [];
    const malformed = envFor(rpcReturning({ activity_counter: "x", groups: [] }).rpc);
    expect(await activitySystrayItem.isDisplayed?.(malformed)).toBe(true);
    const handle = activitySystrayItem.render!(malformed);
    expect((reported[0] as ClientError).message).toBe("The activity menu answer is invalid: activity_counter must be a non-negative safe integer.");
    handle.dispose();

    // No RPC client: nothing to read from.
    expect(await activitySystrayItem.isDisplayed?.({})).toBe(false);
  });

  /// (partial: the answer isDisplayed read is the data the first render uses)
  test("render reuses the answer isDisplayed fetched", async () => {
    const { rpc, call } = rpcReturning(answer([partner], 2));
    const env = envFor(rpc);
    expect(await activitySystrayItem.isDisplayed?.(env)).toBe(true);
    const handle = activitySystrayItem.render!(env);
    container.append(handle.node);
    expect(call).toHaveBeenCalledTimes(1);
    expect(badge(handle.node)?.textContent).toBe("2");
    handle.dispose();
  });

  /// (partial: icon by model, then by addon, then generic)
  test("icon resolves by model, then addon, then a generic document", () => {
    expect(getModelIconName("res.partner")).toBe("users");
    expect(getModelIconName("x.unknown", "base")).toBe("layout-grid");
    expect(getModelIconName("x.unknown", false)).toBe("file-text");
    expect(getModelIconName("x.unknown", "no_such_addon")).toBe("file-text");
  });

  const actionFor = (model: string, ids: number[], name = "Contact"): Record<string, unknown> => ({
    type: "ir.actions.act_window", name, res_model: model, target: "current", views: [[false, "list"], [false, "form"]],
    context: {}, domain: [["id", "in", ids], ["active", "in", [true, false]]]
  });
  /// (partial: the client side: a count opens the list of that state through the server's target)
  test.each([["overdue", ".activity-count-overdue"], ["today", ".activity-count-today"], ["planned", ".activity-count-planned"], ["all", ".activity-group-title"]])(
    "a click on %s asks the server for that state and opens its list", async (state, selector) => {
      const rich = { ...partner, today_count: 3 };
      const target = actionFor("res.partner", rich.activity_ids as number[]);
      const { rpc, call } = rpcReturning(answer([rich], 6), answer([rich], 6), target);
      const handle = renderActivityMenu(envFor(rpc));
      container.append(handle.node);
      await vi.waitFor(() => expect(badge(handle.node)).not.toBeNull());
      trigger(handle.node).click();
      await vi.waitFor(() => expect(call).toHaveBeenCalledTimes(2));
      await vi.waitFor(() => expect(handle.node.querySelector(selector)?.tagName).toBe("BUTTON"));
      handle.node.querySelector<HTMLButtonElement>(selector)!.click();
      await vi.waitFor(() => expect(opened).toEqual([target]));
      expect(call).toHaveBeenLastCalledWith("mail.activity", "erp_activity_group_action", { model: rich.model, state, context: { lang: "en_US" } }, { background: true });
      expect(handle.node.querySelector(".activity-menu-dropdown")?.classList.contains("open")).toBe(false);
      expect(reported).toEqual([]);
      handle.dispose();
    });

  /// (partial: accessible names carry the model and the state)
  test("counts and names are buttons named after their group and state", () => {
    const { rpc } = rpcReturning();
    const handle = renderActivityMenu(envFor(rpc), parseActivityMenuData(answer([partner], 2)));
    container.append(handle.node);
    const names = [...handle.node.querySelectorAll<HTMLElement>(".activity-group-item button")].map(node => node.getAttribute("aria-label"));
    expect(names).toEqual([`${partner.name}, late and today`, `${partner.name}, ${partner.overdue_count} late (overdue)`, `${partner.name}, ${partner.planned_count} future (planned)`]);
    const zero = handle.node.querySelector<HTMLElement>(".activity-count-today")!;
    expect(zero.tagName).toBe("SPAN");
    expect(zero.hasAttribute("aria-label")).toBe(false);
    zero.click();
    expect(opened).toEqual([]);
    handle.dispose();
  });

  /// (partial: Enter and Space activate a count)
  test("a count is a native button, so Enter and Space activate it", async () => {
    const target = actionFor("res.partner", [10, 11, 12]);
    const { rpc } = rpcReturning(target);
    const handle = renderActivityMenu(envFor(rpc), parseActivityMenuData(answer([partner], 2)));
    container.append(handle.node);
    const button = handle.node.querySelector<HTMLButtonElement>(".activity-count-overdue")!;
    expect(button.type).toBe("button");
    button.focus();
    expect(document.activeElement).toBe(button);
    button.click(); // what the browser does for Enter and Space on a button
    await vi.waitFor(() => expect(opened).toEqual([target]));
    handle.dispose();
  });

  /// (partial: a failed target read is reported, the panel stays open and usable)
  test("a failed target read is reported and the panel stays usable", async () => {
    const failure = new ClientError("validation", "You have no open activities on \"res.partner\" any more; open the activity menu again.", 422);
    const target = actionFor("res.partner", [10]);
    const { rpc, call } = rpcReturning(answer([partner], 2), failure, target);
    const notified: unknown[] = [];
    const env = { ...envFor(rpc), messages: { notifications: { add: (message: string, options: unknown) => { notified.push([message, options]); } } } } as unknown as SystrayEnvironment;
    const handle = renderActivityMenu(env, parseActivityMenuData(answer([partner], 2)));
    container.append(handle.node);
    trigger(handle.node).click(); // the open reads the counts again
    await vi.waitFor(() => expect(call).toHaveBeenCalledTimes(1));
    await new Promise(resolve => setTimeout(resolve, 0));
    const button = handle.node.querySelector<HTMLButtonElement>(".activity-count-overdue")!;
    button.click();
    await vi.waitFor(() => expect(notified.length).toBe(1));
    expect((notified[0] as [string, unknown])[0]).toContain("The activities could not be opened.");
    expect((notified[0] as [string, unknown])[0]).toContain("open the activity menu again");
    expect(opened).toEqual([]);
    expect(handle.node.querySelector(".activity-menu-dropdown")?.classList.contains("open")).toBe(true);
    expect(handle.node.querySelector(".activity-menu-body")?.hasAttribute("aria-busy")).toBe(false);
    // usable again: the same button works once the server answers
    handle.node.querySelector<HTMLButtonElement>(".activity-count-overdue")!.click();
    await vi.waitFor(() => expect(opened).toEqual([target]));
    expect(call).toHaveBeenCalledTimes(3);
    handle.dispose();
  });

  /// (partial: a failure of the app's opening handler is reported too)
  test("a failure of the opening handler is reported", async () => {
    const { rpc } = rpcReturning(actionFor("res.partner", [10]));
    const failure = new ClientError("server", "The My Activities list is missing. Ask your administrator to restore it.");
    const env = { ...envFor(rpc), openActivities: async () => { throw failure; } };
    const handle = renderActivityMenu(env, parseActivityMenuData(answer([partner], 2)));
    container.append(handle.node);
    handle.node.querySelector<HTMLButtonElement>(".activity-count-overdue")!.click();
    await vi.waitFor(() => expect(reported).toEqual([failure]));
    handle.dispose();
  });

  /// (partial: an answer that arrives after the panel was closed does not navigate)
  test("an answer after the panel was closed does not navigate", async () => {
    let answerLate: (value: unknown) => void = () => {};
    const call = vi.fn((_model: string, method: string) => method === "erp_activity_groups" ? Promise.resolve(answer([partner], 2))
      : new Promise(resolve => { answerLate = resolve; }));
    const handle = renderActivityMenu(envFor({ call } as unknown as RpcClient), parseActivityMenuData(answer([partner], 2)));
    container.append(handle.node);
    trigger(handle.node).click();
    await vi.waitFor(() => expect(call).toHaveBeenCalledTimes(1));
    await new Promise(resolve => setTimeout(resolve, 0));
    handle.node.querySelector<HTMLButtonElement>(".activity-count-overdue")!.click();
    expect(call).toHaveBeenCalledTimes(2);
    trigger(handle.node).dispatchEvent(new KeyboardEvent("keydown", { key: "Escape", bubbles: true, cancelable: true }));
    answerLate(actionFor("res.partner", [10]));
    await new Promise(resolve => setTimeout(resolve, 10));
    expect(opened).toEqual([]);
    expect(reported).toEqual([]);
    handle.dispose();
  });

  /// (partial: a second click while the answer is pending asks once)
  test("a second click while the first is pending is ignored", async () => {
    const call = vi.fn(() => new Promise(() => {}));
    const handle = renderActivityMenu(envFor({ call } as unknown as RpcClient), parseActivityMenuData(answer([partner], 2)));
    container.append(handle.node);
    const button = handle.node.querySelector<HTMLButtonElement>(".activity-count-overdue")!;
    button.click(); button.click();
    expect(call).toHaveBeenCalledTimes(1);
    handle.dispose();
  });

  /// (partial: "Other Activities" opens through the same call with the mail.activity model)
  test("the Other Activities group opens its own activities", async () => {
    const target = { ...actionFor("mail.activity", [20], "Other Activities"), domain: [["id", "in", [20]]] };
    const { rpc, call } = rpcReturning(target);
    const handle = renderActivityMenu(envFor(rpc), parseActivityMenuData(answer([other], 4)));
    container.append(handle.node);
    handle.node.querySelector<HTMLButtonElement>(".activity-count-today")!.click();
    await vi.waitFor(() => expect(opened).toEqual([target]));
    expect(call).toHaveBeenCalledWith("mail.activity", "erp_activity_group_action", { model: "mail.activity", state: "today", context: { lang: "en_US" } }, { background: true });
    handle.dispose();
  });

  /// (partial: the footer opens the user's own list, with no server call from the menu)
  test("View all activities opens My Activities", async () => {
    const { rpc, call } = rpcReturning();
    const handle = renderActivityMenu(envFor(rpc), parseActivityMenuData(answer([], 0)));
    container.append(handle.node);
    const footer = handle.node.querySelector<HTMLButtonElement>(".activity-menu-footer")!;
    expect(footer.textContent).toBe("View all activities");
    footer.click();
    await vi.waitFor(() => expect(opened).toEqual([null]));
    expect(reported).toEqual([]);
    expect(call).not.toHaveBeenCalledWith("mail.activity", "erp_activity_group_action", expect.anything(), expect.anything());
    handle.dispose();
  });

  /// (partial: the answer's shape is checked: closed keys, constants, bounded ids)
  test.each([
    ["an extra member", { ...actionFor("res.partner", [1]), extra: 1 }, /exactly the members/],
    ["a missing member", (() => { const a = actionFor("res.partner", [1]); delete a["domain"]; return a; })(), /exactly the members/],
    ["another type", { ...actionFor("res.partner", [1]), type: "ir.actions.server" }, /type/],
    ["another model", actionFor("crm.lead", [1]), /res_model/],
    ["another target", { ...actionFor("res.partner", [1]), target: "new" }, /target/],
    ["other views", { ...actionFor("res.partner", [1]), views: [[false, "kanban"]] }, /views/],
    ["a context", { ...actionFor("res.partner", [1]), context: { x: 1 } }, /context/],
    ["no id term", { ...actionFor("res.partner", [1]), domain: [["active", "=", true]] }, /domain/],
    ["an id that is not positive", actionFor("res.partner", [0]), /domain/],
    ["too many ids", actionFor("res.partner", Array.from({ length: 1001 }, (_, i) => i + 1)), /domain/],
    ["not an object", [], /an object/],
    ["an empty name", actionFor("res.partner", [1], ""), /name/],
    ["a too long name", actionFor("res.partner", [1], "x".repeat(257)), /name/],
    ["more than 64 terms", { ...actionFor("res.partner", [1]), domain: [["id", "in", [1]], ...Array.from({ length: 64 }, () => ["active", "=", true])] }, /domain/],
    ["an id that is not an integer", actionFor("res.partner", [1.5]), /domain/],
    ["another operator", { ...actionFor("res.partner", [1]), domain: [["id", "=", [1]]] }, /domain/],
    ["more than 16 KiB", { ...actionFor("res.partner", [1]), domain: [["id", "in", [1]], ["name", "=", "é".repeat(9000)]] }, /bytes/]
  ])("a target with %s is refused", (_name, raw, message) => {
    expect(() => parseActivityGroupAction(raw, "res.partner")).toThrow(message);
  });

  /// (partial: the largest admitted target, 1000 ids, is accepted)
  test("a target of exactly 1000 ids is accepted", () => {
    const ids = Array.from({ length: 1000 }, (_, i) => i + 1);
    expect(parseActivityGroupAction(actionFor("res.partner", ids), "res.partner").domain[0]).toEqual(["id", "in", ids]);
  });

  /// (partial: the menu refuses an environment that cannot open a list, instead of drawing dead controls)
  test("an environment without the opening handler is refused", () => {
    const { rpc } = rpcReturning();
    expect(() => renderActivityMenu({ rpc, report: () => {} })).toThrow(ClientError);
  });

  /// (partial: a group with no late or due-today activity has a plain-text name, so nothing opens an empty list)
  test("the group name is text when late plus today is 0", () => {
    const { rpc } = rpcReturning();
    const quiet = { ...partner, overdue_count: 0, today_count: 0, planned_count: 3, total_count: 3 };
    const handle = renderActivityMenu(envFor(rpc), parseActivityMenuData(answer([quiet], 0)));
    container.append(handle.node);
    const name = handle.node.querySelector<HTMLElement>(".activity-group-title")!;
    expect(name.tagName).toBe("SPAN");
    name.click();
    expect(opened).toEqual([]);
    handle.dispose();
  });

  /// (partial: when opening fails after the panel closed, focus returns to the clock before the failure shows)
  test("a failure after the panel closed returns focus to the trigger", async () => {
    const target = actionFor("res.partner", [10]);
    const { rpc, call } = rpcReturning(answer([partner], 2), target);
    const failure = new ClientError("server", "Found 0 window actions on mail.activity.");
    const env = { ...envFor(rpc), openActivities: async () => { throw failure; } };
    const handle = renderActivityMenu(env, parseActivityMenuData(answer([partner], 2)));
    container.append(handle.node);
    trigger(handle.node).click();
    await vi.waitFor(() => expect(call).toHaveBeenCalledTimes(1));
    await new Promise(resolve => setTimeout(resolve, 0));
    const button = handle.node.querySelector<HTMLButtonElement>(".activity-count-overdue")!;
    button.focus();
    button.click();
    await vi.waitFor(() => expect(reported).toEqual([failure]));
    expect(handle.node.querySelector(".activity-menu-dropdown")?.classList.contains("open")).toBe(false);
    expect(document.activeElement).toBe(trigger(handle.node));
    handle.dispose();
  });

  /// (partial: an error that arrives after dispose is reported, not dropped)
  test("an error after dispose is reported", async () => {
    let fail: (error: unknown) => void = () => {};
    const call = vi.fn(() => new Promise((_resolve, reject) => { fail = reject; }));
    const handle = renderActivityMenu(envFor({ call } as unknown as RpcClient), parseActivityMenuData(answer([partner], 2)));
    container.append(handle.node);
    handle.node.querySelector<HTMLButtonElement>(".activity-count-overdue")!.click();
    handle.dispose();
    const failure = new ClientError("network", "offline");
    fail(failure);
    await vi.waitFor(() => expect(reported).toEqual([failure]));
    expect(opened).toEqual([]);
  });

  /// (partial: a keyboard open keeps focus in the panel when the open-time read repaints it)
  test.each([["the first control", undefined], ["the same count", ".activity-count-overdue"]])("focus stays in the panel after the open-time repaint: %s", async (_name, selector) => {
    const { rpc, call } = rpcReturning(answer([partner, other], 6));
    const handle = renderActivityMenu(envFor(rpc), parseActivityMenuData(answer([partner, other], 6)));
    container.append(handle.node);
    trigger(handle.node).focus();
    trigger(handle.node).click(); // detail 0: opens with focus on the first control
    const before = selector ? handle.node.querySelector<HTMLElement>(selector)! : (document.activeElement as HTMLElement);
    before.focus();
    const key = before.dataset["focusKey"];
    expect(handle.node.querySelector(".activity-menu-dropdown")!.contains(before)).toBe(true);
    await vi.waitFor(() => expect(call).toHaveBeenCalledTimes(1));
    await new Promise(resolve => setTimeout(resolve, 0));
    expect(before.isConnected).toBe(false); // the repaint replaced it
    const now = document.activeElement as HTMLElement;
    expect(handle.node.querySelector(".activity-menu-dropdown")!.contains(now)).toBe(true);
    expect(now.dataset["focusKey"]).toBe(key);
    handle.dispose();
  });

  /// (partial: a repaint never takes focus that was outside the panel)
  test("the open-time repaint does not steal focus from outside the panel", async () => {
    const { rpc, call } = rpcReturning(answer([partner], 2));
    const handle = renderActivityMenu(envFor(rpc), parseActivityMenuData(answer([partner], 2)));
    const field = document.createElement("input");
    container.append(handle.node, field);
    trigger(handle.node).click();
    field.focus();
    await vi.waitFor(() => expect(call).toHaveBeenCalledTimes(1));
    await new Promise(resolve => setTimeout(resolve, 0));
    expect(document.activeElement).toBe(field);
    handle.dispose();
  });

  /// (partial: an open that closed the panel but did not navigate returns focus to the clock)
  test("focus returns to the clock when the open ends without a list", async () => {
    const { rpc, call } = rpcReturning(answer([partner], 2), actionFor("res.partner", [10]));
    const handle = renderActivityMenu(envFor(rpc), parseActivityMenuData(answer([partner], 2)));
    container.append(handle.node);
    trigger(handle.node).click();
    await vi.waitFor(() => expect(call).toHaveBeenCalledTimes(1));
    await new Promise(resolve => setTimeout(resolve, 0));
    const button = handle.node.querySelector<HTMLButtonElement>(".activity-count-overdue")!;
    button.focus();
    button.click(); // the handler resolves without navigating (a kept unsaved form, a pending action)
    await vi.waitFor(() => expect(opened.length).toBe(1));
    await new Promise(resolve => setTimeout(resolve, 0));
    expect(document.activeElement).toBe(trigger(handle.node));
    handle.dispose();
  });

  /// (partial: a failure after the user closed the panel is shown but does not take focus)
  test("a failure after the user closed the panel does not pull focus back", async () => {
    let fail: (error: unknown) => void = () => {};
    const call = vi.fn((_model: string, method: string) => method === "erp_activity_groups" ? Promise.resolve(answer([partner], 2))
      : new Promise((_resolve, reject) => { fail = reject; }));
    const handle = renderActivityMenu(envFor({ call } as unknown as RpcClient), parseActivityMenuData(answer([partner], 2)));
    const field = document.createElement("input");
    container.append(handle.node, field);
    trigger(handle.node).click();
    await vi.waitFor(() => expect(call).toHaveBeenCalledTimes(1));
    await new Promise(resolve => setTimeout(resolve, 0));
    handle.node.querySelector<HTMLButtonElement>(".activity-count-overdue")!.click();
    trigger(handle.node).dispatchEvent(new KeyboardEvent("keydown", { key: "Escape", bubbles: true, cancelable: true }));
    field.focus();
    const failure = new ClientError("network", "offline");
    fail(failure);
    await vi.waitFor(() => expect(reported).toEqual([failure]));
    expect(document.activeElement).toBe(field);
    handle.dispose();
  });
  /// (partial: a failed open-time refresh keeps the keyboard inside the panel, and Escape still closes it)
  test("when the open-time refresh fails, focus stays in the panel on a retry control and Escape closes the panel", async () => {
    const failure = new ClientError("network", "offline");
    const { rpc, call } = rpcReturning(failure, answer([partner], 2));
    const handle = renderActivityMenu(envFor(rpc), parseActivityMenuData(answer([partner], 2)));
    container.append(handle.node);
    trigger(handle.node).focus();
    trigger(handle.node).click(); // detail 0: focus goes to the first control
    const panel = handle.node.querySelector<HTMLElement>(".activity-menu-dropdown")!;
    expect(panel.contains(document.activeElement)).toBe(true);
    await vi.waitFor(() => expect(handle.node.querySelector(".activity-menu-failed")).not.toBeNull());
    const active = document.activeElement as HTMLElement;
    expect(panel.contains(active)).toBe(true);
    expect(active.textContent).toBe("Try again");
    active.click(); // the retry reads again and the answer repaints the groups
    await vi.waitFor(() => expect(call).toHaveBeenCalledTimes(2));
    await vi.waitFor(() => expect(handle.node.querySelector(".activity-group-list")).not.toBeNull());
    expect(panel.contains(document.activeElement)).toBe(true);
    handle.dispose();
  });

  /// (partial: Escape closes the panel from the retry control of the failed state)
  test("Escape on the retry control closes the failed panel", async () => {
    const { rpc } = rpcReturning(new ClientError("network", "offline"));
    const handle = renderActivityMenu(envFor(rpc), parseActivityMenuData(answer([partner], 2)));
    container.append(handle.node);
    trigger(handle.node).focus();
    trigger(handle.node).click();
    await vi.waitFor(() => expect(handle.node.querySelector(".activity-menu-failed")).not.toBeNull());
    (document.activeElement as HTMLElement).dispatchEvent(new KeyboardEvent("keydown", { key: "Escape", bubbles: true, cancelable: true }));
    expect(handle.node.querySelector(".activity-menu-dropdown")!.classList.contains("open")).toBe(false);
    handle.dispose();
  });

  /// (partial: the newest click wins after the panel was closed and reopened while an open was pending)
  test("a click after closing and reopening the panel runs although the first open is still pending", async () => {
    const answers: ((value: unknown) => void)[] = [];
    const call = vi.fn((_model: string, method: string, _body?: unknown) => method === "erp_activity_groups" ? Promise.resolve(answer([partner, other], 6))
      : new Promise(resolve => { answers.push(resolve); }));
    const handle = renderActivityMenu(envFor({ call } as unknown as RpcClient), parseActivityMenuData(answer([partner, other], 6)));
    container.append(handle.node);
    const actionCalls = () => call.mock.calls.filter(([, method]) => method === "erp_activity_group_action");
    trigger(handle.node).click();
    await new Promise(resolve => setTimeout(resolve, 0));
    handle.node.querySelector<HTMLButtonElement>(".activity-count-overdue")!.click(); // the first open, slow
    expect(actionCalls()).toHaveLength(1);
    trigger(handle.node).dispatchEvent(new KeyboardEvent("keydown", { key: "Escape", bubbles: true, cancelable: true }));
    trigger(handle.node).click();
    await new Promise(resolve => setTimeout(resolve, 0));
    handle.node.querySelector<HTMLButtonElement>('[data-focus-key="mail.activity|today"]')!.click(); // the user's real choice
    expect(actionCalls()).toHaveLength(2);
    expect(actionCalls()[1]![2]).toMatchObject({ model: "mail.activity", state: "today" });
    answers[0]!(actionFor("res.partner", [10]));
    await new Promise(resolve => setTimeout(resolve, 0));
    expect(opened).toEqual([]); // the abandoned open is cancelled
    expect(handle.node.querySelector(".activity-menu-body")!.getAttribute("aria-busy")).toBe("true"); // the newer one is still pending
    answers[1]!(actionFor("mail.activity", [20], "Other Activities"));
    await vi.waitFor(() => expect(opened).toEqual([actionFor("mail.activity", [20], "Other Activities")]));
    handle.dispose();
  });
});
