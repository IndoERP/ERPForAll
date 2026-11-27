/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
import { el, errorMessage } from "./dom.js";
import { icon, type IconName } from "./icons.js";
import { attachMenuDropdown } from "./menu-dropdown.js";
import type { SystrayItem, SystrayEnvironment } from "./systray.js";
import { ClientError, type RpcClient } from "./transport.js";
import type { Registry } from "./registry.js";

/** One row of the answer of `mail.activity.erp_activity_groups`, checked field by field. */
export interface ActivityGroupData {
  model: string;
  name: string;
  addon: string | false;
  overdue_count: number;
  today_count: number;
  planned_count: number;
  total_count: number;
  activity_ids: readonly number[];
}

export interface ActivityMenuData {
  activity_counter: number;
  groups: readonly ActivityGroupData[];
}

/** The activity menu needs the RPC client and the error reporter of the app. */
export type ActivityMenuEnvironment = SystrayEnvironment;

const STATE_WORDS: Record<Exclude<ActivityState, "all">, { label: string; spoken: string }> = {
  overdue: { label: "late", spoken: "late (overdue)" },
  today: { label: "today", spoken: "today" },
  planned: { label: "future", spoken: "future (planned)" }
};

const MODEL_ICONS: Record<string, IconName> = {
  "res.partner": "users",
  "crm.lead": "target",
  "sale.order": "shopping-cart",
  "purchase.order": "credit-card",
  "account.move": "book-open",
  "stock.picking": "package",
  "project.task": "clipboard-check",
  "mail.activity": "clock"
};

const ADDON_ICONS: Record<string, IconName> = {
  base: "layout-grid",
  mail: "clock",
  contacts: "users",
  product: "package",
  stock: "package"
};

/** The icon of a group: by model first, then by the addon that declares it, else a generic document. */
export function getModelIconName(model: string, addon?: string | false): IconName {
  const byModel = MODEL_ICONS[model];
  if (byModel) return byModel;
  if (typeof addon === "string") {
    const byAddon = ADDON_ICONS[addon];
    if (byAddon) return byAddon;
  }
  return "file-text";
}

function invalid(path: string, expected: string): ClientError {
  return new ClientError("server", `The activity menu answer is invalid: ${path} must be ${expected}.`);
}

function count(source: Record<string, unknown>, key: string, path: string): number {
  const value = source[key];
  if (typeof value !== "number" || !Number.isSafeInteger(value) || value < 0) throw invalid(`${path}${key}`, "a non-negative safe integer");
  return value;
}

function text(source: Record<string, unknown>, key: string, path: string): string {
  const value = source[key];
  if (typeof value !== "string") throw invalid(`${path}${key}`, "a string");
  return value;
}

function record(value: unknown, path: string): Record<string, unknown> {
  if (typeof value !== "object" || value === null || Array.isArray(value)) throw invalid(path, "an object");
  return value as Record<string, unknown>;
}

/**
 * Checks the answer of `erp_activity_groups`. A field of the wrong shape is an error naming it, never a zero or an
 * empty value: the panel must not show counts the server did not send.
 */
export function parseActivityMenuData(raw: unknown): ActivityMenuData {
  const answer = record(raw, "the answer");
  const activity_counter = count(answer, "activity_counter", "");
  const rawGroups = answer["groups"];
  if (!Array.isArray(rawGroups)) throw invalid("groups", "a list");
  const groups = rawGroups.map((item, index): ActivityGroupData => {
    const path = `groups[${index}].`;
    const group = record(item, `groups[${index}]`);
    const addon = group["addon"];
    if (typeof addon !== "string" && addon !== false) throw invalid(`${path}addon`, "a string or false");
    const ids = group["activity_ids"];
    if (!Array.isArray(ids) || !ids.every(id => typeof id === "number" && Number.isSafeInteger(id) && id > 0)) {
      throw invalid(`${path}activity_ids`, "a list of positive safe integers");
    }
    return {
      model: text(group, "model", path),
      name: text(group, "name", path),
      addon,
      overdue_count: count(group, "overdue_count", path),
      today_count: count(group, "today_count", path),
      planned_count: count(group, "planned_count", path),
      total_count: count(group, "total_count", path),
      activity_ids: ids as number[]
    };
  });
  return { activity_counter, groups };
}

