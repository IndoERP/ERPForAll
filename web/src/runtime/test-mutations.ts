/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
/** @internal Test-only ownership seam. Ordinary applications never create a journal. */
export interface PatchMutation { readonly members: readonly PropertyKey[]; readonly managed: boolean; active: boolean; undo(): void; }
export interface MutationJournal {
  readonly patches: PatchMutation[];
  snapshot(owner: object, restore: () => (() => void)): void;
  patch(mutation: Omit<PatchMutation, "managed">): void;
  manage<T>(fn: () => T): T;
  track(work: PromiseLike<unknown>, preserveFailure?: boolean, teardown?: boolean): void;
  hasPendingTeardown(): boolean;
  drain(): Promise<unknown[]>;
  close(): void;
}
let active: MutationJournal | undefined;
export function mutationJournal(): MutationJournal | undefined { return active; }
/** The caller owns lifecycle and rejects concurrent/nested scopes before calling this. */
export function openMutationJournal(): MutationJournal {
  if (active !== undefined) throw new Error("A component test scope is already active");
  const seen = new WeakSet<object>();
  const restores: Array<() => void> = [];
  let managed = false;
  let closing = false;
  let draining = false;
  const pending = new Set<Promise<void>>();
  const failures = new Set<unknown>();
  let pendingTeardown = 0;
  const journal: MutationJournal = {
    patches: [],
    snapshot(owner, restore) { if (!closing && !seen.has(owner)) { seen.add(owner); restores.push(restore()); } },
    patch(mutation) { if (!closing) journal.patches.push({ ...mutation, managed }); },
    manage<T>(fn: () => T): T { const previous = managed; managed = true; try { return fn(); } finally { managed = previous; } },
    hasPendingTeardown() { return pendingTeardown > 0; },
    track(work, preserveFailure = false, teardown = false) {
      if (teardown) pendingTeardown++;
      const observed = Promise.resolve(work).then(
        () => { pending.delete(observed); if (teardown) pendingTeardown--; },
        error => {
          // Completed runtime work already reports through its normal caller/boundary.
          // Work still pending during teardown, and rejected manage results, belong to cleanup.
          if (draining || preserveFailure) failures.add(error);
          pending.delete(observed);
          if (teardown) pendingTeardown--;
        },
      );
      pending.add(observed);
    },
    async drain() {
      draining = true;
      while (pending.size > 0) await Promise.all([...pending]);
      const errors = [...failures]; failures.clear(); return errors;
    },
    close() {
      closing = true;
      const errors: unknown[] = [];
      for (const patch of [...journal.patches].reverse()) if (patch.active) {
        try { patch.undo(); } catch (error) { errors.push(error); }
      }
      for (const restore of restores.reverse()) { try { restore(); } catch (error) { errors.push(error); } }
      active = undefined;
      journal.patches.length = 0; restores.length = 0;
      if (errors.length) throw new AggregateError(errors, "Component test mutation rollback failed");
    },
  };
  active = journal;
  return journal;
}

/** @internal Observe known runtime work without replacing its result or error semantics. */
export function trackTestWork<T>(work: PromiseLike<T>, teardown = false): PromiseLike<T> {
  if (active === undefined) return work;
  // Normalize once so observing a custom thenable does not invoke it twice.
  const normalized = Promise.resolve(work);
  active.track(normalized, false, teardown);
  return normalized;
}
