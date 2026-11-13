/*! SPDX-License-Identifier: MIT OR Apache-2.0 */

/** What a widget's host provides so an asynchronous failure always reaches somebody. */
export interface FailureOwner {
  run?: (job: () => Promise<void>) => void;
  report?: (error: unknown) => void;
}

/** Hand one error to the owner's report; without a report it surfaces as a global error instead of vanishing. */
export function deliverFailure(owner: FailureOwner, error: unknown): void {
  if (owner.report) owner.report(error);
  // The app's error handler reads Error values only, so anything else is wrapped.
  else setTimeout(() => { throw error instanceof Error ? error : new Error(String(error)); });
}

/** Attach the owner's report to a promise nobody awaits. */
export function reportFailure(promise: Promise<unknown>, owner: FailureOwner): void {
  promise.catch(error => deliverFailure(owner, error));
}

/** Start a user-triggered job through the owner's guarded `run`, else report its failure. */
export function launch(owner: FailureOwner, job: () => Promise<unknown>): void {
  if (owner.run) owner.run(async () => { await job(); });
  else reportFailure(job(), owner);
}
