/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
import { el } from "./dom.js";
import { ClientError } from "./transport.js";

export interface BlockOptions { readonly delay?: number; readonly message?: string }
interface Block { readonly message: string | undefined; visible: boolean; timer: ReturnType<typeof setTimeout> | undefined }
/** Balanced, scope-local blocking feedback. Unlike requestLoading this shields pointer input immediately. */
export class BlockUi {
  private readonly blocks = new Set<Block>();
  private readonly overlay = el("div", undefined, "client-block-ui");
  private readonly feedback = el("div", undefined, "client-block-feedback");
  private readonly label = el("p", undefined, "client-block-message");
  private stage: ReturnType<typeof setTimeout> | undefined;
  private dead = false;
  constructor(private readonly host: HTMLElement) {
    const spinner = el("span", undefined, "client-block-spinner"); spinner.setAttribute("aria-hidden", "true");
    this.feedback.append(spinner, this.label); this.overlay.append(this.feedback);
    // Shielding exists even before feedback is visible; never hide the overlay itself for the delay.
    Object.assign(this.overlay.style, { position: "fixed", inset: "0", zIndex: "1100", pointerEvents: "auto" });
  }
  /** Move this overlay into the active modal before beginning a blocking scope. */
  get node(): HTMLElement { return this.overlay; }
  get blocked(): boolean { return this.blocks.size > 0; }
  begin(options: BlockOptions = {}): () => void {
    if (this.dead) throw new ClientError("validation", "Blocking service is disposed.");
    if (options === null || typeof options !== "object" || Array.isArray(options)) throw new ClientError("validation", "Block options must be an object.");
    for (const key of Object.keys(options)) if (key !== "delay" && key !== "message") throw new ClientError("validation", `Unknown block option: ${key}.`);
    const { delay = 250, message } = options;
    if (typeof delay !== "number" || !Number.isFinite(delay) || delay < 0 || delay > 2_147_483_647) throw new ClientError("validation", "Block delay must be a finite number from 0 to 2147483647.");
    if (message !== undefined && (typeof message !== "string" || message.length > 8192 || new TextEncoder().encode(message).byteLength > 8192)) throw new ClientError("validation", "Block message must be text of at most 8192 UTF-8 bytes.");
    if (this.blocks.size >= 32) throw new ClientError("validation", "Block request limit exceeded: 32.");
    const block: Block = { message, visible: delay === 0, timer: undefined };
    this.blocks.add(block); if (!this.overlay.isConnected) this.host.append(this.overlay);
    if (delay > 0) block.timer = setTimeout(() => { block.timer = undefined; if (!this.dead && this.blocks.has(block)) { block.visible = true; this.render(); } }, delay);
    this.render(); let released = false;
    return () => {
      if (released) return; released = true; clearTimeout(block.timer); block.timer = undefined;
      this.blocks.delete(block); this.render();
    };
  }
  private render(): void {
    clearTimeout(this.stage); this.stage = undefined;
    if (this.dead || this.blocks.size === 0) { this.overlay.remove(); this.label.textContent = ""; return; }
    const current = [...this.blocks].reverse().find(block => block.visible);
    this.overlay.dataset["visible"] = String(current !== undefined); this.feedback.hidden = current === undefined;
    this.label.textContent = current ? current.message ?? "Working…" : "";
    if (current && current.message === undefined) this.stage = setTimeout(() => {
      this.stage = undefined;
      if (!this.dead && this.blocks.has(current)) this.label.textContent = "Still working. Please wait…";
    }, 10_000);
  }
  dispose(): void {
    if (this.dead) return; this.dead = true; clearTimeout(this.stage);
    for (const block of this.blocks) clearTimeout(block.timer);
    this.blocks.clear(); this.overlay.remove(); this.label.textContent = "";
  }
}
