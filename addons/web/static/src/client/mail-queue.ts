/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
import { button, el } from "./dom.js";
import { integer, object } from "./metadata.js";
import { registerClientAction } from "./registry.js";
import { ClientError } from "./transport.js";

registerClientAction("mail.queue_health", {
  async create({ host, rpc, run }) {
    const summary = el("dl");
    let disposed = false;
    const refresh = async () => {
      const data = object(await rpc.call("mail.outgoing", "queue_health", {}), "queue health");
      if (disposed) return;
      const number = (key: string) => integer(data[key], key).toLocaleString();
      if (typeof data["local_runner"] !== "boolean" ||
          (data["latest_error"] !== null && typeof data["latest_error"] !== "string"))
        throw new ClientError("server", "Invalid job queue status.");
      const items = [
        ["Worker on this server", data["local_runner"] ? "Running" : "Stopped"],
        ["Waiting", number("waiting")],
        ["Running", number("running")],
        ["Oldest due email", data["oldest_due_seconds"] === null ? "None due" : number("oldest_due_seconds") + " s"],
        ["Latest delivery error", data["latest_error"] ?? "None"],
      ] as const;
      summary.replaceChildren(...items.flatMap(([key, value]) => [el("dt", key), el("dd", value)]));
    };
    const reload = button("Refresh", () => run(async () => {
      reload.disabled = true;
      try { await refresh(); } finally { reload.disabled = false; }
    }));
    await refresh();
    host.append(summary, reload);
    return { dispose() { disposed = true; } };
  },
});