/** The states a group opens in: the wire value of `erp_activity_group_action`, the visible label and the accessible state. */
export type ActivityState = "all" | "overdue" | "today" | "planned";

/** The window action the server builds for one group and state, after `parseActivityGroupAction`. */
export interface ActivityWindowAction {
  readonly type: "ir.actions.act_window";
  readonly name: string;
  readonly res_model: string;
  readonly target: "current";
  readonly views: readonly [readonly [false, "list"], readonly [false, "form"]];
  readonly context: Record<string, never>;
  readonly domain: readonly unknown[];
}

/** The server refuses a state with more records than this (`MAX_OPEN_RECORDS`), so a longer id list is a malformed answer. */
const MAX_OPEN_IDS = 1000;
const MAX_ACTION_BYTES = 16384;
const ACTION_KEYS = ["context", "domain", "name", "res_model", "target", "type", "views"];

/**
 * Checks the answer of `erp_activity_group_action` for the model that was asked: exactly the documented keys, the
 * documented constants, and a domain that starts with `["id", "in", ids]` with at most 1000 positive ids and at most 64
 * terms. Anything else is an error naming the member; the list is never opened from a shape the server did not promise.
 */
export function parseActivityGroupAction(raw: unknown, model: string): ActivityWindowAction {
  const action = record(raw, "the action");
  const keys = Object.keys(action).sort();
  if (keys.length !== ACTION_KEYS.length || keys.some((key, index) => key !== ACTION_KEYS[index])) throw invalid("the action", `an object with exactly the members ${ACTION_KEYS.join(", ")}`);
  if (new TextEncoder().encode(JSON.stringify(raw)).length > MAX_ACTION_BYTES) throw invalid("the action", `at most ${MAX_ACTION_BYTES} bytes`);
  if (action["type"] !== "ir.actions.act_window") throw invalid("type", "ir.actions.act_window");
  if (action["target"] !== "current") throw invalid("target", "current");
  const name = text(action, "name", "");
  if (!name || name.length > 256) throw invalid("name", "a non-empty string of at most 256 characters");
  if (action["res_model"] !== model) throw invalid("res_model", `the requested model ${model}`);
  if (JSON.stringify(action["views"]) !== '[[false,"list"],[false,"form"]]') throw invalid("views", "the list and form views");
  const context = record(action["context"], "context");
  if (Object.keys(context).length) throw invalid("context", "empty");
  const domain = action["domain"];
  if (!Array.isArray(domain) || domain.length < 1 || domain.length > 64) throw invalid("domain", "a list of 1 to 64 terms");
  const first = domain[0];
  if (!Array.isArray(first) || first.length !== 3 || first[0] !== "id" || first[1] !== "in" || !Array.isArray(first[2])
    || first[2].length > MAX_OPEN_IDS || !first[2].every(id => typeof id === "number" && Number.isSafeInteger(id) && id > 0)) {
    throw invalid("domain", `a list starting with ["id", "in", up to ${MAX_OPEN_IDS} positive ids]`);
  }
  return action as unknown as ActivityWindowAction;
}

/** The activity menu reads the counts through this RPC; it needs the mail addon's `mail.activity` model. */
const ACTIVITY_MODEL = "mail.activity";
const APPROVAL_MODEL = "ir.approval.request";
const APPROVAL_REFRESH_MS = 60_000;

/**
 * True only for the server's refusal of an unknown `mail.activity` model: HTTP 404, `werkzeug.exceptions.NotFound`
 * and the exact text of `crates/erp-rpc/src/service.rs` (`not_found`). An unknown method, a missing right or any
 * other failure is a different error and is reported, not taken for "the addon is not installed".
 */
export function isUnknownActivityModel(error: unknown): boolean {
  return error instanceof ClientError
    && error.status === 404
    && error.diagnostics?.name === "werkzeug.exceptions.NotFound"
    && error.message === `the model '${ACTIVITY_MODEL}' does not exist`;
}

