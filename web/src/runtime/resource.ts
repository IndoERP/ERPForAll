/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
// Resource primitive with AbortController, latest-wins resolution, and error routing.

import { UIError } from "./errors.js";
import { type Owner, getOwner, handleError } from "./owner.js";
import { KEEP, type Signal, createEffect, reportUnhandled, signal, untrack } from "./reactive.js";

export interface ResourceOptions<T> {
  initialValue?: T;
  /** Start only at the first `refetch()`; from then on the resource follows its dependencies. */
  lazy?: boolean;
  /**
   * Handles the error of a fetch started by the resource itself (at creation or on a dependency change)
   * instead of the owner's error chain. An error it throws goes to the owner's chain (or is reported
   * when there is no owner). A fetch started by `refetch()` rejects its promise instead.
   */
  onError?: (err: unknown) => void;
}

export interface Resource<T> {
  data: Signal<T | undefined>;
  loading: Signal<boolean>;
  error: Signal<unknown | null>;
  /**
   * Starts a fetch now (aborting one in flight). Resolves once it settled into `data`, or once it was
   * aborted or superseded by a later fetch; rejects with its error (which also goes to `error()`).
   */
  refetch: () => Promise<void>;
  /** Aborts the fetch in flight, if any: its signal is aborted, `loading` turns false, `data` is kept. */
  abort: () => void;
}

/** Who receives the error of a fetch: the caller of `refetch()`, or the resource's own routing. */
type Origin = "refetch" | "tracked";

/**
 * Creates an async resource bound to component ownership.
 * Guarantees latest-wins resolution and aborts previous pending fetches.
 * Automatically tracks reactive signal dependency changes read inside fetcher (design note §6): the
 * reads before the fetcher's first `await`, in a component (outside one there is no owner to keep the
 * tracking effect, and the resource fetches only at creation and on `refetch()`).
 *
 * Errors are never dropped (review round 3, R10): a fetch started by `refetch()` rejects that call's
 * promise; one the resource started itself goes to `options.onError`, else to the owner's error chain,
 * else (no owner) to `globalThis.reportError`.
 */
export function resource<T>(
  fetcher: (signal: AbortSignal) => Promise<T>,
  options: ResourceOptions<T> = {},
): Resource<T> {
  const data = signal<T | undefined>(options.initialValue);
  const loading = signal<boolean>(false);
  const error = signal<unknown | null>(null);

  const owner = getOwner();
  let controller: AbortController | null = null;
  let activeSequence = 0;
  /** Whether the tracking effect exists (a lazy resource creates it at its first refetch, R11). */
  let tracking = false;

  const route = (err: unknown): void => {
    if (options.onError !== undefined) {
      try {
        options.onError(err);
        return;
      } catch (e) {
        err = e;
      }
    }
    if (owner !== null && !owner.disposed) handleError(owner, err);
    else reportUnhandled(err);
  };

  /**
   * Starts a fetch. `tracked`: called from the tracking effect, whose dependencies are what the fetcher
   * reads before its first await; otherwise the fetcher runs untracked, so a `refetch()` called inside
   * some other computation never makes it depend on the fetcher's reads.
   */
  const start = async (origin: Origin, tracked: boolean): Promise<void> => {
    if (controller !== null) controller.abort();
    const own = new AbortController();
    controller = own;
    const abortSignal = own.signal;
    const currentSeq = ++activeSequence;
    untrack(() => loading.set(true));

    let result: T;
    try {
      result = await (tracked ? fetcher(abortSignal) : untrack(() => fetcher(abortSignal)));
    } catch (err) {
      if (abortSignal.aborted || currentSeq !== activeSequence) return;
      controller = null;
      untrack(() => {
        loading.set(false);
        error.set(err);
      });
      if (origin === "refetch") throw err;
      route(err);
      return;
    }
    if (abortSignal.aborted || currentSeq !== activeSequence) return;
    controller = null;
    untrack(() => {
      data.set(result);
      loading.set(false);
      error.set(null);
    });
  };

  /** Creates the tracking effect; its first run starts a fetch for `origin` and returns its promise. */
  const track = (origin: Origin, keeper: Owner): Promise<void> => {
    let first: Promise<void> = Promise.resolve();
    let initial = true;
    tracking = true;
    createEffect(() => {
      // A later run is a dependency change: a tracked fetch, which routes its own errors.
      const p = start(initial ? origin : "tracked", true);
      if (initial) {
        initial = false;
        first = p;
      }
    }, keeper, KEEP);
    return first;
  };

  if (owner !== null) {
    owner.onCleanup(() => {
      if (controller !== null) {
        controller.abort();
        controller = null;
      }
    });
    if (!options.lazy) void track("tracked", owner);
  } else if (!options.lazy) {
    void start("tracked", false);
  }

  return {
    data,
    loading,
    error,
    refetch: () => {
      if (owner !== null && owner.disposed) {
        return Promise.reject(
          new UIError("resource: refetch() was called after the component that owns the resource was destroyed", "usage"),
        );
      }
      if (owner !== null && !tracking) return track("refetch", owner);
      return start("refetch", false);
    },
    abort: () => {
      if (controller === null) return;
      controller.abort();
      controller = null;
      untrack(() => loading.set(false));
    },
  };
}
