/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
import type { Budget } from "./budget";
import {
  compareDates,
  compareDateTimes,
  compareTimeDeltas,
  dateAddTimeDelta,
  dateSubDate,
  dateTimeAddTimeDelta,
  dateTimeSubDateTime,
  relativeDeltaAddDate,
  relativeDeltaAddDateTime,
  relativeDeltaNeg,
  relativeDeltaPlus,
  timeDeltaAdd,
  timeDeltaDiv,
  timeDeltaMul,
  timeDeltaNeg,
  timeDeltaSub,
  timeDeltaTotalMicros,
} from "./dates";
import { PyDict } from "./dict";
import { printf } from "./format";
import {
  indexError,
  overflowError,
  typeError,
  valueError,
  zeroDivision,
} from "./python-error";
import {
  type ExprValue,
  boolVal,
  bytesVal,
  floatVal,
  intVal,
  isTruthy,
  listVal,
  pythonTypeOf,
  strVal,
  tupleVal,
} from "./values";

export const I64_MIN = -9223372036854775808n;
export const I64_MAX = 9223372036854775807n;

export function checkI64(n: bigint): bigint {
  if (n < I64_MIN || n > I64_MAX) {
    throw overflowError("integer result does not fit in 64 bits");
  }
  return n;
}

export function evalUnary(op: string, operand: ExprValue, budget: Budget): ExprValue {
  budget.tick();
  switch (op) {
    case "not":
      return boolVal(!isTruthy(operand));
    case "u_add":
      if (operand.kind === "bool") return intVal(operand.value ? 1n : 0n);
      if (operand.kind === "int") return operand;
      if (operand.kind === "float") return operand;
      if (operand.kind === "timedelta" || operand.kind === "relativedelta") return operand;
      throw typeError(`bad operand type for unary +: '${pythonTypeOf(operand)}'`);
    case "u_sub":
      if (operand.kind === "bool") return intVal(operand.value ? -1n : 0n);
      if (operand.kind === "int") {
        return intVal(checkI64(-operand.value));
      }
      if (operand.kind === "float") {
        return floatVal(Object.is(operand.value, 0) ? -0.0 : -operand.value);
      }
      if (operand.kind === "timedelta") {
        return timeDeltaNeg(operand);
      }
      if (operand.kind === "relativedelta") {
        return relativeDeltaNeg(operand);
      }
      throw typeError(`bad operand type for unary -: '${pythonTypeOf(operand)}'`);
    case "invert":
      if (operand.kind === "bool") return intVal(operand.value ? -2n : -1n);
      if (operand.kind === "int") {
        return intVal(checkI64(~operand.value));
      }
      throw typeError(`bad operand type for unary ~: '${pythonTypeOf(operand)}'`);
    default:
      throw typeError(`unknown unary operator: ${op}`);
  }
}

function pyFloorDiv(a: bigint, b: bigint): bigint {
  if (b === 0n) throw zeroDivision("integer division or modulo by zero");
  let q = a / b;
  const r = a % b;
  if ((r > 0n && b < 0n) || (r < 0n && b > 0n)) {
    q -= 1n;
  }
  return checkI64(q);
}

function pyMod(a: bigint, b: bigint): bigint {
  if (b === 0n) throw zeroDivision("integer modulo by zero");
  const q = pyFloorDiv(a, b);
  return checkI64(a - q * b);
}

function pyFloatFloorDiv(a: number, b: number): number {
  if (b === 0) throw zeroDivision("float floor division by zero");
  return Math.floor(a / b);
}

function pyFloatMod(a: number, b: number): number {
  if (b === 0) throw zeroDivision("float modulo");
  const mod = a % b;
  if (mod !== 0 && ((mod < 0 && b > 0) || (mod > 0 && b < 0))) {
    return mod + b;
  }
  return Object.is(mod, -0) ? (b < 0 ? -0.0 : 0.0) : mod;
}

