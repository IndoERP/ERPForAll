/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
import { lockDocumentScroll } from "./scroll-lock.js";

const openBottomSheetStack: BottomSheet[] = [];

export function getActiveBottomSheetCount(): number {
  return openBottomSheetStack.length;
}

function isTopmostBottomSheet(sheet: BottomSheet): boolean {
  return openBottomSheetStack[openBottomSheetStack.length - 1] === sheet;
}

const popupSelector = ".editor-popover, .popover, dialog, [role='listbox'], [role='menu'], .dropdown-menu, .autocomplete-dropdown";
// Menus that are shown by a class (or an inline display) and hidden by the stylesheet otherwise.
const classToggledMenu = ".nav-dropdown-menu, .dropdown-menu, .select-menu";

function isOpenPopup(element: HTMLElement): boolean {
  if (element instanceof HTMLDialogElement) return element.open;
  if (element.closest("[hidden], [inert], [aria-hidden='true']")) return false;
  const style = getComputedStyle(element);
  if (style.display === "none" || style.visibility === "hidden") return false;
  return !element.matches(classToggledMenu) || element.classList.contains("open") || element.style.display === "block";
}

/** An open, visible popup around the focus (or the focused trigger of one) takes Escape before the sheet does. */
function hasNestedPopup(): boolean {
  const active = document.activeElement;
  if (!(active instanceof HTMLElement)) return false;
  // An expanded popup trigger (a combobox or a menu button), not a plain disclosure.
  if (active.getAttribute("aria-expanded") === "true" && (active.getAttribute("role") === "combobox" || (active.hasAttribute("aria-haspopup") && active.getAttribute("aria-haspopup") !== "false"))) return true;
  for (let node: HTMLElement | null = active; node; node = node.parentElement) if (node.matches(popupSelector) && isOpenPopup(node)) return true;
  return false;
}

function updateBodySheetState(): void {
  if (typeof document !== "undefined" && document.body) {
    document.body.classList.toggle("bottom-sheet-open", openBottomSheetStack.length >= 1);
  }
}

export interface BottomSheetOptions {
  title?: string;
  content: HTMLElement | (() => HTMLElement);
  className?: string;
  role?: string;
  maxHeightPercent?: number; // default: 90
  onClose?: () => void;
}

export class BottomSheet {
  private open = false;
  private dismissing = false;
  private disposed = false;
  private releaseScroll?: () => void;
  private readonly inertElements = new Map<HTMLElement, boolean>();
  private readonly maxHeightPercent: number;
  private previousActiveElement: HTMLElement | null = null;

  readonly container: HTMLElement;
  readonly backdrop: HTMLElement;
  readonly sheet: HTMLElement;
  readonly closeBtn: HTMLButtonElement;

  constructor(private readonly options: BottomSheetOptions) {
    this.maxHeightPercent = options.maxHeightPercent ?? 90;

    this.container = document.createElement("div");
    this.container.className = "bottom-sheet-container";

    this.backdrop = document.createElement("div");
    this.backdrop.className = "bottom-sheet-backdrop";
    this.backdrop.setAttribute("aria-hidden", "true");

    this.sheet = document.createElement("div");
    this.sheet.className = `bottom-sheet ${options.className ?? ""}`.trim();
    this.sheet.setAttribute("role", options.role ?? "dialog");
    this.sheet.setAttribute("aria-modal", "true");

    const header = document.createElement("div");
    header.className = "bottom-sheet-header";

    const handle = document.createElement("div");
    handle.className = "bottom-sheet-handle";
    handle.setAttribute("aria-hidden", "true");

    if (options.title) {
      const titleEl = document.createElement("h2");
      titleEl.className = "bottom-sheet-title";
      titleEl.textContent = options.title;
      header.append(handle, titleEl);
    } else {
      header.append(handle);
    }

    this.closeBtn = document.createElement("button");
    this.closeBtn.type = "button";
    this.closeBtn.className = "bottom-sheet-close btn-close";
    this.closeBtn.setAttribute("aria-label", "Close");
    header.append(this.closeBtn);

    const body = document.createElement("div");
    body.className = "bottom-sheet-body";
    const contentNode = typeof options.content === "function" ? options.content() : options.content;
    body.append(contentNode);

    this.sheet.append(header, body);
    this.container.append(this.backdrop, this.sheet);

    this.backdrop.addEventListener("click", () => this.slideOut());
    this.closeBtn.addEventListener("click", () => this.slideOut());
  }

  get isOpen(): boolean {
    return this.open;
  }

  get isDismissing(): boolean {
    return this.dismissing;
  }

