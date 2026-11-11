/*! SPDX-License-Identifier: MIT OR Apache-2.0 */

/**
 * Python-compatible expression exception retaining error class and exact message.
 */
export class PythonError extends Error {
  readonly className: string;
  readonly messageDetail: string;

  constructor(className: string, messageDetail: string) {
    super(`${className}: ${messageDetail}`);
    this.name = "PythonError";
    this.className = className;
    this.messageDetail = messageDetail;
    Object.setPrototypeOf(this, PythonError.prototype);
  }

  override toString(): string {
    return `${this.className}: ${this.messageDetail}`;
  }
}

export function zeroDivision(detail: string): PythonError {
  return new PythonError("ZeroDivisionError", detail);
}

export function typeError(detail: string): PythonError {
  return new PythonError("TypeError", detail);
}

export function valueError(detail: string): PythonError {
  return new PythonError("ValueError", detail);
}

export function overflowError(detail: string): PythonError {
  return new PythonError("OverflowError", detail);
}

export function nameError(name: string): PythonError {
  return new PythonError("NameError", `name '${name}' is not defined`);
}

export function attributeError(typeName: string, attrName: string): PythonError {
  return new PythonError("AttributeError", `'${typeName}' object has no attribute '${attrName}'`);
}

/** `type object '<type>' has no attribute '<attr>'` (attributes of classes and namespaces). */
export function typeAttributeError(typeName: string, attrName: string): PythonError {
  return new PythonError("AttributeError", `type object '${typeName}' has no attribute '${attrName}'`);
}

/** `module '<module>' has no attribute '<attr>'`. */
export function moduleAttributeError(moduleName: string, attrName: string): PythonError {
  return new PythonError("AttributeError", `module '${moduleName}' has no attribute '${attrName}'`);
}

export function indexError(detail: string): PythonError {
  return new PythonError("IndexError", detail);
}

export function keyError(keyRepr: string): PythonError {
  return new PythonError("KeyError", keyRepr);
}

export function limitExceeded(detail: string): PythonError {
  return new PythonError("LimitExceeded", detail);
}

export function unsupportedFormat(feature: string): PythonError {
  return new PythonError(
    "NotImplementedError",
    `a later release: ${feature} is implemented in child task a later release`,
  );
}

export function unsupportedDates(feature: string): PythonError {
  return new PythonError(
    "NotImplementedError",
    `a later release: ${feature} is implemented in child task a later release`,
  );
}
