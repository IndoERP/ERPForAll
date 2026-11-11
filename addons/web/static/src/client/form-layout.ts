/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
import { button, el } from "./dom.js";
import type { ButtonNode, CancelNode, Node } from "./metadata.js";
import { createButtonControl } from "./button-control.js";
import { createButtonMenu } from "./button-menu.js";

/** Partition already rendered, visible controls without cloning their action or statistic state. */
function responsiveActions(host: HTMLElement, kind: "header" | "box"): () => void {
  const controls = [...host.children].filter((node): node is HTMLButtonElement => node instanceof HTMLButtonElement);
  if (!controls.length) return () => {};
  const opener = createButtonControl({ label: kind === "header" ? "More actions" : "More", action: () => {}, className: "form-overflow-toggle" });
  const menu = el("div", undefined, "form-overflow-menu");
  const controller = createButtonMenu({ opener, menu, items: () => controls.filter(control => control.parentElement === menu) });
  let directCount = -1;
  const arrange = (): void => {
    // ERPForAll's compact office layout uses its existing 700px phone breakpoint.
    // Box allowances are our own presentation choices, not upstream pixel geometry.
    const width = host.ownerDocument.defaultView!.innerWidth;
    const allowance = kind === "header" ? width <= 700 ? 1 : controls.length : width <= 700 ? 0 : width <= 1000 ? 4 : 6;
    const next = controls.length <= allowance ? controls.length : kind === "header" ? allowance : Math.max(0, allowance - 1);
    if (next === directCount) return;
    const focused = host.ownerDocument.activeElement;
    const focusedControl = focused instanceof HTMLButtonElement && controls.includes(focused) ? focused : undefined;
    const movedFocus = focusedControl !== undefined && controls.indexOf(focusedControl) >= next;
    const lostToggle = focused === opener && next === controls.length;
    controller.close(false);
    directCount = next;
    for (const [index, control] of controls.entries()) (index < next ? host : menu).append(control);
    if (next < controls.length) host.append(opener, menu);
    else { opener.remove(); menu.remove(); }
    // A former menu item becomes an ordinary toolbar/statistic control again.
    controller.refresh();
    if (movedFocus && opener.isConnected && !opener.disabled) opener.focus();
    // Reparenting can blur even a control that remains direct; retain its original identity.
    else if (focusedControl?.parentElement === host && focusedControl.isConnected && !focusedControl.matches(':disabled,[aria-disabled="true"]') && !focusedControl.closest('[hidden],[inert],[aria-hidden="true"]')) focusedControl.focus();
    else if (lostToggle) controls.find(control => !control.disabled)?.focus();
  };
  arrange();
  const view = host.ownerDocument.defaultView!;
  view.addEventListener("resize", arrange);
  return () => { view.removeEventListener("resize", arrange); controller.dispose(); };
}

