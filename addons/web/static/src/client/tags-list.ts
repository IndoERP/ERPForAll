/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
import { button, el } from "./dom.js";
import { editorOptions, editorText, EDITOR_OPTIONS_BYTES, EDITOR_OPTIONS_LIMIT } from "./editor-types.js";
import { ClientError } from "./transport.js";
export interface TagPresentation { id: string; label: string }
export interface TagsListOptions { tags: readonly TagPresentation[]; limit: number; label?: string }
/** Presentation-only overflow expansion; never creates or removes relation records. */
export function createTagsList(options: TagsListOptions): { node: HTMLElement; dispose(): void } {
  editorOptions(options, ["tags", "limit", "label"], "Tags list");
  if (!Number.isInteger(options.limit) || options.limit < 1 || options.limit > 1000) throw new ClientError("validation", "Tag limit must be an integer from 1 to 1000.");
  if (!Array.isArray(options.tags) || options.tags.length > EDITOR_OPTIONS_LIMIT) throw new ClientError("validation", "Tags must be an array of at most 1024 entries.");
  if (options.label !== undefined) editorText(options.label, "Tags label");
  const seen = new Set<string>(); let bytes = 0;
  const tags = options.tags.map(tag => {
    editorOptions(tag, ["id", "label"], "Tag"); editorText(tag.id, "Tag identity"); editorText(tag.label, "Tag label");
    if (!tag.id || seen.has(tag.id)) throw new ClientError("validation", "Tag identities must be nonempty and unique.");
    seen.add(tag.id); bytes += new TextEncoder().encode(tag.id + tag.label).length;
    if (bytes > EDITOR_OPTIONS_BYTES) throw new ClientError("validation", "Tag text exceeds 262144 bytes.");
    return { ...tag };
  });
  const node = el("div", undefined, "choice-tags"); node.setAttribute("role", "list"); node.setAttribute("aria-label", options.label ?? "Tags"); node.tabIndex = -1;
  let dead = false;
  const render = (expanded: boolean): void => {
    node.replaceChildren();
    const count = expanded || tags.length <= options.limit ? tags.length : options.limit - 1;
    for (const tag of tags.slice(0, count)) { const chip = el("span", tag.label, "choice-tag"); chip.setAttribute("role", "listitem"); node.append(chip); }
    if (count < tags.length) {
      const item = el("span"); item.setAttribute("role", "listitem");
      const more = button(`+${tags.length - count}`, () => { if (!dead) { render(true); node.focus(); } }, "choice-tag-more");
      more.setAttribute("aria-label", `Show ${tags.length - count} more tags`); more.title = tags.slice(count).map(tag => tag.label).join(", ");
      item.append(more); node.append(item);
    }
  };
  render(false); return { node, dispose: () => { dead = true; } };
}