export function evalBinary(
  op: string,
  left: ExprValue,
  right: ExprValue,
  budget: Budget,
): ExprValue {
  budget.tick();

  // Sequence concatenation and repetition
  if (op === "add") {
    if (left.kind === "date" && right.kind === "timedelta") return dateAddTimeDelta(left, right);
    if (left.kind === "timedelta" && right.kind === "date") return dateAddTimeDelta(right, left);
    if (left.kind === "datetime" && right.kind === "timedelta") return dateTimeAddTimeDelta(left, right);
    if (left.kind === "timedelta" && right.kind === "datetime") return dateTimeAddTimeDelta(right, left);
    if (left.kind === "timedelta" && right.kind === "timedelta") return timeDeltaAdd(left, right);
    if (left.kind === "date" && right.kind === "relativedelta") return relativeDeltaAddDate(left, right);
    if (left.kind === "relativedelta" && right.kind === "date") return relativeDeltaAddDate(right, left);
    if (left.kind === "datetime" && right.kind === "relativedelta") return relativeDeltaAddDateTime(left, right);
    if (left.kind === "relativedelta" && right.kind === "datetime") return relativeDeltaAddDateTime(right, left);
    if (left.kind === "relativedelta" && right.kind === "relativedelta") return relativeDeltaPlus(left, right);
    if (left.kind === "date" && right.kind === "date") {
      throw typeError("unsupported operand type(s) for +: 'datetime.date' and 'datetime.date'");
    }

    if (left.kind === "str" && right.kind === "str") {
      const res = left.value + right.value;
      budget.scan(res.length);
      budget.alloc(32 + res.length);
      return strVal(res);
    }
    if (left.kind === "bytes" && right.kind === "bytes") {
      const res = new Uint8Array(left.value.length + right.value.length);
      res.set(left.value, 0);
      res.set(right.value, left.value.length);
      budget.alloc(32 + res.length);
      return bytesVal(res);
    }
    if (left.kind === "list" && right.kind === "list") {
      budget.alloc(32 * (left.items.length + right.items.length + 1));
      const items = [...left.items, ...right.items];
      return listVal(items);
    }
    if (left.kind === "tuple" && right.kind === "tuple") {
      budget.alloc(32 * (left.items.length + right.items.length + 1));
      const items = [...left.items, ...right.items];
      return tupleVal(items);
    }
    if (left.kind === "str" || left.kind === "list" || left.kind === "tuple") {
      throw typeError(`can only concatenate ${pythonTypeOf(left)} (not "${pythonTypeOf(right)}") to ${pythonTypeOf(left)}`);
    }
  }

  if (op === "sub") {
    if (left.kind === "date" && right.kind === "timedelta") return dateAddTimeDelta(left, timeDeltaNeg(right));
    if (left.kind === "datetime" && right.kind === "timedelta") return dateTimeAddTimeDelta(left, timeDeltaNeg(right));
    if (left.kind === "date" && right.kind === "date") return dateSubDate(left, right);
    if (left.kind === "datetime" && right.kind === "datetime") return dateTimeSubDateTime(left, right);
    if (left.kind === "timedelta" && right.kind === "timedelta") return timeDeltaSub(left, right);
    if (left.kind === "date" && right.kind === "relativedelta") return relativeDeltaAddDate(left, relativeDeltaNeg(right));
    if (left.kind === "datetime" && right.kind === "relativedelta") return relativeDeltaAddDateTime(left, relativeDeltaNeg(right));
    if (left.kind === "relativedelta" && right.kind === "relativedelta") return relativeDeltaPlus(left, relativeDeltaNeg(right));
    if (
      (left.kind === "datetime" && right.kind === "date") ||
      (left.kind === "date" && right.kind === "datetime")
    ) {
      throw typeError(`unsupported operand type(s) for -: '${pythonTypeOf(left)}' and '${pythonTypeOf(right)}'`);
    }
  }

  // Sequence repetition
  if (op === "mult") {
    if (left.kind === "timedelta" && (right.kind === "int" || right.kind === "bool" || right.kind === "float")) {
      return timeDeltaMul(left, right);
    }
    if (right.kind === "timedelta" && (left.kind === "int" || left.kind === "bool" || left.kind === "float")) {
      return timeDeltaMul(right, left);
    }
    let seq: ExprValue | null = null;
    let countVal: ExprValue | null = null;
    if (left.kind === "str" || left.kind === "bytes" || left.kind === "list" || left.kind === "tuple") {
      seq = left;
      countVal = right;
    } else if (right.kind === "str" || right.kind === "bytes" || right.kind === "list" || right.kind === "tuple") {
      seq = right;
      countVal = left;
    }

    if (seq !== null && countVal !== null) {
      if (countVal.kind !== "int" && countVal.kind !== "bool") {
        throw typeError(`can't multiply sequence by non-int of type '${pythonTypeOf(countVal)}'`);
      }
      const countBig = countVal.kind === "bool" ? (countVal.value ? 1n : 0n) : countVal.value;
      const count = countBig <= 0n ? 0 : countBig > 10000000n ? 10000000 : Number(countBig);

      if (seq.kind === "str") {
        const fullLen = count === 0 ? 0 : seq.value.length * count;
        budget.alloc(32 + fullLen);
        budget.scan(fullLen);
        return strVal(seq.value.repeat(count));
      }
      if (seq.kind === "bytes") {
        const fullLen = count === 0 ? 0 : seq.value.length * count;
        budget.alloc(32 + fullLen);
        const res = new Uint8Array(fullLen);
        for (let i = 0; i < count; i++) {
          res.set(seq.value, i * seq.value.length);
        }
        return bytesVal(res);
      }
      if (seq.kind === "list") {
        const totalItems = count === 0 ? 0 : seq.items.length * count;
        budget.alloc(32 * (totalItems + 1));
        const res: ExprValue[] = [];
        for (let i = 0; i < count; i++) {
          res.push(...seq.items);
        }
        return listVal(res);
      }
      if (seq.kind === "tuple") {
        const totalItems = count === 0 ? 0 : seq.items.length * count;
        budget.alloc(32 * (totalItems + 1));
        const res: ExprValue[] = [];
        for (let i = 0; i < count; i++) {
          res.push(...seq.items);
        }
        return tupleVal(res);
      }
    }
  }

  if (op === "div") {
    if (left.kind === "timedelta" && right.kind === "timedelta") {
      return timeDeltaDiv(left, right);
    }
  }

  // String % formatting
  if (op === "mod" && left.kind === "str") {
    return strVal(printf(left.value, right, budget));
  }

  // Dict merge with |
  if (op === "bit_or" && left.kind === "dict" && right.kind === "dict") {
    const d1 = new PyDict(left.entries);
    const d2 = new PyDict(right.entries);
    const merged = d1.merge(d2);
    budget.alloc(32 + merged.length * 80);
    return merged.toExprValue();
  }

  // Bitwise ops on bools return bool
  if (
    (op === "bit_and" || op === "bit_or" || op === "bit_xor") &&
    left.kind === "bool" &&
    right.kind === "bool"
  ) {
    if (op === "bit_and") return boolVal(left.value && right.value);
    if (op === "bit_or") return boolVal(left.value || right.value);
    if (op === "bit_xor") return boolVal(left.value !== right.value);
  }

  // Numeric operations
  const lIsNum = left.kind === "bool" || left.kind === "int" || left.kind === "float";
  const rIsNum = right.kind === "bool" || right.kind === "int" || right.kind === "float";

  if (!lIsNum || !rIsNum) {
    const OP_SYMBOLS: Record<string, string> = {
      add: "+",
      sub: "-",
      mult: "*",
      div: "/",
      floor_div: "//",
      mod: "%",
      pow: "**",
      bit_and: "&",
      bit_or: "|",
      bit_xor: "^",
      l_shift: "<<",
      r_shift: ">>",
    };
    const sym = OP_SYMBOLS[op] ?? op;
    throw typeError(`unsupported operand type(s) for ${sym}: '${pythonTypeOf(left)}' and '${pythonTypeOf(right)}'`);
  }

  const isFloat = left.kind === "float" || right.kind === "float";

  if (isFloat) {
    const a = left.kind === "float" ? left.value : left.kind === "bool" ? (left.value ? 1 : 0) : Number(left.value);
    const b = right.kind === "float" ? right.value : right.kind === "bool" ? (right.value ? 1 : 0) : Number(right.value);

    switch (op) {
      case "add":
        return floatVal(a + b);
      case "sub":
        return floatVal(a - b);
      case "mult":
        return floatVal(a * b);
      case "div":
        if (b === 0) throw zeroDivision("float division by zero");
        return floatVal(a / b);
      case "floor_div":
        return floatVal(pyFloatFloorDiv(a, b));
      case "mod":
        return floatVal(pyFloatMod(a, b));
      case "pow": {
        if (a === 0 && b < 0) {
          throw zeroDivision("0.0 cannot be raised to a negative power");
        }
        const res = Math.pow(a, b);
        if (!Number.isFinite(res) && Number.isFinite(a) && Number.isFinite(b)) {
          throw overflowError("(34, 'Numerical result out of range')");
        }
        return floatVal(res);
      }
      default:
        throw typeError(`unsupported operand type(s) for ${op}: 'float' and 'float'`);
    }
  }

  // Integer operations
  const a = left.kind === "bool" ? (left.value ? 1n : 0n) : left.value;
  const b = right.kind === "bool" ? (right.value ? 1n : 0n) : right.value;

  switch (op) {
    case "add":
      return intVal(checkI64(a + b));
    case "sub":
      return intVal(checkI64(a - b));
    case "mult":
      return intVal(checkI64(a * b));
    case "div":
      if (b === 0n) {
        throw zeroDivision("division by zero");
      }
      return floatVal(Number(a) / Number(b));
    case "floor_div":
      return intVal(pyFloorDiv(a, b));
    case "mod":
      return intVal(pyMod(a, b));
    case "pow": {
      if (a === 0n && b < 0n) {
        throw zeroDivision("0.0 cannot be raised to a negative power");
      }
      if (b < 0n) {
        // Negative exponent on int produces float
        return floatVal(Math.pow(Number(a), Number(b)));
      }
      if (b > 64n) {
        if (a === 1n) return intVal(1n);
        if (a === -1n) return intVal((b % 2n === 0n) ? 1n : -1n);
        if (a === 0n) return intVal(0n);
        throw overflowError("integer result does not fit in 64 bits");
      }
      return intVal(checkI64(a ** b));
    }
    case "bit_and":
      return intVal(checkI64(a & b));
    case "bit_or":
      return intVal(checkI64(a | b));
    case "bit_xor":
      return intVal(checkI64(a ^ b));
    case "l_shift":
      if (b < 0n) throw valueError("negative shift count");
      if (b >= 64n) throw overflowError("shift count exceeds 63");
      return intVal(checkI64(a << b));
    case "r_shift":
      if (b < 0n) throw valueError("negative shift count");
      if (b >= 64n) return intVal(a < 0n ? -1n : 0n);
      return intVal(checkI64(a >> b));
    default:
      throw typeError(`unknown binary operator: ${op}`);
  }
}

