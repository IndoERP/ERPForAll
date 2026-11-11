/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
import { button, el } from "./dom.js";
import { ClientError } from "./transport.js";
import { notificationClasses } from "./notification-classes.js";

export type NotificationLevel = "info" | "success" | "warning" | "danger";
export interface NotificationOptions {
  readonly title?: string;
  readonly level?: NotificationLevel;
  readonly sticky?: boolean;
  /** Bounded addon CSS identifier hooks; admitted independently of severity. */
  readonly className?: string;
  readonly delay?: number;
  readonly actions?: readonly { readonly label: string; readonly run: () => void | Promise<void> }[];
  readonly onClose?: () => void;
}
const MAX_NOTICES = 32, MAX_BYTES = 65_536;
const LIMIT_MESSAGE = "Notification limit exceeded: 32 notices or 65536 UTF-8 bytes.";
interface Entry { readonly node: HTMLElement; readonly bytes: number; close(): void; detach(): void; notify(): void }

/** Browser-scope notices. A modal owner supplies a host within its own modal. */
export class NotificationService {
  readonly node = el("section", undefined, "client-notifications");
  private readonly entries = new Map<string, Entry>();
  private readonly overflow = el("p", undefined, "client-notification-overflow");
  private bytes = 0;
  private dead = false;
  constructor(host: HTMLElement, private readonly report: (error: unknown) => void) {
    this.node.setAttribute("aria-label", "Notifications");
    this.overflow.setAttribute("role", "alert"); this.overflow.hidden = true;
    this.node.append(this.overflow); host.append(this.node);
  }
  add(message: string, options: NotificationOptions = {}): () => void {
    if (this.dead) throw new ClientError("validation", "Notification service is disposed.");
    if (typeof message !== "string") throw new ClientError("validation", "Notification message must be text.");
    if (options === null || typeof options !== "object" || Array.isArray(options)) throw new ClientError("validation", "Notification options must be an object.");
    const keys = ["title", "level", "sticky", "className", "delay", "actions", "onClose"];
    for (const key of Object.keys(options)) if (!keys.includes(key)) throw new ClientError("validation", `Unknown notification option: ${key}.`);
    const { title, sticky = false, delay = 4000, actions = [], onClose } = options;
    const className = Object.hasOwn(options, "className") ? notificationClasses(options.className, "notification") : "";
    const level = options.level === undefined ? "info" : options.level;
    if (typeof title !== "undefined" && typeof title !== "string") throw new ClientError("validation", "Notification title must be text.");
    if (!["info", "success", "warning", "danger"].includes(level)) throw new ClientError("validation", "Invalid notification severity.");
    if (typeof sticky !== "boolean") throw new ClientError("validation", "Notification sticky option must be boolean.");
    if (typeof delay !== "number" || !Number.isFinite(delay) || delay < 0 || delay > 2_147_483_647) throw new ClientError("validation", "Notification delay must be a finite number from 0 to 2147483647.");
    if (onClose !== undefined && typeof onClose !== "function") throw new ClientError("validation", "Notification close callback must be a function.");
    if (!Array.isArray(actions) || actions.length > 16) throw new ClientError("validation", "Notification actions must be an array of at most 16 controls.");
    for (const action of actions) {
      if (action === null || typeof action !== "object" || typeof action.label !== "string" || typeof action.run !== "function") throw new ClientError("validation", "Notification action requires a text label and callback.");
      if (Object.keys(action).some(key => key !== "label" && key !== "run")) throw new ClientError("validation", "Unknown notification action option.");
    }
    const content = [message, title ?? "", className, ...actions.map(action => action.label)];
    const size = content.some(value => value.length > MAX_BYTES) ? MAX_BYTES + 1 : content.reduce((sum, value) => sum + new TextEncoder().encode(value).byteLength, 0);
    const prior = this.entries.get(message);
    const fits = (): boolean => this.entries.size + (this.entries.has(message) ? 0 : 1) <= MAX_NOTICES && this.bytes - (this.entries.get(message)?.bytes ?? 0) + size <= MAX_BYTES;
    if (!fits()) return this.refuse();
    // Reserve the replacement before invoking user callbacks: onClose may add the same message.
    // No user code runs between detaching the predecessor and publishing this entry.
    prior?.detach();
    const node = el("div", undefined, "client-notification");
    if (className) node.classList.add(...className.split(" "));
    node.dataset["level"] = level; node.setAttribute("role", "alert"); node.setAttribute("aria-live", "assertive"); node.setAttribute("aria-atomic", "true");
    if (title !== undefined) node.append(el("strong", title, "client-notification-title"));
    node.append(el("p", message, "client-notification-message"));
    const controls = el("div", undefined, "client-notification-actions");
    let timer: ReturnType<typeof setTimeout> | undefined, closed = false, notified = false;
    const clear = (): void => { clearTimeout(timer); timer = undefined; };
    const detach = (): void => {
      if (closed) return; closed = true; clear(); node.remove();
      if (this.entries.get(message)?.node === node) { this.entries.delete(message); this.bytes -= size; }
      if (this.entries.size < MAX_NOTICES && this.bytes < MAX_BYTES) { this.overflow.hidden = true; this.overflow.textContent = ""; }
    };
    const notify = (): void => {
      if (notified) return; notified = true;
      if (onClose) { try { onClose(); } catch (error) { this.report(error); } }
    };
    const close = (): void => { detach(); notify(); };
    const reset = (): void => { clear(); if (!closed && !this.dead && !sticky && delay > 0) timer = setTimeout(close, delay); };
    for (const action of actions) {
      let pending = false;
      const control = button(action.label, () => {
        if (pending || closed || this.dead) return;
        pending = true; control.disabled = true;
        void (async () => {
          try { await action.run(); } catch (error) { if (!this.dead) this.report(error); }
          finally { pending = false; if (!closed && !this.dead) control.disabled = false; }
        })();
      }); controls.append(control);
    }
    const dismiss = button("×", close, "client-notification-close"); dismiss.setAttribute("aria-label", "Close notification"); controls.append(dismiss); node.append(controls);
    node.addEventListener("mouseenter", clear); node.addEventListener("mouseleave", reset);
    this.entries.set(message, { node, bytes: size, close, detach, notify }); this.bytes += size;
    this.node.insertBefore(node, this.overflow); reset(); prior?.notify(); return close;
  }
  private refuse(): never {
    this.overflow.textContent = "More notifications arrived. Close a notification to make room. The latest notice could not be shown.";
    this.overflow.hidden = false;
    throw new ClientError("validation", LIMIT_MESSAGE);
  }
  dispose(): void {
    if (this.dead) return; this.dead = true;
    for (const entry of [...this.entries.values()]) entry.close();
    this.node.remove();
  }
}
