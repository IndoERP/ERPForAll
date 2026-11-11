/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
import { afterEach, expect, test, vi } from "vitest";
import { NotificationService } from "../src/client/notifications.js";
import { BlockUi } from "../src/client/block-ui.js";
import { NetworkStatus } from "../src/client/network-status.js";
import { RpcClient } from "../src/client/transport.js";
import { EffectService } from "../src/client/effects.js";

afterEach(() => { vi.useRealTimers(); vi.unstubAllGlobals(); document.body.replaceChildren(); });
test("blocking starts immediately, delays feedback, nests balanced requests and clears timers", () => {
  vi.useFakeTimers(); const block = new BlockUi(document.body);
  const first = block.begin({ delay: 100 }); const overlay = document.querySelector<HTMLElement>(".client-block-ui")!;
  expect(overlay.isConnected).toBe(true); expect(overlay.dataset["visible"]).toBe("false");
  vi.advanceTimersByTime(100); expect(overlay.dataset["visible"]).toBe("true"); expect(overlay.textContent).toContain("Working");
  const second = block.begin({ message: "Preparing report", delay: 0 }); expect(overlay.textContent).toContain("Preparing report");
  first(); first(); expect(overlay.isConnected).toBe(true); second(); expect(overlay.isConnected).toBe(false); expect(vi.getTimerCount()).toBe(0);
  const fast = block.begin({ delay: 100 }); fast(); vi.advanceTimersByTime(300); expect(document.querySelector(".client-block-ui")).toBeNull(); block.dispose();
});
test("block disposal is unconditional and malformed options refuse", () => {
  vi.useFakeTimers(); const block = new BlockUi(document.body);
  expect(() => block.begin({ delay: -1 })).toThrowError(expect.objectContaining({ kind: "validation", message: "Block delay must be a finite number from 0 to 2147483647." }));
  const release = block.begin({ delay: 1000 }); block.dispose(); release(); vi.runAllTimers(); expect(document.querySelector(".client-block-ui")).toBeNull(); expect(vi.getTimerCount()).toBe(0);
});
/// (partial: explicit network status presentation; transport failure/recovery integration is app-owned)
test("network distinguishes offline, synchronizing and sync error without invoking transport", () => {
  const status = new NetworkStatus(document.body); const node = document.querySelector<HTMLElement>(".client-network-status")!;
  expect(node.hidden).toBe(true); status.set("offline"); expect(node.textContent).toContain("Offline"); expect(node.dataset["state"]).toBe("offline");
  status.set("syncing"); expect(node.textContent).toContain("Reconnecting"); status.set("error"); expect(node.textContent).toContain("reconnect");
  status.set("online"); expect(node.hidden).toBe(true); status.dispose(); expect(node.isConnected).toBe(false);
});
test("registered default effect overlays when enabled and notifies when effects are disabled", () => {
  vi.useFakeTimers(); const notices = new NotificationService(document.body, () => {}); const effects = new EffectService(document.body, notices, () => {});
  effects.show("default", { message: "Completed", enabled: true }); expect(document.querySelector(".client-effect")?.textContent).toContain("Completed");
  effects.show("default", { message: "Quiet completion", enabled: false }); expect(document.querySelector(".client-notification")?.textContent).toContain("Quiet completion");
  expect(() => effects.show("unknown", { message: "x", enabled: true })).toThrowError(expect.objectContaining({ kind: "unsupported", message: "Unknown effect: unknown." })); effects.dispose(); notices.dispose(); expect(vi.getTimerCount()).toBe(0);
});
test("effect registry refuses duplicates, honors reduced motion, and disposes custom callbacks", () => {
  vi.useFakeTimers(); vi.stubGlobal("matchMedia", () => ({ matches: true }));
  const report = vi.fn(); const notices = new NotificationService(document.body, report); const effects = new EffectService(document.body, notices, report);
  effects.show("default", { message: "Quiet", enabled: true }); expect(document.querySelector(".client-effect")?.classList.contains("reduced-motion")).toBe(true);
  const cleanup = vi.fn(); effects.register("custom", () => cleanup); effects.show("custom", { message: "Custom", enabled: true });
  expect(() => effects.register("custom", () => cleanup)).toThrowError(expect.objectContaining({ kind: "validation", message: "Duplicate effect: custom." }));
  const error = new Error("effect failed"); effects.register("failure", () => { throw error; }); expect(() => effects.show("failure", { message: "Failure", enabled: true })).toThrow(error); expect(report).toHaveBeenCalledWith(error);
  effects.dispose(); expect(cleanup).toHaveBeenCalledTimes(1); notices.dispose(); expect(vi.getTimerCount()).toBe(0);
});
test("default blocking feedback stages progress and custom text stays stable", () => {
  vi.useFakeTimers(); const block = new BlockUi(document.body); const release = block.begin({ delay: 0 });
  expect(block.blocked).toBe(true); vi.advanceTimersByTime(10_000); expect(document.querySelector(".client-block-message")?.textContent).toBe("Still working. Please wait…");
  release(); expect(block.blocked).toBe(false); const custom = block.begin({ delay: 0, message: "Calculating tax" }); vi.advanceTimersByTime(20_000); expect(document.querySelector(".client-block-message")?.textContent).toBe("Calculating tax"); custom(); block.dispose(); expect(vi.getTimerCount()).toBe(0);
});
test("effect and network boundary errors identify their kinds and messages", () => {
  const notices = new NotificationService(document.body, () => {}); const effects = new EffectService(document.body, notices, () => {}); const status = new NetworkStatus(document.body);
  expect(() => effects.show("default", { message: "x", enabled: "yes" as unknown as boolean })).toThrowError(expect.objectContaining({ kind: "validation", message: "Effect options require text message and boolean enabled." }));
  expect(() => status.set("missing" as "online")).toThrowError(expect.objectContaining({ kind: "validation", message: "Invalid connection state." }));
  status.dispose(); expect(() => status.set("online")).toThrowError(expect.objectContaining({ kind: "validation", message: "Network status is disposed." })); effects.dispose(); notices.dispose();
});

