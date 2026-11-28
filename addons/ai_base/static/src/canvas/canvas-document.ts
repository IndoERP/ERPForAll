/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
/** Rich output is a closed grammar of text and lists. Every value is inserted as literal text. */
function node<K extends keyof HTMLElementTagNameMap>(tag: K, text?: string): HTMLElementTagNameMap[K] {
  const el = document.createElement(tag);
  if (text !== undefined) el.textContent = text;
  return el;
}
function record(value: unknown): Record<string, unknown> {
  if (!value || typeof value !== "object" || Array.isArray(value)) throw new Error("Invalid document section.");
  return value as Record<string, unknown>;
}
function text(value: unknown): string {
  if (typeof value !== "string" || !value.trim() || value.length > 2048) throw new Error("A section requires text of up to 2,048 characters.");
  return value.trim();
}
function items(value: unknown): unknown[] {
  if (!Array.isArray(value) || value.length < 1 || value.length > 32) throw new Error("A section needs between 1 and 32 items.");
  return value;
}
const states = new Map([["pending","Pending"],["active","In progress"],["completed","Completed"],["cancelled","Cancelled"]]);

function block(raw: unknown): HTMLElement {
  const section = record(raw);
  switch (section["kind"]) {
    case "heading": return node("h4", text(section["text"]));
    case "paragraph": return node("p", text(section["text"]));
    case "callout": {
      const aside = node("aside", text(section["text"])); aside.className = "studio-callout";
      return aside;
    }
    case "list": {
      const ul = node("ul"); ul.className = "studio-bullets";
      for (const value of items(section["items"])) ul.append(node("li", text(value)));
      return ul;
    }
    case "facts": {
      const dl = node("dl"); dl.className = "studio-facts";
      for (const rawItem of items(section["items"])) {
        const item = record(rawItem);
        dl.append(node("dt",text(item["label"])),node("dd",text(item["value"])));
      }
      return dl;
    }
    case "code": {
      const figure = node("figure"); figure.className = "studio-code";
      if (section["language"] !== undefined) {
        const language = text(section["language"]);
        if (language.length > 32) throw new Error("Code language labels allow up to 32 characters.");
        figure.append(node("figcaption",language));
      }
      const pre = node("pre");
      // Preserve whitespace and indentation; never evaluate or highlight executable markup.
      text(section["text"]); pre.append(node("code", section["text"] as string));
      pre.tabIndex = 0; figure.append(pre); return figure;
    }
    case "table": {
      const columns = items(section["columns"]).map(text);
      if (columns.length > 12) throw new Error("A comparison table allows up to 12 columns.");
      const scroll = node("div"); scroll.className = "studio-matrix";
      scroll.tabIndex = 0; scroll.setAttribute("role","region"); scroll.setAttribute("aria-label","Comparison table");
      const table = node("table"), head = table.createTHead().insertRow(), body = table.createTBody();
      for (const title of columns) { const cell = node("th",title); cell.scope = "col"; head.append(cell); }
      for (const rawRow of items(section["rows"])) {
        if (!Array.isArray(rawRow) || rawRow.length !== columns.length) throw new Error("Each table row must match the number of columns.");
        const row = body.insertRow();
        for (const value of rawRow) row.insertCell().textContent = text(value);
      }
      scroll.append(table); return scroll;
    }
    case "checklist": {
      const ul = node("ul"); ul.className = "studio-checklist"; ul.setAttribute("aria-label","Checklist");
      for (const rawItem of items(section["items"])) {
        const item = record(rawItem), state = text(item["state"]);
        const label = states.get(state);
        if (!label) throw new Error("Unrecognized checklist state.");
        const li = node("li"), status = node("span", label);
        status.className = "studio-step-status"; status.dataset["state"] = state;
        li.append(node("span", text(item["label"])), status); ul.append(li);
      }
      return ul;
    }
    case "timeline": {
      const ol = node("ol"); ol.className = "studio-timeline"; ol.setAttribute("aria-label","Timeline");
      for (const rawItem of items(section["items"])) {
        const item = record(rawItem), li = node("li"), copy = node("div");
        copy.append(node("strong", text(item["label"])));
        if (item["detail"] !== undefined) copy.append(node("p", text(item["detail"])));
        li.append(node("span", text(item["when"])), copy);
        ol.append(li);
      }
      return ol;
    }
    default: throw new Error("Unsupported document section.");
  }
}

export function renderStudioDocument(kind: string, data: Record<string, unknown>): HTMLElement {
  const content = node("div"); content.className = "studio-document";
  if (kind === "checklist" || kind === "timeline") content.append(block({kind, items:data["items"]}));
  else for (const section of items(data["sections"])) content.append(block(section));
  return content;
}