export function evalCompare(
  op: string,
  left: ExprValue,
  right: ExprValue,
  budget: Budget,
): boolean {
  budget.tick();

  if (op === "is" || op === "is_not") {
    // identity only against None, True, False
    const isSentinel = (v: ExprValue) => v.kind === "none" || v.kind === "bool";
    if (!isSentinel(left) && !isSentinel(right)) {
      throw typeError(`'${op}' comparison only supported against None, True and False`);
    }
    const identical = (left.kind === "none" && right.kind === "none") ||
      (left.kind === "bool" && right.kind === "bool" && left.value === right.value);
    return op === "is" ? identical : !identical;
  }

  if (op === "eq" || op === "not_eq") {
    const eq = compareValuesEqual(left, right);
    return op === "eq" ? eq : !eq;
  }

  if (op === "in" || op === "not_in") {
    const inside = evalIn(left, right);
    return op === "in" ? inside : !inside;
  }

  // Ordering comparisons (<, <=, >, >=)
  const order = compareValuesOrder(op, left, right);
  return order;
}

function compareValuesEqual(left: ExprValue, right: ExprValue): boolean {
  if (left.kind === "none" && right.kind === "none") return true;
  if (left.kind === "none" || right.kind === "none") return false;

  const lNum = left.kind === "bool" || left.kind === "int" || left.kind === "float";
  const rNum = right.kind === "bool" || right.kind === "int" || right.kind === "float";

  if (lNum && rNum) {
    if (left.kind === "float" && Number.isNaN(left.value)) return false;
    if (right.kind === "float" && Number.isNaN(right.value)) return false;

    if (left.kind === "float" || right.kind === "float") {
      const a = left.kind === "bool" ? (left.value ? 1n : 0n) : left.value;
      const b = right.kind === "bool" ? (right.value ? 1n : 0n) : right.value;
      // Abstract relational comparison is exact between bigint and number (including fractions).
      return a <= b && a >= b;
    }
    const a = left.kind === "bool" ? (left.value ? 1n : 0n) : left.value;
    const b = right.kind === "bool" ? (right.value ? 1n : 0n) : right.value;
    return a === b;
  }

  if (left.kind !== right.kind) return false;

  if (left.kind === "date" && right.kind === "date") {
    return left.year === right.year && left.month === right.month && left.day === right.day;
  }
  if (left.kind === "datetime" && right.kind === "datetime") {
    return (
      left.year === right.year &&
      left.month === right.month &&
      left.day === right.day &&
      left.hour === right.hour &&
      left.minute === right.minute &&
      left.second === right.second &&
      left.microsecond === right.microsecond
    );
  }
  if (left.kind === "timedelta" && right.kind === "timedelta") {
    return timeDeltaTotalMicros(left) === timeDeltaTotalMicros(right);
  }
  if (left.kind === "relativedelta" && right.kind === "relativedelta") {
    return (
      left.years === right.years &&
      left.months === right.months &&
      left.days === right.days &&
      left.leapdays === right.leapdays &&
      left.hours === right.hours &&
      left.minutes === right.minutes &&
      left.seconds === right.seconds &&
      left.microseconds === right.microseconds &&
      left.year === right.year &&
      left.month === right.month &&
      left.day === right.day &&
      left.weekday?.day === right.weekday?.day &&
      left.weekday?.n === right.weekday?.n &&
      left.hour === right.hour &&
      left.minute === right.minute &&
      left.second === right.second &&
      left.microsecond === right.microsecond
    );
  }

  if (left.kind === "str" && right.kind === "str") {
    return left.value === right.value;
  }
  if (left.kind === "bytes" && right.kind === "bytes") {
    if (left.value.length !== right.value.length) return false;
    for (let i = 0; i < left.value.length; i++) {
      if (left.value[i] !== right.value[i]) return false;
    }
    return true;
  }
  if (left.kind === "list" && right.kind === "list") {
    if (left.items.length !== right.items.length) return false;
    for (let i = 0; i < left.items.length; i++) {
      if (!compareValuesEqual(left.items[i]!, right.items[i]!)) return false;
    }
    return true;
  }
  if (left.kind === "tuple" && right.kind === "tuple") {
    if (left.items.length !== right.items.length) return false;
    for (let i = 0; i < left.items.length; i++) {
      if (!compareValuesEqual(left.items[i]!, right.items[i]!)) return false;
    }
    return true;
  }
  if (left.kind === "dict" && right.kind === "dict") {
    if (left.entries.length !== right.entries.length) return false;
    const d2 = new PyDict(right.entries);
    for (const [k, v] of left.entries) {
      const val2 = d2.get(k);
      if (val2 === undefined || !compareValuesEqual(v, val2)) return false;
    }
    return true;
  }

  return false;
}

