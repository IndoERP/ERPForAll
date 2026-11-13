/*! SPDX-License-Identifier: MIT OR Apache-2.0 */

export interface MenuDropdownOptions {
  /** The button or clickable trigger that controls menu visibility. */
  readonly trigger: HTMLElement;
  /** The menu container element with role="menu". */
  readonly menu: HTMLElement;
  /**
   * Visibility presentation:
   * - "display": toggles style.display ("none" vs "block"). Default.
   * - "class-open": toggles the "open" CSS class.
   */
  readonly mode?: "display" | "class-open";
  /** Optional function returning items participating in roving focus. */
  readonly items?: () => readonly HTMLElement[];
  /** Optional callback fired when the menu is opened. */
  readonly onOpen?: () => void;
  /** Optional callback fired when the menu is closed. */
  readonly onClose?: () => void;
}

export interface MenuDropdownController {
  open(edge?: "first" | "last", focusItem?: boolean): void;
  close(restoreFocus?: boolean): void;
  toggle(focusItem?: boolean): void;
  isOpen(): boolean;
  refresh(): void;
  dispose(): void;
}

function isAvailable(el: HTMLElement): boolean {
  if (!el.isConnected) return false;
  if (el.matches(':disabled, [aria-disabled="true"]') || el.closest('[hidden], [inert], [aria-hidden="true"]')) {
    return false;
  }
  const view = el.ownerDocument.defaultView;
  if (view) {
    const style = view.getComputedStyle(el);
    if (style.display === "none" || style.visibility === "hidden" || style.visibility === "collapse") {
      return false;
    }
  }
  return true;
}

/**
 * Attaches accessible dropdown behavior to a trigger and menu:
 * - Initializes aria-haspopup, aria-expanded, and role="menu".
 * - Roving tabindex across menuitems with ArrowDown, ArrowUp, Home, and End.
 * - ArrowDown/ArrowUp on trigger opens menu and focuses first/last item.
 * - Escape closes menu and returns focus to trigger.
 * - Tab closes menu without hijacking focus.
 * - Outside pointerdown dismisses menu.
 */
