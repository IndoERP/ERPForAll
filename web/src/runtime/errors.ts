/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
// Errors the runtime throws (§7.2, §8). Every message names what was
// wrong and, where it applies, what to write instead. Every error is a `UIError`: `code` says which
// kind it is (a closed list), `name` which class threw it, so a caller can switch on either.

/** Failures from extension registration, patching, service startup and setup hooks. */
export type ExtensionErrorCode =
  | "registry-missing" | "registry-duplicate" | "registry-schema" | "registry-options" | "registry-limit"
  | "env-options" | "patch-target" | "addon-registration" | "hook-usage"
  | "service-definition" | "service-state" | "service-duplicate" | "service-unavailable"
  | "service-missing-dependency" | "service-cycle" | "service-start" | "service-destroyed";

/**
 * What kind of problem a `UIError` reports:
 * - `usage`: an API called in the wrong place or state (a hook outside `setup()`, a helper with no
 *   current owner, a second `mount` on one element, `new Markup`);
 * - `template`: a template, component class or slot that is missing or malformed;
 * - `value`: a value of the wrong type or range for its position (`t-att-class` true, a non-object
 *   `t-props`, a `cond` index out of range, an invalid event name);
 * - `loop`: a `t-foreach` collection that is not iterable or has a duplicate key;
 * - `handler`: a `t-on` value or handler that is not a function;
 * - `detached`: a dynamic position whose anchor was removed from the DOM by code outside the runtime;
 * - `unsafe-attribute`: `UnsafeAttributeError`;
 * - `reactive-loop`: `ReactiveLoopError`;
 * - `disposed`: `DisposedExpression` (a read of a disposed expression handle, `runtime/expr.ts`).
 */
export type UIErrorCode =
  | ExtensionErrorCode
  | "usage"
  | "template"
  | "value"
  | "loop"
  | "handler"
  | "detached"
  | "unsafe-attribute"
  | "reactive-loop"
  | "disposed";

/** The codes a plain `UIError` carries; the others belong to their subclasses only (type review L1). */
export type UIErrorBaseCode = Exclude<UIErrorCode, SubclassCode>;

/** The codes reserved for the subclasses. */
type SubclassCode = "unsafe-attribute" | "reactive-loop" | "disposed" | ExtensionErrorCode;

/** The class names of the runtime's errors. */
export type UIErrorName = "UIError" | "UnsafeAttributeError" | "ReactiveLoopError" | "DisposedExpression" | "ExtensionError";

/** The path the subclasses pass their own code through (module-private: no other caller has it). */
const SUBCLASS: unique symbol = Symbol("erp.uierror");

/**
 * Base class of every error the Elang runtime throws itself. `new UIError(message, code)` takes one of
 * the base codes; `unsafe-attribute`, `reactive-loop` and `disposed` are reserved for
 * `UnsafeAttributeError`, `ReactiveLoopError` and `DisposedExpression`, so a `code` check narrows to the
 * subclass (see `ElangError`).
 */
export class UIError extends Error {
  override readonly name: UIErrorName = "UIError";
  readonly code: UIErrorCode;

  constructor(message: string, code: UIErrorBaseCode, options?: ErrorOptions);
  /** @internal The subclasses' path. */
  constructor(
    message: string,
    code: SubclassCode,
    options: ErrorOptions | undefined,
    token: typeof SUBCLASS,
  );
  constructor(message: string, code: UIErrorCode, options?: ErrorOptions, token?: typeof SUBCLASS) {
    super(message, options);
    if (!["usage", "template", "value", "loop", "handler", "detached"].includes(code) && token !== SUBCLASS) {
      throw new UIError(`UIError: the code "${code}" belongs to its own error class; use one of the base codes`, "usage");
    }
    this.code = code;
  }
}

/**
 * A value or name the runtime refuses to write to the DOM: an event-handler attribute (`on…`), a
 * script-bearing URL, an invalid attribute name or a property outside `isProp`'s list (§7.2, §8).
 */
export class UnsafeAttributeError extends UIError {
  override readonly name = "UnsafeAttributeError" as const;
  override readonly code = "unsafe-attribute" as const;

  constructor(message: string) {
    super(message, "unsafe-attribute", undefined, SUBCLASS);
  }
}

/**
 * An effect re-ran more often in one flush than the loop limit allows: it keeps writing a signal it
 * reads (§4).
 */
export class ReactiveLoopError extends UIError {
  override readonly name = "ReactiveLoopError" as const;
  override readonly code = "reactive-loop" as const;

  constructor(message: string) {
    super(message, "reactive-loop", undefined, SUBCLASS);
  }
}

/**
 * A read of an expression handle (`createRecordExpression`, `runtime/expr.ts`) after it was disposed,
 * explicitly or by its owner's teardown, including a disposal during the read's own evaluation. The
 * error that evaluation was raising, if any, is the `cause`.
 */
export class DisposedExpression extends UIError {
  override readonly name = "DisposedExpression" as const;
  override readonly code = "disposed" as const;

  constructor(message: string, options?: ErrorOptions) {
    super(message, "disposed", options, SUBCLASS);
  }
}

/**
 * Every error the runtime throws, as a union a `switch (e.code)` narrows: a plain `UIError` with a base
 * code (and the name `"UIError"`), or one of the subclasses (type review L1).
 */
export type ElangError =
  | (UIError & { readonly code: UIErrorBaseCode; readonly name: "UIError" })
  | UnsafeAttributeError
  | ReactiveLoopError
  | DisposedExpression
  | ExtensionError;


/** A named extension failure; schema/startup errors retain their original cause. */
export class ExtensionError extends UIError {
  override readonly name = "ExtensionError" as const;
  override readonly code: ExtensionErrorCode;
  constructor(code: ExtensionErrorCode, message: string, options?: ErrorOptions) {
    super(message, code, options, SUBCLASS);
    this.code = code;
  }
}