async function fetchActivityMenu(rpc: RpcClient, context: Record<string, unknown> | undefined): Promise<ActivityMenuData> {
  const raw = await rpc.call(ACTIVITY_MODEL, "erp_activity_groups", { context: context ?? {} }, { background: true });
  return parseActivityMenuData(raw);
}

async function fetchApprovalCount(rpc: RpcClient, context: Record<string, unknown> | undefined): Promise<number> {
  const raw = await rpc.call(APPROVAL_MODEL, "erp_queue", { scope: "review", offset: 0, limit: 0, context: context ?? {} }, { background: true });
  const answer = raw as { total?: unknown; items?: unknown } | null;
  if (typeof answer?.total !== "number" || !Number.isSafeInteger(answer.total) || answer.total < 0
      || !Array.isArray(answer.items) || answer.items.length !== 0) {
    throw new ClientError("server", "Invalid approval queue count.");
  }
  return answer.total;
}

export interface ActivityMenuHandle {
  node: HTMLElement;
  dispose(): void;
  refresh(): Promise<void>;
}

/** A first read that failed: the entry stays, shows a failed state, and the next open retries. */
export interface ActivityMenuFailure { readonly failure: unknown }

type PanelState<T> =
  | { readonly kind: "loading" }
  | { readonly kind: "failed" }
  | { readonly kind: "unavailable" }
  | { readonly kind: "ready"; readonly data: T };

let panelSerial = 0;

/** A count above zero is a button named after its group and state; a zero is plain text, never a control. */
function countControl(className: string, value: number, state: Exclude<ActivityState, "all">, group: ActivityGroupData, open: (group: ActivityGroupData, state: ActivityState) => void): HTMLElement {
  const words = STATE_WORDS[state];
  const node = document.createElement(value === 0 ? "span" : "button");
  node.className = `activity-count ${className}${value === 0 ? " is-zero" : ""}`;
  node.textContent = `${value} ${words.label}`;
  if (node instanceof HTMLButtonElement) {
    node.type = "button";
    node.dataset["state"] = state;
    node.dataset["focusKey"] = `${group.model}|${state}`;
    node.setAttribute("aria-label", `${group.name}, ${value} ${words.spoken}`);
    node.addEventListener("click", () => open(group, state));
  }
  return node;
}

/**
 * Renders the systray clock with late and due activities, the user's approval queue, and unread Discuss notifications.
 * The badge counts work awaiting the user. A separate marker indicates unread notifications.
 *
 * `initial` is the outcome of a first read already made (an answer, checked, or its failure); without it the entry
 * fetches once when it is rendered. It then fetches again on every open, which is the retry after a failure. A failed
 * or malformed answer is announced once per run of failures (a notification when the app has one, else `env.report`;
 * never a modal for a background read), the button shows a failed state and the panel says so.
 */
