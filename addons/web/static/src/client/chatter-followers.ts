/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
import type { MessagingStore } from "@erpforall/mail-store";
import { applyStorePayload } from "@erpforall/mail-thread-view";
import { el, errorMessage } from "./dom.js";
import { icon } from "./icons.js";
import { ClientError, type RpcClient } from "./transport.js";

const FOLLOWERS = "mail.followers";
const SUBTYPE = "mail.message.subtype";
const PAGE = 100;

/** Options of the followers control in the chatter header. */
export interface FollowersOptions {
  readonly rpc: RpcClient;
  readonly store: MessagingStore;
  readonly model: string;
  readonly recordId: number;
  /** The signed-in user id, used once to find the user's own partner when the user does not follow yet. */
  readonly uid?: number;
  /** Runs after a successful follower change (the `reload_on_follower` form reload). */
  readonly onChange?: () => Promise<void>;
  readonly translate?: (text: string) => string | { toString(): string };
}

interface Follower { readonly id: number; readonly partnerId: number | null; readonly name: string; readonly subtypeIds: readonly number[] }
interface Subtype { readonly id: number; readonly name: string }

const record = (value: unknown, what: string): Record<string, unknown> => {
  if (value === null || typeof value !== "object" || Array.isArray(value)) throw new ClientError("server", `${what} is not an object`);
  return value as Record<string, unknown>;
};
const ids = (value: unknown, what: string): number[] => {
  if (!Array.isArray(value) || !value.every(Number.isSafeInteger)) throw new ClientError("server", `${what} is not an id list`);
  return value as number[];
};
const payload = (raw: unknown): string => typeof raw === "string" || raw instanceof Uint8Array ? (raw as string) : JSON.stringify(raw ?? {});

/**
 * The followers button of the chatter header and its popover: the count, the list, Follow / Unfollow for the
 * current user, removal of other followers and the current user's subtype choice. Every failure renders inside
 * the popover; nothing here opens a dialog or reaches the global error reporter.
 */
export class Followers {
  readonly element: HTMLElement;
  private readonly button: HTMLButtonElement;
  private readonly popover: HTMLElement;
  private readonly t: (text: string) => string;
  private count: number | null = null;
  private followers: Follower[] = [];
  private selfFollowerId: number | null = null;
  private selfPartnerId: number | null = null;
  private subtypes: Subtype[] | null = null;
  private more = false;
  private error: string | null = null;
  private busy = false;
  private disposed = false;

  constructor(private readonly o: FollowersOptions) {
    const tr = o.translate;
    this.t = tr ? (s) => String(tr(s)) : (s) => s;
    this.element = el("div", undefined, "erp-mail-followers");
    this.button = el("button", undefined, "erp-mail-followers-button");
    this.button.type = "button";
    this.button.setAttribute("aria-haspopup", "dialog");
    this.button.setAttribute("aria-expanded", "false");
    this.popover = el("div", undefined, "erp-mail-followers-popover");
    this.popover.setAttribute("role", "dialog");
    this.popover.setAttribute("aria-label", this.t("Followers"));
    this.popover.tabIndex = -1;
    this.popover.hidden = true;
    this.button.addEventListener("click", () => (this.popover.hidden ? this.open() : this.close(true)));
    this.popover.addEventListener("keydown", (event) => {
      if (event.key === "Escape") { event.stopPropagation(); this.close(true); }
    });
    this.element.append(this.button, this.popover);
    this.render();
  }

