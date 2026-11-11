/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
import { DialogService, type DialogHandle } from "./dialog-service.js";
import { icon, type IconName } from "./icons.js";

/** Built-in commands with a fixed icon. Only code opts in through `actionButton`: a label that comes from data
 *  (a menu, view, page or arch button that happens to read "Actions" or "Delete") never gets one. */
const actionIcons = {New:"plus", Save:"save", Discard:"rotate-ccw", Reload:"refresh-cw", Delete:"trash", Search:"search", Previous:"chevron-left", Next:"chevron-right", Actions:"chevron-down"} as const satisfies Readonly<Record<string, IconName>>;
export type ActionLabel = keyof typeof actionIcons;
export function isActionLabel(label: string): label is ActionLabel { return Object.hasOwn(actionIcons, label); }

export function el<K extends keyof HTMLElementTagNameMap>(tag: K, label?: string, className?: string): HTMLElementTagNameMap[K] {
  const node = document.createElement(tag);
  if (label !== undefined) node.textContent = label;
  if (className) node.className = className;
  return node;
}
export function button(label: string, action: () => void, className?: string): HTMLButtonElement {
  const node = el("button", label, className);
  node.type = "button"; node.addEventListener("click", action); return node;
}
export function actionButton(label: ActionLabel, action: () => void, className?: string): HTMLButtonElement {
  const node = button(label, action, className);
  node.classList.add("has-action-icon");
  if (label === "Actions") node.append(icon(actionIcons[label]));
  else node.prepend(icon(actionIcons[label]));
  return node;
}
export function labelled(label: string, control: HTMLElement): HTMLElement {
  const wrapper = el("label", undefined, "field-label"); wrapper.append(el("span", label), control); return wrapper;
}
export function errorMessage(error: unknown): string { return error instanceof Error ? error.message : "The operation failed. Please reload and try again."; }
const choiceServices = new WeakMap<Document, DialogService>();
/** Confirmation decision identity is independent of its visible captions. */
export async function confirmChoice(options: {title:string;message:string;accept:string;cancel:string;signal?:AbortSignal;service?:DialogService}): Promise<boolean> {
  if(options.signal?.aborted)return false;
  let dialogs=options.service??choiceServices.get(document);
  if(dialogs?.active() && !dialogs.active()!.isConnected){dialogs.dispose();dialogs=undefined;}
  if(!dialogs){dialogs=new DialogService(error=>{queueMicrotask(()=>{throw error;});});choiceServices.set(document,dialogs);}
  const handle=dialogs.confirm({title:options.title,message:options.message,confirmLabel:options.accept,cancelLabel:options.cancel,...(options.signal?{signal:options.signal}:{})});
  handle.dialog.querySelector<HTMLButtonElement>('.message-dialog-footer button.primary')?.focus();
  return await handle.result==='confirm';
}
/** Standalone choices share a document stack; app callers supply their owned service explicitly. */
export async function choose(title: string, message: string, choices: readonly string[], signal?: AbortSignal, service?: DialogService): Promise<string | null> {
  if (signal?.aborted) return null;
  let dialogs = service ?? choiceServices.get(document);
  if (dialogs?.active() && !dialogs.active()!.isConnected) { dialogs.dispose(); dialogs = undefined; }
  if (!dialogs) {
    dialogs = new DialogService(error => { queueMicrotask(() => { throw error; }); });
    choiceServices.set(document, dialogs);
  }
  const actions = el("div", undefined, "actions");
  return new Promise(resolve => {
    let settled = false, handle: DialogHandle | undefined;
    const finish = (value: string | null): void => {
      if (settled) return;
      settled = true; handle?.close(); resolve(value);
    };
    for (const [index, choice] of choices.entries()) {
      const action = (): void => finish(choice);
      actions.append(isActionLabel(choice) ? actionButton(choice, action, index === 0 ? "primary" : "") : button(choice, action, index === 0 ? "primary" : ""));
    }
    handle = dialogs.open({ title, body: message, size: "sm", footer: actions,
      dismiss: () => { finish(null); }, onClose: () => finish(null), ...(signal ? { signal } : {}) });
    // Choices preserve their primary-action keyboard path; the generic shell defaults to its close control.
    if (handle.dialog.isConnected) actions.querySelector<HTMLButtonElement>("button")?.focus();
  });
}
