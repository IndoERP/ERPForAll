/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
import { button, choose, el } from "./dom.js";
import { array, object, text } from "./metadata.js";
import { registerClientAction, registerFieldWidget, type ClientActionEnvironment } from "./registry.js";
import { ClientError } from "./transport.js";

const MODEL = "res.users";
const METHOD = "erp_technical_sessions";
const fields = [
  ["created", "Signed in"], ["last_seen", "Last seen"], ["expires", "Expires"],
] as const;

async function technicalSessions(environment: ClientActionEnvironment) {
  const { host, rpc, run, notice } = environment;
  const surface = el("div");
  host.append(surface);
  let disposed = false;
  let reading = 0;
  const lifetime = new AbortController();
  const date = new Intl.DateTimeFormat(environment.editorLocale.lang.replace("_", "-"), {
    dateStyle: "medium", timeStyle: "short", timeZone: environment.editorLocale.tz,
  });
  const format = (value: unknown): string => date.format(new Date(text(value, "session time")));
  const read = async (args: Record<string, string>) =>
    object(await rpc.call(MODEL, METHOD, args), "browser sessions");
  const trust = (session: Record<string, unknown>): string => {
    const trusted = session["trusted"];
    if (typeof trusted !== "boolean") throw new ClientError("server", "Browser session is missing its trusted-device status.");
    return trusted ? "Yes" : "No";
  };

  const list = async (cursor?: string, previous: string[] = []): Promise<void> => {
    const current = ++reading;
    const data = await read(cursor === undefined ? {} : { cursor });
    if (disposed || current !== reading) return;
    const next = data["next"] === null ? null : text(data["next"], "session page cursor");
    const sessions = array(data["sessions"], "browser sessions");
    const section = el("section", undefined, "technical-sessions");
    if (sessions.length === 0) section.append(el("p", "No active browser sessions."));
    else {
      const table = el("table");
      table.setAttribute("aria-label", "Active browser sessions");
      const header = el("tr");
      for (const label of ["User", "Last seen", "Trusted device"]) {
        const cell = el("th", label);
        cell.scope = "col";
        header.append(cell);
      }
      const head = el("thead");
      head.append(header);
      const body = el("tbody");
      for (const item of sessions) {
        const session = object(item, "browser session");
        const id = text(session["id"], "session identifier");
        const user = text(session["user"], "session user");
        const row = el("tr");
        const userCell = el("td");
        userCell.append(button(user, () => run(() => detail(id, () => list(cursor, previous))), "link technical-session-open"));
        row.append(userCell, el("td", format(session["last_seen"])), el("td", trust(session)));
        body.append(row);
      }
      table.append(head, body);
      section.append(table);
    }
    const pager = el("div", undefined, "technical-session-controls");
    if (previous.length > 0) {
      const before = previous.at(-1);
      pager.append(button("Previous", () => run(() => list(before || undefined, previous.slice(0, -1)))));
    }
    if (next) pager.append(button("Next", () => run(() => list(next, [...previous, cursor ?? ""]))));
    section.append(pager);
    surface.replaceChildren(section);
  };

  const detail = async (id: string, back: () => Promise<void>): Promise<void> => {
    const current = ++reading;
    const data = await read({ session: id });
    if (disposed || current !== reading) return;
    const session = object(array(data["sessions"], "browser sessions")[0], "browser session");
    if (session["id"] !== id) throw new ClientError("server", "Returned browser session does not match the selected session.");
    const user = text(session["user"], "session user");
    const section = el("section", undefined, "technical-sessions technical-session-detail");
    const heading = el("h2", user);
    heading.tabIndex = -1;
    const details = el("dl");
    const pair = (label: string, value: string): void => { details.append(el("dt", label), el("dd", value)); };
    for (const [key, label] of fields) pair(label, format(session[key]));
    pair("Trusted device", trust(session));
    pair("Browser", session["agent"] === null ? "Unavailable" : text(session["agent"], "browser"));
    const controls = el("div", undefined, "technical-session-controls");
    const revoke = button("End Session", () => run(async () => {
      revoke.disabled = true;
      try {
        if (await choose("End browser session?", `${user} will need to sign in again on this device.`, ["End Session", "Cancel"], lifetime.signal) !== "End Session") return;
        const ended = await rpc.call(MODEL, "erp_revoke_technical_session", { session: id }, { write: true });
        if (disposed) return;
        notice(ended === true ? "Browser session ended." : "This browser session has already ended.");
        await back();
      } finally {
        revoke.disabled = false;
      }
    }), "danger");
    controls.append(button("Back to Sessions", () => run(back)), revoke);
    section.append(heading, details, controls);
    surface.replaceChildren(section);
    heading.focus();
  };

  await list();
  return { dispose() { disposed = true; lifetime.abort(); } };
}

registerClientAction("base.technical_sessions", { create: technicalSessions });

registerFieldWidget("technical_attachment", {
  kinds: ["char"],
  render({ model, value, values }) {
    const node = el("span", typeof value === "string" ? value : "");
    const id = values?.["id"];
    if (model === "ir.attachment" && values?.["type"] === "binary" && typeof id === "number" && Number.isSafeInteger(id) && id > 0) {
      const link = el("a", "Download", "technical-attachment-download");
      link.href = `/erp/content/${id}?download=true`;
      link.download = "";
      node.append(link);
    }
    return { node, dispose() {} };
  },
});
