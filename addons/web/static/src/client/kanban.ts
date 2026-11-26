/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
// The kanban view's cards: the card layout read from the
// description, the ungrouped card grid and its keyboard. The application owns every read; this file only draws records.
import { prepareRestrictedCard } from "@erpforall/elang";
import { displayValue } from "./fields.js";
import { icon, type IconName } from "./icons.js";
import { ClientError } from "./transport.js";
import type { Field, Node, View } from "./metadata.js";

/** Where each card field goes: the first field of the card (or of `<main>`) is the title, `<aside>` is the leading tile.
 *  `accent` is an integer `color` field the card declares invisible: it colours the card's edge and is not a line. */
export interface KanbanCardLayout { title: Field | undefined; lines: Field[]; aside: Field | undefined; footer: Field[]; accent: Field | undefined; restricted?: (record: Record<string, unknown>) => DocumentFragment }

/** The kanban root of a described view (`describeViews` already checked its shape). */
export function kanbanRoot(view: View): Extract<Node, { kind: "kanban" }> {
  const root = view.nodes[0];
  if (view.type !== "kanban" || root?.kind !== "kanban") throw new ClientError("server", "Invalid kanban view description.");
  return root;
}

export function kanbanLayout(view: View): KanbanCardLayout {
  const card = kanbanRoot(view).children.find(child => child.kind === "kanban_card");
  if (!card || card.kind !== "kanban_card") throw new ClientError("server", "A kanban view has no card.");
  const shown = (nodes: readonly Node[]): Field[] => nodes.flatMap(node => node.kind === "field" && !node.field.invisible ? [node.field] : []);
  const region = (name: "main" | "aside" | "footer"): Field[] => {
    const section = card.children.find(child => child.kind === "kanban_section" && child.region === name);
    return section && "children" in section ? shown(section.children) : [];
  };
  // Fields directly in the card read as `<main>` lines (reference card_arch_parser.js: the card is free markup).
  const main = [...shown(card.children), ...region("main")];
  const [title, ...lines] = main;
  // shortcut: the arch has no colour attribute on the kanban root, so an invisible `color` card field stands for it; read the
  // attribute instead once the server sends one.
  const declared = [...card.children, ...card.children.flatMap(child => child.kind === "kanban_section" ? child.children : [])];
  const accent = declared.flatMap(node => node.kind === "field" && node.field.invisible && node.field.name === "color" && node.field.kind === "integer" ? [node.field] : [])[0];
  const layout: KanbanCardLayout = { title, lines, aside: region("aside")[0], footer: region("footer"), accent };
  if (card.restricted) {
    const { arch, language, source_sha256, plan } = card.restricted;
    const render = prepareRestrictedCard(plan, { arch, language, sourceSha256: source_sha256 });
    const locale = language.replaceAll("_", "-");
    layout.restricted = record => {
      const projected: Record<string, unknown> = Object.create(null);
      for (const field of view.fields) {
        if (!Object.hasOwn(record, field.name)) continue;
        const value = record[field.name];
        const raw = value !== null && typeof value === "object" && field.kind === "many2one"
          ? (value as Record<string, unknown>)["id"] : value;
        if (raw === null || typeof raw === "boolean" || typeof raw === "number" || typeof raw === "string") {
          projected[field.name] = { raw_value: raw, value: displayValue(field, value, locale) };
        }
      }
      return render("card", JSON.stringify(projected), document);
    };
  }
  return layout;
}

/** Up to two initials of a value, for the tile that stands where an image will go (is Proposed). */
export function initials(value: unknown): string {
  const text = typeof value === "string" ? value : Array.isArray(value) && typeof value[1] === "string" ? value[1] : "";
  const words = text.trim().split(/\s+/u).filter(Boolean);
  const letters = (words.length > 1 ? [words[0]!, words[words.length - 1]!] : words.slice(0, 1)).map(word => [...word][0]!.toLocaleUpperCase());
  return letters.join("");
}

