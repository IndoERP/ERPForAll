/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
/** The identity obtained with the authorized arch, independent of the plan being rendered. */
export interface CardIdentity {
  readonly arch: string;
  readonly sourceSha256: string;
  readonly language: string;
}

const tags = new Set([
  "div", "span", "p", "strong", "b", "em", "i", "small", "br", "ul", "ol", "li",
  "section", "article", "header", "footer", "h2", "h3", "h4", "time",
]);
const attributes = new Set(["class", "title", "aria-label", "role", "dir", "lang"]);
const properties = new Set(["tag", "when", "field", "call", "attrs", "children"]);

function object(value: unknown): Record<string, unknown> {
  if (typeof value !== "object" || value === null || Array.isArray(value)) {
    throw new Error("Expected a card object");
  }
  return value as Record<string, unknown>;
}

function readJson(source: string, maxBytes: number): Record<string, unknown> {
  if (source.length > maxBytes || new TextEncoder().encode(source).length > maxBytes) {
    throw new Error("Card input exceeds its byte budget");
  }
  return object(JSON.parse(source));
}

function own(record: Record<string, unknown>, key: string): unknown {
  if (!Object.hasOwn(record, key)) throw new Error("Missing card property");
  return record[key];
}

/** Prepare once per authorized arch and render into a detached fragment for each record. */
export function prepareRestrictedCard(source: string, identity: CardIdentity) {
  const plan = readJson(source, 4 * 1024 * 1024);
  if (plan["abi"] !== "erpforall.card.v1" || plan["policy"] !== "1" ||
      plan["arch"] !== identity.arch || plan["language"] !== identity.language ||
      !/^[a-f0-9]{64}$/.test(identity.sourceSha256) || plan["source_sha256"] !== identity.sourceSha256) {
    throw new Error("Card identity or compiler policy does not match the authorized arch");
  }
  if (!Array.isArray(plan["fields"]) || plan["fields"].length > 256 ||
      !plan["fields"].every(name => typeof name === "string" && /^[A-Za-z][A-Za-z0-9_]{0,127}$/.test(name))) {
    throw new Error("Invalid card field declarations");
  }
  const fields = new Set<string>(plan["fields"]);
  const templates = object(plan["templates"]);
  if (Object.keys(templates).length > 256) throw new Error("Too many card templates");

  return function render(template: string, recordJson: string, doc: Document): DocumentFragment {
    const record = readJson(recordJson, 128 * 1024);
    const root = doc.createDocumentFragment();
    let nodes = 0;
    let characters = 0;

    function project(projection: unknown): string | number | boolean | null {
      if (!Array.isArray(projection) || projection.length !== 2) throw new Error("Invalid card projection");
      const [name, property] = projection;
      if (!fields.has(name) || (property !== "value" && property !== "raw_value")) {
        throw new Error("Unknown card field or projection");
      }
      const value = own(object(own(record, name)), property);
      if (typeof value === "string" || typeof value === "boolean" || value === null ||
          (typeof value === "number" && Number.isFinite(value))) return value;
      throw new Error("Card fields must be scalar");
    }

    function account(value: string): string {
      characters += value.length;
      if (characters > 1024 * 1024) throw new Error("Card output exceeds its character budget");
      return value;
    }

    function append(raw: unknown, parent: ParentNode, depth: number): void {
      if (++nodes > 16_384 || depth > 64) throw new Error("Card expansion exceeds its node or depth budget");
      const node = object(raw);
      const keys = Object.keys(node);
      if (keys.length === 1 && typeof node["text"] === "string") {
        parent.appendChild(doc.createTextNode(account(node["text"])));
        return;
      }
      if (keys.some(key => !properties.has(key)) || typeof node["tag"] !== "string") throw new Error("Unknown card node");
      const transparent = node["tag"] === "t" || node["tag"] === "field";
      if (!transparent && !tags.has(node["tag"])) throw new Error("Unknown card element");
      if (Object.hasOwn(node, "when") && !project(node["when"])) return;
      const element = transparent ? null : doc.createElement(node["tag"]);
      if (node["attrs"] !== undefined) {
        if (!element) throw new Error("Virtual card elements cannot carry attributes");
        for (const [key, value] of Object.entries(object(node["attrs"]))) {
          if (!attributes.has(key) || typeof value !== "string") throw new Error("Invalid card attribute");
          element.setAttribute(key, account(value));
        }
      }
      const content = element ?? parent;
      if (["field", "call", "children"].filter(key => Object.hasOwn(node, key)).length !== 1) {
        throw new Error("A card node must have one body");
      }
      if (Object.hasOwn(node, "field")) {
        content.appendChild(doc.createTextNode(account(String(project(node["field"]) ?? ""))));
      } else if (typeof node["call"] === "string" && node["tag"] === "t") {
        append(own(templates, node["call"]), content, depth + 1);
      } else if (Array.isArray(node["children"])) {
        for (const child of node["children"]) append(child, content, depth + 1);
      } else {
        throw new Error("Invalid card body");
      }
      if (element) parent.appendChild(element);
    }

    append(own(templates, template), root, 1);
    return root;
  };
}
