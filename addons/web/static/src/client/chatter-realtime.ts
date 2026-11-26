/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
import { ClientError } from "./transport.js";
import type { ThreadRef } from "@erpforall/mail-store";

/** The socket carries invalidations. Only the existing permitted mail.store read populates the UI. */
export function watchChatterThread(ref: ThreadRef, policy: { company: number; csrf: string; retry: readonly [number, number] }, refresh: () => Promise<void>, report: (error: unknown) => void,
  connect: (url: string) => WebSocket = url => new WebSocket(url)): () => void {
  let closed = false;
  let busy = false;
  let dirty = false;
  let socket: WebSocket | undefined;
  let timer: ReturnType<typeof setTimeout> | undefined;
  let delay = policy.retry[0];
  const url = new URL("/erp/bus/websocket", location.href);
  url.protocol = url.protocol === "https:" ? "wss:" : "ws:";
  async function resync() {
    dirty = true;
    if (busy) return;
    busy = true;
    try {
      while (dirty && !closed) {
        dirty = false;
        await refresh();
      }
    } catch (error) { if (!closed) report(error); }
    finally { busy = false; }
  }
  function open() {
    if (closed) return;
    try { socket = connect(url.href); }
    catch (error) { report(error); return; }
    const current = socket;
    current.addEventListener("open", () => {
      if (!closed) current.send(JSON.stringify({type:"subscribe", model:ref.model, id:ref.id, company: policy.company, csrf: policy.csrf}));
    });
    current.addEventListener("message", event => {
      if (closed) return;
      if (event.data !== '{"type":"resync"}') {
        closed = true; current.close();
        report(new Error("Thread updates are unavailable because the server returned an unexpected frame."));
        return;
      }
      delay = policy.retry[0];
      void resync();
    });
    current.addEventListener("close", event => {
      if (closed) return;
      if (event.code === 1008) {
        report(new ClientError("permission", "Live thread updates are unavailable for this record. Refresh to check access."));
        return;
      }
      // Transport recovery uses bounded exponential backoff with jitter to avoid synchronized reconnects.
      timer = setTimeout(open, Math.min(policy.retry[1], delay * (1 + Math.random() / 4)));
      delay = Math.min(policy.retry[1], delay * 2);
    });
  }
  open();
  return () => { closed = true; clearTimeout(timer); socket?.close(); };
}
