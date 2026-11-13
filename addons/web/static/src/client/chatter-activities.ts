/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
import { button, el, errorMessage } from "./dom.js";
import { icon, type IconName } from "./icons.js";
import { ClientError, type RpcClient } from "./transport.js";

/** Options of the activities card and its header button in the chatter. */
export interface ActivitiesOptions {
  readonly rpc: RpcClient;
  readonly model: string;
  readonly recordId: number;
  /** Signed-in user id: the default assignee of a new activity. */
  readonly uid?: number;
  /** Runs after any change (the `reload_on_activity` form reload). */
  readonly onChange?: () => Promise<void>;
  /** Runs after an activity was marked done: its feedback is logged in the thread, which must reload. */
  readonly onLogged?: () => Promise<void>;
  /** Today, injectable for tests. */
  readonly now?: () => Date;
}

type State = "overdue" | "today" | "planned";
interface Activity {
  readonly id: number;
  readonly summary: string;
  readonly note: string;
  readonly deadline: string;
  readonly state: State;
  readonly canWrite: boolean;
  readonly user: { readonly id: number; readonly name: string } | null;
  readonly type: { readonly id: number; readonly name: string; readonly icon: string } | null;
}
interface ActivityType { readonly id: number; readonly name: string; readonly summary: string; readonly delayCount: number; readonly delayUnit: string; readonly defaultUser: number | null }
interface User { readonly id: number; readonly name: string }
let nextFormHost = 0;

const GROUPS: readonly { readonly state: State; readonly title: string }[] = [
  { state: "overdue", title: "Overdue" },
  { state: "today", title: "Today" },
  { state: "planned", title: "Planned" },
];
/** Type icons name Material glyphs on the server; this maps them onto the client's icon set. */
const TYPE_ICONS: Readonly<Record<string, IconName>> = { mail: "mail", phone: "phone", calendar_today: "calendar", check: "square-check-big", upload: "files", warning: "bell" };

const obj = (value: unknown, what: string): Record<string, unknown> => {
  if (value === null || typeof value !== "object" || Array.isArray(value)) throw new ClientError("server", `${what} is not an object`);
  return value as Record<string, unknown>;
};
const text = (value: unknown): string => (typeof value === "string" ? value : "");
/** A server value the card cannot do without: a malformed answer is an error, never a default. */
const positiveId = (value: unknown, what: string): number => {
  if (!Number.isSafeInteger(value) || (value as number) <= 0) throw new ClientError("server", `${what} has no valid id`);
  return value as number;
};
const requiredText = (value: unknown, what: string): string => {
  if (typeof value !== "string") throw new ClientError("server", `${what} is not text`);
  return value;
};
const user = (value: unknown, what: string): User => {
  const row = obj(value, what);
  return { id: positiveId(row["id"], what), name: requiredText(row["display_name"], `${what} name`) };
};
const STATES: readonly State[] = ["overdue", "today", "planned"];
const UNITS: readonly string[] = ["days", "weeks", "months"];
const pair = (value: unknown): { id: number; name: string } | null => {
  if (value === null || typeof value !== "object") return null;
  const row = value as Record<string, unknown>;
  return Number.isSafeInteger(row["id"]) ? { id: row["id"] as number, name: text(row["display_name"]) } : null;
};

/** `YYYY-MM-DD` as a local date, or null when it is not one. */
export function parseDay(value: string): Date | null {
  const m = /^(\d{4})-(\d{2})-(\d{2})/.exec(value);
  return m ? new Date(Number(m[1]), Number(m[2]) - 1, Number(m[3])) : null;
}
const isoDay = (d: Date): string => `${d.getFullYear()}-${String(d.getMonth() + 1).padStart(2, "0")}-${String(d.getDate()).padStart(2, "0")}`;

