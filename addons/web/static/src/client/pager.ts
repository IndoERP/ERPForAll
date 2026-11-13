/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
import { launch } from "./failure-owner.js";

export interface PagerProps {
  offset: number;
  limit: number;
  total: number;
  onUpdate?: (bounds: { offset: number; limit: number }, hasNavigated: boolean) => Promise<void> | void;
  isEditable?: boolean;
  /** The host's guarded runner and error report: a failed `onUpdate` reaches one of them. */
  run?: (job: () => Promise<void>) => void;
  report?: (error: unknown) => void;
}

/** Parses range string e.g. "1-80", "1 80", "1,80", "20" and clamps to 1..total. */
export function parsePagerRange(value: string, total: number): { offset: number; limit: number } | null {
  const parts = value.trim().split(/\s*[-\s,;]\s*/);
  if (!parts[0]) return null;

  const minRaw = parseInt(parts[0], 10);
  if (isNaN(minRaw)) return null;

  const maxRaw = parts.length > 1 && parts[1] ? parseInt(parts[1], 10) : minRaw;
  if (isNaN(maxRaw)) return null;

  if (minRaw > maxRaw) return null;

  const clampedMin = Math.max(1, Math.min(minRaw, total));
  const clampedMax = Math.max(1, Math.min(maxRaw, total));

  if (clampedMin > clampedMax) return null;

  const offset = clampedMin - 1;
  const limit = Math.max(1, clampedMax - clampedMin + (parts.length > 1 ? 1 : 0));
  return { offset, limit };
}

export class Pager {
  public offset: number;
  public limit: number;
  public total: number;
  public onUpdate?: (bounds: { offset: number; limit: number }, hasNavigated: boolean) => Promise<void> | void;
  public isEditable: boolean;
  public run?: (job: () => Promise<void>) => void;
  public report?: (error: unknown) => void;

  public isDisabled = false;
  public isEditing = false;
  private editToken = 0;

  private container: HTMLElement | null = null;
  private countEl: HTMLElement | null = null;
  private prevBtn: HTMLButtonElement | null = null;
  private nextBtn: HTMLButtonElement | null = null;
  private inputEl: HTMLInputElement | null = null;

  constructor(props: PagerProps) {
    this.offset = Math.max(0, props.offset);
    this.limit = Math.max(1, props.limit);
    this.total = Math.max(0, props.total);
    this.onUpdate = props.onUpdate;
    this.run = props.run;
    this.report = props.report;
    this.isEditable = props.isEditable ?? (this.limit > 1);
  }

  get minimum(): number {
    return this.offset + 1;
  }

  get maximum(): number {
    return Math.min(this.offset + this.limit, this.total);
  }

  get value(): string {
    if (this.total <= 0) return "";
    if (this.limit === 1) return `${this.minimum} / ${this.total}`;
    return `${this.minimum}-${this.maximum} / ${this.total}`;
  }

  async navigate(direction: -1 | 1): Promise<void> {
    if (this.isDisabled || this.total <= 0) return;

    let targetOffset = this.offset + this.limit * direction;
    if (direction > 0) {
      if (targetOffset >= this.total) {
        targetOffset = 0; // Loop to start
      }
    } else {
      if (targetOffset < 0 || this.offset === 0) {
        if (this.limit === 1) {
          targetOffset = Math.max(0, this.total - 1); // Loop to last record
        } else {
          targetOffset = Math.max(0, this.total - (this.total % this.limit || this.limit)); // Loop to last page
        }
      }
    }

    await this.update(targetOffset, this.limit, true);
  }

  startEditing(): void {
    if (this.isDisabled || !this.isEditable || this.isEditing || this.total <= 0) return;
    this.isEditing = true;
    const token = ++this.editToken;

    if (this.countEl) {
      this.inputEl = document.createElement("input");
      this.inputEl.type = "text";
      this.inputEl.className = "pager-range-input";
      this.inputEl.setAttribute("aria-label", "Edit page range");
      this.inputEl.value = this.limit === 1 ? String(this.minimum) : `${this.minimum}-${this.maximum}`;

      this.inputEl.addEventListener("keydown", (e: KeyboardEvent) => {
        if (e.key === "Enter") {
          e.preventDefault();
          launch(this, () => this.commitEditing());
        } else if (e.key === "Escape") {
          e.preventDefault();
          this.cancelEditing();
        }
      });

      this.inputEl.addEventListener("blur", () => {
        if (this.isEditing && this.editToken === token) {
          launch(this, () => this.commitEditing());
        }
      });

      this.countEl.replaceWith(this.inputEl);
      this.inputEl.focus();
      this.inputEl.select();
    }
  }

