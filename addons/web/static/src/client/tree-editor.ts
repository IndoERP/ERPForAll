/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
import {
  appendConditionNode,
  type ConditionNode,
  nestConditionNode,
  removeConditionNode,
  replaceConditionNode,
  type TreeLimits,
  validateConditionTree,
} from "./condition-tree.js";
import { serializeDomainTree } from "./domain-editor-codec.js";
import { serializeExpressionTree } from "./expression-editor-codec.js";
import {
  type EditorEnvironment,
  editorEnvironment,
  type EditorHandle,
  editorOptions,
  editorText,
} from "./editor-types.js";
import { button, el, errorMessage } from "./dom.js";
import type { Field } from "./metadata.js";
import { createModelFieldSelector } from "./model-field-selector.js";
import {
  defaultTreeEditorProvider,
  type TreeEditorOperator,
  type TreeEditorProvider,
  treeFieldMap,
} from "./tree-editor-fields.js";
import { createTreeValueEditor } from "./tree-editor-values.js";
import { ClientError } from "./transport.js";
import { createRovingTabindex } from "../../../../../web/src/runtime/accessibility.js";
export interface TreeEditorOptions {
  environment: EditorEnvironment;
  label: string;
  model: string;
  value: ConditionNode;
  fields(
    model: string,
    signal: AbortSignal,
  ): ReadonlyMap<string, Field> | Promise<ReadonlyMap<string, Field>>;
  mode?: "domain" | "expression";
  readonly?: boolean;
  touch?: boolean;
  limits?: TreeLimits;
  provider?: TreeEditorProvider;
  change(value: ConditionNode): void | Promise<void>;
}
export interface TreeEditorHandle {
  node: HTMLElement;
  ready: Promise<void>;
  setValue(value: ConditionNode): Promise<void>;
  dispose(): void;
}
let identity = 0;
/** Compose an owner-scoped authoring tree without evaluating or persisting source. */
export function createTreeEditor(options: TreeEditorOptions): TreeEditorHandle {
  editorOptions(options, [
    "environment",
    "label",
    "model",
    "value",
    "fields",
    "mode",
    "readonly",
    "touch",
    "limits",
    "provider",
    "change",
  ], "Tree editor");
  editorEnvironment(options.environment);
  editorText(options.label, "Tree editor label");
  editorText(options.model, "Tree editor model", 256);
  if (!/^[a-z][a-z0-9_]*(?:\.[a-z][a-z0-9_]*)*$/.test(options.model)) {
    throw new ClientError("validation", "Invalid tree editor model.");
  }
  for (const key of ["readonly", "touch"] as const) {
    if (options[key] !== undefined && typeof options[key] !== "boolean") {
      throw new ClientError("validation", "Invalid tree editor policy.");
    }
  }
  if (
    typeof options.fields !== "function" || typeof options.change !== "function"
  ) {
    throw new ClientError(
      "validation",
      "Tree editor requires fields and change callbacks.",
    );
  }
  const mode = options.mode === undefined ? "domain" : options.mode;
  if (mode !== "domain" && mode !== "expression") {
    throw new ClientError("validation", "Invalid tree editor mode.");
  }
  const provider = options.provider === undefined
    ? defaultTreeEditorProvider(mode)
    : options.provider;
  editorOptions(provider, [
    "operators",
    "initialValue",
    "accepts",
    "describe",
    "valueEditor",
  ], "Tree editor provider");
  for (
    const key of ["operators", "initialValue", "accepts", "describe"] as const
  ) {
    if (typeof provider[key] !== "function") {
      throw new ClientError(
        "validation",
        `Tree editor provider requires ${key}.`,
      );
    }
  }
  if (
    provider.valueEditor !== undefined &&
    typeof provider.valueEditor !== "function"
  ) {
    throw new ClientError(
      "validation",
      "Tree editor valueEditor must be a function.",
    );
  }
  const admit = (value: ConditionNode): void => {
    validateConditionTree(value, options.limits);
    if (mode === "domain") serializeDomainTree(value, options.limits);
    else serializeExpressionTree(value, options.limits);
  };
  admit(options.value);
  let tree = structuredClone(options.value),
    dead = false,
    busy = false,
    ownerEpoch = 0,
    renderEpoch = 0,
    sequence = 0;
  let callbackPending = false;
  let activeOperation: symbol | undefined;
  let lifetime = new AbortController(), renderAbort = new AbortController();
  const children: EditorHandle[] = [];
  const id = `editor-tree-${++identity}`,
    node = el("div", undefined, "editor-tree"),
    label = el("span", options.label, "editor-tree-label"),
    body = el("div", undefined, "editor-tree-body"),
    status = el("div", undefined, "editor-tree-status");
  label.id = `${id}-label`;
  node.setAttribute("role", "group");
  node.setAttribute("aria-labelledby", label.id);
  status.setAttribute("role", "status");
  node.append(label, body, status);
  const fail = (error: unknown): void => {
    if (dead) return;
    status.setAttribute("role", "alert");
    status.textContent = errorMessage(error).slice(0, 1024);
    options.environment.report(error);
  };
  const setBusy = (value: boolean): void => {
    busy = value;
    node.setAttribute("aria-busy", String(value));
    body.inert = value;
  };
  const operators = (field: Field): readonly TreeEditorOperator[] => {
    const values = provider.operators(field);
    if (!Array.isArray(values) || values.length > 64) {
      throw new ClientError(
        "validation",
        "Tree operator provider must return 0 to 64 operators.",
      );
    }
    const seen = new Set<string>();
    let bytes = 0;
    for (const item of values) {
      editorOptions(item, ["value", "label"], "Tree operator");
      editorText(item.value, "Tree operator identity", 128);
      editorText(item.label, "Tree operator label");
      if (!item.value || seen.has(item.value)) {
        throw new ClientError(
          "validation",
          "Tree operator identities must be nonempty and unique.",
        );
      }
      seen.add(item.value);
      bytes += new TextEncoder().encode(item.value + item.label).length;
      if (bytes > 262144) {
        throw new ClientError(
          "validation",
          "Tree operator metadata exceeds 262144 bytes.",
        );
      }
    }
    return values.map((item) => ({ ...item }));
  };
  const accepts = (
    field: Field,
    operator: string,
    value: Extract<ConditionNode, { kind: "condition" }>["value"],
  ): boolean => {
    const result = provider.accepts(field, operator, value);
    if (typeof result !== "boolean") {
      throw new ClientError(
        "validation",
        "Tree value policy must return a boolean.",
      );
    }
    return result;
  };
  const load = async (
    model: string,
    signal: AbortSignal,
  ): Promise<ReadonlyMap<string, Field>> =>
    treeFieldMap(await options.fields(model, signal));
  const resolve = async (path: string, signal: AbortSignal): Promise<Field> => {
    editorText(path, "Field path", 1024);
    const segments = path.split(".");
    if (
      segments.length > 32 || mode === "expression" && segments.length !== 1
    ) {
      throw new ClientError(
        "validation",
        "The field path cannot follow this relation.",
      );
    }
    let model = options.model;
    let selected: Field | undefined;
    for (const [index, name] of segments.entries()) {
      const fields = await load(model, signal);
      if (signal.aborted) {
        throw new ClientError(
          "validation",
          "Tree metadata request was cancelled.",
        );
      }
      selected = fields.get(name);
      if (!selected) {
        throw new ClientError("validation", "The field path is invalid.");
      }
      if (index < segments.length - 1) {
        if (!selected.relation) {
          throw new ClientError(
            "validation",
            "The field path cannot follow this relation.",
          );
        }
        model = selected.relation;
      }
    }
    if (!selected) {
      throw new ClientError("validation", "The field path is invalid.");
    }
    return selected;
  };
  const fresh = (): string => {
    const ids = new Set<string>();
    const collect = (value: ConditionNode): void => {
      ids.add(value.id);
      if (value.kind === "and" || value.kind === "or") {
        value.children.forEach(collect);
      } else if (value.kind === "not") collect(value.child);
    };
    collect(tree);
    let next: string;
    do {
      next = `${id}-n${sequence++}`;
    } while (ids.has(next));
    return next;
  };
  const rowFor = (key: string): HTMLElement | undefined =>
    [...body.querySelectorAll<HTMLElement>("[data-tree-node]")].find((row) =>
      row.dataset["treeNode"] === key
    );
  const focusRow = (key?: string): void => {
    const row = key ? rowFor(key) : undefined;
    const target =
      row?.querySelector<HTMLElement>("button,input,select,textarea") ??
        body.querySelector<HTMLElement>("button,input,select,textarea");
    target?.focus();
  };
  let render: () => Promise<void>;
  const commit = async (
    make: (signal: AbortSignal) => ConditionNode | Promise<ConditionNode>,
    focus?: string,
  ): Promise<void> => {
    if (dead || options.readonly || lifetime.signal.aborted) return;
    if (busy) {
      throw new ClientError(
        "validation",
        "A tree change is already pending.",
      );
    }
    const epoch = ownerEpoch;
    const operation = Symbol("tree operation");
    activeOperation = operation;
    const releaseOperation = (): void => {
      if (activeOperation !== operation) return;
      activeOperation = undefined;
      if (!dead) setBusy(false);
    };
    setBusy(true);
    status.textContent = "";
    status.setAttribute("role", "status");
    try {
      const candidate = await make(lifetime.signal);
      if (dead || epoch !== ownerEpoch) return;
      admit(candidate);
      const snapshot = structuredClone(candidate);
      callbackPending = true;
      try {
        await options.change(structuredClone(candidate));
      } finally {
        callbackPending = false;
      }
      if (dead || epoch !== ownerEpoch) return;
      tree = snapshot;
      await render();
      if (!dead && epoch === ownerEpoch) {
        releaseOperation();
        const active = node.ownerDocument.activeElement;
        // Awaiting owners may move focus to another live surface. Only restore
        // focus lost with this editor's replaced controls, never take it back.
        const external = active instanceof HTMLElement && active.isConnected &&
          active !== node.ownerDocument.body &&
          active !== node.ownerDocument.documentElement &&
          !node.contains(active) && !active.matches(":disabled") &&
          !active.closest('[hidden],[inert],[aria-hidden="true"]');
        if (!external) focusRow(focus);
      }
    } catch (error) {
      if (!dead && epoch === ownerEpoch) {
        // Report before asynchronous rollback rendering: a source wrapper may
        // process a newer draft and dispose this tree while rendering awaits.
        fail(error);
        if (dead || epoch !== ownerEpoch) return;
        await render();
        if (!dead && epoch === ownerEpoch) {
          releaseOperation();
          status.setAttribute("role", "alert");
          status.textContent = errorMessage(error).slice(0, 1024);
        }
      }
    } finally {
      // A replacement owner still shares the callback lock. Release this exact
      // operation after settlement, without unlocking a newer operation.
      releaseOperation();
    }
  };
  // Native events cannot await rejection. Provider and picker callbacks do
  // await commit directly so a refused pending edit cannot look accepted.
  const runAction = (operation: Promise<void>): void => {
    void operation.catch(fail);
  };
  const replace = (value: ConditionNode): Promise<void> =>
    commit(
      () => replaceConditionNode(tree, value.id, value, options.limits),
      value.id,
    );
  const condition = async (signal: AbortSignal): Promise<ConditionNode> => {
    const fields = await load(options.model, signal);
    if (signal.aborted) {
      throw new ClientError(
        "validation",
        "Tree metadata request was cancelled.",
      );
    }
    for (const field of fields.values()) {
      const first = operators(field)[0];
      if (!first) continue; // Provider explicitly marks this field unavailable.
      const value = provider.initialValue(field, first.value);
      if (!accepts(field, first.value, value)) {
        throw new ClientError(
          "validation",
          "Tree provider returned an incompatible initial value.",
        );
      }
      return {
        id: fresh(),
        kind: "condition",
        path: field.name,
        operator: first.value,
        value,
      };
    }
    throw new ClientError(
      "validation",
      "No fields are available for a new condition.",
    );
  };
  const actions = (value: ConditionNode, ordinal: number): HTMLElement => {
    const group = el("div", undefined, "editor-tree-actions");
    group.setAttribute("role", "toolbar");
    group.setAttribute("aria-label", `Actions for rule ${ordinal}`);
    const action = (key: string, text: string, run: () => void): void => {
      const control = button(text, run);
      control.dataset["treeAction"] = key;
      control.setAttribute("aria-label", `${text} ${ordinal}`);
      control.tabIndex = group.childElementCount === 0 ? 0 : -1;
      group.append(control);
    };
    action("add", "Add condition", () => {
      runAction(commit(async (signal) => {
        const child = await condition(signal);
        if (value.kind === "and" || value.kind === "or") {
          return appendConditionNode(tree, value.id, child, options.limits);
        }
        const parentGroup = (
          current: ConditionNode,
          parent?: string,
        ): string | undefined => {
          if (current.id === value.id) return parent;
          if (current.kind === "not") return parentGroup(current.child, parent);
          if (current.kind === "and" || current.kind === "or") {
            for (const descendant of current.children) {
              const found = parentGroup(descendant, current.id);
              if (found !== undefined) return found;
            }
          }
          return undefined;
        };
        const parent = parentGroup(tree);
        if (parent !== undefined) {
          return appendConditionNode(tree, parent, child, options.limits);
        }
        return { id: fresh(), kind: "and", children: [tree, child] };
      }, value.id));
    });
    action("nest", "Nest condition", () => {
      runAction(commit(
        () => nestConditionNode(tree, value.id, fresh(), options.limits),
        value.id,
      ));
    });
    action("delete", "Delete condition", () => {
      const rows = [...body.querySelectorAll<HTMLElement>("[data-tree-node]")],
        index = rows.findIndex((row) => row.dataset["treeNode"] === value.id),
        next = rows[index + 1]?.dataset["treeNode"] ??
          rows[index - 1]?.dataset["treeNode"];
      runAction(commit(
        () => removeConditionNode(tree, value.id, options.limits),
        next,
      ));
    });
    let roving: ReturnType<typeof createRovingTabindex> | undefined;
    // Editors may be composed while detached. The controller requires a mounted
    // root, so preserve the native first tab stop until focus enters it.
    const activate = (): void => {
      if (dead || roving || !group.isConnected) return;
      roving = createRovingTabindex(group, {
        items: () => [...group.querySelectorAll<HTMLButtonElement>("button")],
        orientation: "horizontal",
      });
      group.removeEventListener("focusin", activate, true);
      group.removeEventListener("keydown", activate, true);
    };
    group.addEventListener("focusin", activate, true);
    group.addEventListener("keydown", activate, true);
    children.push({
      node: group,
      dispose: () => {
        group.removeEventListener("focusin", activate, true);
        group.removeEventListener("keydown", activate, true);
        roving?.dispose();
      },
    });
    return group;
  };
  render = async (): Promise<void> => {
    if (dead) return;
    const epoch = ++renderEpoch;
    renderAbort.abort();
    renderAbort = new AbortController();
    const signal = renderAbort.signal;
    children.splice(0).forEach((handle) => handle.dispose());
    body.replaceChildren();
    const tasks: Promise<void>[] = [];
    let ordinal = 0;
    const visit = (value: ConditionNode, depth: number): HTMLElement => {
      const position = ++ordinal;
      const group = el(
        "div",
        undefined,
        value.kind === "and" || value.kind === "or" || value.kind === "not"
          ? "editor-tree-group"
          : "editor-tree-row",
      );
      group.dataset["treeNode"] = value.id;
      group.dataset["depth"] = String(depth);
      group.setAttribute("role", "group");
      group.setAttribute(
        "aria-label",
        `${
          value.kind === "condition"
            ? "Condition"
            : value.kind === "field"
            ? "Field"
            : "Group"
        } ${position}`,
      );
      const controls = el("div", undefined, "editor-tree-controls");
      group.append(controls);
      if (value.kind === "and" || value.kind === "or") {
        if (options.readonly) {
          controls.append(
            el(
              "span",
              value.kind === "and" ? "All conditions" : "Any condition",
            ),
          );
        } else {
          const select = el("select");
          select.dataset["treeConnector"] = "";
          select.setAttribute("aria-label", `Connector ${position}`);
          for (
            const [kind, label] of [["and", "All"], ["or", "Any"]] as const
          ) {
            const option = el("option", label);
            option.value = kind;
            select.append(option);
          }
          select.value = value.kind;
          select.addEventListener("change", () => {
            const kind = select.value;
            if (kind !== "and" && kind !== "or") {
              fail(new ClientError("validation", "Invalid tree connector."));
              return;
            }
            runAction(replace({ ...value, kind }));
          });
          controls.append(select);
        }
        if (!options.readonly) group.append(actions(value, position));
        const inner = el("div", undefined, "editor-tree-body");
        for (const child of value.children) {
          inner.append(visit(child, depth + 1));
        }
        if (!value.children.length) {
          inner.append(el("span", "No conditions", "editor-tree-empty"));
        }
        group.append(inner);
      } else if (value.kind === "not") {
        controls.append(el("span", "Not"));
        if (!options.readonly) group.append(actions(value, position));
        group.append(visit(value.child, depth + 1));
      } else if (value.kind === "constant") {
        if (options.readonly) {
          controls.append(
            el(
              "span",
              value.value ? "True" : "False",
              "editor-tree-description",
            ),
          );
        } else {
          const select = el("select");
          select.setAttribute("aria-label", `Constant ${position}`);
          for (const text of ["True", "False"]) {
            const option = el("option", text);
            option.value = text;
            select.append(option);
          }
          select.value = value.value ? "True" : "False";
          select.addEventListener("change", () => {
            if (select.value !== "True" && select.value !== "False") {
              fail(new ClientError("validation", "Invalid tree constant."));
              return;
            }
            runAction(replace({ ...value, value: select.value === "True" }));
          });
          controls.append(select);
          group.append(actions(value, position));
        }
      } else if (value.kind === "field" || value.kind === "condition") {
        const rowStatus = el("span", undefined, "editor-tree-status");
        rowStatus.setAttribute("role", "status");
        rowStatus.textContent = "Loading field metadata…";
        controls.setAttribute("aria-busy", "true");
        controls.append(rowStatus);
        const task = (async (): Promise<void> => {
          try {
            const field = await resolve(value.path, signal);
            if (dead || signal.aborted || epoch !== renderEpoch) return;
            if (options.readonly) {
              const description = value.kind === "field"
                ? field.label
                : provider.describe(field, value.operator, value.value);
              editorText(description, "Tree condition description", 65536);
              controls.prepend(
                el("span", description, "editor-tree-description"),
              );
              rowStatus.textContent = "";
              controls.setAttribute("aria-busy", "false");
              return;
            }
            const choices = value.kind === "condition" ? operators(field) : [];
            if (
              value.kind === "condition" &&
              (!choices.some((choice) => choice.value === value.operator) ||
                !accepts(field, value.operator, value.value))
            ) {
              throw new ClientError(
                "unsupported",
                "Condition value or operator requires a compatible tree editor provider (a later release advanced controls).",
              );
            }
            const picker = createModelFieldSelector({
              environment: { ...options.environment, signal },
              label: "Field",
              model: options.model,
              path: value.path,
              fields: load,
              followRelation: mode === "domain",
              ...(options.touch === undefined ? {} : { touch: options.touch }),
              change: async (path) => {
                await commit(async (abort) => {
                  const selected = await resolve(path, abort);
                  if (value.kind === "field") {
                    return replaceConditionNode(tree, value.id, {
                      ...value,
                      path,
                    }, options.limits);
                  }
                  const first = operators(selected)[0];
                  if (!first) {
                    throw new ClientError(
                      "unsupported",
                      "The selected field has no compatible condition operators (a later release advanced controls).",
                    );
                  }
                  const operator = first.value,
                    literal = provider.initialValue(selected, operator);
                  if (!accepts(selected, operator, literal)) {
                    throw new ClientError(
                      "validation",
                      "Tree provider returned an incompatible initial value.",
                    );
                  }
                  return replaceConditionNode(tree, value.id, {
                    ...value,
                    path,
                    operator,
                    value: literal,
                  }, options.limits);
                }, value.id);
              },
            });
            children.push(picker);
            controls.insertBefore(picker.node, rowStatus);
            if (value.kind === "condition") {
              const select = el("select");
              select.dataset["treeOperator"] = "";
              select.setAttribute("aria-label", `Operator ${position}`);
              for (const choice of choices) {
                const option = el("option", choice.label);
                option.value = choice.value;
                select.append(option);
              }
              select.value = value.operator;
              select.addEventListener("change", () => {
                const operator = select.value;
                if (!choices.some((item) => item.value === operator)) {
                  fail(
                    new ClientError(
                      "validation",
                      "Invalid condition operator.",
                    ),
                  );
                  return;
                }
                try {
                  const literal = accepts(field, operator, value.value)
                    ? value.value
                    : provider.initialValue(field, operator);
                  if (!accepts(field, operator, literal)) {
                    throw new ClientError(
                      "validation",
                      "Tree provider returned an incompatible initial value.",
                    );
                  }
                  runAction(replace({ ...value, operator, value: literal }));
                } catch (error) {
                  fail(error);
                }
              });
              controls.insertBefore(select, rowStatus);
              const valueOptions = {
                environment: { ...options.environment, signal },
                field,
                operator: value.operator,
                value: value.value,
                label: `Value ${position}`,
                ...(options.limits === undefined
                  ? {}
                  : { limits: options.limits }),
                change: async (literal: typeof value.value) => {
                  if (!accepts(field, value.operator, literal)) {
                    throw new ClientError(
                      "validation",
                      "Condition value is incompatible with the selected field and operator.",
                    );
                  }
                  await replace({ ...value, value: literal });
                },
              };
              const editor = provider.valueEditor === undefined
                ? createTreeValueEditor(valueOptions)
                : provider.valueEditor(valueOptions);
              if (
                !editor || !(editor.node instanceof HTMLElement) ||
                typeof editor.dispose !== "function"
              ) {
                throw new ClientError(
                  "validation",
                  "Tree value editor provider must return a disposable element.",
                );
              }
              children.push(editor);
              controls.insertBefore(editor.node, rowStatus);
            }
            await picker.ready;
            if (!dead && !signal.aborted && epoch === renderEpoch) {
              rowStatus.textContent = "";
              controls.setAttribute("aria-busy", "false");
            }
          } catch (error) {
            if (!dead && !signal.aborted && epoch === renderEpoch) {
              controls.setAttribute("aria-busy", "false");
              rowStatus.setAttribute("role", "alert");
              rowStatus.textContent = errorMessage(error).slice(0, 1024);
              options.environment.report(error);
            }
          }
        })();
        tasks.push(task);
        if (!options.readonly) group.append(actions(value, position));
      }
      return group;
    };
    body.append(visit(tree, 0));
    setBusy(busy);
    await Promise.all(tasks);
    if (!dead && epoch === renderEpoch && busy) setBusy(true);
  };
  const setValue = async (value: ConditionNode): Promise<void> => {
    if (dead) return;
    admit(value);
    ownerEpoch++;
    lifetime.abort();
    lifetime = new AbortController();
    tree = structuredClone(value);
    // Obsolete metadata preparation can be cancelled. An invoked owner callback
    // must settle before another user callback is admitted, even on a new tree.
    if (!callbackPending) activeOperation = undefined;
    setBusy(callbackPending);
    status.textContent = "";
    status.setAttribute("role", "status");
    await render();
  };
  const dispose = (): void => {
    if (dead) return;
    dead = true;
    ownerEpoch++;
    renderEpoch++;
    lifetime.abort();
    renderAbort.abort();
    children.splice(0).forEach((handle) => handle.dispose());
    options.environment.signal?.removeEventListener("abort", dispose);
  };
  options.environment.signal?.addEventListener("abort", dispose, {
    once: true,
  });
  if (options.environment.signal?.aborted) dispose();
  const ready = dead ? Promise.resolve() : render();
  return { node, ready, setValue, dispose };
}