/** The deadline a type proposes: today plus its delay. */
export function defaultDeadline(type: Pick<ActivityType, "delayCount" | "delayUnit"> | undefined, today: Date): string {
  const d = new Date(today.getFullYear(), today.getMonth(), today.getDate());
  const n = type?.delayCount ?? 0;
  if (type?.delayUnit === "weeks") d.setDate(d.getDate() + 7 * n);
  else if (type?.delayUnit === "months") {
    // Jan 31 plus a month is the last day of February, not early March.
    const day = d.getDate();
    d.setDate(1);
    d.setMonth(d.getMonth() + n);
    d.setDate(Math.min(day, new Date(d.getFullYear(), d.getMonth() + 1, 0).getDate()));
  }
  else d.setDate(d.getDate() + n);
  return isoDay(d);
}

/** Due date as relative text ("Tomorrow", "In 3 days", "2 days ago"). */
export function relativeDay(value: string, today: Date): string {
  const day = parseDay(value);
  if (!day) return value;
  const base = new Date(today.getFullYear(), today.getMonth(), today.getDate());
  const diff = Math.round((day.getTime() - base.getTime()) / 86_400_000);
  const rel = new Intl.RelativeTimeFormat("en", { numeric: "auto" }).format(diff, "day");
  return rel.charAt(0).toUpperCase() + rel.slice(1);
}

/**
 * The planned-activities card and the header "Activities" button. It talks to the `mail.activity` model through the
 * ordinary ORM methods (`web_search_read`, `create`, `write`, `unlink`, `action_feedback`): the store has no activities
 * topic yet. Every failure renders inside the card; nothing reaches the global error dialog.
 */
export class Activities {
  /** The card; hidden while there is nothing to show. */
  readonly element: HTMLElement;
  /** The "Activities" header button that opens the schedule form. */
  readonly button: HTMLButtonElement;
  private readonly list = el("div", undefined, "erp-mail-activities-list");
  private readonly status = el("div", undefined, "erp-mail-activities-status");
  private readonly formHost = el("div", undefined, "erp-mail-activities-form-host");
  private readonly countBadge = el("span", undefined, "erp-mail-activities-count");
  private items: Activity[] = [];
  private types: ActivityType[] | null = null;
  private users: User[] | null = null;
  private disposed = false;
  /** Set while the schedule form is being prepared, so a second click does not open a second form. */
  private opening = false;
  /** Set while a change runs: the row actions are disabled and a second one is refused. */
  private busy = false;
  private readonly fail = (error: unknown): void => this.setError(errorMessage(error));

  constructor(private readonly o: ActivitiesOptions) {
    this.element = el("section", undefined, "erp-mail-activities");
    this.element.setAttribute("aria-label", "Activities");
    this.element.hidden = true;
    this.formHost.id = `erp-mail-activities-form-${++nextFormHost}`;
    this.element.append(this.status, this.list, this.formHost);
    this.button = el("button", undefined, "erp-mail-activities-button");
    this.button.type = "button";
    // A name from the start: the label can be hidden on a narrow column before the first load answers.
    this.button.setAttribute("aria-label", "Activity");
    this.button.setAttribute("aria-expanded", "false");
    this.button.setAttribute("aria-controls", this.formHost.id);
    this.button.append(icon("clock"), el("span", "Activity"), this.countBadge);
    this.button.addEventListener("click", () => {
      if (this.formHost.childElementCount) this.closeForm();
      else this.openForm().catch(this.fail);
    });
  }

  private today(): Date { return this.o.now?.() ?? new Date(); }