function compareValuesOrder(op: string, left: ExprValue, right: ExprValue): boolean {
  const lNum = left.kind === "bool" || left.kind === "int" || left.kind === "float";
  const rNum = right.kind === "bool" || right.kind === "int" || right.kind === "float";

  if (lNum && rNum) {
    const a = left.kind === "bool" ? (left.value ? 1n : 0n) : left.value;
    const b = right.kind === "bool" ? (right.value ? 1n : 0n) : right.value;
    switch (op) {
      case "lt": return a < b;
      case "lt_e": return a <= b;
      case "gt": return a > b;
      case "gt_e": return a >= b;
    }
  }

  if (
    (left.kind === "date" && right.kind === "datetime") ||
    (left.kind === "datetime" && right.kind === "date")
  ) {
    throw typeError(`can't compare ${pythonTypeOf(left)} to ${pythonTypeOf(right)}`);
  }

  if (left.kind !== right.kind) {
    const opSym = op === "lt" ? "<" : op === "lt_e" ? "<=" : op === "gt" ? ">" : ">=";
    throw typeError(`'${opSym}' not supported between instances of '${pythonTypeOf(left)}' and '${pythonTypeOf(right)}'`);
  }

  if (left.kind === "date" && right.kind === "date") {
    const c = compareDates(left, right);
    switch (op) {
      case "lt": return c < 0;
      case "lt_e": return c <= 0;
      case "gt": return c > 0;
      case "gt_e": return c >= 0;
    }
  }

  if (left.kind === "datetime" && right.kind === "datetime") {
    const c = compareDateTimes(left, right);
    switch (op) {
      case "lt": return c < 0;
      case "lt_e": return c <= 0;
      case "gt": return c > 0;
      case "gt_e": return c >= 0;
    }
  }

  if (left.kind === "timedelta" && right.kind === "timedelta") {
    const c = compareTimeDeltas(left, right);
    switch (op) {
      case "lt": return c < 0;
      case "lt_e": return c <= 0;
      case "gt": return c > 0;
      case "gt_e": return c >= 0;
    }
  }

  if (left.kind === "str" && right.kind === "str") {
    switch (op) {
      case "lt": return left.value < right.value;
      case "lt_e": return left.value <= right.value;
      case "gt": return left.value > right.value;
      case "gt_e": return left.value >= right.value;
    }
  }

  if (left.kind === "list" && right.kind === "list") {
    return compareSequencesLexicographically(op, left.items, right.items);
  }
  if (left.kind === "tuple" && right.kind === "tuple") {
    return compareSequencesLexicographically(op, left.items, right.items);
  }

  const opSym = op === "lt" ? "<" : op === "lt_e" ? "<=" : op === "gt" ? ">" : ">=";
  throw typeError(`'${opSym}' not supported between instances of '${pythonTypeOf(left)}' and '${pythonTypeOf(right)}'`);
}