export function renderActivityMenu(env: ActivityMenuEnvironment, initial?: ActivityMenuData | ActivityMenuFailure): ActivityMenuHandle {
  const rpc = env.rpc;
  const report = env.report;
  const openList = env.openActivities;
  if (!rpc || !report || !openList) throw new ClientError("validation", "The activity menu needs the RPC client, the error reporter and the handler that opens an activity list.");
  const openApprovals = env.openApprovals;
  const notifications = env.discussNotifications;

  const container = document.createElement("div");
  container.className = "activity-menu-systray";

  const trigger = document.createElement("button");
  trigger.type = "button";
  trigger.className = "activity-menu-trigger";
  trigger.setAttribute("aria-label", "Activities");
  trigger.append(icon("clock"));
  const unreadIndicator = document.createElement("span");
  unreadIndicator.className = "activity-menu-unread";
  unreadIndicator.setAttribute("aria-hidden", "true");
  unreadIndicator.hidden = true;
  trigger.append(unreadIndicator);

  const dropdown = document.createElement("div");
  dropdown.className = "activity-menu-dropdown";
  dropdown.id = `activity-menu-panel-${++panelSerial}`;
  // Read-only content: a labelled region, not a menu. Set before attachMenuDropdown, which keeps an existing role.
  dropdown.setAttribute("role", "region");
  dropdown.setAttribute("aria-label", "Activities");
  const body = document.createElement("div");
  body.className = "activity-menu-body";
  dropdown.append(body);

  const first = initial === undefined ? undefined : "failure" in initial ? initial : { data: initial };
  let state: PanelState<ActivityMenuData> = first === undefined ? { kind: "loading" } : "data" in first ? { kind: "ready", data: first.data } : { kind: "failed" };
  let approvalState: PanelState<number> = { kind: openApprovals ? "loading" : "unavailable" };
  let notificationState: PanelState<number> = { kind: notifications ? "loading" : "unavailable" };
  let announced = false;
  let badge: HTMLElement | null = null;
  let disposed = false;
  let opening = false;
  let openingEpoch = 0; // the panel opening the pending open belongs to
  let openTicket = 0; // the newest open: an older one that settles later must not clear the busy state of a newer one
  // Bumped on every close: an answer that arrives for an earlier open of the panel must not navigate.
  let openEpoch = 0;

  const announce = (error: unknown): void => {
    if (announced || disposed) return;
    announced = true;
    const notifications = env.messages?.notifications;
    if (notifications) notifications.add(`Activities could not be loaded. ${errorMessage(error)}`, { level: "danger" });
    else report(error);
  };

  const paintBadge = (): void => {
    const activityCount = state.kind === "ready" ? state.data.activity_counter : 0;
    const approvalCount = approvalState.kind === "ready" ? approvalState.data : 0;
    const notificationCount = notificationState.kind === "ready" ? notificationState.data : 0;
    const total = activityCount + approvalCount;
    trigger.classList.toggle("is-failed", state.kind === "failed" && total === 0 && notificationCount === 0);
    unreadIndicator.hidden = notificationCount === 0;
    if (total > 0) {
      if (!badge) {
        badge = document.createElement("span");
        badge.className = "activity-menu-counter";
        badge.setAttribute("data-testid", "activity-counter");
        badge.setAttribute("aria-hidden", "true");
        trigger.append(badge);
      }
      badge.textContent = total > 999 ? "999+" : String(total);
    } else {
      badge?.remove();
      badge = null;
    }
    const labels = ["Activities"];
    if (state.kind === "failed") labels.push(openApprovals || notifications ? "scheduled activities unavailable" : "could not be loaded");
    if (approvalState.kind === "failed") labels.push("approvals unavailable");
    if (notificationState.kind === "failed") labels.push("notification count unavailable");
    if (activityCount > 0) labels.push(`${activityCount} late or due today`);
    if (approvalCount > 0) labels.push(`${approvalCount} awaiting your review`);
    if (notificationCount > 0) {
      labels.push(`${notificationCount >= 1000 ? "at least 1000" : notificationCount} unread ${notificationCount === 1 ? "notification" : "notifications"}`);
    }
    trigger.setAttribute("aria-label", labels.join(", "));
  };

  const paintActivities = (section: HTMLElement): void => {
    if (state.kind === "unavailable") return;
    if (state.kind === "loading") { section.append(el("p", "Loading activities…", "activity-menu-empty")); return; }
    if (state.kind === "failed") {
      // A control keeps the keyboard inside the panel (Escape closes it from there) and is the retry the open-time refresh already is.
      const retry = document.createElement("button");
      retry.type = "button";
      retry.className = "activity-menu-footer";
      retry.dataset["focusKey"] = "retry";
      retry.textContent = "Try again";
      retry.addEventListener("click", () => { void refreshActivities(); });
      section.append(el("p", "Activities could not be loaded.", "activity-menu-empty activity-menu-failed"), retry);
      return;
    }
    if (state.data.groups.length === 0) section.append(el("p",
      openApprovals || notifications ? "No scheduled activities." : "No activities to do.", "activity-menu-empty"));
    const list = document.createElement("ul");
    list.className = "activity-group-list";
    for (const group of state.data.groups) {
      const item = document.createElement("li");
      item.className = "activity-group-item";
      item.dataset["modelName"] = group.model;
      const iconSpan = document.createElement("span");
      iconSpan.className = "activity-group-icon";
      iconSpan.setAttribute("aria-hidden", "true");
      iconSpan.append(icon(getModelIconName(group.model, group.addon)));
      const details = document.createElement("div");
      details.className = "activity-group-details";
      // "all" opens late plus today, so a group with neither has nothing to open: plain text, not a control.
      const title = group.overdue_count + group.today_count === 0 ? document.createElement("span") : document.createElement("button");
      title.className = "activity-group-title";
      title.textContent = group.name;
      if (title instanceof HTMLButtonElement) {
        title.type = "button";
        title.dataset["state"] = "all";
        title.dataset["focusKey"] = `${group.model}|all`;
        title.setAttribute("aria-label", `${group.name}, late and today`);
        title.addEventListener("click", () => openGroup(group, "all"));
      }
      const counts = document.createElement("div");
      counts.className = "activity-group-counts";
      counts.append(
        countControl("activity-count-overdue", group.overdue_count, "overdue", group, openGroup),
        countControl("activity-count-today", group.today_count, "today", group, openGroup),
        countControl("activity-count-planned", group.planned_count, "planned", group, openGroup)
      );
      details.append(title, counts);
      item.append(iconSpan, details);
      list.append(item);
    }
    if (state.data.groups.length > 0) section.append(list);
    const footer = document.createElement("button");
    footer.type = "button";
    footer.className = "activity-menu-footer";
    footer.dataset["focusKey"] = "footer";
    footer.textContent = "View all activities";
    footer.addEventListener("click", () => openGroup(null, "all"));
    section.append(footer);
  };

  const failOpen = (error: unknown, what: string): void => {
    const notifications = env.messages?.notifications;
    const message = `The ${what} could not be opened. ${errorMessage(error)}`;
    if (notifications) notifications.add(message, { level: "danger" });
    else report(error);
  };

  const openDestination = (
    what: string,
    open: (action: ActivityWindowAction | null) => void | Promise<void>,
    prepare?: () => Promise<ActivityWindowAction>
  ): void => {
    // A pending open from an earlier opening of the panel (closed since) does not block the user's new click: the newest click wins.
    if (disposed || opening && openingEpoch === openEpoch) return;
    opening = true;
    openingEpoch = openEpoch;
    const ticket = ++openTicket;
    let epoch = openEpoch;
    let closedByOpen = false;
    const restoreFocus = (): void => {
      const active = document.activeElement;
      if (active === null || active === document.body || dropdown.contains(active)) trigger.focus();
    };
    body.setAttribute("aria-busy", "true");
    void (async () => {
      try {
        const action = prepare ? await prepare() : null;
        // Closed (Escape, outside click, another open) or removed while the server answered: do nothing.
        if (disposed || epoch !== openEpoch) return;
        controller.close(false);
        closedByOpen = true;
        epoch = openEpoch;
        await open(action);
        // The open ended without a list (an unsaved form was kept, an action is pending): focus must not stay in the hidden panel.
        if (!disposed) restoreFocus();
      } catch (error) {
        // Focus goes back to the clock only when this open closed the panel and focus was left inside it; a close the user made leaves focus where they put it.
        if (!disposed && closedByOpen && epoch === openEpoch) restoreFocus();
        // After dispose the notification surface may be gone: report it instead of dropping it.
        if (disposed) report(error); else failOpen(error, what);
      } finally {
        if (ticket === openTicket) { opening = false; body.removeAttribute("aria-busy"); }
      }
    })();
  };

  /** `group` null opens the user's whole "My Activities" list. */
  const openGroup = (group: ActivityGroupData | null, wanted: ActivityState): void => {
    openDestination("activities", openList, group ? async () => {
      const raw = await rpc.call(ACTIVITY_MODEL, "erp_activity_group_action", { model: group.model, state: wanted, context: env.context ?? {} }, { background: true });
      return parseActivityGroupAction(raw, group.model);
    } : undefined);
  };

  const paintSummary = (key: string, title: string, glyph: IconName, summary: PanelState<number>,
    amount: (value: number) => string, open: () => void, retry: () => void): void => {
    const section = el("section", undefined, "activity-menu-section activity-menu-summary");
    section.setAttribute("aria-label", title);
    if (summary.kind === "loading") {
      section.append(el("p", `Loading ${title.toLowerCase()}…`, "activity-menu-empty"));
    } else if (summary.kind === "failed") {
      const control = el("button", `Retry ${title.toLowerCase()}`, "activity-menu-summary-control");
      control.type = "button";
      control.dataset["focusKey"] = key;
      control.addEventListener("click", retry);
      section.append(el("p", `${title} could not be loaded.`, "activity-menu-empty activity-menu-failed"), control);
    } else if (summary.kind === "ready") {
      const control = el("button", undefined, "activity-menu-summary-control");
      control.type = "button";
      control.dataset["focusKey"] = key;
      const count = amount(summary.data);
      control.setAttribute("aria-label", `${title}, ${count}`);
      control.append(icon(glyph), el("span", title), el("span", count, "activity-menu-summary-count"));
      control.addEventListener("click", open);
      section.append(control);
    }
    body.append(section);
  };

  const paint = (): void => {
    // A repaint replaces every control: when focus was inside the panel it moves to the same group and state, else the first control.
    const active = document.activeElement;
    const keptKey = active instanceof HTMLElement && dropdown.contains(active) ? active.dataset["focusKey"] ?? "" : undefined;
    paintBadge();
    body.replaceChildren();
    if (state.kind !== "unavailable") {
      const section = el("section", undefined, "activity-menu-section");
      if (openApprovals || notifications) {
        section.setAttribute("aria-label", "Scheduled activities");
        section.append(el("h2", "Scheduled activities", "activity-menu-heading"));
      }
      paintActivities(section);
      body.append(section);
    }
    if (openApprovals && approvalState.kind !== "unavailable") {
      paintSummary("approvals", "Approvals", "badge-check", approvalState, value => `${value} to review`,
        () => openDestination("approvals", openApprovals), () => { void refreshApprovals(); });
    }
    if (notifications && notificationState.kind !== "unavailable") {
      paintSummary("notifications", "Notifications", "inbox", notificationState, value => `${value >= 1000 ? "999+" : value} unread`,
        () => openDestination("notifications", () => notifications.open()), () => { void refreshNotifications(); });
    }
    if (!body.firstChild) body.append(el("p", "Nothing to review.", "activity-menu-empty"));
    if (keptKey !== undefined) {
      const same = keptKey ? [...dropdown.querySelectorAll<HTMLElement>("[data-focus-key]")].find(node => node.dataset["focusKey"] === keptKey) : undefined;
      (same ?? dropdown.querySelector<HTMLElement>("button") ?? trigger).focus();
    }
    if (!controller.isOpen() || !active || active === trigger || active === document.body || keptKey !== undefined) {
      controller.refresh();
    }
  };

  // A wake that arrives in flight can describe a write after that read's snapshot, so read once more when it settles.
  function makeRefresh<T>(load: () => Promise<T>, apply: (outcome: { data: T } | { error: unknown }) => void) {
    let pending: Promise<void> | undefined;
    let queued = false;
    return (afterChange = false): Promise<void> => {
      if (disposed) return Promise.resolve();
      if (pending) {
        queued ||= afterChange;
        return pending;
      }
      pending = (async () => {
        do {
          queued = false;
          let outcome: { data: T } | { error: unknown };
          try { outcome = { data: await load() }; }
          catch (error) { outcome = { error }; }
          if (disposed) return;
          apply(outcome);
          paint();
        } while (queued);
      })().finally(() => { pending = undefined; });
      return pending;
    };
  }

  const readActivities = makeRefresh(() => fetchActivityMenu(rpc, env.context), outcome => {
    if ("data" in outcome) {
      announced = false;
      state = { kind: "ready", data: outcome.data };
    } else if (isUnknownActivityModel(outcome.error)) {
      state = { kind: "unavailable" };
    } else {
      announce(outcome.error);
      state = { kind: "failed" };
    }
  });
  const readApprovals = makeRefresh(() => fetchApprovalCount(rpc, env.context), outcome => {
    approvalState = "data" in outcome ? { kind: "ready", data: outcome.data } :
      { kind: outcome.error instanceof ClientError && outcome.error.kind === "permission" ? "unavailable" : "failed" };
  });
  const readNotifications = notifications && makeRefresh(() => notifications.count(), outcome => {
    notificationState = "data" in outcome ? { kind: "ready", data: outcome.data } :
      { kind: outcome.error instanceof ClientError && outcome.error.kind === "permission" ? "unavailable" : "failed" };
  });
  const refreshActivities = (afterChange = false) => state.kind === "unavailable" ? undefined : readActivities(afterChange);
  const refreshApprovals = (afterChange = false) => openApprovals ? readApprovals(afterChange) : undefined;
  const refreshNotifications = (afterChange = false) => readNotifications?.(afterChange);
  const refresh = async (): Promise<void> => {
    await Promise.all([refreshActivities(), refreshApprovals(), refreshNotifications()]);
  };

  const controller = attachMenuDropdown({
    trigger,
    menu: dropdown,
    mode: "class-open",
    onOpen: () => { void refresh(); },
    onClose: () => { openEpoch++; }
  });
  // The panel is a disclosure, not a menu: attachMenuDropdown stamped aria-haspopup="true" (meaning a menu).
  trigger.removeAttribute("aria-haspopup");
  trigger.setAttribute("aria-controls", dropdown.id);

  const onActivityChange = () => {
    void refreshActivities(true);
    void refreshApprovals(true);
  };
  // An approval may be requested in someone else's session. Keep the closed badge up to date in the foreground.
  let approvalTimer: ReturnType<typeof setTimeout> | undefined;
  const scheduleApprovalRefresh = (): void => {
    clearTimeout(approvalTimer);
    if (document.visibilityState !== "visible") return;
    approvalTimer = setTimeout(() => {
      void refreshApprovals();
      scheduleApprovalRefresh();
    }, APPROVAL_REFRESH_MS);
  };
  const onVisibilityChange = (): void => {
    if (document.visibilityState === "visible") {
      void refreshApprovals(true);
      void refreshNotifications(true);
    }
    if (openApprovals) scheduleApprovalRefresh();
  };
  if (openApprovals || notifications) document.addEventListener("visibilitychange", onVisibilityChange);
  if (openApprovals) scheduleApprovalRefresh();
  document.addEventListener("erp:activities-changed", onActivityChange);
  const stopNotifications = notifications?.subscribe(() => { void refreshNotifications(true); });
  container.append(trigger, dropdown);
  paint();
  if (first === undefined) void refresh();
  else {
    if ("failure" in first) announce(first.failure);
    void refreshApprovals();
    void refreshNotifications();
  }

  return {
    node: container,
    dispose: () => {
      if (disposed) return;
      disposed = true;
      document.removeEventListener("erp:activities-changed", onActivityChange);
      document.removeEventListener("visibilitychange", onVisibilityChange);
      clearTimeout(approvalTimer);
      stopNotifications?.();
      controller.dispose();
      container.remove();
    },
    refresh
  };
}

