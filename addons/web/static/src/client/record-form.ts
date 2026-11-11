/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
import type { MessageServices } from "./messages.js";
import type { EditorLocale } from "./datetime-values.js";
import { CurrencyDisplayCache } from "./field-currency.js";
import { effectiveNodes, occurrenceFields } from "./field-modifiers.js";
import { combineFields } from "./metadata.js";
import { disposeViewButton } from "./view-button.js";
import { renderRibbon, notificationPermissionWidget } from "./message-widgets.js";
import { el } from "./dom.js";
import { fieldTooltip } from "./field-shell.js";
import { relationCount } from "./relation-count.js";
import { renderFormLayout } from "./form-layout.js";
import { Chatter } from "./chatter.js";
import type { MessagingStore } from "@erpforall/mail-store";
import type { ButtonNode, CancelNode, Node, View } from "./metadata.js";
import { resolveFieldWidget, type WidgetHandle } from "./registry.js";
import type { RecordState } from "./record.js";
import { ClientError, type RpcClient } from "./transport.js";

let nextRecordForm = 0;
let nextFieldInfo = 0;

/** Node-keyed state that survives re-rendering from per-render effective node copies.
 * Effective modifiers copy field and container nodes on every render; keys resolve back to the
 * declared view node so retained draft editors and selected notebook pages keep their identity. */
class SourceKeyed<V> extends Map<Node, V> {
  origin = new Map<Node, Node>();
  private source(key: Node): Node { return this.origin.get(key) ?? key; }
  override get(key: Node): V | undefined { return super.get(this.source(key)); }
  override set(key: Node, value: V): this { return super.set(this.source(key), value); }
  override has(key: Node): boolean { return super.has(this.source(key)); }
  override delete(key: Node): boolean { return super.delete(this.source(key)); }
}