function compareSequencesLexicographically(
  op: string,
  left: readonly ExprValue[],
  right: readonly ExprValue[],
): boolean {
  const minLen = Math.min(left.length, right.length);
  for (let i = 0; i < minLen; i++) {
    if (!compareValuesEqual(left[i]!, right[i]!)) {
      return compareValuesOrder(op, left[i]!, right[i]!);
    }
  }
  switch (op) {
    case "lt": return left.length < right.length;
    case "lt_e": return left.length <= right.length;
    case "gt": return left.length > right.length;
    case "gt_e": return left.length >= right.length;
    default: return false;
  }
}

function evalIn(item: ExprValue, container: ExprValue): boolean {
  if (container.kind === "str") {
    if (item.kind !== "str") {
      throw typeError(`'in <string>' requires string as left operand, not ${pythonTypeOf(item)}`);
    }
    return container.value.includes(item.value);
  }
  if (container.kind === "bytes") {
    if (item.kind !== "int") {
      throw typeError(`'in <bytes>' requires integer as left operand, not ${pythonTypeOf(item)}`);
    }
    const byte = Number(item.value);
    return container.value.includes(byte);
  }
  if (container.kind === "list" || container.kind === "tuple") {
    for (const elem of container.items) {
      if (compareValuesEqual(item, elem)) return true;
    }
    return false;
  }
  if (container.kind === "dict") {
    const d = new PyDict(container.entries);
    return d.has(item);
  }
  throw typeError(`argument of type '${pythonTypeOf(container)}' is not iterable`);
}

