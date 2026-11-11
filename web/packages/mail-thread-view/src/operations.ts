/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
import { ThreadViewError } from "./errors";
import type { ThreadViewContext } from "./registries";
import { applyStorePayload, requireOperation, type MessageOperations, type OperationAnswer } from "./transport";

/**
 * Call a transport operation and apply the store payload it answers with. A missing operation is
 * `ThreadViewError("unavailable")`; an answer that cannot be applied is `ThreadViewError("transport")`.
 *
 * @param ctx - the thread's context
 * @param name - the operation
 * @param args - its arguments
 */
export async function performOperation<K extends keyof MessageOperations>(
  ctx: ThreadViewContext,
  name: K,
  ...args: Parameters<NonNullable<MessageOperations[K]>>
): Promise<void> {
  const operation = requireOperation(ctx.transport, name) as (...a: unknown[]) => Promise<OperationAnswer>;
  const answer = await operation(...args);
  if (answer === null || typeof answer !== "object") {
    throw new ThreadViewError("transport", `${String(name)} answered with something that is not an object`);
  }
  if (answer.store === undefined) return;
  if (typeof answer.store !== "string" && !(answer.store instanceof Uint8Array)) {
    throw new ThreadViewError("transport", `${String(name)} answer.store must be the store payload text`);
  }
  const result = applyStorePayload(ctx.store, answer.store);
  if (result.status !== "applied") {
    throw new ThreadViewError(
      "transport",
      result.status === "needs_schema"
        ? `${String(name)} answered with schema ${result.generation}, which this tab does not hold`
        : `${String(name)} answer was refused: ${result.error.message}`,
    );
  }
}
