/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
import { lockDocumentScroll } from "./scroll-lock.js";
/** Owns one rendered drawer's modal effects and bounded, horizontal close gesture. */
export class MobileDrawer {
  private open = false;
  private disposed = false;
  private previousOverflow = "";
  private releaseScroll?: () => void;
  private readonly inertElements = new Map<HTMLElement, boolean>();
  private readonly pointers = new Set<number>();
  private gesture?: { id: number; x: number; y: number; direction: "pending" | "left" | "rejected" };
  private suppressClick = false;

  constructor(
    private readonly panel: HTMLElement,
    private readonly backdrop: HTMLElement,
    private readonly toggle: HTMLButtonElement,
    private readonly focusDesktop?: () => void,
  ) {
    toggle.setAttribute("aria-controls", panel.id);
    toggle.setAttribute("aria-expanded", "false");
    backdrop.addEventListener("click", this.close);
    panel.addEventListener("click", this.onClick, true);
    panel.addEventListener("pointermove", this.onPointerMove, { passive: false });
    window.addEventListener("pointerdown", this.onPointerDown);
    window.addEventListener("pointerup", this.onPointerUp);
    window.addEventListener("pointercancel", this.onPointerCancel);
    panel.addEventListener("lostpointercapture", this.onPointerCancel);
    window.addEventListener("keydown", this.onKeyDown);
    window.addEventListener("resize", this.onResize);
  }

  get isOpen(): boolean { return this.open; }

  toggleOpen(): void {
    if (this.disposed) return;
    if (this.open) { this.close(); return; }
    this.open = true;
    this.suppressClick = false;
    this.panel.classList.remove("hidden");
    this.backdrop.classList.remove("hidden");
    this.toggle.setAttribute("aria-expanded", "true");
    this.previousOverflow = document.body.style.overflow;
    document.body.style.overflow = "hidden";
    this.releaseScroll = lockDocumentScroll();
    // Inert siblings along the ancestor chain, without hiding the drawer itself.
    let branch: HTMLElement = this.panel;
    while (branch.parentElement) {
      for (const sibling of branch.parentElement.children) {
        if (!(sibling instanceof HTMLElement) || sibling === branch || sibling === this.backdrop) continue;
        this.inertElements.set(sibling, sibling.inert === true);
        sibling.inert = true;
      }
      branch = branch.parentElement;
      if (branch === document.body) break;
    }
    this.panel.querySelector<HTMLButtonElement>(".drawer-close-btn")?.focus();
  }

  close = (): void => {
    if (!this.open) return;
    this.open = false;
    this.resetGesture();
    this.panel.classList.add("hidden");
    this.backdrop.classList.add("hidden");
    this.toggle.setAttribute("aria-expanded", "false");
    document.body.style.overflow = this.previousOverflow;
    this.releaseScroll?.();
    this.releaseScroll = undefined;
    for (const [element, wasInert] of this.inertElements) element.inert = wasInert;
    this.inertElements.clear();
    if (this.toggle.isConnected) this.toggle.focus();
  };

  dispose(): void {
    this.close();
    this.disposed = true;
    this.backdrop.removeEventListener("click", this.close);
    this.panel.removeEventListener("click", this.onClick, true);
    this.panel.removeEventListener("pointermove", this.onPointerMove);
    this.panel.removeEventListener("lostpointercapture", this.onPointerCancel);
    window.removeEventListener("pointerdown", this.onPointerDown);
    window.removeEventListener("pointerup", this.onPointerUp);
    window.removeEventListener("pointercancel", this.onPointerCancel);
    window.removeEventListener("keydown", this.onKeyDown);
    window.removeEventListener("resize", this.onResize);
  }

  private readonly onResize = (): void => {
    if (this.open && window.innerWidth > 768) {
      this.close();
      if (this.focusDesktop) this.focusDesktop();
      else this.toggle.parentElement?.querySelector<HTMLButtonElement>("[data-app-switcher]")?.focus();
    }
  };

