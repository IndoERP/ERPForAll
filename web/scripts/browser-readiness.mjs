/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
// Read-only readiness polling for the compiled browser fixture. Commands that mutate
// the page remain direct calls; this helper never retries a click or a write.
/**
 * @param {(expression: string) => Promise<unknown>} evaluate Read-only probe.
 * @param {string} expression
 * @param {() => Promise<string>} describeFailure
 * @param {(error: Error) => boolean} [isNavigationError] Caller-specific protocol classifier.
 * @returns {Promise<void>}
 */
export async function waitForReadiness(evaluate, expression, describeFailure = async () => "",
  isNavigationError = error => error.name === "Error" && error.message === "Inspected target navigated or closed") {
  const end = Date.now() + 10000;
  let navigationError;
  while (Date.now() < end) {
    try {
      if (await evaluate(expression)) return;
      // A successful false probe has a live context: use its normal diagnostics if
      // the predicate never becomes true, rather than attributing an earlier reload.
      navigationError = undefined;
    } catch (error) {
      // A readiness read may meet the navigation-context error seen in the CI receipt.
      // Only the observed protocol Error is a retryable readiness observation.
      // Keep it for a terminal refusal; every other error propagates immediately.
      if (!(error instanceof Error) || !isNavigationError(error)) throw error;
      navigationError = error;
    }
    await new Promise(resolve => setTimeout(resolve, 40));
  }
  const detail = navigationError ? `navigation=${navigationError.message}` : await describeFailure();
  throw new Error(`UI readiness failed: ${expression}${detail ? "; " + detail : ""}`,
    navigationError ? { cause: navigationError } : undefined);
}