  async commitEditing(): Promise<void> {
    if (!this.isEditing || !this.inputEl) return;
    this.isEditing = false;
    this.editToken = 0;
    const input = this.inputEl;
    this.inputEl = null;
    if (this.countEl && input.parentNode) {
      input.replaceWith(this.countEl);
    }
    const textVal = input.value;
    const parsed = parsePagerRange(textVal, this.total);

    if (parsed) {
      try {
        await this.update(parsed.offset, parsed.limit, false);
      } finally {
        this.countEl?.focus();
      }
    } else {
      this.cancelEditing();
      this.countEl?.focus();
    }
  }

  cancelEditing(): void {
    this.isEditing = false;
    this.editToken = 0;
    const input = this.inputEl;
    this.inputEl = null;
    if (this.countEl && input && input.parentNode) {
      input.replaceWith(this.countEl);
    }
    this.syncDom();
    this.countEl?.focus();
  }

  async update(offset: number, limit: number, hasNavigated: boolean): Promise<void> {
    this.isDisabled = true;
    this.syncDisabled();
    try {
      if (this.onUpdate) {
        await this.onUpdate({ offset, limit }, hasNavigated);
      }
      this.offset = offset;
      this.limit = limit;
    } finally {
      this.isDisabled = false;
      this.isEditing = false;
      this.syncDom();
    }
  }

  render(): HTMLElement {
    const pager = document.createElement("div");
    pager.className = "pager";
    pager.setAttribute("role", "search");
    pager.setAttribute("aria-label", "Pagination");

    const count = this.isEditable
      ? document.createElement("button")
      : document.createElement("span");
    count.className = "pager-count muted";
    count.textContent = this.value;
    count.setAttribute("aria-live", "polite");

    if (count instanceof HTMLButtonElement) {
      count.type = "button";
      count.title = "Click to edit range";
      count.addEventListener("click", () => this.startEditing());
    } else {
      count.setAttribute("role", "status");
    }

    const prev = document.createElement("button");
    prev.type = "button";
    prev.className = "pager-btn pager-prev";
    prev.setAttribute("aria-label", "Previous page");
    prev.title = "Previous page";
    prev.innerHTML = `<i class="oi oi-chevron-left" aria-hidden="true"></i>`;
    prev.addEventListener("click", () => { launch(this, () => this.navigate(-1)); });

    const next = document.createElement("button");
    next.type = "button";
    next.className = "pager-btn pager-next";
    next.setAttribute("aria-label", "Next page");
    next.title = "Next page";
    next.innerHTML = `<i class="oi oi-chevron-right" aria-hidden="true"></i>`;
    next.addEventListener("click", () => { launch(this, () => this.navigate(1)); });

    pager.append(count, prev, next);

    this.container = pager;
    this.countEl = count;
    this.prevBtn = prev;
    this.nextBtn = next;

    this.syncDom();
    return pager;
  }

  syncDom(): void {
    if (this.countEl && !this.isEditing) {
      this.countEl.textContent = this.value;
    }
    this.syncDisabled();
  }

  syncDisabled(): void {
    if (this.prevBtn) this.prevBtn.disabled = this.isDisabled || this.total <= 0;
    if (this.nextBtn) this.nextBtn.disabled = this.isDisabled || this.total <= 0;
    if (this.inputEl) this.inputEl.disabled = this.isDisabled;
    if (this.countEl instanceof HTMLButtonElement) {
      this.countEl.disabled = this.isDisabled || this.total <= 0;
    }
  }

  dispose(): void {
    this.container?.remove();
    this.container = null;
    this.countEl = null;
    this.prevBtn = null;
    this.nextBtn = null;
    this.inputEl = null;
  }
}