  /** Reads the first follower page (count, rows, own row) through the `thread` store topic. */
  async load(): Promise<void> {
    try {
      const res = record(await this.o.rpc.call("mail.store", "store_fetch", {
        wire: 1,
        topics: [{ name: "thread", params: { model: this.o.model, id: this.o.recordId, followers: true, followers_limit: PAGE } }],
      }), "store_fetch answer");
      const topics = Array.isArray(res["topics"]) ? (res["topics"] as unknown[]).map((t) => record(t, "topic")) : [];
      const topic = topics.find((t) => t["topic"] === "thread");
      if (!topic) throw new ClientError("server", "store_fetch answered no thread topic");
      if (topic["ok"] === false) {
        throw new ClientError("permission", (topic["error"] as { message?: string } | undefined)?.message ?? "The followers are not available.");
      }
      const panel = record(record(topic["data"], "thread topic")["followers"], "follower panel");
      const result = applyStorePayload(this.o.store, payload(res["store"]));
      if (result.status !== "applied") throw new ClientError("server", "The follower records could not be applied.");
      const rows = ids(panel["follower_ids"], "follower_ids");
      const own = panel["self_follower_id"];
      const read = (id: number): Follower => {
        const handle = this.o.store.record(FOLLOWERS, id);
        const partner = handle.get("partner_id") as number | null | undefined;
        return { id, partnerId: partner ?? null, name: "", subtypeIds: (handle.get("subtype_ids") as readonly number[] | undefined) ?? [] };
      };
      const list = rows.map(read);
      if (typeof own === "number" && !rows.includes(own)) list.push(read(own));
      const partnerIds = list.flatMap((f) => (f.partnerId === null ? [] : [f.partnerId]));
      const names = await this.names(partnerIds);
      this.followers = list.map((f) => ({ ...f, name: f.partnerId === null ? "" : names.get(f.partnerId) ?? `#${f.partnerId}` }));
      this.selfFollowerId = typeof own === "number" ? own : null;
      if (this.selfFollowerId !== null) this.selfPartnerId = this.followers.find((f) => f.id === own)?.partnerId ?? this.selfPartnerId;
      const count = Number(panel["count"]);
      if (!Number.isSafeInteger(count) || count < 0) {
        throw new ClientError("server", "store_fetch answered an invalid follower count");
      }
      this.count = count;
      this.more = panel["next_after"] !== null && panel["next_after"] !== undefined;
      this.error = null;
    } catch (error) {
      this.error = errorMessage(error);
    }
    this.render();
  }

  dispose(): void { this.disposed = true; this.element.remove(); }

  private async names(partnerIds: number[]): Promise<Map<number, string>> {
    const out = new Map<number, string>();
    if (!partnerIds.length) return out;
    const rows = await this.o.rpc.call("res.partner", "web_read", { ids: partnerIds, specification: { display_name: {} } }, { background: true });
    for (const row of Array.isArray(rows) ? rows : []) {
      const r = record(row, "partner");
      out.set(Number(r["id"]), String(r["display_name"] ?? ""));
    }
    return out;
  }

  private async ownPartner(): Promise<number> {
    if (this.selfPartnerId !== null) return this.selfPartnerId;
    if (this.o.uid === undefined) throw new ClientError("unsupported", "The current user is unknown, so it cannot follow.");
    const rows = await this.o.rpc.call("res.users", "web_read", { ids: [this.o.uid], specification: { partner_id: { fields: { display_name: {} } } } }, { background: true });
    const partner = Array.isArray(rows) && rows[0] ? record(rows[0], "user")["partner_id"] : null;
    const id = partner && typeof partner === "object" ? Number((partner as Record<string, unknown>)["id"]) : Number(partner);
    if (!Number.isSafeInteger(id) || id < 1) throw new ClientError("server", "The current user has no partner.");
    return (this.selfPartnerId = id);
  }

  private async loadSubtypes(): Promise<void> {
    const res = record(await this.o.rpc.call(this.o.model, "thread_subtypes", { ids: [this.o.recordId] }), "thread_subtypes answer");
    const result = applyStorePayload(this.o.store, payload(res["store"]));
    if (result.status !== "applied") throw new ClientError("server", "The subtype records could not be applied.");
    this.subtypes = ids(res["subtype_ids"], "subtype_ids").map((id) => ({ id, name: String(this.o.store.record(SUBTYPE, id).get("name") ?? `#${id}`) }));
  }

  /** Runs one follower change; the failure stays in the popover and the list is read again either way. */
  private async change(call: () => Promise<unknown>): Promise<void> {
    if (this.busy) return;
    const focusKey = this.popover.contains(document.activeElement)
      ? (document.activeElement as HTMLElement).dataset["key"]
      : undefined;
    this.busy = true; this.error = null; this.render();
    try {
      await call();
      await this.load();
      await this.o.onChange?.();
    } catch (error) {
      this.error = errorMessage(error);
    } finally {
      this.busy = false;
      if (!this.disposed) this.render(focusKey);
    }
  }

  private write(method: string, args: Record<string, unknown>): Promise<unknown> {
    return this.o.rpc.call(this.o.model, method, { ids: [this.o.recordId], ...args }, { write: true });
  }

  private open(): void {
    this.popover.hidden = false;
    this.button.setAttribute("aria-expanded", "true");
    if (this.subtypes === null) void this.loadSubtypes().then(() => this.render(), (error) => { this.error = errorMessage(error); this.render(); });
    this.render();
    this.popover.querySelector<HTMLElement>("button, input")?.focus();
  }