export interface KanbanRendered { node: HTMLElement; dispose: () => void }
export interface KanbanCardsOptions {
  layout: KanbanCardLayout;
  records: readonly Record<string, unknown>[];
  /** Draws one field of one record read-only (the application's field widgets). */
  renderField: (field: Field, record: Record<string, unknown>) => KanbanRendered;
  /** Opens a record; absent when the arch says `can_open="0"` or the action has no form. */
  open?: (id: number) => void;
}

const el = <K extends keyof HTMLElementTagNameMap>(tag: K, className: string, text?: string): HTMLElementTagNameMap[K] => {
  const node = document.createElement(tag);
  node.className = className;
  if (text !== undefined) node.textContent = text;
  return node;
};

/** Deterministic muted color palette index (1..11) for initials tiles when no portrait image exists. */
function tileTone(seed: string): number {
  let hash = 0;
  for (const ch of seed) hash = (hash * 31 + ch.codePointAt(0)!) >>> 0;
  return (hash % 11) + 1;
}

/** An icon beside a card line, from what its widget or kind says it holds (never from a field's name). */
function cardLineIcon(field: Field): IconName | undefined {
  if (field.widget === "email") return "mail";
  if (field.widget === "phone") return "phone";
  if (field.kind === "date" || field.kind === "datetime") return "calendar";
  return undefined;
}

function isEmptyCardValue(value: unknown): boolean {
  if (value === false || value === null || value === undefined || value === "") return true;
  if (Array.isArray(value) && value.length === 0) return true;
  return false;
}

export function kanbanCard(options: Pick<KanbanCardsOptions, "layout" | "renderField" | "open">, record: Record<string, unknown>, stops: (() => void)[]): HTMLElement {
  const {layout, renderField, open} = options;
  const hasMediaAside = layout.aside !== undefined && (layout.aside.widget === "image" || layout.aside.kind === "binary");
  const field = (target: HTMLElement, item: Field, record: Record<string, unknown>, className: string, withIcon = false): void => {
    const shown = renderField(item, record);
    stops.push(shown.dispose);
    const slot = el("div", className);
    slot.dataset["field"] = item.name;
    if (withIcon) {
      const glyph = cardLineIcon(item);
      if (glyph) {
        slot.classList.add("kanban-card-line-icon");
        slot.append(icon(glyph));
      }
    }
    slot.append(shown.node);
    target.append(slot);
  };
  const id = record["id"];
  if (typeof id !== "number") throw new ClientError("server", "A kanban record has no id.");
  const rawColor = layout.accent ? record[layout.accent.name] : undefined;
  const colorClass = typeof rawColor === "number" && rawColor > 0 ? ` kanban-card-accent-${Math.abs(Math.trunc(rawColor)) % 12}` : "";
  const card = el("article", `kanban-card${open ? " kanban-card-open" : ""}${hasMediaAside ? " kanban-card-has-media" : ""}${colorClass}`);
  card.setAttribute("role", "listitem");
  card.dataset["id"] = String(id);
  card.tabIndex = -1;
  if (layout.restricted) {
    card.append(layout.restricted(record));
    card.setAttribute("aria-label", card.textContent?.trim() || `Record ${id}`);
  } else {
    if (layout.aside) {
      if (hasMediaAside) {
        const media = el("div", "kanban-card-media");
        const rawImage = record[layout.aside.name];
        if (typeof rawImage === "string" && rawImage.length > 0) {
          const shown = renderField(layout.aside, record);
          stops.push(shown.dispose);
          media.append(shown.node);
        } else {
          const seedVal = layout.title ? record[layout.title.name] : record["name"];
          const seedStr = typeof seedVal === "string" ? seedVal : `Record ${id}`;
          const letter = initials(seedVal) || "?";
          const fallback = el("div", `kanban-card-media-initials kanban-tone-${tileTone(seedStr)}`, letter);
          fallback.setAttribute("aria-hidden", "true");
          media.append(fallback);
        }
        card.append(media);
      } else {
        const tile = el("div", "kanban-card-tile", initials(record[layout.aside.name]));
        tile.setAttribute("aria-hidden", "true");
        card.append(tile);
      }
    }
    const body = el("div", "kanban-card-body");
    if (layout.title) field(body, layout.title, record, "kanban-card-title");
    for (const line of layout.lines) {
      if (hasMediaAside && isEmptyCardValue(record[line.name])) continue;
      field(body, line, record, "kanban-card-line", hasMediaAside);
    }
    if (layout.footer.length) {
      const footer = el("div", "kanban-card-footer");
      for (const item of layout.footer) {
        if (hasMediaAside && isEmptyCardValue(record[item.name])) continue;
        field(footer, item, record, `kanban-card-meta${item.name === "activity_state" ? " kanban-card-meta-end" : ""}`);
      }
      if (footer.childElementCount > 0) body.append(footer);
    }
    card.append(body);
    const title = layout.title ? body.querySelector(".kanban-card-title")?.textContent?.trim() : "";
    card.setAttribute("aria-label", title || `Record ${id}`);
  }
  if (open) card.addEventListener("click", event => {
    if ((event.target as HTMLElement).closest("a, button, input, select, textarea")) return;
    open(id);
  });
  return card;
}

