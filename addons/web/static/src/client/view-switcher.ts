/*! SPDX-License-Identifier: MIT OR Apache-2.0 */

export interface ViewSwitcherEntry {
  type: string;
  label?: string;
  name?: string;
  icon?: string;
  active?: boolean;
}

export interface ViewSwitcherProps {
  entries: readonly ViewSwitcherEntry[];
  activeType: string;
  onSwitchView: (viewType: string, options?: { newWindow?: boolean }) => Promise<void> | void;
}

export class ViewSwitcher {
  public entries: readonly ViewSwitcherEntry[];
  public activeType: string;
  public onSwitchView: (viewType: string, options?: { newWindow?: boolean }) => Promise<void> | void;

  private container: HTMLElement | null = null;
  private keydownHandler?: (e: KeyboardEvent) => void;

  constructor(props: ViewSwitcherProps) {
    this.entries = props.entries;
    this.activeType = props.activeType;
    this.onSwitchView = props.onSwitchView;
  }

  cycleThroughViews(): void {
    if (this.entries.length <= 1) return;
    const currentIndex = this.entries.findIndex(entry => entry.type === this.activeType);
    const nextIndex = (currentIndex + 1) % this.entries.length;
    const nextEntry = this.entries[nextIndex];
    if (nextEntry) {
      void this.onSwitchView(nextEntry.type, { newWindow: false });
    }
  }

  render(): HTMLElement {
    const switcher = document.createElement("div");
    switcher.className = "view-switcher";
    switcher.setAttribute("role", "group");
    switcher.setAttribute("aria-label", "View switcher");

    for (const entry of this.entries) {
      const btn = document.createElement("button");
      btn.type = "button";
      const isActive = entry.type === this.activeType;
      btn.className = `view-btn ${isActive ? "active" : ""}`.trim();
      const label = entry.label || entry.name || entry.type;
      btn.setAttribute("aria-label", `${label} view`);
      if (isActive) {
        btn.setAttribute("aria-current", "page");
      }

      // Icon or text
      const iconClass = entry.icon || (entry.type === "list" ? "oi-view-list" : entry.type === "form" ? "oi-view-form" : `oi-view-${entry.type}`);
      // Built with DOM calls: the label and the icon name come from view descriptions (addon data), and
      // markup in them must stay text. An icon name that is not a plain class token falls back to the type's.
      const icon = document.createElement("i");
      icon.className = `oi ${/^[\w-]+$/.test(iconClass) ? iconClass : `oi-view-${entry.type === "list" || entry.type === "form" ? entry.type : "list"}`}`;
      icon.setAttribute("aria-hidden", "true");
      const text = document.createElement("span");
      text.className = "view-btn-label";
      text.textContent = label;
      btn.replaceChildren(icon, document.createTextNode(" "), text);

      // Standard click (left button)
      btn.addEventListener("click", (e: MouseEvent) => {
        e.preventDefault();
        if (entry.type !== this.activeType) {
          void this.onSwitchView(entry.type, { newWindow: false });
        }
      });

      // Middle-click (button 1 on auxclick) opens view in new window/tab
      btn.addEventListener("auxclick", (e: MouseEvent) => {
        if (e.button === 1) {
          e.preventDefault();
          void this.onSwitchView(entry.type, { newWindow: true });
        }
      });

      switcher.append(btn);
    }

    // Global cycle shortcut Alt+Shift+V
    this.keydownHandler = (e: KeyboardEvent) => {
      // e.code: on macOS Option+Shift+V produces another character in e.key.
      const target = e.target instanceof HTMLElement ? e.target : null;
      if (target?.closest("input,textarea,select,[contenteditable]:not([contenteditable=false])")) return;
      if (e.altKey && e.shiftKey && e.code === "KeyV") {
        e.preventDefault();
        this.cycleThroughViews();
      }
    };
    window.addEventListener("keydown", this.keydownHandler);

    this.container = switcher;
    return switcher;
  }

  dispose(): void {
    if (this.keydownHandler) {
      window.removeEventListener("keydown", this.keydownHandler);
      this.keydownHandler = undefined;
    }
    this.container?.remove();
    this.container = null;
  }
}
