/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
import { button, el } from "./dom.js";

interface Person { id: number; name: string }
export type DiscussCall = (method: string, args: Record<string, unknown>, write?: boolean) => Promise<unknown>;

export function peoplePicker(host: HTMLElement, call: DiscussCall, invite: number | null,
  done: (channel: number) => Promise<void>, close: () => void, failed: (error: unknown) => void): () => void {
  const heading = el("h3", invite ? "Invite people" : "New message");
  const query = el("input");
  query.type = "search";
  query.placeholder = "Search people";
  query.setAttribute("aria-label", "Search people");
  query.maxLength = 120;
  const choices = el("div", undefined, "discuss-people");
  const chips = el("div", undefined, "discuss-chips");
  const group = el("input");
  group.placeholder = "Group name";
  group.setAttribute("aria-label", "Group name");
  group.maxLength = 120;
  const status = el("p", undefined, "discuss-status");
  status.setAttribute("role", "status");
  const selected = new Map<number, string>();
  const form = el("form", undefined, "discuss-picker");
  form.setAttribute("aria-label", heading.textContent!);
  const submit = el("button", undefined, "primary");
  submit.type = "submit";
  const dismiss = button("Cancel", close);
  let active = true, token = 0, busy = false, result: Person[] = [];
  let timer: ReturnType<typeof setTimeout>;
  function draw() {
    choices.replaceChildren();
    chips.replaceChildren();
    for (const [id, name] of selected) {
      const remove = button(name, () => { selected.delete(id); draw(); query.focus(); });
      remove.setAttribute("aria-label", `Remove ${name}`);
      chips.append(remove);
    }
    for (const person of result.filter(p => !selected.has(p.id))) {
      choices.append(button(person.name, () => {
        selected.set(person.id, person.name);
        draw();
        query.focus();
      }));
    }
    if (result.length === 0) choices.append(el("p", "No matching people."));
    group.hidden = invite !== null || selected.size < 2;
    submit.textContent = invite ? "Invite" : selected.size > 1 ? "Create group" : "Start conversation";
    submit.disabled = busy || selected.size === 0 || (!group.hidden && !group.value.trim());
  }
  async function search(request: number) {
    try {
      const rows = await call("conversation_people", { query: query.value.trim() });
      if (!active || token !== request) return;
      if (!Array.isArray(rows) || rows.length > 50 || rows.some(r => !Number.isSafeInteger(r?.id) || r.id < 1 || typeof r.name !== "string")) {
        throw new Error("Invalid people search results.");
      }
      result = rows;
      draw();
    } catch (e) { if (active && token === request) failed(e); }
  }
  query.addEventListener("input", () => {
    clearTimeout(timer);
    const request = ++token;
    result = [];
    choices.replaceChildren();
    timer = setTimeout(() => void search(request), 150);
  });
  group.addEventListener("input", draw);
  form.addEventListener("submit", async e => {
    e.preventDefault();
    if (busy || submit.disabled) return;
    busy = true;
    submit.disabled = true;
    try {
      let id = invite;
      if (invite) {
        for (const user_id of [...selected.keys()]) {
          if (!active) return;
          await call("conversation_invite", { ids: [invite], user_id }, true);
          selected.delete(user_id);
        }
      } else {
        const answer = await call("conversation_start", { user_ids: [...selected.keys()], ...(group.hidden ? {} : { name: group.value.trim() }) }, true);
        if (typeof answer !== "number" || !Number.isSafeInteger(answer) || answer < 1) throw new Error("Invalid conversation.");
        id = answer;
      }
      if (active && id) await done(id);
    } catch (error) {
      if (active) {
        status.textContent = error instanceof Error ? error.message : String(error);
        failed(error);
      }
    } finally { busy = false; if (active) draw(); }
  });
  form.addEventListener("keydown", e => { if (e.key === "Escape") { e.stopPropagation(); close(); } });
  const actions = el("div", undefined, "discuss-picker-actions");
  actions.append(dismiss, submit);
  form.append(heading, query, chips, choices, group, actions, status);
  host.replaceChildren(form);
  draw();
  query.focus();
  void search(++token);
  return () => { active = false; clearTimeout(timer); form.remove(); };
}
