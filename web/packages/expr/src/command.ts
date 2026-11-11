/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
import type { Budget } from "./budget";
import { SLOT } from "./budget";
import { type Sig, bindSig, intArg, requiredSlot } from "./args";
import { typeAttributeError, typeError } from "./python-error";
import {
  type ExprObject,
  type ExprValue,
  callableVal,
  intVal,
  pythonTypeOf,
  tupleVal,
} from "./values";

/**
 * Command builders take named parameters, so CPython's function messages apply
 * (`Command.link() missing 1 required positional argument: 'id'`); Rust `builtins/command.rs` `sig`.
 */
function commandSig(name: string, params: readonly string[]): Sig {
  return { name, params, required: params.length, posOnly: 0, maxPos: params.length };
}

/** Binds a command call; slot `i` is the value of `params[i]` (all required). */
function bind(name: string, params: readonly string[], args: readonly ExprValue[], kwargs: Record<string, ExprValue>): ExprValue[] {
  const sig = commandSig(name, params);
  const slots = bindSig(sig, args, kwargs);
  return params.map((_, i) => requiredSlot(sig, slots, i));
}

/** `(code, id, value)`: the value was charged when admitted; the triple charges its slots and is checked. */
function triple(code: bigint, id: bigint, value: ExprValue, budget: Budget): ExprValue {
  budget.alloc(SLOT * 4);
  const t = tupleVal([intVal(code), intVal(id), value]);
  budget.checkValue(t);
  return t;
}

/** The `values` dict of create/update, admitted to the budget (Rust `values_arg`). */
function valuesArg(fname: string, v: ExprValue, budget: Budget): ExprValue {
  if (v.kind !== "dict") {
    throw typeError(`${fname}() argument 'values' must be dict, not ${pythonTypeOf(v)}`);
  }
  return budget.admit(v);
}

export class CommandNamespace implements ExprObject {
  typeName(): string {
    return "Command";
  }

  repr(): string {
    return "<enum 'Command'>";
  }

  getAttr(name: string, _budget: Budget): ExprValue {
    switch (name) {
      case "CREATE":
        return intVal(0n);
      case "UPDATE":
        return intVal(1n);
      case "DELETE":
        return intVal(2n);
      case "UNLINK":
        return intVal(3n);
      case "LINK":
        return intVal(4n);
      case "CLEAR":
        return intVal(5n);
      case "SET":
        return intVal(6n);

      case "create":
        return callableVal("create", (args, kwargs, budget) => {
          const [valuesSlot] = bind("Command.create", ["values"], args, kwargs);
          const values = valuesArg("Command.create", valuesSlot!, budget);
          return triple(0n, 0n, values, budget);
        });

      case "update":
        return callableVal("update", (args, kwargs, budget) => {
          const [idSlot, valuesSlot] = bind("Command.update", ["id", "values"], args, kwargs);
          const id = intArg("Command.update", "id", idSlot!);
          const values = valuesArg("Command.update", valuesSlot!, budget);
          return triple(1n, id, values, budget);
        });

      case "delete":
        return callableVal("delete", (args, kwargs, budget) => {
          const [idSlot] = bind("Command.delete", ["id"], args, kwargs);
          const id = intArg("Command.delete", "id", idSlot!);
          return triple(2n, id, intVal(0n), budget);
        });

      case "unlink":
        return callableVal("unlink", (args, kwargs, budget) => {
          const [idSlot] = bind("Command.unlink", ["id"], args, kwargs);
          const id = intArg("Command.unlink", "id", idSlot!);
          return triple(3n, id, intVal(0n), budget);
        });

      case "link":
        return callableVal("link", (args, kwargs, budget) => {
          const [idSlot] = bind("Command.link", ["id"], args, kwargs);
          const id = intArg("Command.link", "id", idSlot!);
          return triple(4n, id, intVal(0n), budget);
        });

      case "clear":
        return callableVal("clear", (args, kwargs, budget) => {
          bind("Command.clear", [], args, kwargs);
          return triple(5n, 0n, intVal(0n), budget);
        });

      case "set":
        return callableVal("set", (args, kwargs, budget) => {
          const idsVal = bind("Command.set", ["ids"], args, kwargs)[0]!;
          if (idsVal.kind !== "list" && idsVal.kind !== "tuple") {
            throw typeError(
              `Command.set() argument 'ids' must be a list or tuple of ids, not ${pythonTypeOf(idsVal)}`,
            );
          }
          budget.checkValue(idsVal);
          for (let i = 0; i < idsVal.items.length; i++) {
            const item = idsVal.items[i]!;
            if (item.kind !== "int" && item.kind !== "bool") {
              throw typeError(`Command.set() id #${i} must be int, not ${pythonTypeOf(item)}`);
            }
          }
          return triple(6n, 0n, budget.admit(idsVal), budget);
        });

      default:
        throw typeAttributeError("Command", name);
    }
  }
}
