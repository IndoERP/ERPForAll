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

function reportUnknownMenuIcon(value: string, problem = "is not a shipped icon"): void {
  const key = truncated(value);
  if (reportedMenuIcons.has(key)) return;
  if (reportedMenuIcons.size >= REPORTED_MENU_ICONS_MAX) {
    const oldest = reportedMenuIcons.values().next().value;
    if (oldest !== undefined) reportedMenuIcons.delete(oldest);
  }
  reportedMenuIcons.add(key);
  console.warn(`Menu icon ${JSON.stringify(key)} ${problem}; showing the default icon (${DEFAULT_MENU_ICON}).`);
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

/** Longest `<addon>,static/<path>` reference, in characters (the server caps a menu icon value at 2048 bytes). */
const MAX_ADDON_ICON_REFERENCE = 512;
const ADDON_NAME = /^[a-z][a-z0-9_]*$/;
const STATIC_SEGMENT = /^[A-Za-z0-9_-][A-Za-z0-9._-]*$/;
const IMAGE_DATA_URL = /^data:image\/(?:svg\+xml|png)(?:;base64)?,/;
/** Longest `data:` image a menu may carry, in characters: the server's menu icon cap. */
const MAX_DATA_URL = 2048;

/** The same-origin URL of an addon image a menu value names, or null when the value names none.
 *
 *  Accepted: `<addon>,static/<path>.svg|.png` (served as `/<addon>/static/<path>` from the addon's declared static
 *  files; the server publishes those only for installed addons and sends SVG with a sandboxing policy) and
 *  `data:image/svg+xml` or `data:image/png` URLs. Segments are plain names: no `..`, query, fragment or escape. */
export function menuIconImageUrl(value: string | null | undefined): string | null {
  if (typeof value !== "string") return null;
  if (IMAGE_DATA_URL.test(value)) return value.length <= MAX_DATA_URL ? value : null;
  if (value.length > MAX_ADDON_ICON_REFERENCE) return null;
  const comma = value.indexOf(",");
  if (comma < 0) return null;
  const addon = value.slice(0, comma), path = value.slice(comma + 1);
  if (!ADDON_NAME.test(addon)) return null;
  const segments = path.split("/");
  if (segments.length < 2 || segments[0] !== "static") return null;
  if (!segments.slice(1).every(segment => STATIC_SEGMENT.test(segment) && segment !== "." && segment !== "..")) return null;
  if (!/\.(?:svg|png)$/.test(path)) return null;
  return `/${addon}/${path}`;
}

/** An addon image drawn as `<img>`: an SVG in an image context runs no script and loads nothing else. The label
 *  beside it names the menu, so the image is decorative. A file that fails to load becomes the default glyph. */
function menuIconImage(value: string, url: string): HTMLImageElement {
  const image = document.createElement("img");
  image.src = url;
  image.alt = "";
  image.decoding = "async";
  image.setAttribute("aria-hidden", "true");
  image.setAttribute("width", "18");
  image.setAttribute("height", "18");
  image.className = "erp-icon erp-icon-image";
  image.dataset["icon"] = "image";
  image.addEventListener("error", () => {
    // Loading starts when `src` is set, but the error event is a later task: every caller has placed the image by then.
    reportUnknownMenuIcon(value, "could not be loaded (is its addon installed and the file a declared static file?)");
    const svg = icon(DEFAULT_MENU_ICON);
    svg.dataset["requestedIcon"] = "unavailable";
    for (const name of image.classList) if (name !== "erp-icon-image") svg.classList.add(name);
    image.replaceWith(svg);
  }, { once: true });
  return image;
}

/** Menu metadata selects a checked local identity or a checked same-origin addon image; no markup is evaluated.
 *  A value that names neither (an addon written against a newer icon set, a typo, a path outside `static/`) shows
 *  the default icon and is reported on the console, so one menu never stops the application from starting. */
export function menuIcon(value: string | null): SVGSVGElement | HTMLImageElement {
  if (value !== null && knownMenuIcon(value) === null) {
    const url = menuIconImageUrl(value);
    if (url !== null) return menuIconImage(value, url);
  }
  return menuGlyph(value);
}

/** The shipped glyph a menu value names, or the reported default. */
function menuGlyph(value: string | null): SVGSVGElement {
  if (value === null) return icon(DEFAULT_MENU_ICON);
  const name = knownMenuIcon(value);
  if (name !== null) return icon(name);
  reportUnknownMenuIcon(value);
  const svg = icon(DEFAULT_MENU_ICON);
  svg.dataset["requestedIcon"] = "unknown";
  return svg;
}

/** Use each entry's declared glyph or the shared fallback, including menus supplied by addons. */
export function menuEntryIcons(declared: readonly (string | null | undefined)[]): SVGSVGElement[] {
  return declared.map(name => menuGlyph(typeof name === "string" ? menuIconValue(name as IconName) : null));
}