  /** Reads the planned activities of the record. */
  async load(): Promise<void> {
    try {
      const res = obj(await this.o.rpc.call("mail.activity", "web_search_read", {
        domain: [["res_model", "=", this.o.model], ["res_id", "=", this.o.recordId]],
        specification: { summary: {}, note: {}, date_deadline: {}, state: {}, can_write: {}, user_id: { fields: { display_name: {} } }, activity_type_id: { fields: { display_name: {}, icon: {} } } },
        order: "date_deadline, id",
      }), "activities answer");
      if (!Array.isArray(res["records"])) throw new ClientError("server", "activities answer has no records list");
      this.items = res["records"].map((r) => {
        const row = obj(r, "activity");
        const state = STATES.find((candidate) => candidate === row["state"]);
        if (!state) throw new ClientError("server", `activity ${String(row["id"])} has an unknown state`);
        const type = row["activity_type_id"] && typeof row["activity_type_id"] === "object" ? (row["activity_type_id"] as Record<string, unknown>) : null;
        const t = pair(type);
        return { id: positiveId(row["id"], "activity"), summary: text(row["summary"]), note: text(row["note"]), deadline: text(row["date_deadline"]), state, canWrite: row["can_write"] === true, user: pair(row["user_id"]), type: t ? { ...t, icon: text(type?.["icon"]) } : null } satisfies Activity;
      });
      this.setError(null);
    } catch (error) {
      this.setError(errorMessage(error), () => this.load().catch(this.fail));
    }
    this.render();
  }

  private setError(message: string | null, retry?: () => void): void {
    this.status.replaceChildren();
    if (message === null) return;
    const alert = el("div", undefined, "erp-mail-activities-error");
    alert.setAttribute("role", "alert");
    alert.append(el("span", message));
    if (retry) alert.append(button("Retry", retry, "erp-mail-retry"));
    this.status.append(alert);
    this.element.hidden = false;
  }

  private render(): void {
    if (this.disposed) return;
    this.list.replaceChildren();
    this.countBadge.textContent = this.items.length ? String(this.items.length) : "";
    this.button.setAttribute("aria-label", this.items.length ? `Activity: ${this.items.length}` : "Activity");
    const today = this.today();
    for (const group of GROUPS) {
      const rows = this.items.filter((a) => a.state === group.state);
      if (!rows.length) continue;
      const section = el("div", undefined, `erp-mail-activity-group is-${group.state}`);
      section.dataset["state"] = group.state;
      section.append(el("h3", `${group.title} (${rows.length})`, "erp-mail-activity-group-title"));
      const ul = el("ul", undefined, "erp-mail-activity-rows");
      for (const a of rows) ul.append(this.row(a, today));
      section.append(ul);
      this.list.append(section);
    }
    this.element.hidden = this.items.length === 0 && this.status.childElementCount === 0 && !this.formHost.childElementCount;
  }

  private row(a: Activity, today: Date): HTMLElement {
    const li = el("li", undefined, "erp-mail-activity");
    li.dataset["id"] = String(a.id);
    const glyph = el("span", undefined, "erp-mail-activity-icon");
    glyph.append(icon(a.type && TYPE_ICONS[a.type.icon] ? TYPE_ICONS[a.type.icon]! : "list-todo"));
    const main = el("div", undefined, "erp-mail-activity-main");
    main.append(el("div", a.summary || a.type?.name || "Activity", "erp-mail-activity-summary"));
    const due = el("time", relativeDay(a.deadline, today), "erp-mail-activity-due");
    due.title = a.deadline;
    due.dateTime = a.deadline;
    const meta = el("div", undefined, "erp-mail-activity-meta");
    meta.append(...(a.type ? [el("span", a.type.name), " · "] : []), due, ...(a.user ? [" · ", el("span", a.user.name, "erp-mail-activity-user")] : []));
    main.append(meta);
    li.append(glyph, main);
    if (a.canWrite) {
      const actions = el("div", undefined, "erp-mail-activity-actions");
      actions.setAttribute("role", "group");
      actions.setAttribute("aria-label", `Actions for ${a.summary || "activity"}`);
      actions.append(
        this.iconButton("square-check-big", "Mark done", "done", () => this.openDone(li, a)),
        this.iconButton("notebook-pen", "Edit", "edit", () => this.openForm(a).catch(this.fail)),
        this.iconButton("x", "Cancel activity", "cancel", () => this.cancel(a).catch(this.fail)),
      );
      li.append(actions);
    }
    return li;
  }

  private iconButton(name: IconName, label: string, action: string, run: () => void): HTMLButtonElement {
    const b = el("button", undefined, "erp-mail-activity-action");
    b.type = "button"; b.title = label; b.setAttribute("aria-label", label); b.dataset["action"] = action;
    b.append(icon(name));
    b.addEventListener("click", () => { if (!this.busy) run(); });
    return b;
  }

