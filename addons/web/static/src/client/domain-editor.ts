/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
import {
  checkAuthoredOutput,
  checkAuthoredSource,
  type CodecResult,
  type ConditionNode,
  resolveTreeLimits,
  type TreeLimits,
} from "./condition-tree.js";
import {
  parseDomainSource,
  serializeDomainTree,
} from "./domain-editor-codec.js";
import {
  parseExpressionSource,
  serializeExpressionTree,
} from "./expression-editor-codec.js";
import { createTreeEditor, type TreeEditorHandle } from "./tree-editor.js";
import { button, el, errorMessage } from "./dom.js";
import {
  EDITOR_OPTIONS_BYTES,
  EDITOR_OPTIONS_LIMIT,
  editorEnvironment,
  editorOptions,
  editorText,
} from "./editor-types.js";
import { ClientError } from "./transport.js";
import type { EditorEnvironment } from "./editor-types.js";
import type { Field } from "./metadata.js";
import { type TreeEditorProvider, treeFieldMap } from "./tree-editor-fields.js";
/** Caller-owned domain authoring; changes confer no execution or persistence authority. */
export interface DomainEditorOptions {
  environment: EditorEnvironment;
  label: string;
  model: string;
  value: string;
  fields(
    model: string,
    signal: AbortSignal,
  ): ReadonlyMap<string, Field> | Promise<ReadonlyMap<string, Field>>;
  readonly?: boolean;
  debug?: boolean;
  touch?: boolean;
  limits?: TreeLimits;
  provider?: TreeEditorProvider;
  change(result: CodecResult): void | Promise<void>;
}
/** Owner value replacement never emits a user change; disposal suppresses late results. */
export interface DomainEditorHandle {
  node: HTMLElement;
  ready: Promise<void>;
  setValue(value: string): Promise<void>;
  dispose(): void;
}
/** Build an owner-scoped domain editor. Failed conversions retain the represented source. */
export function createDomainEditor(
  options: DomainEditorOptions,
): DomainEditorHandle {
  return createStructuredSourceEditor(options, "domain");
}
/** Local properties descriptor, excluded before adapting to generated field metadata. */
export type SourceEditorField = Field | {
  readonly name: string;
  readonly label: string;
  readonly kind: "properties";
};
/** Internal wrapper composition contract; trusted callbacks author source only. */
export interface StructuredSourceEditorOptions
  extends Omit<DomainEditorOptions, "fields"> {
  fields(
    model: string,
    signal: AbortSignal,
  ):
    | ReadonlyMap<string, SourceEditorField>
    | Promise<ReadonlyMap<string, SourceEditorField>>;
}
let identity = 0;
const modelPattern = /^[a-z][a-z0-9_]*(?:\.[a-z][a-z0-9_]*)*$/;
const fieldPattern = /^[a-zA-Z_][a-zA-Z0-9_]*$/;
function isArchive(node: ConditionNode): boolean {
  return node.kind === "condition" && node.path === "active" &&
    node.operator === "in" && node.value.kind === "list" &&
    node.value.items.length === 2 && node.value.items[0]?.kind === "boolean" &&
    node.value.items[0].value === true &&
    node.value.items[1]?.kind === "boolean" &&
    node.value.items[1].value === false;
}
function extractArchive(
  tree: ConditionNode,
): { tree: ConditionNode; archived: boolean } {
  if (isArchive(tree)) {
    return { tree: { id: tree.id, kind: "and", children: [] }, archived: true };
  }
  if (tree.kind !== "and") return { tree, archived: false };
  const children = tree.children.filter((child) => !isArchive(child));
  return {
    tree: { ...tree, children },
    archived: children.length !== tree.children.length,
  };
}
function includeArchive(tree: ConditionNode, archived: boolean): ConditionNode {
  if (!archived) return tree;
  const ids = new Set<string>(),
    visit = (node: ConditionNode): void => {
      ids.add(node.id);
      if (node.kind === "and" || node.kind === "or") {
        node.children.forEach(visit);
      } else if (node.kind === "not") visit(node.child);
    };
  visit(tree);
  let index = 0;
  const nextId = (): string => {
    while (ids.has(`archive-${index}`)) index++;
    const id = `archive-${index++}`;
    ids.add(id);
    return id;
  };
  const marker: ConditionNode = {
    id: nextId(),
    kind: "condition",
    path: "active",
    operator: "in",
    value: {
      kind: "list",
      items: [{ kind: "boolean", value: true }, {
        kind: "boolean",
        value: false,
      }],
    },
  };
  return tree.kind === "and"
    ? { ...tree, children: [...tree.children, marker] }
    : { id: nextId(), kind: "and", children: [tree, marker] };
}
/** Shared lifecycle implementation for the two public source wrappers. */
export function createStructuredSourceEditor(
  options: StructuredSourceEditorOptions,
  mode: "domain" | "expression",
): DomainEditorHandle {
  const title = mode === "domain" ? "Domain editor" : "Expression editor";
  editorOptions(options, [
    "environment",
    "label",
    "model",
    "value",
    "fields",
    "readonly",
    "debug",
    "touch",
    "limits",
    "provider",
    "change",
  ], title);
  editorEnvironment(options.environment);
  editorText(options.label, `${title} label`);
  editorText(options.model, `${title} model`, 256);
  if (!modelPattern.test(options.model)) {
    throw new ClientError("validation", `Invalid ${mode} editor model.`);
  }
  for (const policy of ["readonly", "debug", "touch"] as const) {
    if (options[policy] !== undefined && typeof options[policy] !== "boolean") {
      throw new ClientError("validation", `Invalid ${mode} editor policy.`);
    }
  }
  if (
    typeof options.fields !== "function" || typeof options.change !== "function"
  ) throw new ClientError("validation", `Invalid ${mode} editor callback.`);
  if (options.provider !== undefined) {
    editorOptions(options.provider, [
      "operators",
      "initialValue",
      "accepts",
      "describe",
      "valueEditor",
    ], "Tree editor provider");
    for (
      const callback of [
        "operators",
        "initialValue",
        "accepts",
        "describe",
      ] as const
    ) {
      if (typeof options.provider[callback] !== "function") {
        throw new ClientError(
          "validation",
          `Tree editor provider requires ${callback}.`,
        );
      }
    }
    if (
      options.provider.valueEditor !== undefined &&
      typeof options.provider.valueEditor !== "function"
    ) {
      throw new ClientError(
        "validation",
        "Tree editor valueEditor must be a function.",
      );
    }
  }
  resolveTreeLimits(options.limits);
  checkAuthoredSource(options.value, options.limits);
  const node = el("div", undefined, `editor-${mode}`),
    label = el("span", options.label, "editor-label"),
    body = el("div", undefined, "editor-source-body"),
    status = el("div", undefined, "editor-source-status");
  // Native source history belongs to this continuously mounted host. Tree and action
  // updates must never detach or reinsert its textarea during a user edit.
  const structuredHost = el("div", undefined, "editor-source-structured");
  const rawHost = el("div", undefined, "editor-source-raw");
  const resetHost = el("div", undefined, "editor-source-actions");
  body.append(structuredHost, rawHost, resetHost);
  const id = `source-editor-${++identity}`;
  label.id = `${id}-label`;
  node.setAttribute("role", "group");
  node.setAttribute("aria-labelledby", label.id);
  status.setAttribute("role", "status");
  node.append(label, body, status);
  let dead = false,
    generation = 0,
    busy = false,
    value = options.value,
    draftSource = options.value,
    result: CodecResult | undefined,
    tree: TreeEditorHandle | undefined,
    rootFields = new Map<string, Field>(),
    archive = false,
    archiveAllowed = false;
  let controller = new AbortController();
  let pendingRaw: { generation: number; source: string } | undefined;
  let drainingRaw = false;
  let rawWait: MutationObserver | undefined;
  let activeOperation: symbol | undefined;
  let drainRaw: () => Promise<void>;
  const fail = (error: unknown): void => {
    if (dead) return;
    status.textContent = errorMessage(error).slice(0, 1024);
    status.setAttribute("role", "alert");
    node.dataset["state"] = "error";
    options.environment.report(error);
  };
  const setBusy = (): void => {
    node.setAttribute("aria-busy", String(busy));
    if (tree) tree.node.inert = busy;
    for (
      const control of body.querySelectorAll<
        HTMLInputElement | HTMLButtonElement
      >("[data-domain-archived],[data-editor-reset]")
    ) control.disabled = busy || Boolean(options.readonly);
    const raw = body.querySelector<HTMLTextAreaElement>("[data-editor-source]");
    if (raw) {
      raw.disabled = false;
      raw.readOnly = Boolean(options.readonly);
    }
  };
  const load = async (
    model: string,
    signal: AbortSignal,
  ): Promise<ReadonlyMap<string, Field>> => {
    const metadata = await options.fields(model, signal);
    if (!(metadata instanceof Map) || metadata.size > EDITOR_OPTIONS_LIMIT) {
      throw new ClientError(
        "validation",
        "Field metadata must be a map of at most 1024 fields.",
      );
    }
    const admitted = new Map<string, Field>();
    let bytes = 0;
    for (const [name, field] of metadata) {
      editorText(name, "Field identity", 256);
      if (
        !field || typeof field !== "object" || !fieldPattern.test(name) ||
        field.name !== name
      ) throw new ClientError("validation", "Invalid field metadata identity.");
      editorText(field.label, "Field label");
      bytes += new TextEncoder().encode(name + field.label).length;
      if (bytes > EDITOR_OPTIONS_BYTES) {
        throw new ClientError(
          "validation",
          "Field metadata text exceeds 262144 bytes.",
        );
      }
      if (field.kind === "properties" && mode === "expression") continue;
      if (field.kind === "properties") {
        throw new ClientError(
          "unsupported",
          "Properties fields cannot be edited here; use the code editor.",
        );
      }
      admitted.set(name, field);
    }
    return treeFieldMap(admitted);
  };
  const parse = (source: string): CodecResult =>
    mode === "domain"
      ? parseDomainSource(source, options.limits)
      : parseExpressionSource(source, [...rootFields.keys()], options.limits);
  const serialize = (candidate: ConditionNode): string =>
    mode === "domain"
      ? serializeDomainTree(candidate, options.limits)
      : serializeExpressionTree(candidate, options.limits);
  const sourceControl = (): HTMLTextAreaElement | null =>
    body.querySelector<HTMLTextAreaElement>("[data-editor-source]");
  const restoreSource = (): void => {
    const raw = sourceControl();
    if (raw && raw.value !== draftSource) raw.value = draftSource;
    const checkbox = body.querySelector<HTMLInputElement>("[data-domain-archived]");
    if (checkbox) checkbox.checked = archive;
  };
  const publish = async (
    candidate: CodecResult,
    redraw: boolean,
  ): Promise<void> => {
    if (dead || options.readonly) return;
    if (busy) {
      throw new ClientError("validation", "A source change is already pending.");
    }
    const current = generation;
    const operation = Symbol("source operation");
    activeOperation = operation;
    busy = true;
    setBusy();
    try {
      checkAuthoredOutput(candidate.source, options.limits);
      if (candidate.status !== "supported" && candidate.error.admission) {
        throw new ClientError(candidate.error.kind, candidate.error.message);
      }
      await options.change(candidate);
      if (dead || current !== generation) return;
      value = candidate.source;
      result = candidate;
      // A newer admitted raw edit owns the textarea even while an older callback
      // completes. The accepted source advances independently of that draft.
      if (!pendingRaw) draftSource = value;
      if (redraw) await render();
      else {
        restoreSource();
        status.textContent = "";
        status.setAttribute("role", "status");
        node.dataset["state"] = candidate.status;
      }
    } catch (error) {
      if (!dead && current === generation) {
        if (!pendingRaw) draftSource = value;
        restoreSource();
      }
      // The originating action owns reporting. Tree callbacks reject to the
      // nested tree; raw/archive/reset errors are reported by their boundary.
      throw error;
    } finally {
      if (activeOperation === operation) {
        activeOperation = undefined;
        busy = false;
        if (!dead) {
          setBusy();
          void drainRaw();
        }
      }
    }
  };
  const action = (operation: () => Promise<void>): void => {
    const current = generation;
    void operation().catch((error: unknown) => {
      if (!dead && current === generation) fail(error);
    });
  };
  drainRaw = async (): Promise<void> => {
    if (dead || busy || drainingRaw || !pendingRaw) return;
    // A nested tree commits its own represented value and reports its own
    // rejection after the source callback settles. Wait for that operation's
    // release before a raw redraw can dispose it. No timer or callback replay.
    if (tree?.node.getAttribute("aria-busy") === "true") {
      if (!rawWait) {
        const mounted = tree;
        rawWait = new MutationObserver(() => {
          if (dead || tree !== mounted || mounted.node.getAttribute("aria-busy") !== "true") {
            rawWait?.disconnect();
            rawWait = undefined;
            void drainRaw();
          }
        });
        rawWait.observe(mounted.node, { attributes: true, attributeFilter: ["aria-busy"] });
      }
      return;
    }
    drainingRaw = true;
    try {
      while (!dead && !busy && pendingRaw) {
        const draft = pendingRaw;
        pendingRaw = undefined;
        if (draft.generation !== generation) continue;
        try {
          await publish(parse(draft.source), true);
        } catch (error) {
          if (!dead && draft.generation === generation) fail(error);
        }
      }
    } finally {
      drainingRaw = false;
    }
  };
  const render = async (): Promise<void> => {
    if (dead || !result) return;
    const current = generation, retainedRaw = sourceControl();
    rawWait?.disconnect();
    rawWait = undefined;
    tree?.dispose();
    tree = undefined;
    structuredHost.replaceChildren();
    resetHost.replaceChildren();
    status.textContent = result.status === "supported"
      ? ""
      : result.error.message;
    status.setAttribute(
      "role",
      result.status === "supported" ? "status" : "alert",
    );
    node.dataset["state"] = result.status;
    if (result.status === "supported") {
      const extracted = mode === "domain" && archiveAllowed
        ? extractArchive(result.tree)
        : { tree: result.tree, archived: false };
      archive = extracted.archived;
      tree = createTreeEditor({
        environment: { ...options.environment, signal: controller.signal },
        label: `${options.label} conditions`,
        model: options.model,
        value: extracted.tree,
        fields: (model, signal) =>
          model === options.model ? rootFields : load(model, signal),
        mode,
        ...(options.readonly !== undefined
          ? { readonly: options.readonly }
          : {}),
        ...(options.touch !== undefined ? { touch: options.touch } : {}),
        ...(options.limits ? { limits: options.limits } : {}),
        ...(options.provider ? { provider: options.provider } : {}),
        change: async (candidate) => {
          const source = serialize(
            mode === "domain" ? includeArchive(candidate, archive) : candidate,
          );
          const authored = parse(source);
          if (authored.status !== "supported") {
            throw new ClientError(authored.error.kind, authored.error.message);
          }
          await publish(authored, false);
        },
      });
      structuredHost.append(tree.node);
      if (mode === "domain" && archiveAllowed) {
        const wrapper = el("label", undefined, "editor-domain-archived"),
          checkbox = el("input");
        checkbox.type = "checkbox";
        checkbox.checked = archive;
        checkbox.dataset["domainArchived"] = "";
        checkbox.disabled = Boolean(options.readonly);
        wrapper.append(checkbox, document.createTextNode("Include archived"));
        structuredHost.append(wrapper);
        checkbox.addEventListener("change", () => {
          action(async () => {
            if (!result || result.status !== "supported") return;
            const clean = extractArchive(result.tree).tree;
            let source: string;
            try {
              source = serialize(includeArchive(clean, checkbox.checked));
            } catch (error) {
              checkbox.checked = archive;
              throw error;
            }
            await publish(parse(source), true);
          });
        });
      }
    } else archive = false;
    if (
      options.debug &&
      (mode === "expression" || result.status !== "supported" ||
        !options.readonly)
    ) {
      if (retainedRaw) {
        if (retainedRaw.value !== draftSource) retainedRaw.value = draftSource;
      } else {
        const rawLabel = el("label", "Source", "editor-source-label"),
          raw = el("textarea", undefined, "editor-source");
        raw.dataset["editorSource"] = "";
        raw.setAttribute("aria-label", `${options.label} source`);
        raw.readOnly = Boolean(options.readonly);
        raw.value = draftSource;
        raw.spellcheck = false;
        rawLabel.append(raw);
        rawHost.append(rawLabel);
        raw.addEventListener("input", () => {
          if (dead || options.readonly) return;
          try {
            checkAuthoredSource(raw.value, options.limits);
            checkAuthoredOutput(raw.value, options.limits);
            const candidate = parse(raw.value);
            if (candidate.status !== "supported" && candidate.error.admission) {
              throw new ClientError(candidate.error.kind, candidate.error.message);
            }
            draftSource = raw.value;
            pendingRaw = { generation, source: draftSource };
            void drainRaw();
          } catch (error) {
            raw.value = draftSource;
            fail(error);
          }
        });
      }
    } else rawHost.replaceChildren();
    if (!options.readonly && result.status !== "supported") {
      const reset = button("Reset", () => {
        action(() => publish(parse(mode === "domain" ? "[]" : "True"), true));
      }, "editor-source-reset");
      reset.dataset["editorReset"] = "";
      reset.setAttribute("aria-label", `Reset ${options.label}`);
      resetHost.append(reset);
    }
    setBusy();
    const mounted = tree;
    if (mounted) await mounted.ready;
    if (dead || current !== generation) return;
  };
  const setValue = async (source: string): Promise<void> => {
    checkAuthoredSource(source, options.limits);
    if (dead) return;
    const current = ++generation;
    pendingRaw = undefined;
    rawWait?.disconnect();
    rawWait = undefined;
    controller.abort();
    controller = new AbortController();
    tree?.dispose();
    tree = undefined;
    value = source;
    draftSource = source;
    node.dataset["state"] = "pending";
    status.textContent = "Loading field metadata…";
    structuredHost.replaceChildren();
    rawHost.replaceChildren();
    resetHost.replaceChildren();
    try {
      const metadata = await load(options.model, controller.signal);
      if (dead || current !== generation) return;
      rootFields = new Map(metadata);
      archiveAllowed = mode === "domain" &&
        rootFields.get("active")?.kind === "boolean";
      result = parse(source);
      await render();
    } catch (error) {
      if (!dead && current === generation) fail(error);
    }
  };
  const dispose = (): void => {
    if (dead) return;
    dead = true;
    generation++;
    pendingRaw = undefined;
    rawWait?.disconnect();
    rawWait = undefined;
    controller.abort();
    tree?.dispose();
    options.environment.signal?.removeEventListener("abort", dispose);
  };
  options.environment.signal?.addEventListener("abort", dispose, {
    once: true,
  });
  if (options.environment.signal?.aborted) dispose();
  const ready = dead ? Promise.resolve() : setValue(value);
  return { node, ready, setValue, dispose };
}
