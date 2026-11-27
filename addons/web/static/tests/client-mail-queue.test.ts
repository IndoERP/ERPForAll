/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
import { expect, test, vi } from "vitest";
import { MessageServices } from "../src/client/messages.js";
import { clientActions } from "../src/client/registry.js";
import type { RpcClient } from "../src/client/transport.js";
import "../src/client/mail-queue.js";

test("queue monitor refreshes the current worker and distinguishes an empty backlog", async () => {
  const host = document.createElement("main");
  document.body.append(host);
  const call = vi.fn()
    .mockResolvedValueOnce({ local_runner: false, waiting: 0, running: 0, oldest_due_seconds: null, latest_error: null })
    .mockResolvedValueOnce({ local_runner: true, waiting: 3, running: 1, oldest_due_seconds: 14, latest_error: "transient" });
  const pending: Promise<void>[] = [];
  const handle = await clientActions.get("mail.queue_health").create({
    host, panel: document.createElement("div"), controls: document.createElement("div"),
    rpc: { call } as unknown as RpcClient, companies: [], notice: vi.fn(), run: fn => pending.push(fn()),
    messages: new MessageServices(host, error => { throw error; }),
    editorLocale: { lang: "en_US", tz: "UTC", date_format: "%m/%d/%Y", time_format: "%H:%M:%S" },
    action: { id: 1, name: "Outgoing Mail Queue", tag: "mail.queue_health", params: {} },
    restored: null, open: async () => {},
  });
  expect(host.textContent).toContain("Stopped");
  expect(host.textContent).toContain("None due");
  host.querySelector("button")!.click();
  await Promise.all(pending);
  expect(call).toHaveBeenLastCalledWith("mail.outgoing", "queue_health", {});
  expect(host.textContent).toContain("14 s");
  expect(host.textContent).toContain("transient");
  handle.dispose();
  host.remove();
});