  /** A change went through: reload the card and tell the host. */
  private async changed(logged: boolean): Promise<void> {
    await this.load();
    if (logged) await this.o.onLogged?.();
    await this.o.onChange?.();
  }

  private setBusy(value: boolean): void {
    this.busy = value;
    for (const control of this.list.querySelectorAll<HTMLButtonElement>(".erp-mail-activity-action")) control.disabled = value;
  }

  private async mutate(run: () => Promise<unknown>, logged = false): Promise<boolean> {
    if (this.busy) return false;
    this.setBusy(true);
    try {
      try {
        this.setError(null);
        await run();
      } catch (error) {
        this.setError(errorMessage(error));
        return false;
      }
      try {
        await this.changed(logged);
      } catch (error) {
        this.setError(errorMessage(error));
      }
      return true;
    } finally {
      this.setBusy(false);
    }
  }

  /** After a row went away or the list was rebuilt, focus the action now at that place, or the toggle. */
  private focusRowAt(index: number): void {
    const rows = [...this.list.querySelectorAll<HTMLElement>(".erp-mail-activity")];
    (rows[Math.min(index, rows.length - 1)]?.querySelector<HTMLElement>(".erp-mail-activity-action") ?? this.button).focus();
  }

  private rowIndex(row: HTMLElement): number { return [...this.list.querySelectorAll(".erp-mail-activity")].indexOf(row); }

  private async cancel(a: Activity): Promise<boolean> {
    const at = this.rowIndex(this.list.querySelector<HTMLElement>(`[data-id="${a.id}"]`)!);
    const ok = await this.mutate(() => this.o.rpc.call("mail.activity", "unlink", { ids: [a.id] }, { write: true }));
    if (ok) this.focusRowAt(at);
    return ok;
  }

  private openDone(row: HTMLElement, a: Activity): void {
    this.formHost.replaceChildren();
    this.button.setAttribute("aria-expanded", "false");
    row.querySelector(".erp-mail-activity-done")?.remove();
    const form = el("form", undefined, "erp-mail-activity-done");
    form.setAttribute("aria-label", "Mark done");
    const field = el("textarea", undefined, "erp-mail-activity-feedback");
    field.rows = 2; field.placeholder = "Feedback (optional, logged in the thread)"; field.setAttribute("aria-label", "Feedback");
    const submit = el("button", "Done", "erp-mail-send");
    submit.type = "submit";
    const dismissForm = (): void => { form.remove(); row.querySelector<HTMLElement>('[data-action="done"]')?.focus(); };
    const dismiss = button("Cancel", dismissForm, "erp-mail-discard");
    form.addEventListener("keydown", (event) => { if (event.key === "Escape") { event.preventDefault(); event.stopPropagation(); dismissForm(); } });
    form.append(field, el("div", undefined, "erp-mail-activity-form-bar"));
    form.lastElementChild!.append(dismiss, submit);
    form.addEventListener("submit", (event) => {
      event.preventDefault();
      const feedback = field.value.trim();
      submit.disabled = true;
      const at = this.rowIndex(row);
      this.mutate(() => this.o.rpc.call("mail.activity", "action_feedback", { ids: [a.id], feedback: feedback || null, attachment_ids: [] }, { write: true }), true)
        .then((ok) => { if (ok) this.focusRowAt(at); else submit.disabled = false; })
        .catch((error) => { submit.disabled = false; this.fail(error); });
    });
    row.append(form);
    field.focus();
  }

  closeForm(): void {
    this.formHost.replaceChildren();
    this.button.setAttribute("aria-expanded", "false");
    this.render();
    this.button.focus();
  }

