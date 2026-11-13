/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
import { decodeGroups, type ListGroup } from "./list-groups.js";
import { decodePath, encodePath, type ListGroupState } from "./list-group-state.js";
import { ClientError } from "./transport.js";
import type { GroupableField, ListAggregate } from "./search/types.js";

export interface ListGroupNode {
  group: ListGroup; path: string; depth: number; domain: unknown[];
  expanded: boolean; busy: boolean; offset: number; total: number;
  children: ListGroupNode[]; records: unknown[]; error?: string; token: number;
}
interface Config {
  dimensions: { spec: string; field: GroupableField }[]; aggregates: ListAggregate[]; limit: number;
  groups(domain: unknown[], specs: string[], offset: number): Promise<unknown>;
  records(domain: unknown[], offset: number): Promise<unknown>;
  /** Validate the proposed address before publishing a mutation. */
  accept?(open: ListGroupState["open"]): void;
}
const bad = (): never => { throw new ClientError("server", "Invalid group records."); };
/** Owns only one captured search/context generation; disposing it invalidates every in-flight branch. */
export class ListGroupModel {
  roots: ListGroupNode[] = [];
  private dead = false;
  constructor(private readonly config: Config) {}
  private nodes(raw: unknown, depth: number, parent?: ListGroupNode): ListGroupNode[] {
    const dimension = this.config.dimensions[depth];
    if (!dimension) throw new ClientError("validation", "Invalid group depth.");
    return decodeGroups(raw, dimension.field, this.config.limit, { spec: dimension.spec, aggregates: this.config.aggregates }).groups.map(group => ({
      group, path: encodePath([...(parent ? decodePath(parent.path) : []), [dimension.spec, group.value]]), depth,
      domain: [...(parent?.domain ?? []), ...group.domain], expanded: false, busy: false, offset: 0, total: 0, children: [], records: [], token: 0
    }));
  }
  adopt(raw: unknown): void { this.roots = this.nodes(raw, 0); }
  opened(): ListGroupState["open"] {
    const result: ListGroupState["open"] = [];
    const visit = (nodes: ListGroupNode[]): void => { for (const node of nodes) if (node.expanded) { result.push({ path: node.path, offset: node.offset }); visit(node.children); } };
    visit(this.roots); return result;
  }
  private proposed(node: ListGroupNode, offset: number, expanded: boolean): ListGroupState["open"] {
    const path = decodePath(node.path), result = this.opened().filter(entry => {
      const other = decodePath(entry.path);
      return !(other.length >= path.length && path.every((part, index) => JSON.stringify(part) === JSON.stringify(other[index])));
    });
    if (expanded) result.push({ path: node.path, offset });
    return result;
  }
  close(node: ListGroupNode): void {
    this.config.accept?.(this.proposed(node, node.offset, false));
    this.invalidate(node); node.expanded = false; node.children = []; node.records = []; delete node.error;
  }
  private invalidate(node: ListGroupNode): void { node.token++; node.busy = false; for (const child of node.children) this.invalidate(child); }
  async open(node: ListGroupNode): Promise<void> { if (!node.busy && !node.expanded) await this.page(node, node.offset); }
  async page(node: ListGroupNode, offset: number): Promise<void> {
    if (this.dead || node.busy) return;
    if (!Number.isSafeInteger(offset) || offset < 0) throw new ClientError("validation", "Invalid group page.");
    this.config.accept?.(this.proposed(node, offset, true));
    const token = ++node.token; node.busy = true; delete node.error;
    const leaf = node.depth + 1 === this.config.dimensions.length;
    try {
      const read = (from: number) => leaf ? this.config.records(node.domain, from) : this.config.groups(node.domain, this.config.dimensions.slice(node.depth + 1).map(d => d.spec), from);
      let raw = await read(offset);
      if (this.dead || node.token !== token) return;
      const length = (answer: unknown): number => {
        if (!answer || typeof answer !== "object" || Array.isArray(answer)) return bad();
        const value = (answer as Record<string, unknown>)["length"];
        return typeof value === "number" && Number.isSafeInteger(value) && value >= 0 ? value : bad();
      };
      let total = length(raw);
      if (offset > 0 && offset >= total) {
        offset = total ? Math.floor((total - 1) / this.config.limit) * this.config.limit : 0;
        raw = await read(offset);
        if (this.dead || node.token !== token) return;
        total = length(raw);
      }
      let records: unknown[] = [], children: ListGroupNode[] = [];
      if (leaf) {
        const value = (raw as Record<string, unknown>)["records"];
        if (!Array.isArray(value) || value.length > this.config.limit || value.length > total || value.some(row => !row || typeof row !== "object" || !Number.isSafeInteger((row as Record<string, unknown>)["id"]) || Number((row as Record<string, unknown>)["id"]) <= 0)) return bad();
        records = value;
      } else children = this.nodes(raw, node.depth + 1, node);
      this.config.accept?.(this.proposed(node, offset, true));
      for (const child of node.children) this.invalidate(child);
      Object.assign(node, { offset, total, records, children, expanded: true });
    } catch (error) {
      if (this.dead || node.token !== token) return;
      for (const child of node.children) this.invalidate(child);
      node.children = []; node.records = []; node.error = error instanceof Error ? error.message : String(error);
      throw error;
    } finally { if (node.token === token) node.busy = false; }
  }
  async restore(open: ListGroupState["open"]): Promise<string[]> {
    const missing: string[] = [];
    const failures: string[] = [];
    // Siblings restore independently; a failed branch must not prevent another from reopening.
    for (let depth = 1; depth <= this.config.dimensions.length && !this.dead; depth++) {
      const entries = open.filter(entry => decodePath(entry.path).length === depth);
      const outcomes = await Promise.allSettled(entries.map(async entry => {
        const parts = decodePath(entry.path); let nodes = this.roots, found: ListGroupNode | undefined;
        for (let i = 0; i < parts.length; i++) {
          found = nodes.find(node => node.path === encodePath(parts.slice(0, i + 1)));
          if (!found || i + 1 < parts.length && !found.expanded) { found = undefined; break; }
          nodes = found.children;
        }
        if (!found) { missing.push(entry.path); return; }
        try { await this.page(found, entry.offset); }
        catch (error) { throw new ClientError("server", `${found.group.label}: ${error instanceof Error ? error.message : String(error)}`); }
      }));
      for (const result of outcomes) if (result.status === "rejected") failures.push(result.reason instanceof Error ? result.reason.message : String(result.reason));
    }
    if (failures.length) throw new ClientError("server", `Could not reopen ${failures.join("; ")}`);
    return missing;
  }
  dispose(): void { this.dead = true; for (const root of this.roots) this.invalidate(root); }
}
