/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
import { iconNodes } from "../../../../../web/vendor/lucide/nodes.js";
import { ClientError } from "./transport.js";

export type IconName = keyof typeof iconNodes;
/** The icon an app menu shows when it names none, or names one this client does not ship. */
export const DEFAULT_MENU_ICON: IconName = "layout-grid";
const MENU_ICON_PREFIX = "lucide:";
/** Unknown menu icon values already reported, so a re-rendered menu does not repeat the warning. */
// Unknown menu icon values already warned about: a bounded window of the most recent ones (oldest
// evicted first), each kept by a truncated key, so menu data cannot grow the page's memory.
const REPORTED_MENU_ICONS_MAX = 64;
const REPORTED_VALUE_MAX = 120;
const reportedMenuIcons = new Set<string>();

function truncated(value: string): string {
  return value.length <= REPORTED_VALUE_MAX ? value : `${value.slice(0, REPORTED_VALUE_MAX)}… (${value.length} characters)`;
}

function reportUnknownMenuIcon(value: string): void {
  const key = truncated(value);
  if (reportedMenuIcons.has(key)) return;
  if (reportedMenuIcons.size >= REPORTED_MENU_ICONS_MAX) {
    const oldest = reportedMenuIcons.values().next().value;
    if (oldest !== undefined) reportedMenuIcons.delete(oldest);
  }
  reportedMenuIcons.add(key);
  console.warn(`Menu icon ${JSON.stringify(key)} is not a shipped icon; showing the default icon (${DEFAULT_MENU_ICON}).`);
}

/** Decorative local SVG only. The owning control supplies its visible or accessible label. */
export function icon(name: IconName): SVGSVGElement {
  if (!Object.hasOwn(iconNodes, name)) throw new ClientError("unsupported", `Unknown client icon: ${name}.`);
  const svg = document.createElementNS("http://www.w3.org/2000/svg", "svg");
  for (const [key, value] of Object.entries({viewBox:"0 0 24 24", width:"18", height:"18", fill:"none", stroke:"currentColor", "stroke-width":"2", "stroke-linecap":"round", "stroke-linejoin":"round", "aria-hidden":"true", focusable:"false", class:"erp-icon", "data-icon":name})) svg.setAttribute(key, value);
  for (const [tag, attributes] of iconNodes[name]) {
    const child = document.createElementNS("http://www.w3.org/2000/svg", tag);
    for (const [key, value] of Object.entries(attributes)) child.setAttribute(key, value);
    svg.append(child);
  }
  return svg;
}

/** Every icon a menu may name, sorted, as the `lucide:<name>` values a menu stores. */
export function menuIconChoices(): readonly IconName[] {
  return (Object.keys(iconNodes) as IconName[]).sort();
}

export function menuIconValue(name: IconName): string { return MENU_ICON_PREFIX + name; }

/** The shipped icon a menu value names, or null when the value names none this client ships. */
export function knownMenuIcon(value: string | null | undefined): IconName | null {
  if (typeof value !== "string" || !value.startsWith(MENU_ICON_PREFIX)) return null;
  const name = value.slice(MENU_ICON_PREFIX.length);
  return Object.hasOwn(iconNodes, name) ? name as IconName : null;
}

/** Menu metadata selects a checked local identity; no markup or URL is evaluated. A value that names no
 *  shipped icon (an addon written against a newer icon set, a typo) shows the default icon and is reported
 *  on the console, so one menu never stops the application from starting. */
export function menuIcon(value: string | null): SVGSVGElement {
  if (value === null) return icon(DEFAULT_MENU_ICON);
  const name = knownMenuIcon(value);
  if (name === null) {
    reportUnknownMenuIcon(value);
    const svg = icon(DEFAULT_MENU_ICON);
    svg.dataset["requestedIcon"] = "unknown";
    return svg;
  }
  return icon(name);
}

/** The icons for the entries of one menu, in order. A menu shows an icon on every entry or on none, so the labels
 *  share one column: when any entry names an icon (or the menu has built-in entries that always carry one, `always`),
 *  the entries that name none get the default icon. A name this client does not ship is reported once and also
 *  shows the default icon. */
export function menuEntryIcons(declared: readonly (string | null | undefined)[], always = false): (SVGSVGElement | null)[] {
  const shown = always || declared.some(name => typeof name === "string");
  return declared.map(name => shown ? menuIcon(typeof name === "string" ? menuIconValue(name as IconName) : null) : null);
}
