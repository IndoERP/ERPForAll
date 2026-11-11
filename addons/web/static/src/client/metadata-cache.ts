/*! SPDX-License-Identifier: MIT OR Apache-2.0 */

/**
 * Page-lifetime reuse of read-only action and view metadata.
 *
 * Adopted: one shared answer per identical request, in-flight
 * requests joined. Modified: memory only (nothing outlives the page), bounded, failed requests are never
 * kept, every reader gets its own copy, and any write to a metadata-bearing model drops every entry.
 */
export class MetadataCache {
  private epoch: object = {};
  private readonly entries = new Map<string, Promise<unknown>>();

  constructor(private readonly limit = 128) {
    if (!Number.isSafeInteger(limit) || limit < 1) throw new RangeError("Metadata cache limit must be a positive integer.");
  }

  read(key: string, load: () => Promise<unknown>): Promise<unknown> {
    let entry = this.entries.get(key);
    if (entry) {
      // Most recently used entries are evicted last.
      this.entries.delete(key);
      this.entries.set(key, entry);
    } else {
      const loaded = load();
      entry = loaded;
      this.entries.set(key, loaded);
      while (this.entries.size > this.limit) this.entries.delete(this.entries.keys().next().value as string);
      // The rejection still reaches every caller below; only the cache forgets it.
      loaded.catch(() => { if (this.entries.get(key) === loaded) this.entries.delete(key); });
    }
    return entry.then(value => structuredClone(value));
  }

  /** Token for one invalidation lifetime; callers capture it before loading a bundle. */
  get generation(): object { return this.epoch; }

  /** Publish a companion description without replacing a value or producer already owned here. */
  seed(key: string, value: unknown, generation: object): void {
    if (generation !== this.epoch || this.entries.has(key)) return;
    this.entries.set(key, Promise.resolve(structuredClone(value)));
    while (this.entries.size > this.limit) this.entries.delete(this.entries.keys().next().value as string);
  }

  clear(): void {
    this.epoch = {};
    this.entries.clear();
  }

  get size(): number {
    return this.entries.size;
  }
}

/**
 * Writes that can change what an action or view description contains for this user: technical
 * records (`ir.*`: views, actions, menus, filters, access, settings, modules) and the user/group
 * records that decide group-restricted fields and nodes.
 */
export function invalidatesMetadata(model: string): boolean {
  return model.startsWith("ir.") || model === "res.users" || model === "res.groups";
}
