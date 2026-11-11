/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
import { createRovingTabindex } from "../../../../../web/src/runtime/accessibility.js";
import { ClientError } from "./transport.js";

export type ButtonMenuClosingMode = "none" | "closest" | "all";
export interface ButtonMenuOptions {
  readonly opener: HTMLButtonElement;
  readonly menu: HTMLElement;
  readonly items: () => readonly HTMLButtonElement[];
  readonly closingMode?: ButtonMenuClosingMode | ((item: HTMLButtonElement) => ButtonMenuClosingMode);
}
export interface ButtonMenuController {
  open(edge?: "first" | "last"): void;
  close(restoreFocus?: boolean): void;
  refresh(): void;
  dispose(): void;
}
interface MenuOwner { readonly opener: HTMLButtonElement; readonly menu: HTMLElement; readonly controller: ButtonMenuController }
const menuOwners = new WeakMap<HTMLElement, MenuOwner>();
const openerOwners = new WeakMap<HTMLButtonElement, MenuOwner>();
let nextMenu = 0;
const modes: readonly string[] = ["none", "closest", "all"];
function closingMode(value: unknown): asserts value is ButtonMenuClosingMode {
  if (typeof value !== "string" || !modes.includes(value)) throw new ClientError("validation", `Unknown button menu closing mode: ${String(value)}.`);
}
function available(element: HTMLElement): boolean {
  if (!element.isConnected || element.matches(':disabled,[aria-disabled="true"]') || element.closest('[hidden],[inert],[aria-hidden="true"]')) return false;
  for (let parent: HTMLElement | null = element; parent; parent = parent.parentElement) {
    const style = parent.ownerDocument.defaultView!.getComputedStyle(parent);
    if (style.display === "none" || style.visibility === "hidden" || style.visibility === "collapse") return false;
  }
  return true;
}
function restore(element: HTMLElement, name: string, value: string | null): void {
  if (value === null) element.removeAttribute(name); else element.setAttribute(name, value);
}
/** Owns one dropdown's semantics and keyboard lifetime; original controls retain their handlers and disabled states. */
export function createButtonMenu(options: ButtonMenuOptions): ButtonMenuController {
  const { opener, menu } = options, doc = opener.ownerDocument;
  if (menu.ownerDocument !== doc) throw new ClientError("validation", "Button menu and opener must belong to the same document.");
  if (opener === menu || menu.contains(opener)) throw new ClientError("validation", "A button menu cannot contain its own opener.");
  if (menuOwners.has(menu) || openerOwners.has(opener)) throw new ClientError("validation", "Button menu already has an owner.");
  if (typeof options.items !== "function") throw new ClientError("validation", "Button menu items must be provided by a function.");
  if (options.closingMode !== undefined && typeof options.closingMode !== "function") closingMode(options.closingMode);
  const originalOpener = new Map(["id", "aria-controls", "aria-haspopup", "aria-expanded"].map(name => [name, opener.getAttribute(name)]));
  const originalMenu = new Map(["id", "role", "aria-labelledby", "hidden"].map(name => [name, menu.getAttribute(name)]));
  const originalItems = new Map<HTMLButtonElement, Map<string, string | null>>();
  const hadMenuClass = menu.classList.contains("erp-button-menu"), hadOpenerClass = opener.classList.contains("erp-button-menu-opener");
  let opened = false, disposed = false;
  let roving: ReturnType<typeof createRovingTabindex> | undefined;
  const view = doc.defaultView!;
  const placementProperties = new Set(["position", "inset", "inset-block", "inset-inline", "inset-block-start", "inset-block-end", "inset-inline-start", "inset-inline-end", "top", "right", "bottom", "left", "max-height", "max-width", "min-width", "min-inline-size", "overflow-y", "box-sizing"]);
  let placementStyles: Map<string, { value: string; priority: string }> | undefined;
  const restorePlacement = (): void => {
    view.removeEventListener("resize", positionMenu); doc.removeEventListener("scroll", positionMenu, true);
    if (!placementStyles) return;
    for (const property of placementProperties) menu.style.removeProperty(property);
    for (const [property, { value, priority }] of placementStyles) menu.style.setProperty(property, value, priority);
    placementStyles = undefined;
  };
  // A translated dialog establishes a fixed containing block. Retain descendant ownership
  // while converting viewport placement back into that ancestor's padding-box coordinates.
  const fixedOrigin = (): { left: number; top: number } => {
    for (let parent = menu.parentElement; parent; parent = parent.parentElement) {
      const style = view.getComputedStyle(parent);
      if (style.transform && style.transform !== "none" || style.perspective && style.perspective !== "none" || style.filter && style.filter !== "none" || /(?:^|\s)(?:paint|layout|strict|content)(?:\s|$)/.test(style.contain) || /(?:^|,\s*)(?:transform|perspective|filter)(?:,|$)/.test(style.willChange)) {
        const bounds = parent.getBoundingClientRect();
        return { left: bounds.left + parent.clientLeft - parent.scrollLeft, top: bounds.top + parent.clientTop - parent.scrollTop };
      }
    }
    return { left: 0, top: 0 };
  };
  const positionMenu = (): void => {
    if (!opened) return;
    const margin = 8, gap = 4;
    const maxWidth = Math.max(0, view.innerWidth - margin * 2), viewportHeight = view.innerHeight;
    // Clear the stylesheet's logical anchoring before setting physical viewport coordinates.
    menu.style.setProperty("position", "fixed");
    for (const property of ["inset-block-start", "inset-block-end", "inset-inline-start", "inset-inline-end", "right", "bottom"]) menu.style.setProperty(property, "auto");
    menu.style.setProperty("top", "0px"); menu.style.setProperty("left", "0px");
    menu.style.setProperty("max-width", `${maxWidth}px`); menu.style.setProperty("max-height", `${Math.max(0, viewportHeight - margin * 2)}px`);
    menu.style.setProperty("min-width", `min(10rem, ${maxWidth}px)`); menu.style.setProperty("min-inline-size", `min(10rem, ${maxWidth}px)`);
    menu.style.setProperty("overflow-y", "auto"); menu.style.setProperty("box-sizing", "border-box");
    const anchor = opener.getBoundingClientRect(), measured = menu.getBoundingClientRect();
    const below = Math.max(0, viewportHeight - margin - anchor.bottom - gap), above = Math.max(0, anchor.top - gap - margin);
    const useAbove = measured.height > below && above > below;
    const allowance = Math.min(Math.max(0, viewportHeight - margin * 2), useAbove ? above : below);
    const height = Math.min(measured.height, allowance), width = Math.min(measured.width, maxWidth);
    const desiredTop = useAbove ? anchor.top - gap - height : anchor.bottom + gap;
    const top = Math.max(margin, Math.min(desiredTop, viewportHeight - margin - height));
    const left = Math.max(margin, Math.min(anchor.right - width, view.innerWidth - margin - width));
    const origin = fixedOrigin();
    menu.style.setProperty("max-height", `${allowance}px`);
    menu.style.setProperty("top", `${top - origin.top}px`); menu.style.setProperty("left", `${left - origin.left}px`);
  };
  const currentItems = (): HTMLButtonElement[] => {
    const values = options.items();
    if (!Array.isArray(values)) throw new ClientError("validation", "Button menu items must be an array.");
    const items: HTMLButtonElement[] = [...values];
    if (items.length > 4096) throw new ClientError("validation", "Button menu exceeds the 4096-item limit.");
    if (new Set(items).size !== items.length || items.some(item => !(item instanceof doc.defaultView!.HTMLButtonElement) || !menu.contains(item) || item === opener || item.ownerDocument !== doc)) {
      throw new ClientError("validation", "Button menu items must be unique descendants of the menu.");
    }
    return items;
  };
  const syncItems = (): HTMLButtonElement[] => {
    const items = currentItems();
    for (const [item, attrs] of originalItems) if (!items.includes(item)) {
      for (const [name, value] of attrs) restore(item, name, value);
      originalItems.delete(item);
    }
    for (const item of items) {
      if (!originalItems.has(item)) originalItems.set(item, new Map(["role", "tabindex"].map(name => [name, item.getAttribute(name)])));
      item.setAttribute("role", "menuitem");
    }
    return items;
  };
  // Validate caller ownership before changing any attributes or registering listeners.
  currentItems();
  const uniqueId = (prefix: string): string => { let id: string; do { id = `${prefix}-${++nextMenu}`; } while (doc.getElementById(id)); return id; };
  if (!menu.id) menu.id = uniqueId("erp-button-menu");
  if (!opener.id) opener.id = uniqueId("erp-button-menu-opener");
  menu.setAttribute("role", "menu"); menu.setAttribute("aria-labelledby", opener.id); menu.hidden = true;
  opener.setAttribute("aria-controls", menu.id); opener.setAttribute("aria-haspopup", "menu"); opener.setAttribute("aria-expanded", "false");
  menu.classList.add("erp-button-menu"); opener.classList.add("erp-button-menu-opener");
  const parentOwner = (owner: MenuOwner): MenuOwner | undefined => {
    for (let parent = owner.opener.parentElement; parent; parent = parent.parentElement) {
      const candidate = menuOwners.get(parent); if (candidate && candidate !== owner) return candidate;
    }
    return undefined;
  };
  const focusOpener = (): void => { if (available(opener)) opener.focus(); };
  const close = (restoreFocus = true): void => {
    if (disposed || !opened) return;
    for (const childOpener of menu.querySelectorAll<HTMLButtonElement>("button")) {
      const child = openerOwners.get(childOpener); if (child && child.menu !== menu) child.controller.close(false);
    }
    opened = false; roving?.dispose(); roving = undefined; restorePlacement();
    menu.hidden = true; opener.setAttribute("aria-expanded", "false");
    if (restoreFocus) focusOpener();
  };
  const refresh = (): void => {
    if (disposed) throw new ClientError("validation", "Button menu is disposed.");
    const items = syncItems();
    if (!opened) return;
    const active = doc.activeElement, eligible = items.filter(available);
    roving?.refresh();
    if (!eligible.length || !available(opener)) { close(menu.contains(active)); return; }
    positionMenu();
    if (active instanceof HTMLElement && menu.contains(active) && !eligible.includes(active as HTMLButtonElement)) eligible[0]!.focus();
  };
  const open = (edge: "first" | "last" = "first"): void => {
    if (disposed) throw new ClientError("validation", "Button menu is disposed.");
    if (edge !== "first" && edge !== "last") throw new ClientError("validation", `Unknown button menu focus edge: ${edge}.`);
    if (!opener.isConnected || !menu.isConnected) throw new ClientError("validation", "Button menu and opener must be connected before opening.");
    if (!available(opener)) return;
    if (opened) {
      refresh();
      if (opened) { const eligible = syncItems().filter(available); (edge === "last" ? eligible.at(-1) : eligible[0])?.focus(); }
      return;
    }
    const items = syncItems(); menu.hidden = false;
    const eligible = items.filter(available);
    if (!eligible.length) { menu.hidden = true; return; }
    opened = true; opener.setAttribute("aria-expanded", "true");
    placementStyles = new Map(Array.from({ length: menu.style.length }, (_, index) => menu.style.item(index)).filter(property => placementProperties.has(property)).map(property => [property, { value: menu.style.getPropertyValue(property), priority: menu.style.getPropertyPriority(property) }]));
    try {
      positionMenu();
      roving = createRovingTabindex(menu, { items: () => syncItems(), orientation: "vertical" });
      view.addEventListener("resize", positionMenu); doc.addEventListener("scroll", positionMenu, true);
      (edge === "last" ? eligible.at(-1) : eligible[0])!.focus();
    } catch (error) { close(false); throw error; }
  };
  const openerClick = (): void => { if (!available(opener)) return; if (opened) close(); else open(); };
  const openerKey = (event: KeyboardEvent): void => {
    if (event.defaultPrevented || event.altKey || event.ctrlKey || event.metaKey || event.shiftKey) return;
    if (event.key === "ArrowDown" || event.key === "ArrowUp") { event.preventDefault(); open(event.key === "ArrowUp" ? "last" : "first"); }
  };
  const menuKey = (event: KeyboardEvent): void => {
    if (!opened || event.defaultPrevented || event.altKey || event.ctrlKey || event.metaKey) return;
    const items = syncItems().filter(available), index = items.indexOf(event.target as HTMLButtonElement);
    if (index < 0) return;
    if (event.key === "Escape") { event.preventDefault(); event.stopPropagation(); close(); }
    else if (event.key === "Tab") {
      event.preventDefault(); const next = (index + (event.shiftKey ? -1 : 1) + items.length) % items.length; items[next]!.focus();
    } else if (event.key === "Enter" || event.key === " ") { event.preventDefault(); items[index]!.click(); }
  };
  const eligibleClicks = new WeakSet<Event>();
  const captureClick = (event: MouseEvent): void => {
    if (!opened || !(event.target instanceof doc.defaultView!.Node)) return;
    const target = event.target;
    const item = syncItems().find(candidate => candidate === target || candidate.contains(target));
    if (item && available(item)) eligibleClicks.add(event);
  };
  const menuClick = (event: MouseEvent): void => {
    if (!opened) return;
    const target = event.target;
    if (!(target instanceof doc.defaultView!.Node)) return;
    const item = syncItems().find(candidate => candidate === target || candidate.contains(target));
    if (!item || !eligibleClicks.has(event) || openerOwners.has(item)) return;
    const mode = typeof options.closingMode === "function" ? options.closingMode(item) : options.closingMode ?? "all";
    closingMode(mode);
    if (mode === "none") return;
    const owners: MenuOwner[] = [owner];
    if (mode === "all") { for (let parent = parentOwner(owner); parent; parent = parentOwner(parent)) owners.push(parent); }
    const active = doc.activeElement;
    const restoreFocus = active === doc.body || owners.some(entry => entry.menu.contains(active) || entry.opener === active);
    owners.forEach(entry => entry.controller.close(false));
    if (restoreFocus) { const last = owners.at(-1)!; if (available(last.opener)) last.opener.focus(); }
  };
  const outside = (event: Event): void => {
    if (!opened || !(event.target instanceof doc.defaultView!.Node)) return;
    if (!menu.contains(event.target) && !opener.contains(event.target)) close(false);
  };
  const controller: ButtonMenuController = { open, close, refresh, dispose(): void {
    if (disposed) return;
    close(menu.contains(doc.activeElement)); disposed = true;
    opener.removeEventListener("click", openerClick); opener.removeEventListener("keydown", openerKey);
    menu.removeEventListener("keydown", menuKey); menu.removeEventListener("click", captureClick, true); menu.removeEventListener("click", menuClick); doc.removeEventListener("pointerdown", outside);
    for (const [item, attrs] of originalItems) for (const [name, value] of attrs) restore(item, name, value);
    originalItems.clear();
    for (const [name, value] of originalOpener) restore(opener, name, value);
    for (const [name, value] of originalMenu) restore(menu, name, value);
    if (!hadMenuClass) menu.classList.remove("erp-button-menu"); if (!hadOpenerClass) opener.classList.remove("erp-button-menu-opener");
    menuOwners.delete(menu); openerOwners.delete(opener);
  } };
  const owner: MenuOwner = { opener, menu, controller };
  menuOwners.set(menu, owner); openerOwners.set(opener, owner);
  opener.addEventListener("click", openerClick); opener.addEventListener("keydown", openerKey);
  menu.addEventListener("keydown", menuKey); menu.addEventListener("click", captureClick, true); menu.addEventListener("click", menuClick); doc.addEventListener("pointerdown", outside);
  return controller;
}