export function attachMenuDropdown(options: MenuDropdownOptions): MenuDropdownController {
  const { trigger, menu } = options;
  const mode = options.mode ?? "display";
  const doc = trigger.ownerDocument;

  let open = false;
  let disposed = false;
  let currentFocusedIndex = -1;
  let wantFocus = true;

  // Set initial accessibility attributes
  if (!trigger.hasAttribute("aria-haspopup")) {
    trigger.setAttribute("aria-haspopup", "true");
  }
  trigger.setAttribute("aria-expanded", "false");
  if (!menu.hasAttribute("role")) {
    menu.setAttribute("role", "menu");
  }

  if (mode === "class-open") {
    menu.classList.remove("open");
  } else {
    menu.style.display = "none";
  }

  const getCandidateItems = (): HTMLElement[] => {
    if (options.items) {
      return [...options.items()].filter(isAvailable);
    }
    const nodes = menu.querySelectorAll<HTMLElement>(
      '[role="menuitem"], .dropdown-item, .nav-more-item, button:not(:disabled), a[href]'
    );
    return [...nodes].filter(isAvailable);
  };

  const syncRovingTabindex = (focusTarget?: HTMLElement): void => {
    const items = getCandidateItems();
    if (items.length === 0) return;

    if (focusTarget) {
      currentFocusedIndex = items.indexOf(focusTarget);
    }
    if (currentFocusedIndex < 0 || currentFocusedIndex >= items.length) {
      currentFocusedIndex = 0;
    }

    for (let i = 0; i < items.length; i++) {
      const item = items[i]!;
      item.tabIndex = i === currentFocusedIndex ? 0 : -1;
    }
  };

  const setVisible = (show: boolean): void => {
    if (mode === "class-open") {
      menu.classList.toggle("open", show);
    } else {
      menu.style.display = show ? "block" : "none";
    }
  };

  // Other code (Escape handler, outside-click) may drop the "open" class directly; follow it so the next click toggles correctly.
  const sync = (): void => {
    if (mode === "class-open" && open && !menu.classList.contains("open")) {
      open = false;
      trigger.setAttribute("aria-expanded", "false");
    }
  };

  const close = (restoreFocus = true): void => {
    sync();
    if (disposed || !open) return;
    open = false;
    trigger.setAttribute("aria-expanded", "false");
    setVisible(false);
    options.onClose?.();

    if (restoreFocus && isAvailable(trigger)) {
      trigger.focus();
    }
  };

  // Scroll cue and row snapping: a panel taller than the window ends on a whole row, and `data-more` is set while rows
  // hide below (CSS fades the last rows then), so a row is never cut mid-way without a cue.
  const fit = (): void => {
    menu.style.maxHeight = "";
    const view = doc.defaultView;
    if (!view) return;
    const avail = view.innerHeight - menu.getBoundingClientRect().top - 16;
    if (avail > 0 && menu.scrollHeight > avail) {
      const style = view.getComputedStyle(menu), pad = parseFloat(style.paddingTop) + parseFloat(style.paddingBottom);
      const frame = parseFloat(style.borderTopWidth) + parseFloat(style.borderBottomWidth);
      const limit = avail - pad - frame;
      let bottom = 0;
      for (const child of Array.from(menu.children) as HTMLElement[]) {
        const end = child.offsetTop - parseFloat(style.paddingTop) + child.offsetHeight;
        if (end <= limit) bottom = end;
        else break;
      }
      if (bottom > 0) menu.style.maxHeight = `${style.boxSizing === "border-box" ? bottom + pad + frame : bottom}px`;
    }
    cue();
  };
  const cue = (): void => {
    menu.dataset["more"] = String(menu.scrollHeight - menu.scrollTop - menu.clientHeight > 1);
  };
  menu.addEventListener("scroll", cue, { passive: true });

  const openMenu = (edge: "first" | "last" = "first", focusItem = true): void => {
    if (disposed) return;
    sync();
    if (open) {
      refresh();
      if (focusItem) {
        const items = getCandidateItems();
        const target = items[edge === "last" ? items.length - 1 : 0];
        if (target) { syncRovingTabindex(target); target.focus(); }
      }
      return;
    }

    open = true;
    wantFocus = focusItem;
    trigger.setAttribute("aria-expanded", "true");
    setVisible(true);
    options.onOpen?.();
    fit();

    const items = getCandidateItems();
    if (items.length > 0) {
      const targetIndex = edge === "last" ? items.length - 1 : 0;
      currentFocusedIndex = targetIndex;
      syncRovingTabindex(items[targetIndex]);
      // A pointer opens the panel without moving focus (no focus ring); a key press (or the API) focuses the first or last entry.
      if (focusItem) items[targetIndex]?.focus();
    }
  };

  const toggle = (focusItem = true): void => {
    sync();
    if (open) {
      close(true);
    } else {
      openMenu("first", focusItem);
    }
  };

  const refresh = (): void => {
    if (disposed || !open) return;
    fit();
    const items = getCandidateItems();
    if (items.length === 0) {
      currentFocusedIndex = -1;
      return;
    }
    const hadNoItems = currentFocusedIndex < 0;
    if (currentFocusedIndex >= items.length) {
      currentFocusedIndex = items.length - 1;
    } else if (currentFocusedIndex < 0) {
      currentFocusedIndex = 0;
    }
    syncRovingTabindex(items[currentFocusedIndex]);
    if (hadNoItems && wantFocus && (doc.activeElement === trigger || !menu.contains(doc.activeElement))) {
      items[currentFocusedIndex]?.focus();
    }
  };

  // Trigger keyboard listener
  const onTriggerKeydown = (event: KeyboardEvent): void => {
    if (disposed) return;
    if (event.key === "ArrowDown") {
      event.preventDefault();
      openMenu("first");
    } else if (event.key === "ArrowUp") {
      event.preventDefault();
      openMenu("last");
    } else if (event.key === "Escape" && open) {
      event.preventDefault();
      event.stopPropagation();
      close(true);
    }
  };

  // `detail` is 0 for a keyboard-activated click and the click count for a pointer.
  const onTriggerClick = (event: MouseEvent): void => {
    if (disposed) return;
    toggle(event.detail === 0);
  };

  // Menu keyboard listener
  const onMenuKeydown = (event: KeyboardEvent): void => {
    sync();
    if (disposed || !open) return;

    if (event.key === "Escape") {
      event.preventDefault();
      event.stopPropagation();
      close(true);
      return;
    }

    if (event.key === "Tab") {
      close(false);
      return;
    }

    const items = getCandidateItems();
    if (items.length === 0) return;

    const currentIndex = items.indexOf(doc.activeElement as HTMLElement);
    let nextIndex = currentIndex;

    if (event.key === "ArrowDown") {
      event.preventDefault();
      nextIndex = currentIndex < 0 ? 0 : (currentIndex + 1) % items.length;
    } else if (event.key === "ArrowUp") {
      event.preventDefault();
      nextIndex = currentIndex < 0 ? items.length - 1 : (currentIndex - 1 + items.length) % items.length;
    } else if (event.key === "Home") {
      event.preventDefault();
      nextIndex = 0;
    } else if (event.key === "End") {
      event.preventDefault();
      nextIndex = items.length - 1;
    } else if (event.key.length === 1 && event.key !== " " && !event.ctrlKey && !event.metaKey && !event.altKey && !(event.target instanceof HTMLElement && event.target.closest("input, textarea, select, [contenteditable]:not([contenteditable=false])"))) {
      // Type-ahead: the next entry (after the focused one, wrapping) whose label starts with the typed letter.
      const letter = event.key.toLowerCase();
      const hit = [...items.slice(currentIndex + 1), ...items.slice(0, currentIndex + 1)]
        .find(item => (item.textContent ?? "").trim().toLowerCase().startsWith(letter));
      if (!hit) return;
      event.preventDefault();
      nextIndex = items.indexOf(hit);
    } else {
      return;
    }

    currentFocusedIndex = nextIndex;
    syncRovingTabindex(items[nextIndex]);
    items[nextIndex]?.focus();
  };

  // Menu focusin listener to keep current index in sync with user interaction
  const onMenuFocusin = (event: FocusEvent): void => {
    if (disposed || !open) return;
    const target = event.target as HTMLElement | null;
    if (target && menu.contains(target)) {
      const items = getCandidateItems();
      const idx = items.indexOf(target);
      if (idx >= 0) {
        currentFocusedIndex = idx;
        syncRovingTabindex(target);
      }
    }
  };

  // Document pointerdown listener for click-outside
  const onDocPointerdown = (event: PointerEvent): void => {
    sync();
    if (disposed || !open) return;
    const target = event.target as Node | null;
    if (!target) return;
    if (trigger.contains(target) || menu.contains(target)) {
      return;
    }
    close(false);
  };

  trigger.addEventListener("keydown", onTriggerKeydown);
  trigger.addEventListener("click", onTriggerClick);
  menu.addEventListener("keydown", onMenuKeydown);
  menu.addEventListener("focusin", onMenuFocusin);
  doc.addEventListener("pointerdown", onDocPointerdown);

  return {
    open: openMenu,
    close,
    toggle,
    isOpen: () => open,
    refresh,
    dispose: () => {
      if (disposed) return;
      disposed = true;
      close(false);
      trigger.removeEventListener("keydown", onTriggerKeydown);
      trigger.removeEventListener("click", onTriggerClick);
      menu.removeEventListener("scroll", cue);
      menu.removeEventListener("keydown", onMenuKeydown);
      menu.removeEventListener("focusin", onMenuFocusin);
      doc.removeEventListener("pointerdown", onDocPointerdown);
    }
  };
}
