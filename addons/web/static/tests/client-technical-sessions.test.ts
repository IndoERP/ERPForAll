/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
import { afterEach, expect, test, vi } from "vitest";
import { MessageServices } from "../src/client/messages.js";
import { clientActions } from "../src/client/registry.js";
import type { RpcClient } from "../src/client/transport.js";
import "../src/client/technical-sessions.js";

afterEach(() => document.body.replaceChildren());

test("a session row opens and returns after the shell moves the mounted action", async () => {
  const staging = document.createElement("div");
  const content = document.createElement("main");
  const panel = document.createElement("div");
  const controls = document.createElement("div");
  const notice = vi.fn();
  const feedback = document.createElement("div");
  document.body.append(content, feedback);
  const messages = new MessageServices(feedback, error => { throw error; });
  staging.append(panel);
  const when = "2026-11-01T10:00:00Z";
  const entry = { id: "browser-session", user: "Operator", trusted: true, created: when, last_seen: when, expires: when, agent: "Chrome" };
  const call = vi.fn(async () => ({ sessions: [entry], next: null }));
  const jobs: Promise<void>[] = [];
  const handle = await clientActions.get("base.technical_sessions").create({
    host: staging, panel, controls, rpc: { call } as unknown as RpcClient,
    companies: [], run: job => { jobs.push(job()); }, notice, messages,
    editorLocale: { lang: "en_US", tz: "UTC", date_format: "%m/%d/%Y", time_format: "%H:%M:%S" },
    action: { id: 1, name: "Sessions and Devices", tag: "base.technical_sessions", params: {} },
    restored: null, open: async () => {},
  });
  content.replaceChildren(...staging.childNodes);
  content.querySelector<HTMLButtonElement>(".technical-session-open")!.click();
  await jobs.shift();
  expect(call).toHaveBeenLastCalledWith("res.users", "erp_technical_sessions", { session: entry.id });
  const detail = content.querySelector(".technical-session-detail")!;
  expect(detail.textContent).toContain("Chrome");
  Array.from(detail.querySelectorAll("button")).find(button => button.textContent === "Back to Sessions")!.click();
  await jobs.shift();
  expect(content.querySelector(".technical-session-open")?.textContent).toBe("Operator");
  expect(content.contains(panel)).toBe(true);
  handle.dispose();
});
