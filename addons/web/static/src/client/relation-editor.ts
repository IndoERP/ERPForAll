/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
import { button, el, errorMessage } from "./dom.js";
import type { DialogHandle, DialogService } from "./dialog-service.js";
import type { EditorLocale } from "./datetime-values.js";
import type { MessageServices } from "./messages.js";
import { array, describeViews, object, text, type Node } from "./metadata.js";
import { RecordForm } from "./record-form.js";
import { RecordState } from "./record.js";
import { checkedRelationChoice, type RelationChoice } from "./relation-picker.js";
import { ClientError, type CallOptions, type RpcClient } from "./transport.js";
import { attachSubviews } from "./x2many.js";
import "./fields.js";

export interface RelationEditorOptions {
  readonly dialogs: DialogService; readonly rpc: RpcClient; readonly model: string;
  readonly messages?: MessageServices; readonly editorLocale?: EditorLocale; readonly numericLocale?: import("./numeric-values.js").NumericLocale;
  readonly context: Record<string, unknown>; readonly id?: number;
  readonly report: (error: unknown) => void; readonly signal?: AbortSignal;
}
/** Embedded record creation/editing through the same validated metadata, widgets and save path as a root form. */
export function editRelation(options: RelationEditorOptions): Promise<RelationChoice | undefined> {
  if (options.signal?.aborted) return Promise.resolve(undefined);
  if (options.id !== undefined && (!Number.isSafeInteger(options.id) || options.id <= 0)) return Promise.reject(new ClientError("validation", "Relation editor id must be a positive safe integer."));
  const scope = new AbortController(), context = structuredClone(options.context);
  // RecordState owns write admission; cancellation applies only to reads and never to a sent mutation.
  const readOptions = (callOptions: CallOptions = {}): CallOptions => callOptions.write ? callOptions : {
    ...callOptions, signal: callOptions.signal ? AbortSignal.any([scope.signal, callOptions.signal]) : scope.signal,
  };
  const rpc = new Proxy(options.rpc, { get(target, key) {
    if (key === "call") return (model: string, method: string, args: Record<string, unknown>, callOptions?: CallOptions): Promise<unknown> => target.call(model, method, args, readOptions(callOptions));
    if (key === "post") return (path: string, args: Record<string, unknown>, callOptions?: CallOptions): Promise<unknown> => target.post(path, args, readOptions(callOptions));
    return Reflect.get(target, key, target);
  } });
  const body = el("div", undefined, "relation-editor-body");
  const status = el("p", "Loading record…"); status.setAttribute("role", "status");
  const form = el("form", undefined, "record-form"); form.noValidate = true; body.append(status, form);
  const footer = el("div", undefined, "actions");
  let handle: DialogHandle, state: RecordState | undefined, surface: RecordForm | undefined;
  let dead = false, pending = false, uncertain = false, savePerformed = false;
  let resolve!: (choice: RelationChoice | undefined) => void;
  const result = new Promise<RelationChoice | undefined>(done => { resolve = done; });
  const externalAbort = (): void => handle.dispose();
  const report = (error: unknown): void => {
    if (dead) return;
    status.setAttribute("role", "alert"); status.textContent = errorMessage(error); options.report(error);
  };
  const disable = (): void => {
    surface?.setDisabled(pending || uncertain || savePerformed);
    save.disabled = pending || uncertain || surface === undefined;
    cancel.disabled = pending || uncertain;
    const close = handle.dialog.querySelector<HTMLButtonElement>(".message-dialog-close"); if (close) close.disabled = pending || uncertain;
    handle.dialog.setAttribute("aria-busy", String(pending));
    if (pending || uncertain || savePerformed) for (const control of surface?.controls() ?? []) control.disabled = true;
  };
  const render = (focus?: string): void => { if (dead) return; surface?.render(focus); disable(); };
  const run = (job: () => Promise<void>): void => {
    if (pending || uncertain || dead || savePerformed) return;
    const focus = surface?.focus(); pending = true; disable();
    void job().catch(report).finally(() => { pending = false; if (!dead) render(focus); });
  };
  const save = button("Save", () => {
    if (pending || uncertain || dead || !state || !surface) return;
    if (!savePerformed && !surface.valid()) return;
    const current = state;
    pending = true; disable();
    void (async () => {
      if (!savePerformed) { await current.save(); savePerformed = true; }
      if (dead) return;
      const rows = array(await rpc.call(options.model, "web_read", { ids: [current.id], specification: { display_name: {} }, context }), "saved relation records");
      if (dead) return;
      if (rows.length !== 1) throw new ClientError("server", "The saved relation could not be read back.");
      const selected = checkedRelationChoice(rows[0]);
      if (selected.id !== current.id) throw new ClientError("server", "Saved relation readback did not match the record.");
      resolve(selected); handle.close();
    })().catch(error => {
      if (error instanceof ClientError && error.kind === "ambiguous") uncertain = true;
      report(error);
    }).finally(() => {
      pending = false;
      if (!dead) { if (savePerformed) save.textContent = "Read saved record"; render(); }
    });
  }, "primary");
  const cancel = button("Cancel", () => { if (!pending && !uncertain) handle.close(); }); footer.append(save, cancel);
  form.addEventListener("submit", event => { event.preventDefault(); save.click(); });
  handle = options.dialogs.open({ title: options.id === undefined ? "Create related record" : "Edit related record", body, footer, size: "lg",
    dismiss: () => !pending && !uncertain,
    onClose: () => { dead = true; scope.abort(); surface?.dispose(); options.signal?.removeEventListener("abort", externalAbort); resolve(undefined); } });
  options.signal?.addEventListener("abort", externalAbort, { once: true }); disable();
  const rejectCommands = (nodes: readonly Node[]): void => {
    for (const node of nodes) {
      if (node.kind === "button" || node.kind === "stat_button" || node.kind === "cancel_button") throw new ClientError("unsupported", "Object and footer commands in the relation editor are not available here; open the record to run its actions.");
      if (node.kind === "widget") throw new ClientError("unsupported", "Service widgets in the relation editor are not available here.");
      if ("children" in node) rejectCommands(node.children);
    }
  };
  void (async () => {
    const raw = await rpc.call(options.model, "get_views", { wire_major: 1, views: [[null, "form"]], options: { mobile: window.innerWidth < 640 }, context });
    if (dead) return;
    const registry = text(object(raw, "view description")["registry_version"], "registry version");
    const description = describeViews(raw, options.model, registry), view = description.views.get("form");
    if (!view || view.mode !== "record") throw new ClientError("unsupported", "The relation editor requires a record form.");
    rejectCommands(view.nodes);
    await attachSubviews(description, view, async relation => {
      const child = describeViews(await rpc.call(relation, "get_views", { wire_major: 1, views: [[null, "list"], [null, "form"]], options: { mobile: window.innerWidth < 640 }, context }), relation, registry);
      const childForm = child.views.get("form"); if (childForm) rejectCommands(childForm.nodes);
      return child;
    });
    if (dead) return;
    state = new RecordState(rpc, options.model, view.fields, context); await state.load(options.id);
    if (dead) return;
    const current = state;
    surface = new RecordForm({ form, state: current, view, rpc, context, signal: scope.signal, messages: options.messages, editorLocale: options.editorLocale, numericLocale: options.numericLocale,
      notify: message => { status.setAttribute("role", "alert"); status.textContent = message; }, run, report,
      openRecord: async () => { throw new ClientError("unsupported", "Nested relation-editor navigation is not available here; open the related record from its own screen."); },
      changeValues: (changes, focus) => run(async () => { if (surface!.needsCommit()) await surface!.commit(); current.changeMany(changes); await current.onchange(Object.keys(changes)); if (!dead) render(focus); }),
      stage: (name, value) => {
        if (dead || uncertain || savePerformed) throw new ClientError("validation", "The relation editor cannot be edited during this operation.");
        current.change(name, value);
      },
      commit: async (name, focus) => {
        if (dead || uncertain || savePerformed) throw new ClientError("validation", "The relation editor cannot commit this draft.");
        await current.onchange(name); if (!dead) { render(focus); if (current.warning) { status.setAttribute("role", "alert"); status.textContent = current.warning; } }
      },
      change: (name, value, focus) => run(async () => { if (surface!.needsCommit()) await surface!.commit(); current.change(name, value); await current.onchange(name); if (!dead) { render(focus); if (current.warning) { status.setAttribute("role", "alert"); status.textContent = current.warning; } } }),
      button: () => { throw new ClientError("unsupported", "Object buttons in the relation editor are not available here."); } });
    status.textContent = current.warning;
  })().catch(error => { if (!scope.signal.aborted) report(error); }).finally(() => { pending = false; if (!dead) render(); });
  if (options.signal?.aborted) handle.dispose();
  return result;
}