/** Python Unicode code points slicing */
export function getUnicodeCodePoints(s: string): string[] {
  return Array.from(s);
}

export function evalSubscript(
  target: ExprValue,
  sliceNode: { kind: "index"; value: ExprValue } | { kind: "slice"; lower: ExprValue | null; upper: ExprValue | null; step: ExprValue | null },
  budget: Budget,
): ExprValue {
  budget.tick();

  if (sliceNode.kind === "index") {
    const idxVal = sliceNode.value;
    if (target.kind === "dict") {
      const d = new PyDict(target.entries);
      return d.getItem(idxVal);
    }

    if (idxVal.kind !== "int" && idxVal.kind !== "bool") {
      if (target.kind === "list") throw typeError(`list indices must be integers or slices, not ${pythonTypeOf(idxVal)}`);
      if (target.kind === "tuple") throw typeError(`tuple indices must be integers or slices, not ${pythonTypeOf(idxVal)}`);
      if (target.kind === "str") throw typeError(`string indices must be integers, not '${pythonTypeOf(idxVal)}'`);
      throw typeError(`'${pythonTypeOf(target)}' object is not subscriptable`);
    }

    const idxNum = idxVal.kind === "bool" ? (idxVal.value ? 1 : 0) : Number(idxVal.value);

    if (target.kind === "list" || target.kind === "tuple") {
      const len = target.items.length;
      const actualIdx = idxNum < 0 ? len + idxNum : idxNum;
      if (actualIdx < 0 || actualIdx >= len) {
        throw indexError(`${target.kind} index out of range`);
      }
      return target.items[actualIdx]!;
    }
    if (target.kind === "str") {
      const chars = getUnicodeCodePoints(target.value);
      const len = chars.length;
      const actualIdx = idxNum < 0 ? len + idxNum : idxNum;
      if (actualIdx < 0 || actualIdx >= len) {
        throw indexError("string index out of range");
      }
      return strVal(chars[actualIdx]!);
    }
    if (target.kind === "bytes") {
      const len = target.value.length;
      const actualIdx = idxNum < 0 ? len + idxNum : idxNum;
      if (actualIdx < 0 || actualIdx >= len) {
        throw indexError("index out of range");
      }
      return intVal(BigInt(target.value[actualIdx]!));
    }
    throw typeError(`'${pythonTypeOf(target)}' object is not subscriptable`);
  }

  // Slice
  const step = sliceNode.step !== null && sliceNode.step.kind !== "none" ? Number((sliceNode.step as { value: bigint }).value) : 1;
  if (step === 0) {
    throw valueError("slice step cannot be zero");
  }

  const toIntOrNull = (v: ExprValue | null): number | null => {
    if (v === null || v.kind === "none") return null;
    if (v.kind === "bool") return v.value ? 1 : 0;
    if (v.kind === "int") {
      // Bound to safe integer for slice indices
      if (v.value > BigInt(Number.MAX_SAFE_INTEGER)) return Number.MAX_SAFE_INTEGER;
      if (v.value < BigInt(Number.MIN_SAFE_INTEGER)) return Number.MIN_SAFE_INTEGER;
      return Number(v.value);
    }
    throw typeError("slice indices must be integers or None");
  };

  const lower = toIntOrNull(sliceNode.lower);
  const upper = toIntOrNull(sliceNode.upper);

  if (target.kind === "list" || target.kind === "tuple") {
    const items = sliceArray(target.items, lower, upper, step);
    budget.alloc(32 * (items.length + 1));
    return target.kind === "list" ? listVal(items) : tupleVal(items);
  }
  if (target.kind === "str") {
    const chars = getUnicodeCodePoints(target.value);
    const sliced = sliceArray(chars, lower, upper, step).join("");
    budget.alloc(32 + sliced.length);
    budget.scan(sliced.length);
    return strVal(sliced);
  }
  if (target.kind === "bytes") {
    const bytes = sliceArray(Array.from(target.value), lower, upper, step);
    budget.alloc(32 + bytes.length);
    return bytesVal(new Uint8Array(bytes));
  }
  throw typeError(`'${pythonTypeOf(target)}' object is not subscriptable`);
}

function sliceArray<T>(items: readonly T[], lower: number | null, upper: number | null, step: number): T[] {
  const len = items.length;
  let start: number;
  let stop: number;

  if (step > 0) {
    start = lower === null ? 0 : lower < 0 ? Math.max(0, len + lower) : Math.min(len, lower);
    stop = upper === null ? len : upper < 0 ? Math.max(0, len + upper) : Math.min(len, upper);
  } else {
    start = lower === null ? len - 1 : lower < 0 ? Math.max(-1, len + lower) : Math.min(len - 1, lower);
    stop = upper === null ? -1 : upper < 0 ? Math.max(-1, len + upper) : Math.min(len - 1, upper);
  }

  const res: T[] = [];
  if (step > 0) {
    for (let i = start; i < stop; i += step) {
      res.push(items[i]!);
    }
  } else {
    for (let i = start; i > stop; i += step) {
      res.push(items[i]!);
    }
  }
  return res;
}