// What isDisplayed read, handed to render for the same environment so the entry costs one call at start.
const prefetched = new WeakMap<object, ActivityMenuData | ActivityMenuFailure>();

/**
 * The systray entry, registered under "mail.activity_menu" for existing callers. Approvals and
 * personal notifications can keep it visible without the optional scheduled activity model.
 */
export const activitySystrayItem: SystrayItem = {
  key: "mail.activity_menu",
  sequence: 20,
  isDisplayed: async (env: SystrayEnvironment): Promise<boolean> => {
    if (!env.rpc) return false;
    if (env.openApprovals || env.discussNotifications) return true;
    try {
      prefetched.set(env, await fetchActivityMenu(env.rpc, env.context));
    } catch (error) {
      if (isUnknownActivityModel(error)) return false;
      // A systray predicate must not throw; render announces the failure once.
      prefetched.set(env, { failure: error });
    }
    return true;
  },
  render: (env: SystrayEnvironment) => {
    const outcome = prefetched.get(env);
    prefetched.delete(env);
    return renderActivityMenu(env, outcome);
  }
};

/** Adds the built-in entries to the systray registry; called once by the production boot, safe to call again. */
export function registerBuiltinSystrayItems(registry: Registry<SystrayItem>): void {
  if (!registry.has("mail.activity_menu")) registry.add("mail.activity_menu", activitySystrayItem);
}