  private readonly onKeyDown = (event: KeyboardEvent): void => {
    if (!this.open || event.key !== "Tab") return;
    const targets = [...this.panel.querySelectorAll<HTMLButtonElement>("button:not(:disabled)")];
    const first = targets[0], last = targets[targets.length - 1];
    if (!first || !last) return;
    if (event.shiftKey && (document.activeElement === first || !this.panel.contains(document.activeElement))) {
      event.preventDefault(); last.focus();
    } else if (!event.shiftKey && (document.activeElement === last || !this.panel.contains(document.activeElement))) {
      event.preventDefault(); first.focus();
    }
  };

  private readonly onClick = (event: MouseEvent): void => {
    if (!this.suppressClick || event.detail === 0) return;
    this.suppressClick = false;
    event.preventDefault();
    event.stopImmediatePropagation();
  };

  private readonly onPointerDown = (event: PointerEvent): void => {
    if (!this.open) return;
    if (event.pointerType !== "touch" && event.pointerType !== "pen") {
      this.suppressClick = false;
      return;
    }
    this.pointers.add(event.pointerId);
    if (this.pointers.size > 1) {
      if (this.gesture) this.gesture.direction = "rejected";
      this.clearDrag();
      return;
    }
    if (!(event.target instanceof Node) || !this.panel.contains(event.target)) return;
    this.suppressClick = false;
    this.gesture = { id: event.pointerId, x: event.clientX, y: event.clientY, direction: "pending" };
  };

  private readonly onPointerMove = (event: PointerEvent): void => {
    const gesture = this.gesture;
    if (!this.open || !gesture || gesture.id !== event.pointerId || gesture.direction === "rejected") return;
    const dx = event.clientX - gesture.x, dy = event.clientY - gesture.y;
    if (gesture.direction === "pending") {
      if (Math.max(Math.abs(dx), Math.abs(dy)) < 10) return;
      if (dx >= 0 || Math.abs(dx) <= Math.abs(dy) * 1.3) {
        gesture.direction = "rejected";
        return;
      }
      gesture.direction = "left";
      this.panel.setPointerCapture?.(event.pointerId);
    }
    event.preventDefault();
    this.suppressClick = true;
    const distance = Math.min(0, dx);
    this.panel.classList.add("is-dragging");
    this.panel.style.transform = `translateX(${distance}px)`;
    const width = this.panel.getBoundingClientRect().width;
    if (width > 0) this.backdrop.style.opacity = String(Math.max(0, 1 + distance / width));
  };

  private readonly onPointerUp = (event: PointerEvent): void => {
    this.pointers.delete(event.pointerId);
    const gesture = this.gesture;
    if (!gesture || gesture.id !== event.pointerId) return;
    const threshold = Math.max(48, Math.min(96, this.panel.getBoundingClientRect().width * .25));
    const dismiss = gesture.direction === "left" && gesture.x - event.clientX >= threshold;
    this.resetGesture(false);
    if (dismiss) this.close();
  };

  private readonly onPointerCancel = (event: PointerEvent): void => {
    // Taking capture from a button emits a bubbling loss on the button. Only
    // losing the panel's own capture cancels the drag we now own.
    if (event.type === "lostpointercapture" && event.target !== this.panel) return;
    this.pointers.delete(event.pointerId);
    if (this.gesture?.id === event.pointerId) {
      this.suppressClick = false;
      this.resetGesture(false);
    }
  };

  private clearDrag(): void {
    this.panel.classList.remove("is-dragging");
    this.panel.style.transform = "";
    this.backdrop.style.opacity = "";
  }

  private resetGesture(clearPointers = true): void {
    const id = this.gesture?.id;
    this.gesture = undefined;
    if (id !== undefined && this.panel.hasPointerCapture?.(id)) this.panel.releasePointerCapture(id);
    if (clearPointers) this.pointers.clear();
    this.clearDrag();
  }
}
