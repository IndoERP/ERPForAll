/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
import { button, confirmChoice, el } from "./dom.js";
import { integer, object, text } from "./metadata.js";
import { registerClientAction } from "./registry.js";
import { ClientError } from "./transport.js";

const endpoint = "/erp/session/external/provider";
const keyStates: Record<string, string> = {
  ready: "Ready", not_loaded: "Waiting for the first sign-in", refresh_failed: "Last refresh failed",
  expired: "Keys expired",
};

registerClientAction("base.identity_provider", {
  async create({ host, run, csrfToken, editorLocale }) {
    const lifetime = new AbortController();
    const controls = el("div", undefined, "technical-session-controls");
    const summary = el("dl");
    const panel = el("section", undefined, "technical-sessions");
    panel.append(summary, controls);
    const format = new Intl.DateTimeFormat(editorLocale.lang.replace("_", "-"), {
      dateStyle: "medium", timeStyle: "short", timeZone: editorLocale.tz,
    });
    const timestamp = (raw: unknown): string => raw == null ? "—" : format.format(integer(raw, "refresh time") * 1000);
    const load = async (): Promise<void> => {
      const result = await fetch(endpoint, { credentials: "same-origin", cache: "no-store", signal: lifetime.signal });
      if (!result.ok) throw new ClientError("server", "Could not read the sign-in provider status.");
      const data = object(await result.json(), "provider status");
      if (lifetime.signal.aborted) return;
      const rows: [string, string][] = [];
      const enabled = data["enabled"] === true;
      const configured = data["configured"] === true;
      const neutralized = data["neutralized"] === true;
      if (configured) {
        const keys = object(data["keys"], "signing keys");
        rows.push(["Provider", text(data["label"], "provider")],
          ["Sign-in", neutralized ? "Disabled on this neutralized database" : enabled ? "Enabled" : "Disabled"],
          ["Issuer", text(data["issuer"], "issuer")], ["Redirect URI", text(data["callback_url"], "redirect URI")],
          ["Signing keys", keys["mode"] === "file" ? "Operator file" : "HTTPS key set"],
          ["Verification status", keyStates[text(keys["status"], "key status")] ?? "Unknown"],
          ["Cached signing keys", String(integer(keys["key_count"], "key count"))]);
        if (keys["mode"] === "remote") rows.push(["Last refresh attempt", timestamp(keys["last_attempt_unix"])],
          ["Last successful refresh", timestamp(keys["last_success_unix"])]);
      } else rows.push(["Sign-in provider", "Configure an OpenID Connect provider on the server."]);
      summary.replaceChildren(...rows.flatMap(([label, value]) => [el("dt", label), el("dd", value)]));
      const refresh = button("Refresh status", () => run(() => busy(load)));
      controls.replaceChildren(refresh);
      if (configured && !neutralized && csrfToken) {
        const toggle = button(enabled ? "Disable provider" : "Enable provider", () => run(() => busy(async () => {
          if (enabled && !await confirmChoice({ title: "Disable single sign-on?", message: "Active sessions issued by this provider will end.",
            accept: "Disable provider", cancel: "Cancel", signal: lifetime.signal })) return;
          const response = await fetch(endpoint, { method: "POST", credentials: "same-origin", signal: lifetime.signal,
            body: new URLSearchParams({ csrf_token: csrfToken, enabled: String(!enabled) }) });
          if (!response.ok) throw new ClientError("server", "Could not change the sign-in policy.");
          await load();
        })));
        controls.append(toggle);
      }
    };
    const busy = async (work: () => Promise<void>) => {
      const buttons = controls.querySelectorAll("button");
      buttons.forEach(b => { b.disabled = true; });
      try { await work(); }
      finally { buttons.forEach(b => { b.disabled = false; }); }
    };
    await load();
    host.append(panel);
    return { dispose() { lifetime.abort(); panel.remove(); } };
  },
});