  private async loadTypes(): Promise<ActivityType[]> {
    if (this.types) return this.types;
    const res = obj(await this.o.rpc.call("mail.activity.type", "web_search_read", {
      domain: ["|", ["res_model", "=", false], ["res_model", "=", this.o.model]],
      specification: { name: {}, summary: {}, delay_count: {}, delay_unit: {}, default_user_id: { fields: { display_name: {} } } },
      order: "sequence, id",
    }), "activity types answer");
    if (!Array.isArray(res["records"])) throw new ClientError("server", "activity types answer has no records list");
    this.types = res["records"].map((r) => {
      const row = obj(r, "activity type");
      const delayCount = row["delay_count"];
      if (!Number.isSafeInteger(delayCount) || (delayCount as number) < 0) throw new ClientError("server", "activity type has no valid delay_count");
      const delayUnit = requiredText(row["delay_unit"], "activity type delay_unit");
      if (!UNITS.includes(delayUnit)) throw new ClientError("server", `activity type has an unknown delay_unit "${delayUnit}"`);
      return { id: positiveId(row["id"], "activity type"), name: requiredText(row["name"], "activity type name"), summary: text(row["summary"]), delayCount: delayCount as number, delayUnit, defaultUser: pair(row["default_user_id"])?.id ?? null };
    });
    return this.types;
  }

  private async searchUsers(name: string): Promise<User[]> {
    const found = await this.o.rpc.call("res.users", "web_name_search", { name, specification: { display_name: {} }, limit: 20 }, { background: true });
    if (!Array.isArray(found)) throw new ClientError("server", "the user search answer is not a list");
    return found.map((r) => user(r, "user"));
  }

  private async loadUsers(): Promise<User[]> {
    if (this.users) return this.users;
    const users = await this.searchUsers("");
    if (this.o.uid !== undefined && !users.some((u) => u.id === this.o.uid)) {
      const own = await this.o.rpc.call("res.users", "web_read", { ids: [this.o.uid], specification: { display_name: {} } }, { background: true });
      if (!Array.isArray(own) || own.length !== 1) throw new ClientError("server", "the signed-in user could not be read");
      users.unshift(user(own[0], "signed-in user"));
    }
    return (this.users = users);
  }

  /** Opens the schedule form (a new activity) or, given an activity, the edit form. */
  async openForm(editing?: Activity): Promise<void> {
    if (this.opening) return;
    this.opening = true;
    try {
      await this.buildForm(editing);
    } finally {
      this.opening = false;
    }
  }