let nextNotebook = 0;
/** Renders only admitted structure; record widgets and persistence remain with the caller. */
export function renderFormLayout(
  host: HTMLElement, nodes: readonly Node[],
  renderField: (node: Extract<Node, { kind: "field" }>, parent: HTMLElement) => void,
  selectedPages: Map<Node, number>,
  /** Header buttons act on the open record; a surface without one (a one2many line form) shows no header. */
  renderButton?: (node: ButtonNode, parent: HTMLElement) => HTMLElement | void,
  renderFooter?: (node: ButtonNode | CancelNode, parent: HTMLElement) => void,
  presentation?: { headerHost?: HTMLElement; headerActionsHost?: HTMLElement; notebookTabsHost?: HTMLElement; asideHost?: HTMLElement; chatterHost?: HTMLElement; renderChatter?(node: Extract<Node, { kind: "chatter" }>, slot: HTMLElement): void; titleHost?: HTMLElement; displayWidget?(node: Extract<Node,{kind:"ribbon"|"notification_alert"}>,parent:HTMLElement):void }
): { revealField(field: HTMLElement): void; dispose(): void } {
  const stops: (() => void)[] = [];
  const revealers = new Map<HTMLElement, () => void>();
  const asideContent = (children: readonly Node[]): boolean => children.some(node => {
    if (node.kind === "field") return !node.field.invisible;
    if (node.kind === "group") return asideContent(node.children);
    if (node.kind === "ribbon" || node.kind === "notification_alert") return true;
    if (node.kind === "separator") return false;
    throw new Error(`Unexpected aside node: ${node.kind}.`);
  });
  const titleContent = (children: readonly Node[]): boolean => children.some(node => {
    if (node.kind === "field") return !node.field.invisible;
    if (node.kind === "heading") return titleContent(node.children);
    throw new Error(`Unexpected title node: ${node.kind}.`);
  });
  const render = (node: Node, parent: HTMLElement, columns: number): void => {
    if (node.kind === "root") { node.children.forEach(child => render(child, parent, columns)); return; }
    if (node.kind === "field") { if (!node.field.invisible) renderField(node, parent); return; }
    if(node.kind === "ribbon" || node.kind === "notification_alert") {
      if(!presentation?.displayWidget)throw new Error("Record display widgets require their form message owner.");
      presentation.displayWidget(node,parent);return;
    }
    if (node.kind === "title") {
      if (!titleContent(node.children)) return;
      const title = el("div", undefined, "form-title-block");
      const topLevel = parent === host || parent.classList.contains("form-sheet-layout");
      (topLevel ? presentation?.titleHost ?? parent : parent).append(title);
      node.children.forEach(child => render(child, title, 2)); return;
    }
    if (node.kind === "heading") {
      const heading = el(node.level === 1 ? "h1" : "h2", undefined, "form-title-heading");
      parent.append(heading); node.children.forEach(child => render(child, heading, 2));
      // The title's caption is visually hidden (client.css), so an empty title shows it as its placeholder.
      for (const input of heading.querySelectorAll<HTMLInputElement>("input:not([type=checkbox]):not([type=radio]):not([placeholder])")) {
        const caption = input.closest(".field-label")?.querySelector(":scope > span")?.textContent?.replace(/\s*\*$/, "").trim();
        if (caption) input.placeholder = caption;
      }
      return;
    }
    if (node.kind === "aside") {
      if (!asideContent(node.children)) return;
      const aside = el("div", undefined, "form-aside");
      (presentation?.asideHost ?? parent).append(aside);
      node.children.forEach(child => render(child, aside, 2)); return;
    }
    if (node.kind === "chatter") {
      // Only the main-route form passes a chatter host; dialogs, x2many forms and the relation editor render nothing.
      if (!presentation?.chatterHost) return;
      const slot = el("div", undefined, "form-chatter-slot");
      slot.dataset["reloadOnPost"] = String(node.reloadOnPost);
      slot.dataset["reloadOnAttachment"] = String(node.reloadOnAttachment);
      slot.dataset["reloadOnActivity"] = String(node.reloadOnActivity);
      slot.dataset["reloadOnFollower"] = String(node.reloadOnFollower);
      slot.dataset["openAttachments"] = String(node.openAttachments);
      slot.dataset["hasActivities"] = String(node.hasActivities);
      presentation.chatterHost.append(slot);
      presentation.renderChatter?.(node, slot);
      return;
    }
    if (node.kind === "separator") { parent.append(el("h2", node.label, "form-separator")); return; }
    if (node.kind === "header") {
      const bar = el("div", undefined, "form-header");
      const actions = el("div", undefined, "form-header-actions");
      actions.setAttribute("role", "toolbar"); actions.setAttribute("aria-label", "Record actions");
      (presentation?.headerHost ?? parent).append(bar);
      node.children.forEach(child => {
        if (child.kind === "button") { if (renderButton) render(child, actions, 2); }
        else render(child, bar, 2);
      });
      if (actions.childElementCount) {
        if (presentation?.headerActionsHost) presentation.headerActionsHost.append(actions);
        else bar.prepend(actions);
        stops.push(responsiveActions(actions, "header"));
      }
      if (!bar.childElementCount) bar.remove();
      return;
    }
    // The footer belongs to a dialog presentation; the main-route form never renders it.
    if (node.kind === "footer") {
      if (renderFooter) node.children.forEach(child => {
        if (child.kind !== "button" && child.kind !== "cancel_button") throw new Error("Invalid dialog footer.");
        renderFooter(child, parent);
      });
      return;
    }
    if (node.kind === "button_box") {
      if (!renderButton) return;
      const box = el("div", undefined, "form-button-box"); box.setAttribute("role", "group");
      box.setAttribute("aria-label", "Related records"); parent.append(box);
      node.children.forEach(child => render(child, box, 2));
      if (!box.children.length) box.remove();
      else stops.push(responsiveActions(box, "box"));
      return;
    }
    if (node.kind === "button" || node.kind === "stat_button") {
      if (!renderButton) throw new Error("Header buttons need a button renderer.");
      const control = renderButton(node, parent);
      if (node.kind === "stat_button" && control) {
        control.classList.add("form-stat-button");
        const declaredIcon = control.querySelector(":scope > svg.erp-icon");
        control.replaceChildren(...(declaredIcon ? [declaredIcon] : []));
        const stats = el("span", undefined, "form-stat-content"); control.append(stats);
        node.children.forEach(child => render(child, stats, 2));
        if (!stats.children.length) stats.append(el("span", node.label, "form-stat-label"));
        const label = (): void => {
          const rendered = stats.textContent?.trim() ?? "";
          const identity=node.label.trim()?node.label:node.details?.title??"";
          control.setAttribute("aria-label", rendered.includes(identity) ? rendered : `${rendered} ${identity}`.trim());
        };
        control.addEventListener("statistic-change", label); label();
      }
      return;
    }
    if (node.kind === "sheet") {
      const sheet = el("div", undefined, "form-sheet-layout"); parent.append(sheet);
      node.children.forEach(child => render(child, sheet, 2)); return;
    }
    if (node.kind === "group") {
      if (parent.closest(".form-aside") && !asideContent(node.children)) return;
      const group = el("section", undefined, "form-group");
      group.style.gridColumn = parent.classList.contains("form-group-grid") ? `span ${Math.min(columns, node.colspan / 2)}` : "1 / -1";
      if (node.label) {
        const title = el("h2", node.label, "form-group-title"); group.append(title);
      }
      const grid = el("div", undefined, "form-group-grid");
      grid.style.setProperty("--form-columns", String(node.columns / 2)); group.append(grid); parent.append(group);
      node.children.forEach(child => render(child, grid, node.columns / 2)); return;
    }
    if (node.kind === "notebook") {
      const notebook = el("div", undefined, "form-notebook"), tabs = el("div", undefined, "form-tabs");
      tabs.setAttribute("role", "tablist"); tabs.setAttribute("aria-label", "Record details");
      // Only lift sheet-level navigation; nested notebooks keep their declared containing page.
      const tabsHost = parent === host || parent.classList.contains("form-sheet-layout") ? presentation?.notebookTabsHost : undefined;
      (tabsHost ?? notebook).append(tabs); parent.append(notebook);
      const id = `form-notebook-${++nextNotebook}`;
      const pages: { tab: HTMLButtonElement; panel: HTMLElement }[] = [];
      const activate = (index: number, focus = false): void => {
        selectedPages.set(node, index);
        pages.forEach(({ tab, panel }, i) => {
          const current = i === index; tab.setAttribute("aria-selected", String(current));
          tab.tabIndex = current ? 0 : -1; panel.hidden = !current;
        });
        if (focus) pages[index]?.tab.focus();
      };
      node.children.forEach((page, index) => {
        // Metadata admission guarantees a notebook contains only pages.
        if (page.kind !== "page") throw new Error("Invalid notebook page structure.");
        const panel = el("div", undefined, "form-tab-panel");
        panel.id = `${id}-panel-${index}`; panel.setAttribute("role", "tabpanel"); panel.tabIndex = 0;
        const tab = button(page.label, () => activate(index), "form-tab");
        tab.id = `${id}-tab-${index}`; tab.setAttribute("role", "tab"); tab.setAttribute("aria-controls", panel.id);
        panel.setAttribute("aria-labelledby", tab.id);
        tab.addEventListener("keydown", event => {
          let next: number;
          if (event.key === "Home") next = 0;
          else if (event.key === "End") next = pages.length - 1;
          else if (event.key === "ArrowRight") next = (index + 1) % pages.length;
          else if (event.key === "ArrowLeft") next = (index + pages.length - 1) % pages.length;
          else return;
          event.preventDefault(); activate(next, true);
        });
        pages.push({ tab, panel }); tabs.append(tab); notebook.append(panel);
        revealers.set(panel, () => activate(index));
        page.children.forEach(child => render(child, panel, 2));
      });
      const selected = selectedPages.get(node) ?? 0;
      if (pages.length) activate(Math.min(selected, pages.length - 1));
      return;
    }
    throw new Error(`Unexpected record form node: ${node.kind}.`);
  };
  nodes.forEach(node => render(node, host, 2));
  return { dispose(): void { stops.splice(0).forEach(stop => stop()); }, revealField(field): void {
    const ancestors: HTMLElement[] = [];
    for (let parent = field.parentElement; parent && parent !== host; parent = parent.parentElement) ancestors.push(parent);
    for (const panel of ancestors.reverse()) revealers.get(panel)?.();
  } };
}
