/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
import { el } from "./dom.js";
import { Registry } from "./registry.js";
import { ClientError } from "./transport.js";
import type { NotificationService } from "./notifications.js";

export interface EffectOptions { readonly message: string; readonly enabled: boolean }
export type Effect = (options: EffectOptions) => () => void;
/** Original, optional client feedback; disabled effects retain their message through notifications. */
export class EffectService {
  private readonly effects = new Registry<Effect>("effect");
  private readonly active = new Set<() => void>();
  private dead = false;
  constructor(private readonly host: HTMLElement, private readonly notifications: NotificationService, private readonly report: (error: unknown) => void) {
    this.register("default", options => {
      const node = el("div", options.message, "client-effect");
      if (typeof window.matchMedia === "function" && window.matchMedia("(prefers-reduced-motion: reduce)").matches) node.classList.add("reduced-motion");
      this.host.append(node); let stopped = false;
      const stop = (): void => { if (stopped) return; stopped = true; clearTimeout(timer); node.remove(); this.active.delete(stop); };
      const timer = setTimeout(stop, 2500); return stop;
    });
  }
  register(key: string, effect: Effect): void {
    if (this.dead) throw new ClientError("validation", "Effect service is disposed.");
    if (typeof effect !== "function") throw new ClientError("validation", "Effect registration requires a callback.");
    this.effects.add(key, effect);
  }
  show(key: string, options: EffectOptions): void {
    if (this.dead) throw new ClientError("validation", "Effect service is disposed.");
    if (options === null || typeof options !== "object" || typeof options.message !== "string" || typeof options.enabled !== "boolean" || Object.keys(options).some(name => name !== "message" && name !== "enabled")) throw new ClientError("validation", "Effect options require text message and boolean enabled.");
    if (options.message.length > 8192 || new TextEncoder().encode(options.message).byteLength > 8192) throw new ClientError("validation", "Effect message exceeds 8192 UTF-8 bytes.");
    const effect = this.effects.get(key);
    if (!options.enabled) { this.notifications.add(options.message, { level: "success" }); return; }
    if (this.active.size >= 8) throw new ClientError("validation", "Active effect limit exceeded: 8.");
    try {
      const stop = effect(options);
      if (typeof stop !== "function") throw new ClientError("validation", "Effect callback must return a cleanup function.");
      this.active.add(stop);
    } catch (error) { this.report(error); throw error; }
  }
  dispose(): void {
    if (this.dead) return; this.dead = true;
    const failures: unknown[] = [];
    for (const stop of this.active) { try { stop(); } catch (error) { failures.push(error); } }
    this.active.clear();
    for (const failure of failures) this.report(failure);
  }
}