  private close(refocus: boolean): void {
    this.popover.hidden = true;
    this.button.setAttribute("aria-expanded", "false");
    if (refocus) this.button.focus();
  }

  private render(restoreKey?: string): void {
    if (this.disposed) return;
    const t = this.t;
    const following = this.selfFollowerId !== null;
    const label = this.count === null ? (this.error ? "!" : "…") : String(this.count);
    this.button.replaceChildren(icon("users"), el("span", t("Followers"), "erp-mail-sr-only"), el("span", label, "erp-mail-followers-count"));
    this.button.setAttribute("aria-label", this.count === null ? t("Followers") : `${t("Followers")}: ${this.count}`);
    this.button.classList.toggle("is-following", following);
    if (this.popover.hidden) return;
    const focused = restoreKey ?? (this.popover.contains(document.activeElement) ? (document.activeElement as HTMLElement).dataset["key"] : undefined);
    const body: HTMLElement[] = [];
    if (this.error) {
      const alert = el("div", undefined, "erp-mail-followers-error");
      alert.setAttribute("role", "alert");
      const retry = el("button", t("Retry"), "erp-mail-retry"); retry.type = "button";
      retry.addEventListener("click", () => { this.error = null; void this.load(); });
      alert.append(el("span", this.error), retry);
      body.push(alert);
    }
    const toggle = el("button", following ? t("Unfollow") : t("Follow"), "erp-mail-followers-toggle");
    toggle.type = "button"; toggle.dataset["key"] = "toggle"; toggle.disabled = this.busy || this.count === null;
    toggle.addEventListener("click", () => void this.change(async () => {
      const partner = await this.ownPartner();
      if (following) await this.write("thread_unfollow", { partner_ids: [partner] });
      else await this.write("thread_follow", { partner_ids: [partner] });
    }));
    body.push(toggle);
    if (this.count === null && !this.error) body.push(el("p", t("Loading followers…"), "erp-mail-followers-status"));
    else if (this.count === 0) body.push(el("p", t("No followers yet."), "erp-mail-followers-status"));
    const list = el("ul", undefined, "erp-mail-followers-list");
    list.setAttribute("aria-label", t("Followers"));
    for (const f of this.followers) {
      const item = el("li", undefined, "erp-mail-follower");
      const remove = el("button", "×", "erp-mail-follower-remove"); remove.type = "button";
      remove.setAttribute("aria-label", `${t("Remove follower")} ${f.name}`);
      remove.dataset["key"] = `remove-${f.id}`; remove.disabled = this.busy || f.partnerId === null;
      remove.addEventListener("click", () => void this.change(() => this.write("thread_unfollow", { partner_ids: [f.partnerId] })));
      item.append(el("span", f.name), remove);
      list.append(item);
    }
    if (this.followers.length) body.push(list);
    if (this.more) body.push(el("p", t("More followers exist than are listed."), "erp-mail-followers-status"));
    const self = this.followers.find((f) => f.id === this.selfFollowerId);
    if (self && this.subtypes?.length) {
      const set = el("fieldset", undefined, "erp-mail-follower-subtypes");
      set.append(el("legend", t("Receive notifications for")));
      for (const subtype of this.subtypes) {
        const box = el("input"); box.type = "checkbox"; box.checked = self.subtypeIds.includes(subtype.id);
        box.disabled = this.busy; box.dataset["key"] = `subtype-${subtype.id}`;
        box.addEventListener("change", () => {
          const next = box.checked ? [...self.subtypeIds, subtype.id] : self.subtypeIds.filter((s) => s !== subtype.id);
          void this.change(() => this.write("thread_set_subtypes", { partner_id: self.partnerId, subtype_ids: next }));
        });
        const row = el("label", undefined, "erp-mail-follower-subtype"); row.append(box, el("span", subtype.name));
        set.append(row);
      }
      body.push(set);
    }
    this.popover.replaceChildren(...body);
    if (this.busy) {
      if (focused && !this.popover.contains(document.activeElement)) {
        this.popover.focus();
      }
    } else if (focused) {
      const target = this.popover.querySelector<HTMLElement>(`[data-key="${focused}"]:not([disabled])`);
      if (target) {
        target.focus();
      } else {
        const toggle = this.popover.querySelector<HTMLElement>('[data-key="toggle"]:not([disabled])');
        if (toggle) {
          toggle.focus();
        } else {
          this.popover.focus();
        }
      }
    }
  }
}