test("cancelled recovery reads settle without losing concurrent requests or replaying writes", async () => {
  const status = new NetworkStatus(document.body);
  const fetcher = vi.fn((_path: RequestInfo | URL, init?: RequestInit) => new Promise<Response>((_resolve, reject) => {
    init?.signal?.addEventListener('abort', () => reject(new DOMException('Read cancelled', 'AbortError')), { once: true });
  }));
  const rpc = new RpcClient(fetcher, () => {}, undefined, undefined, (event, options) => status.observe(event, options));
  status.set('offline');
  const first = new AbortController(), second = new AbortController();
  const a = rpc.call('test.record', 'web_read', {}, { signal: first.signal }).catch(error => error);
  const b = rpc.call('test.record', 'web_read', {}, { signal: second.signal }).catch(error => error);
  expect(status.node.dataset['state']).toBe('syncing');
  first.abort(); expect(await a).toMatchObject({name:'AbortError'});
  expect(status.node.dataset['state']).toBe('syncing');
  second.abort(); expect(await b).toMatchObject({name:'AbortError'});
  expect(status.node.dataset['state']).toBe('offline');
  expect(fetcher).toHaveBeenCalledTimes(2);
  expect(fetcher.mock.calls.every(([, options]) => options?.signal !== undefined)).toBe(true);
  status.dispose();
});

test("recovery cancellation retains error or successful concurrent read state", () => {
 const status=new NetworkStatus(document.body);
 status.set('error');status.observe('request',{});status.observe('cancelled',{});expect(status.node.dataset['state']).toBe('error');
 status.set('offline');status.observe('request',{});status.observe('request',{});status.observe('online',{});status.observe('cancelled',{});expect(status.node.hidden).toBe(true);
 status.observe('request',{write:true});status.observe('offline',{write:true});expect(status.node.dataset['state']).toBe('offline');
 status.observe('request',{write:true});status.observe('online',{write:true});expect(status.node.dataset['state']).toBe('offline');status.dispose();
});
