/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
import { ExtensionError } from "./extension-error.js";
import { ServiceContainer } from "./services.js";
/** Shallow-frozen application environment; service state and the bus remain mutable. */
export interface Env {
  readonly services: ServiceContainer;
  readonly bus: EventTarget;
  readonly debug: boolean;
  readonly isSmall: boolean;
  readonly context: Readonly<Record<string, unknown>>;
}
/** @internal Validate standard fields without dropping addon-defined environment properties. */
export function validateEnvOptions(options: Partial<Env>): void {
  if (options === null || typeof options !== "object" || Array.isArray(options) || Reflect.ownKeys(options).length > 256 ||
    Reflect.ownKeys(options).some(k => k === "__proto__" || k === "constructor" || k === "prototype") ||
    (options.services !== undefined && !(options.services instanceof ServiceContainer)) ||
    (options.bus !== undefined && !(options.bus instanceof EventTarget)) ||
    (options.debug !== undefined && typeof options.debug !== "boolean") ||
    (options.isSmall !== undefined && typeof options.isSmall !== "boolean") ||
    (options.context !== undefined && (options.context === null || typeof options.context !== "object" || Array.isArray(options.context)))) {
    throw new ExtensionError("env-options", "Invalid environment options");
  }
}
/** @internal A supplied complete environment must not contain missing standard fields. */
export function validateEnv(env: Env): void {
  validateEnvOptions(env);
  if (env.services === undefined || env.bus === undefined || env.debug === undefined || env.isSmall === undefined || env.context === undefined) {
    throw new ExtensionError("env-options", "Invalid environment options");
  }
}
/** Create isolated defaults for each mounted application; explicit wrong types fail. */
export function createEnv(options: Partial<Env> = {}): Env {
  validateEnvOptions(options);
  return Object.freeze({
    ...options,
    services: options.services === undefined ? new ServiceContainer() : options.services,
    bus: options.bus === undefined ? new EventTarget() : options.bus,
    debug: options.debug === undefined ? false : options.debug,
    isSmall: options.isSmall === undefined ? false : options.isSmall,
    context: options.context === undefined ? Object.freeze({}) : options.context,
  });
}