  private async buildForm(editing?: Activity): Promise<void> {
    this.formHost.replaceChildren();
    this.element.hidden = false;
    this.button.setAttribute("aria-expanded", "true");
    let types: ActivityType[], loaded: User[];
    try {
      [types, loaded] = await Promise.all([this.loadTypes(), this.loadUsers()]);
    } catch (error) {
      this.setError(errorMessage(error));
      return;
    }
    if (this.disposed) return;
    // The edited activity's own assignee is always a choice, however far down the list or the search it is.
    const users = editing?.user && !loaded.some((u) => u.id === editing.user!.id) ? [editing.user, ...loaded] : loaded;
    const names = new Map(users.map((u) => [u.id, u.name]));
    const form = el("form", undefined, "erp-mail-activity-form");
    form.setAttribute("aria-label", editing ? "Edit activity" : "Schedule an activity");
    const select = el("select", undefined, "erp-mail-activity-type");
    for (const t of types) { const op = el("option", t.name); op.value = String(t.id); select.append(op); }
    const due = el("input", undefined, "erp-mail-activity-date"); due.type = "date"; due.required = true;
    const summary = el("input", undefined, "erp-mail-activity-summary-input"); summary.type = "text";
    const who = el("select", undefined, "erp-mail-activity-user-select");
    const fillWho = (list: readonly User[], keep: number): void => {
      who.replaceChildren();
      const shown = list.some((u) => u.id === keep) || !names.has(keep) ? list : [{ id: keep, name: names.get(keep)! }, ...list];
      for (const u of shown) { const op = el("option", u.name); op.value = String(u.id); who.append(op); }
      who.value = String(keep);
    };
    fillWho(users, users[0]?.id ?? 0);
    const find = el("input", undefined, "erp-mail-activity-user-search"); find.type = "search"; find.placeholder = "Search users"; find.setAttribute("aria-label", "Search users to assign");
    find.addEventListener("keydown", (event) => { if (event.key === "Enter") event.preventDefault(); });
    let searching = 0;
    find.addEventListener("input", () => {
      const ticket = ++searching;
      this.searchUsers(find.value.trim()).then((found) => {
        if (ticket !== searching || this.disposed) return;
        for (const u of found) names.set(u.id, u.name);
        fillWho(found, Number(who.value));
      }).catch(this.fail);
    });
    const field = (label: string, control: HTMLElement): HTMLElement => { const l = el("label", undefined, "erp-mail-activity-field"); l.append(el("span", label), control); return l; };
    let dueTouched = false, whoTouched = false;
    const typeNow = (): ActivityType | undefined => types.find((t) => String(t.id) === select.value);
    const applyDefaults = (): void => {
      const t = typeNow();
      summary.placeholder = t?.summary || "";
      if (!dueTouched) due.value = defaultDeadline(t, this.today());
      if (!whoTouched) {
        const pick = t?.defaultUser ?? this.o.uid;
        if (pick !== undefined && pick !== null && users.some((u) => u.id === pick)) who.value = String(pick);
      }
    };
    if (editing) {
      if (editing.type) select.value = String(editing.type.id);
      due.value = editing.deadline; summary.value = editing.summary; dueTouched = whoTouched = true;
      if (editing.user) who.value = String(editing.user.id);
      select.disabled = true;
    } else applyDefaults();
    select.addEventListener("change", applyDefaults);
    due.addEventListener("input", () => { dueTouched = true; });
    who.addEventListener("change", () => { whoTouched = true; });
    // The search box sits in the assignee field so it gets the same styling and grouping.
    const assignee = field("Assigned to", who);
    assignee.append(find);
    const note = el("textarea", undefined, "erp-mail-activity-note"); note.rows = 2; note.setAttribute("aria-label", "Note");
    if (editing) note.value = editing.note;
    const submit = el("button", editing ? "Save" : "Schedule", "erp-mail-send"); submit.type = "submit";
    const bar = el("div", undefined, "erp-mail-activity-form-bar");
    bar.append(button("Discard", () => this.closeForm(), "erp-mail-discard"), submit);
    form.append(field("Activity type", select), field("Due date", due), field("Summary", summary), assignee, field("Note", note), bar);
    form.addEventListener("keydown", (event) => { if (event.key === "Escape") { event.preventDefault(); event.stopPropagation(); this.closeForm(); } });
    form.addEventListener("submit", (event) => {
      event.preventDefault();
      if (!due.value) { this.setError("Choose a due date."); return; }
      // A select with no matching option reads as 0: refuse it instead of sending an id that is nobody's.
      const userId = Number(who.value), typeId = Number(select.value);
      if (!Number.isSafeInteger(userId) || userId <= 0) { this.setError("Choose who the activity is assigned to."); return; }
      if (!editing && (!Number.isSafeInteger(typeId) || typeId <= 0)) { this.setError("Choose an activity type."); return; }
      this.setError(null);
      const vals: Record<string, unknown> = { summary: summary.value.trim() || null, date_deadline: due.value, user_id: userId, note: note.value.trim() || null };
      submit.disabled = true;
      const run = editing
        ? () => this.o.rpc.call("mail.activity", "write", { ids: [editing.id], vals }, { write: true })
        : () => this.o.rpc.call("mail.activity", "create", { vals_list: [{ ...vals, res_model: this.o.model, res_id: this.o.recordId, activity_type_id: typeId }] }, { write: true });
      this.mutate(run)
        .then((ok) => { if (ok) this.closeForm(); else submit.disabled = false; })
        .catch((error) => { submit.disabled = false; this.fail(error); });
    });
    this.formHost.append(form);
    (editing ? summary : select).focus();
  }

  dispose(): void { this.disposed = true; this.element.remove(); this.button.remove(); }
}
