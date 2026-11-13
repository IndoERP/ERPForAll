/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
import type { MessageServices } from "./messages.js";
import type { EditorLocale } from "./datetime-values.js";
import type { DialogHandle } from "./dialog-service.js";
import type { ActionResult } from "../generated/human-ui.js";
import { buttonHidden, runObjectButton } from "./buttons.js";
import { actionButton, button, confirmChoice, el } from "./dom.js";
import type { ButtonNode, CancelNode, View } from "./metadata.js";
import { passwordArguments, passwordModels, passwordSubmission, type Resubmission } from "./password-flow.js";
import { RecordForm, formFocus } from "./record-form.js";
import type { RecordState } from "./record.js";
import { ClientError, type RpcClient } from "./transport.js";
import { viewButton, ButtonDebouncer, ButtonHotkeys } from "./view-button.js";

type FormElement = HTMLInputElement | HTMLSelectElement | HTMLTextAreaElement | HTMLButtonElement;

/** One disposable action scope, independent of the caller's route and record. */
export class WizardDialog {
  readonly dialog = el("dialog", undefined, "wizard-dialog");
  private readonly form = el("form", undefined, "record-form");
  private readonly status = el("p", undefined, "wizard-status");
  private readonly statusRow = el("div", undefined, "wizard-status-row");
  private readonly notification = el("p", undefined, "wizard-notice");
  private readonly surface: RecordForm;
  private readonly abort = new AbortController();
  private modal: DialogHandle | undefined;
  private pending = false;
  private commitPending: Promise<void> | undefined;
  private queuedCommits: (() => Promise<void>)[] = [];
  private queuedActivation: (() => Promise<void>) | undefined;
  private uncertain = false;
  private saveUncertain = false;
  private nextFocus: string | undefined;
  private dead = false;
  private readonly debounce=new ButtonDebouncer();
  private readonly hotkeys:ButtonHotkeys;
  private disabled = new Map<FormElement, boolean>();
  private readonly prior = document.activeElement;
  constructor(private readonly env: {
    /** The My Preferences presentation of the title block (avatar initial, row icons). */
    profileTitle?: boolean; messages?: MessageServices; editorLocale?: EditorLocale; numericLocale?: import("./numeric-values.js").NumericLocale;debug?:boolean;uid?:number;action?(id:number,context:Record<string,unknown>):Promise<void>;url?(href:string):Promise<void>; name: string; state: RecordState; view: View; rpc: RpcClient; context: Record<string, unknown>;
    report(error: unknown): void; notice(message: string): void;
    openRecord(model: string, id: number): Promise<void>;
    result(value: unknown, context?:Record<string,unknown>, button?:string, resubmit?:Resubmission): Promise<void>; closed(): Promise<void>;
  }) {
    const heading = el("h2", env.name); heading.id = "wizard-dialog-heading";
    this.dialog.setAttribute("aria-labelledby", heading.id);
    // Reload is a recovery step, so it is shown
    // only next to an uncertain outcome (see disable()).
    const close = button("×", () => this.run(async () => { if (await this.canLeave(true)) await this.close(); }), "wizard-close");
    close.setAttribute("aria-label", "Close"); close.title = "Close";
    const reload = actionButton("Reload", () => this.reload(), "wizard-reload");
    reload.setAttribute("aria-label", "Reload wizard");
    const head = el("div", undefined, "wizard-heading"); head.append(heading, close);
    const sheet = el("div", undefined, "content-sheet"); sheet.append(this.form);
    const footer = el("div", undefined, "wizard-footer");
    footer.setAttribute("role", "toolbar"); footer.setAttribute("aria-label", "Wizard actions");
    this.status.setAttribute("role", "status");
    this.statusRow.append(this.status, reload);
    this.statusRow.hidden = true;
    this.notification.hidden = true;
    this.notification.setAttribute("data-wizard-notice", "");
    this.dialog.append(head, this.statusRow, this.notification, sheet, footer);
    this.surface = new RecordForm({ ...(env.profileTitle ? { profileTitle: true } : {}), ...(env.messages ? { messages: env.messages } : {}), editorLocale: env.editorLocale, numericLocale: env.numericLocale, notify: message => env.messages ? env.messages.notifications.add(message, { level: "danger" }) : env.notice(message), signal: this.abort.signal, form: this.form, state: env.state, view: env.view, rpc: env.rpc, context: env.context,
      run: job => {
        if (this.dead || this.pending || this.commitPending !== undefined || this.uncertain || this.saveUncertain) return;
        void job().catch(error => env.report(error));
      }, report: error => env.report(error),
      openRecord: async (model, id) => {
        if (!await this.canLeave(true)) return;
        await this.close();
        await env.openRecord(model, id);
      },
      changeValues: (changes, focus) => this.runCommit(async () => {
        if (this.surface.needsCommit()) await this.surface.commit();
        env.state.changeMany(changes); await env.state.onchange(Object.keys(changes));
        if (!this.dead) { this.surface.render(focus); if (env.messages) env.messages.warning(env.state.warningDetails); else if (env.state.warning) env.notice(env.state.warning); }
      }),
      stage: (name, value) => {
        if (this.dead || this.uncertain || this.saveUncertain) throw new ClientError("validation", "The wizard cannot be edited during this operation.");
        env.state.change(name, value);
      },
      commit: async (name, focus) => {
        if (this.dead || this.uncertain || this.saveUncertain) throw new ClientError("validation", "The wizard cannot commit this draft.");
        await env.state.onchange(name);
        if (!this.dead) { this.surface.render(focus); if (env.messages) env.messages.warning(env.state.warningDetails); else if (env.state.warning) env.notice(env.state.warning); }
      },
      change: (name, value, focus) => {
        // A password form has no server onchange (item 7). Its field commit stages the value at once:
        // a pending job would disable the buttons, and the click whose blur committed the field would be lost.
        if (passwordModels.has(env.state.model)) {
          if (this.dead || this.pending || this.commitPending !== undefined || this.uncertain || this.saveUncertain) throw new ClientError("validation", "The wizard cannot be edited during this operation.");
          env.state.change(name, value);
          // A typed secret is not re-rendered: that would replace the button the click is about to land on. The reset
          // wizard's selected-user lines (an array, changed from their own line dialog) are rendered as before.
          if (Array.isArray(value) && !this.dead) this.surface.render(focus);
          return;
        }
        this.runCommit(async () => {
          if (this.surface.needsCommit()) await this.surface.commit();
          env.state.change(name, value);
          await env.state.onchange(name);
          if (!this.dead) { this.surface.render(focus); if (env.messages) env.messages.warning(env.state.warningDetails); else if (env.state.warning) env.notice(env.state.warning); }
        });
      },
      button: (node, parent) => this.renderButton(node, parent),
      footerHost: footer,
      footer: (node, parent) => this.renderButton(node, parent)
    });
    this.hotkeys=new ButtonHotkeys(()=>[...this.dialog.querySelectorAll<HTMLButtonElement>("button")],{blocked:()=>this.pending||this.commitPending!==undefined||this.uncertain||this.saveUncertain||this.env.messages?.block.blocked===true,report:error=>this.env.report(error)});
    this.form.addEventListener("submit", event => {
      event.preventDefault();
      // Enter in a password form presses its submission button: a password form is never saved.
      if (passwordModels.has(env.state.model)) { this.submissionButton()?.click(); return; }
      this.run(() => this.save());
    });
    this.dialog.addEventListener("cancel", event => {
      event.preventDefault(); this.run(async () => { if (await this.canLeave(true)) await this.close(); });
    });
    this.surface.render();
  }
  mount(): void {
    if (this.dead) throw new ClientError("validation", "The wizard is closed.");
    if (this.env.messages) this.modal = this.env.messages.dialogs.attach(this.dialog, {
      dismiss: async () => { if (await this.canLeave()) await this.close(); return false; }
    });
    else { document.body.append(this.dialog); this.dialog.showModal(); }
    if (this.env.messages) this.env.messages.warning(this.env.state.warningDetails);
    // A profile header opens on its notebook tab: the name, e-mail and phone fields come before it in tab order, so the name shows no focus rule at first sight.
    ((this.env.profileTitle ? this.form.querySelector<HTMLElement>("[role=tab][aria-selected=true]") : null) ?? this.form.querySelector<HTMLElement>("input,select,textarea,button"))?.focus();
  }
  /** Text-only announcement in the active native modal, never its inert background. */
  notify(result: Extract<ActionResult, {kind: "notification"}>): void {
    if (this.dead) return;
    this.notification.textContent = result.title ? `${result.title}: ${result.message}` : result.message;
    this.notification.dataset["level"] = result.level;
    this.notification.setAttribute("role", result.level === "warning" || result.level === "danger" ? "alert" : "status");
    this.notification.hidden = false;
  }
  get dirty(): boolean { return this.uncertain || this.saveUncertain || this.env.state.dirty || this.env.state.id === undefined; }
  get busy(): boolean { return this.pending || this.commitPending !== undefined || this.queuedCommits.length > 0 || this.env.state.busy; }
  private run(job: () => Promise<void>): void {
    if (this.dead || this.uncertain || this.saveUncertain) return;
    if (this.pending) return;
    if (this.commitPending !== undefined) {
      this.queuedActivation = job;
      return;
    }
    const focus = formFocus(this.dialog);
    this.pending = true; this.disable();
    void job().catch(error => { this.env.report(error); }).finally(() => {
      this.pending = false;
      if (!this.dead) {
        for (const [input, disabled] of this.disabled) input.disabled = disabled;
        this.disabled.clear();
        const target = this.nextFocus ?? focus; this.nextFocus = undefined;
        this.surface.render(target);
        this.disable();
        if (target) for (const control of this.dialog.querySelectorAll<HTMLElement>("[data-focus-key]")) if (control.dataset["focusKey"] === target) control.focus();
      }
    }).catch(error => { this.env.report(error); });
  }
  private runCommit(job: () => Promise<void>): void {
    if (this.dead || this.pending || this.uncertain || this.saveUncertain) return;
    if (this.commitPending !== undefined) {
      this.queuedCommits.push(job);
      return;
    }
    const focus = formFocus(this.dialog);
    let commitOk = false;
    let commitError: unknown;
    const work = (async () => {
      try {
        await job();
        commitOk = true;
      } catch (error) {
        commitError = error;
      }
    })();
    this.commitPending = work;
    this.disable();
    void work.finally(() => {
      if (this.commitPending === work) this.commitPending = undefined;
      this.disable();
      if (!this.dead) {
        if (!commitOk) {
          for (const [input, disabled] of this.disabled) input.disabled = disabled;
          this.disabled.clear();
          const target = this.nextFocus ?? focus; this.nextFocus = undefined;
          this.surface.render(target);
          this.disable();
          if (target) for (const control of this.dialog.querySelectorAll<HTMLElement>("[data-focus-key]")) if (control.dataset["focusKey"] === target) control.focus();
          if (commitError !== undefined) this.env.report(commitError);
        }
        if (commitOk && this.queuedCommits.length > 0) {
          const nextCommit = this.queuedCommits.shift()!;
          this.runCommit(nextCommit);
        } else {
          this.queuedCommits.length = 0;
          if (commitOk && this.queuedActivation) {
            const next = this.queuedActivation;
            this.queuedActivation = undefined;
            this.run(next);
          } else {
            this.queuedActivation = undefined;
          }
        }
      }
    });
  }
  private disable(): void {
    const isAction = this.pending || this.uncertain || this.saveUncertain;
    const isCommit = this.commitPending !== undefined;
    this.surface.setDisabled(isAction || isCommit);
    for (const input of this.dialog.querySelectorAll<HTMLInputElement | HTMLSelectElement | HTMLTextAreaElement | HTMLButtonElement>("input,select,textarea,button")) {
      if (input.closest(".code-field")) continue; // Its engine/history are owned by RecordForm.
      const isBtn = input instanceof HTMLButtonElement || input.type === "button" || input.type === "submit";
      if (isBtn) {
        if (isAction) {
          if (!this.disabled.has(input)) this.disabled.set(input, input.disabled);
          input.disabled = true;
        } else if (this.disabled.has(input)) {
          input.disabled = this.disabled.get(input)!;
          this.disabled.delete(input);
        }
      } else {
        if (isAction || isCommit) {
          if (!this.disabled.has(input)) this.disabled.set(input, input.disabled);
          input.disabled = true;
        } else if (this.disabled.has(input)) {
          input.disabled = this.disabled.get(input)!;
          this.disabled.delete(input);
        }
      }
    }
    const reload = this.dialog.querySelector<HTMLButtonElement>(".wizard-reload");
    if (reload) reload.disabled = this.pending;
    this.statusRow.hidden = !this.uncertain && !this.saveUncertain;
    this.status.textContent = this.uncertain
      ? "The action may have completed. This dialog is read-only; reloading cannot authorize another attempt."
      : this.saveUncertain ? "The save outcome is unknown. Reload the record to check it before continuing." : "";
    this.dialog.setAttribute("aria-busy", String(isAction || isCommit));
  }
  /** The enabled button of this dialog that submits its password operation, if any. */
  private submissionButton(): HTMLButtonElement | undefined {
    return [...this.dialog.querySelectorAll<HTMLButtonElement>("button[data-focus-key]")].find(control => {
      const name = control.dataset["focusKey"]?.replace(/^button:/, "");
      return !control.disabled && name !== undefined && passwordSubmission(this.env.state.model, name) !== undefined;
    });
  }
  /** Commit the open draft and run the form's own validation, focusing the first invalid control. */
  private async validated(): Promise<boolean> {
    await this.surface.commit();
    for (const [input, disabled] of this.disabled) input.disabled = disabled;
    const valid = this.surface.valid(), invalidFocus = formFocus(this.form);
    this.disable();
    if (!valid) this.nextFocus = invalidFocus;
    return valid;
  }
  private async save(): Promise<void> {
    if (!await this.validated()) return;
    try { await this.env.state.save(); }
    catch (error) {
      // RecordState retains ambiguous web_save recovery; no mutation is replayed here.
      if (error instanceof ClientError && error.kind === "ambiguous") this.saveUncertain = true;
      throw error;
    }
  }
  private renderButton(node: ButtonNode | CancelNode, parent: HTMLElement): HTMLButtonElement | undefined {
    if (buttonHidden(node, this.env.view.fields, this.env.state.values)) return;
    const control = viewButton(node,{model:this.env.state.model,debug:this.env.debug,...(this.env.messages?{messages:this.env.messages}:{}),report:error=>this.env.report(error),activate:()=>this.debounce.run(node,node.details?.debounce_ms??0,()=>this.run(async () => {
      if (node.kind === "cancel_button") {
        if(node.details?.block_ui && !this.env.messages)throw new ClientError("unsupported","This button owner does not supply UI blocking.");
        const release=node.details?.block_ui?this.env.messages!.block.begin():undefined;
        try{await this.close();}finally{release?.();}return;
      }
      await runObjectButton(node, {
        model: this.env.state.model, context: this.env.context,...(this.env.uid===undefined?{}:{uid:this.env.uid}), fields:this.env.view.fields,values:()=>this.env.state.values,id: () => this.env.state.id,
        ...(this.env.action?{action:this.env.action}:{}),...(this.env.url?{url:this.env.url}:{}),...(this.env.messages?{block:()=>this.env.messages!.block.begin()}:{}),
        dirty: () => this.env.state.dirty, save: () => this.save(),
        // A password submission sends its typed secrets as parameters of the managed operation and is never
        // saved first: the server refuses a saved secret.
        secretArguments: async (method, context) => {
          const kind = passwordSubmission(this.env.state.model, method);
          if (kind === undefined) return undefined;
          if (!await this.validated()) return null;
          return passwordArguments(kind, this.env.state.values, this.env.state.id, context());
        },
        // The confirmation that answers a submission replaces this wizard, so the kept operation is sent again
        // without this wizard's closed-state guard.
        resubmitCall: (model, method, args) => this.env.rpc.call(model, method, args, { write: true }),
        confirm: async (title, message, accept, cancel="Cancel") => await confirmChoice({title,message,accept,cancel,signal:this.abort.signal,...(this.env.messages?{service:this.env.messages.dialogs}:{})}) && !this.dead,
        call: async (model, method, args) => {
          if (this.dead) throw new ClientError("validation", "The wizard is closed.");
          try { return await this.env.rpc.call(model, method, args, { write: true }); }
          catch (error) { if (error instanceof ClientError && error.kind === "ambiguous") this.uncertain = true; throw error; }
        },
        reload: async () => { throw new ClientError("server", "Wizard result handler is required."); },
        result: async (value,effective,resubmit) => {
          if (this.dead) return;
          try { await this.env.result(value,effective,node.name,resubmit); }
          catch (error) { this.uncertain = true; throw error; }
        }
      });
    })), className: `form-header-btn ${node.variant}` });
    if (node.kind !== "cancel_button") control.dataset["focusKey"] = `button:${node.name}`;
    parent.append(control); return control;
  }
  /** Read-only recovery, offered only while an outcome is uncertain; it clears save uncertainty only and
   * a method operation can never be replayed. */
  private reload(): void {
    if (this.dead || this.pending || this.commitPending !== undefined) return;
    this.pending = true; this.disable();
    void (async () => {
      if (this.dead) return;
      await this.env.state.load(this.env.state.id);
      if (!this.dead) this.saveUncertain = false;
    })().catch(error => { this.env.report(error); }).finally(() => {
      this.pending = false;
      if (!this.dead) {
        for (const [input, disabled] of this.disabled) input.disabled = disabled;
        this.disabled.clear(); this.surface.render(); this.disable();
      }
    }).catch(error => { this.env.report(error); });
  }
  async canLeave(insideOperation = false): Promise<boolean> {
    return !(this.dead || (this.pending || this.commitPending !== undefined) && !insideOperation || this.uncertain || this.saveUncertain || this.env.state.busy);
  }
  async close(): Promise<void> {
    if (this.dead || this.uncertain || this.saveUncertain) return;
    this.dispose();
    await this.env.closed();
  }
  dispose(): void {
    if (this.dead) return;
    this.dead = true; this.queuedCommits.length = 0; this.queuedActivation = undefined;
    this.hotkeys.dispose(); this.debounce.dispose(); this.abort.abort(); this.surface.dispose();
    if (this.modal) { this.modal.dispose(); this.modal = undefined; } else { this.dialog.close(); this.dialog.remove(); }
    if (this.prior instanceof HTMLElement && this.prior.isConnected) this.prior.focus();
  }
}