  openSheet(parent: HTMLElement = document.body): void {
    if (this.disposed || this.open) return;
    this.open = true;
    this.dismissing = false;
    this.previousActiveElement = document.activeElement instanceof HTMLElement ? document.activeElement : null;

    openBottomSheetStack.push(this);
    updateBodySheetState();

    parent.append(this.container);
    this.releaseScroll = lockDocumentScroll();

    // Inert siblings
    let branch: HTMLElement = this.container;
    while (branch.parentElement) {
      for (const sibling of branch.parentElement.children) {
        if (!(sibling instanceof HTMLElement) || sibling === branch) continue;
        this.inertElements.set(sibling, sibling.inert === true);
        sibling.inert = true;
      }
      branch = branch.parentElement;
      if (branch === document.body) break;
    }

    window.addEventListener("resize", this.onViewportChange);
    if (window.visualViewport) {
      window.visualViewport.addEventListener("resize", this.onViewportChange);
    }
    window.addEventListener("keydown", this.onKeyDown, true);

    this.updateDimensions();
    this.closeBtn.focus();
  }

  updateDimensions = (): void => {
    if (!this.open || this.dismissing) return;
    const viewportHeight = window.visualViewport?.height ?? window.innerHeight;
    const maxHeightPx = (this.maxHeightPercent / 100) * viewportHeight;

    this.sheet.style.removeProperty("min-height");
    this.sheet.style.removeProperty("height");
    this.sheet.style.maxHeight = `${maxHeightPx}px`;
  };

  private readonly onViewportChange = (): void => {
    this.updateDimensions();
  };

  private readonly onKeyDown = (event: KeyboardEvent): void => {
    if (!this.open || this.dismissing) return;
    if (!isTopmostBottomSheet(this)) return;

    if (event.key === "Escape") {
      if (hasNestedPopup()) return;
      event.preventDefault();
      event.stopPropagation();
      event.stopImmediatePropagation();
      const isReduced =
        window.matchMedia?.("(prefers-reduced-motion: reduce)")?.matches === true ||
        (typeof window.getComputedStyle === "function" && window.getComputedStyle(this.sheet).animationName === "none");
      if (isReduced) {
        this.finishClose();
      } else {
        this.slideOut();
      }
      return;
    }

    // Focus trapping
    if (event.key === "Tab") {
      const focusables = [...this.sheet.querySelectorAll<HTMLElement>(
        'button:not(:disabled), [href], input:not(:disabled), select:not(:disabled), textarea:not(:disabled), [tabindex]:not([tabindex="-1"])'
      )];
      if (focusables.length === 0) return;
      const first = focusables[0];
      const last = focusables[focusables.length - 1];
      if (event.shiftKey && (document.activeElement === first || !this.sheet.contains(document.activeElement))) {
        event.preventDefault();
        last?.focus();
      } else if (!event.shiftKey && (document.activeElement === last || !this.sheet.contains(document.activeElement))) {
        event.preventDefault();
        first?.focus();
      }
    }
  };

  slideOut(): void {
    if (!this.open || this.dismissing) return;
    this.dismissing = true;

    const isReduced =
      window.matchMedia?.("(prefers-reduced-motion: reduce)")?.matches === true ||
      (typeof window.getComputedStyle === "function" && window.getComputedStyle(this.sheet).animationName === "none");

    if (isReduced) {
      this.finishClose();
    } else {
      this.sheet.classList.add("bottom-sheet-dismissing");
      let finished = false;
      const onEnd = () => {
        if (finished) return;
        finished = true;
        this.finishClose();
      };
      this.sheet.addEventListener("animationend", onEnd, { once: true });
      this.sheet.addEventListener("animationcancel", onEnd, { once: true });
      // Fallback timer in case animationend doesn't trigger
      setTimeout(onEnd, 350);
    }
  }

  private finishClose(): void {
    if (!this.open) return;
    this.open = false;
    this.dismissing = false;

    window.removeEventListener("resize", this.onViewportChange);
    if (window.visualViewport) {
      window.visualViewport.removeEventListener("resize", this.onViewportChange);
    }
    window.removeEventListener("keydown", this.onKeyDown, true);

    this.releaseScroll?.();
    this.releaseScroll = undefined;

    for (const [element, wasInert] of this.inertElements) {
      element.inert = wasInert;
    }
    this.inertElements.clear();

    this.container.remove();

    const stackIdx = openBottomSheetStack.indexOf(this);
    if (stackIdx !== -1) {
      openBottomSheetStack.splice(stackIdx, 1);
    }
    updateBodySheetState();

    if (this.previousActiveElement?.isConnected) {
      this.previousActiveElement.focus();
    }
    this.options.onClose?.();
  }

  dispose(): void {
    if (this.disposed) return;
    if (this.open) {
      this.finishClose();
    } else {
      const stackIdx = openBottomSheetStack.indexOf(this);
      if (stackIdx !== -1) {
        openBottomSheetStack.splice(stackIdx, 1);
        updateBodySheetState();
      }
    }
    this.disposed = true;
  }
}