export function formFocus(form: HTMLElement, element: Element | null = document.activeElement): string | undefined {
  if (!(element instanceof HTMLElement) || !form.contains(element)) return undefined;
  return element.dataset["focusReturn"] ?? element.dataset["focusKey"] ??
    (element instanceof HTMLInputElement || element instanceof HTMLTextAreaElement || element instanceof HTMLSelectElement ? element.name || undefined : undefined);
}
/** Shared record widgets and admitted layout; the owner supplies operation and lifetime policy. */
export class RecordForm {
  private readonly currencies = new CurrencyDisplayCache();
  private stops: (() => void)[] = [];
  private validators: { node: HTMLElement; validate(): boolean }[] = [];
  private represented: Readonly<Record<string, unknown>> = {};
  private draftVersion = -1;
  private widgetHandles: { name: string; handle: WidgetHandle }[] = [];
  private retained = new Map<Node, WidgetHandle>();
  /** The latest effective occurrence of each declared field node; retained draft callbacks read it, not their first render. */
  private effective = new Map<Node, Extract<Node, { kind: "field" }>>();
  private committed = new Map<string, unknown>();
  private pending = new Set<string>();
  private committing: Promise<void> | undefined;
  private dead = false;
  private rendering = false;
  private disabled = false;
  private saving = false;
  private epoch = 0;
  private nativeDisabled = new Map<HTMLInputElement | HTMLSelectElement | HTMLTextAreaElement | HTMLButtonElement, boolean>();
  private unbind: () => void;
  private readonly onAbort = (): void => this.dispose();
  private readonly pages = new SourceKeyed<number>();
  private layout: ReturnType<typeof renderFormLayout> | undefined;
  constructor(private readonly env: {
    signal?: AbortSignal;
    notify?(message: string): void;
    messages?: MessageServices;
    editorLocale?: EditorLocale;
    numericLocale?: import("./numeric-values.js").NumericLocale;
    form: HTMLFormElement; state: RecordState; view: View; rpc: RpcClient; context: Record<string, unknown>;
    messagingStore?: MessagingStore;
    /** Signed-in user id (chatter followers). */
    uid?: number;
    run(job: () => Promise<void>): void; report(error: unknown): void;
    openRecord(model: string, id: number): Promise<void>;
    change(name: string, value: unknown, focus: string | undefined): void;
    changeValues?(changes: Readonly<Record<string, unknown>>, focus: string | undefined): void;
    /** Stage immediately without RPC or replacing an editing surface. */
    stage?(name: string, value: unknown): void;
    /** Commit a staged field through the owner's existing onchange path. */
    commit?(name: string, focus: string | undefined): Promise<void>;
    button(node: ButtonNode, parent: HTMLElement): HTMLElement | void;
    headerHost?: HTMLElement; headerActionsHost?: HTMLElement; titleHost?: HTMLElement; notebookTabsHost?: HTMLElement; asideHost?: HTMLElement; chatterHost?: HTMLElement;
    footerHost?: HTMLElement;
    footer?(node: ButtonNode | CancelNode, parent: HTMLElement): HTMLElement | void;
  }) {
    this.unbind = env.state.bindEditorOwner({
      admit: values => { for (const [node, handle] of this.retained) if (node.kind === "field") handle.draft?.admit(values[node.field.name]); },
      onchangeAccepted: values => {
        for (const [node] of this.retained) if (node.kind === "field") this.committed.set(node.field.name, values[node.field.name]);
      },
      beforeSave: () => {
        // Ordinary forms keep their existing validation/admission path.
        if (!this.retained.size && !this.pending.size && !this.committing) return undefined;
        return this.commit().then(() => { if (!this.valid()) throw new ClientError("validation", "Complete the required fields before saving."); });
      },
      setSaving: value => { this.saving = value; this.applyDisabled(); },
      reset: () => this.reset(),
    });
    env.signal?.addEventListener("abort", this.onAbort, { once: true });
    if (env.signal?.aborted) this.dispose();
    env.form.noValidate = true;
    if ((env.titleHost || env.asideHost) && !env.form.id) env.form.id = `record-form-${++nextRecordForm}`;
  }
  private hosts(): HTMLElement[] { return [this.env.form, this.env.headerHost, this.env.headerActionsHost, this.env.titleHost, this.env.notebookTabsHost, this.env.asideHost].filter((host): host is HTMLElement => host !== undefined); }
  focus(): string | undefined { return this.hosts().map(host => formFocus(host)).find(key => key !== undefined); }
  controls(): (HTMLInputElement | HTMLSelectElement | HTMLTextAreaElement | HTMLButtonElement)[] {
    // Draft engines own their internal input/history policy through setDisabled, not native snapshots.
    return this.hosts().flatMap(host => [...host.querySelectorAll<HTMLInputElement | HTMLSelectElement | HTMLTextAreaElement | HTMLButtonElement>("input,select,textarea,button")])
      .filter(control => ![...this.retained.values()].some(handle => handle.node.contains(control)));
  }
  private clearLayout(): void { this.layout?.dispose(); this.layout = undefined; this.stops.forEach(stop => stop()); this.stops = []; this.validators = []; this.widgetHandles = []; }
  dispose(): void {
    if (this.dead) return;
    this.dead = true; this.epoch++; this.unbind(); this.env.signal?.removeEventListener("abort", this.onAbort); this.clearLayout();
    this.retained.forEach(handle => handle.dispose()); this.retained.clear(); this.effective.clear(); this.pending.clear(); this.nativeDisabled.clear();
  }
  /** Authoritative load/save/discard starts a fresh editor baseline, even for identical source. */
  reset(): void {
    if (this.dead) return;
    if (!this.retained.size) { this.epoch++; this.pending.clear(); this.committed.clear(); return; }
    this.rendering = true; this.epoch++;
    try { this.retained.forEach(handle => handle.dispose()); this.retained.clear(); this.effective.clear(); this.pending.clear(); this.committed.clear(); }
    finally { this.rendering = false; }
    this.render();
  }
  /** Owner operation policy for retained engines; ordinary controls keep their existing owner policy. */
  setDisabled(value: boolean): void {
    if (value && !this.disabled && !this.committing && !this.dead) for (const handle of this.retained.values()) handle.draft?.flush();
    this.disabled = value; this.applyDisabled();
  }
  private applyDisabled(): void {
    if (this.dead) return;
    const disabled = this.disabled || this.saving || this.committing !== undefined || this.env.state.writeUncertain;
    for (const control of this.controls()) {
      if (this.committing) { if (!this.nativeDisabled.has(control)) this.nativeDisabled.set(control, control.disabled); control.disabled = true; }
      else if (this.nativeDisabled.has(control)) { control.disabled = this.nativeDisabled.get(control)! || this.disabled || this.saving || this.env.state.writeUncertain; this.nativeDisabled.delete(control); }
    }
    for (const handle of this.retained.values()) handle.draft?.setDisabled(disabled);
  }
  /** Preserve the synchronous ordinary-field path when there are no draft commits. */
  needsCommit(): boolean {
    if (this.committing) return true;
    for (const handle of this.retained.values()) handle.draft?.flush();
    return this.pending.size !== 0;
  }
  /** Flush synchronous editor batches and serialize pending onchange before every Save path. */
  commit(): Promise<void> {
    if (this.dead) return Promise.reject(new ClientError("validation", "The record form is disposed."));
    if (this.committing) return this.committing;
    for (const handle of this.retained.values()) handle.draft?.flush();
    if (!this.pending.size) return Promise.resolve();
    const epoch = this.epoch;
    // Start in a microtask so the shared promise exists before owner callbacks can reenter.
    const operation = Promise.resolve().then(async () => {
      while (this.pending.size) {
        if (this.dead || epoch !== this.epoch) throw new ClientError("validation", "The record form commit was superseded.");
        const name = this.pending.values().next().value;
        if (name === undefined) break;
        if (!this.env.commit) throw new ClientError("validation", "Draft fields require an owner commit callback.");
        await this.env.commit(name, this.focus());
        if (this.dead || epoch !== this.epoch) throw new ClientError("validation", "The record form commit was superseded.");
        this.pending.delete(name); this.committed.set(name, this.env.state.values[name]);
      }
    });
    this.committing = operation.finally(() => { this.committing = undefined; this.applyDisabled(); });
    this.applyDisabled(); return this.committing;
  }
  render(focus = this.focus()): void {
    if (this.dead) throw new ClientError("validation", "The record form is disposed.");
    const { form, state, view } = this.env;
    if (this.draftVersion !== state.draftVersion) this.currencies.clear();
    // Capture editor-local text drafts of ordinary controls before the layout is cleared.
    const drafts = new Map<string, { value: string; message: string }>();
    for (const control of this.controls()) {
      if ((control instanceof HTMLInputElement || control instanceof HTMLTextAreaElement) && control.name &&
        (control.closest(".field-dirty,.field-invalid") || !control.validity.valid) && this.draftVersion === state.draftVersion && Object.is(this.represented[control.name], state.values[control.name])) drafts.set(control.name, { value: control.value, message: control.validationMessage });
    }
    const managed = new Set<string>();
    if (this.draftVersion === state.draftVersion) for (const { name, handle } of this.widgetHandles) {
      if (!Object.is(this.represented[name], state.values[name])) continue;
      const draft = handle.captureDraft?.();
      if (draft) { drafts.set(name, draft); managed.add(name); }
    }
    let nodes: Node[];
    try {
      nodes = effectiveNodes(view.nodes, state.fields, state.values);
      const occurrences = occurrenceFields(nodes);
      const present = new Set(occurrences.map(field => field.name));
      state.updatePolicy(combineFields([...occurrences, ...view.fields.filter(field => !present.has(field.name))]));
    } catch (error) { state.updatePolicy(view.fields, error instanceof Error ? error : new Error(String(error))); throw error; }
    const origin = new Map<Node, Node>();
    const pair = (effective: readonly Node[], source: readonly Node[]): void => effective.forEach((node, index) => {
      const declared = source[index];
      if (declared === undefined) throw new ClientError("validation", "Effective form nodes changed shape.");
      origin.set(node, declared);
      if ("children" in node && "children" in declared) pair(node.children, declared.children);
    });
    pair(nodes, view.nodes);
    this.pages.origin = origin;
    this.rendering = true;
    const seen = new Set<Node>();
    this.clearLayout();
    // Restore retained controls before detaching; rebuilt ordinary controls carry fresh policy.
    for (const [control, value] of this.nativeDisabled) control.disabled = value;
    this.nativeDisabled.clear();
    try {
    form.replaceChildren();
    this.env.footerHost?.replaceChildren();
    this.env.headerHost?.replaceChildren();
    this.env.headerActionsHost?.replaceChildren();
    this.env.titleHost?.replaceChildren();
    this.env.notebookTabsHost?.replaceChildren();
    this.env.asideHost?.replaceChildren();
    this.env.chatterHost?.replaceChildren();
    const declaredTitle = (nodes: readonly Node[]): boolean => nodes.some(node => node.kind === "title" || (node.kind === "root" || node.kind === "sheet") && declaredTitle(node.children));
    // Legacy fallback remains only for views that have not declared a standard XML title region.
    const title = declaredTitle(view.nodes) ? undefined : view.fields.find(f => f.name === "name" && f.kind === "char") ?? view.fields.find(f => f.kind === "char");
    let titleRendered = false;
    let pendingTips: { anchor: HTMLElement; text: string }[] = [];
    this.layout = renderFormLayout(form, nodes, (node, parent) => {
      if (node.widget === "relation_count") {
        const handle = relationCount({ model: state.model, field: node.field.name, label: node.label,
          id: () => state.id, context: this.env.context, rpc: this.env.rpc,
          signal: this.env.signal, report: this.env.report });
        parent.append(handle.node); this.stops.push(handle.dispose);
        if (state.id === undefined) { const action = parent.closest("button"); if (action) action.disabled = true; }
        return;
      }
      if (node.widget === "statinfo") {
        const value = state.values[node.field.name];
        if (value !== undefined && value !== null && value !== false && (typeof value !== "number" || !Number.isFinite(value))) throw new Error(`Invalid statistic value for ${node.field.name}.`);
        const box = el("span", undefined, "form-stat-field");
        box.append(el("span", typeof value === "number" ? String(value) : "—", "form-stat-value"), document.createTextNode(" "), el("span", node.label, "form-stat-label"));
        parent.append(box); return;
      }
      const source = origin.get(node) ?? node;
      this.effective.set(source, node);
      const widget = resolveFieldWidget(node.widget, "form");
      let handle = this.retained.get(source);
      if (handle) handle.draft!.update(state.values[node.field.name], { ...node.field, widget: node.widget, label: node.label });
      else handle = widget.render({
        field: { ...node.field, widget: node.widget, label: node.label }, value: state.values[node.field.name], values: state.values, fields: state.currentFields, changeValues: this.env.changeValues ? changes => this.env.changeValues!(changes, this.focus()) : undefined, numericLocale: this.env.numericLocale, currencies: this.currencies, viewContext: "form",
        rpc: this.env.rpc, context: this.env.context, ...(this.env.messages ? { messages: this.env.messages } : {}), editorLocale: this.env.editorLocale, signal: this.env.signal, run: this.env.run, report: this.env.report,
        openRecord: this.env.openRecord,
        draft: {
          stage: value => {
            const current = this.effective.get(source) ?? node;
            if (this.dead || this.rendering || this.disabled || this.saving || this.committing || current.field.readonly) throw new ClientError("validation", "This field cannot be edited.");
            if (!this.env.stage) throw new ClientError("validation", "Draft fields require an owner stage callback.");
            this.env.stage(node.field.name, value);
            const previous = this.committed.get(node.field.name);
            const empty = (item: unknown): unknown => item === null || item === undefined || item === "" ? false : item;
            if (Object.is(empty(previous), empty(value))) this.pending.delete(node.field.name); else this.pending.add(node.field.name);
            // Other occurrences echo the same field without resetting identical history.
            for (const [other, retained] of this.retained) {
              if (other === source || other.kind !== "field" || other.field.name !== node.field.name) continue;
              const occurrence = this.effective.get(other) ?? other;
              retained.draft?.update(value, { ...occurrence.field, widget: occurrence.widget, label: occurrence.label });
            }
          },
          commit: () => {
            if (this.dead || this.rendering) return;
            const epoch = this.epoch;
            queueMicrotask(() => {
              if (this.dead || this.rendering || epoch !== this.epoch) return;
              void this.commit().catch(error => {
                // Discard/reload/disposal intentionally cancels this background read owner.
                // Foreground Save still receives the rejected commit; current-owner failures report.
                if (!this.dead && epoch === this.epoch) this.env.report(error);
              });
            });
          },
        },
        change: value => this.env.change(node.field.name, value, this.focus())
      });
      // Register disposal before anything below can throw (tooltip bounds, emptiness hooks).
      if (!handle.draft) this.stops.push(handle.dispose);
      if (handle.draft) {
        seen.add(source); this.retained.set(source, handle);
        if (!this.committed.has(node.field.name)) this.committed.set(node.field.name, state.values[node.field.name]);
      } else {
        this.widgetHandles.push({ name: node.field.name, handle });
        if (managed.has(node.field.name)) handle.restoreDraft?.(drafts.get(node.field.name)!);
      }
      if (this.env.asideHost?.contains(parent) || this.env.titleHost?.contains(parent)) {
        if (handle.node.matches("input,select,textarea,button")) handle.node.setAttribute("form", form.id);
        for (const control of handle.node.querySelectorAll("input,select,textarea,button")) control.setAttribute("form", form.id);
      }
      if (!titleRendered && node.widget !== "statusbar" && !parent.closest(".form-aside,.form-title-block") && node.field.name === title?.name) {
        titleRendered = true;
        const box = el("div", undefined, "form-title-field"); box.append(handle.node);
        // Only lift a title from the always-visible sheet; a notebook title must retain its page semantics.
        const titleHost = !parent.closest(".form-tab-panel,.form-header") ? this.env.titleHost : undefined;
        if (titleHost) for (const control of box.querySelectorAll("input,select,textarea,button")) control.setAttribute("form", form.id);
        (titleHost ?? parent).append(box);
      } else parent.append(handle.node);
      const value = state.values[node.field.name];
      const empty = widget.isEmpty ? widget.isEmpty(value) : node.field.kind === "boolean" ? false : value === undefined || value === null || value === false || value === "" || Array.isArray(value) && value.length === 0;
      if (typeof empty !== "boolean") throw new ClientError("validation", "Field emptiness hook must return a boolean.");
      handle.node.classList.toggle("field-empty", empty);
      this.validators.push({ node: handle.node, validate: () => {
        const current = state.values[node.field.name];
        const currentEmpty = widget.isEmpty ? widget.isEmpty(current) : node.field.kind === "boolean" ? false : current === undefined || current === null || current === false || current === "" || Array.isArray(current) && current.length === 0;
        if (typeof currentEmpty !== "boolean") throw new ClientError("validation", "Field emptiness hook must return a boolean.");
        const result = widget.isValid ? widget.isValid(current) : true;
        if (typeof result !== "boolean") throw new ClientError("validation", "Field validity hook must return a boolean.");
        return result && !(widget.isEmpty && node.field.required && !node.field.readonly && !node.field.invisible && currentEmpty);
      } });
      const tooltip = fieldTooltip(node.field, node.widget, this.env.context["debug"] === true);
      if (tooltip && this.env.messages) {
        if (new TextEncoder().encode(tooltip).length > 16384) throw new ClientError("validation", "Field tooltip metadata exceeds 16384 bytes.");
        // The handle host survives asynchronous child replacement (e.g. currency metadata).
        const anchor = handle.node;
        pendingTips.push({ anchor, text: tooltip });
        if (this.env.context["debug"] === true) {
          const detail = el("span", tooltip, "visually-hidden"); detail.id = `field-info-${++nextFieldInfo}`; handle.node.after(detail);
          if (!anchor.matches("input,select,textarea,button,a[href],[role]")) { anchor.setAttribute("role", "group"); anchor.setAttribute("aria-label", node.field.label); }
          anchor.setAttribute("aria-describedby", [anchor.getAttribute("aria-describedby"), detail.id].filter(Boolean).join(" "));
        }
      }
    }, this.pages, (node,parent)=>{const control=this.env.button(node,parent);if(control)this.stops.push(()=>disposeViewButton(control));return control;}, this.env.footer ? (node, parent) => {const control=this.env.footer!(node, this.env.footerHost ?? parent);if(control)this.stops.push(()=>disposeViewButton(control));} : undefined, { headerHost: this.env.headerHost, headerActionsHost: this.env.headerActionsHost, notebookTabsHost: this.env.notebookTabsHost, asideHost: this.env.asideHost, chatterHost: this.env.chatterHost, titleHost: this.env.titleHost, displayWidget:(node,parent)=>{
      if(node.kind==="ribbon"){parent.append(renderRibbon({text:node.text,background:node.background,...(node.tooltip!==undefined?{tooltip:node.tooltip}:{})}));return;}
      const messages=this.env.messages;
      if(!messages)throw new ClientError("unsupported","Notification permission guidance requires the form message owner.");
      const handle=notificationPermissionWidget({open:()=>{messages.dialogs.alert({title:"Browser notification permission",message:"Notifications are blocked for this site. Review the site's notification permission in your browser settings."});},report:this.env.report});
      parent.append(handle.node);this.stops.push(handle.dispose);
    }, renderChatter: (node, slot) => {
      const chatter = new Chatter({
        host: slot,
        model: state.model,
        recordId: state.id,
        rpc: this.env.rpc,
        store: this.env.messagingStore,
        reportError: this.env.report,
        reloadOnPost: node.reloadOnPost,
        reloadOnFollower: node.reloadOnFollower,
        ...(this.env.uid !== undefined ? { uid: this.env.uid } : {}),
        reloadForm: () => state.id !== undefined ? this.env.state.load(state.id).then(() => this.render()) : Promise.resolve(),
        document: this.env.form.ownerDocument,
      });
      this.stops.push(() => chatter.dispose());
    } });
    if (pendingTips.length && this.env.messages) {
      const messages = this.env.messages; let live = true, observer: MutationObserver | undefined;
      const attach = (): void => {
        if (!live) return;
        pendingTips = pendingTips.filter(tip => {
          if (!tip.anchor.isConnected) return true;
          try { this.stops.push(messages.tooltips.attach(tip.anchor, { content: () => el("span", tip.text) })); }
          catch (error) { this.env.report(error); }
          return false;
        });
        if (!pendingTips.length) observer?.disconnect();
      };
      attach();
      if (pendingTips.length) { observer = new MutationObserver(attach); observer.observe(form.ownerDocument.documentElement, { childList: true, subtree: true }); }
      this.stops.push(() => { live = false; observer?.disconnect(); });
    }
    this.represented = { ...state.values }; this.draftVersion = state.draftVersion;
    for (const control of this.controls()) {
      if ((control instanceof HTMLInputElement || control instanceof HTMLTextAreaElement) && !control.disabled && !managed.has(control.name) && drafts.has(control.name)) {
        const draft = drafts.get(control.name)!;
        control.value = draft.value; control.dispatchEvent(new Event("input"));
        if (draft.message && control.validity.valid) {
          control.setCustomValidity(draft.message); control.setAttribute("aria-invalid", "true");
          const error = control.closest(".datetime-editor,.field-shell")?.querySelector<HTMLElement>(".editor-validation,.field-error");
          if (error) { error.textContent = draft.message; error.hidden = false; }
        }
      }
    }
    if (focus) for (const control of this.hosts().flatMap(host => [...host.querySelectorAll<HTMLElement>("input,textarea,select,[data-focus-key]")])) {
      const name = control instanceof HTMLInputElement || control instanceof HTMLTextAreaElement || control instanceof HTMLSelectElement ? control.name : "";
      if (control.dataset["focusKey"] === focus || name === focus) { control.focus(); break; }
    }
    for (const [node, handle] of this.retained) if (!seen.has(node)) { handle.dispose(); this.retained.delete(node); }
    this.applyDisabled();
    } finally { this.rendering = false; }
  }
  valid(notify = true): boolean {
    let draftValid = true;
    for (const handle of this.retained.values()) if (handle.draft && !handle.draft.validate()) {
      if (draftValid) { this.layout?.revealField(handle.node); handle.draft.focus(); }
      draftValid = false;
    }
    const controls = this.controls().filter(control => !(control instanceof HTMLButtonElement) && !control.closest(".code-field"));
    for (const control of controls) {
      if (control.validity.valid) control.removeAttribute("aria-invalid");
      else control.setAttribute("aria-invalid", "true");
    }
    const invalid = controls.find(control => !control.validity.valid);
    if (invalid) { this.layout?.revealField(invalid); invalid.focus(); if (notify) this.env.notify?.("Complete the required fields before saving."); }
    const widgetInvalid = this.validators.find(item => !item.validate());
    if (widgetInvalid) { widgetInvalid.node.setAttribute("aria-invalid", "true"); widgetInvalid.node.classList.add("field-invalid"); this.layout?.revealField(widgetInvalid.node); if (!invalid && draftValid) { const control = widgetInvalid.node.querySelector<HTMLElement>("input,select,textarea,button"); control?.focus(); if (notify) this.env.notify?.("Complete the required fields before saving."); } }
    return this.env.form.reportValidity() && draftValid && widgetInvalid === undefined;
  }
}