/** The card grid: one focusable article per record, roving focus by the arrow keys, Enter opens. */
export function renderKanbanCards(options: KanbanCardsOptions): KanbanRendered {
  const { layout, records, open } = options;
  const stops: (() => void)[] = [];
  const hasMediaAside = layout.aside !== undefined && (layout.aside.widget === "image" || layout.aside.kind === "binary");
  const grid = el("div", `kanban-grid${hasMediaAside ? " kanban-grid-profile" : ""}`);
  grid.setAttribute("role", "list");
  grid.setAttribute("aria-label", "Records");
  const cards = records.map(record => kanbanCard(options, record, stops));
  if (cards[0]) cards[0].tabIndex = 0;
  grid.append(...cards);
  // Rows come from the layout, so Up and Down follow what the user sees at any width.
  const move = (from: number, key: string): number => {
    if (key === "ArrowLeft") return from - 1;
    if (key === "ArrowRight") return from + 1;
    if (key === "Home") return 0;
    if (key === "End") return cards.length - 1;
    const top = cards[from]!.offsetTop, left = cards[from]!.offsetLeft;
    const rows = key === "ArrowDown" ? cards.slice(from + 1).filter(card => card.offsetTop > top) : cards.slice(0, from).filter(card => card.offsetTop < top).reverse();
    const row = rows.filter(card => card.offsetTop === rows[0]?.offsetTop);
    const target = row.reduce<HTMLElement | undefined>((best, card) => !best || Math.abs(card.offsetLeft - left) < Math.abs(best.offsetLeft - left) ? card : best, undefined);
    return target ? cards.indexOf(target) : from;
  };
  const onKey = (event: KeyboardEvent): void => {
    const card = (event.target as HTMLElement).closest<HTMLElement>(".kanban-card");
    if (!card || event.target !== card || event.altKey || event.ctrlKey || event.metaKey) return;
    const from = cards.indexOf(card);
    if (event.key === "Enter" && open) { event.preventDefault(); open(Number(card.dataset["id"])); return; }
    if (!["ArrowLeft", "ArrowRight", "ArrowUp", "ArrowDown", "Home", "End"].includes(event.key)) return;
    event.preventDefault();
    focusCard(cards[Math.max(0, Math.min(cards.length - 1, move(from, event.key)))]!);
  };
  const focusCard = (card: HTMLElement): void => {
    for (const other of cards) other.tabIndex = other === card ? 0 : -1;
    card.focus();
  };
  grid.addEventListener("keydown", onKey);
  return { node: grid, dispose: () => { grid.removeEventListener("keydown", onKey); for (const stop of stops.splice(0)) stop(); } };
}

/** Moves the roving focus to the card of `id` (the one a user opened and came back from). */
export function focusKanbanCard(grid: HTMLElement, id: number): boolean {
  const cards = [...grid.querySelectorAll<HTMLElement>(".kanban-card")];
  const card = cards.find(item => item.dataset["id"] === String(id));
  if (!card) return false;
  for (const other of cards) other.tabIndex = other === card ? 0 : -1;
  card.focus({ preventScroll: true });
  return true;
}
