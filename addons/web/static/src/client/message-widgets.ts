/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
import { Component, type TemplateModule } from "@erpforall/elang";
import { button, el } from "./dom.js";
import { ClientError } from "./transport.js";

export type MessageLevel = "info" | "success" | "warning" | "danger";
export interface RibbonProps { text: string; tooltip?: string; background?: MessageLevel; backgroundClass?: string }
export interface PermissionAlertProps { permission: NotificationPermission | "unsupported"; open(): void }
const LEVELS: readonly string[] = ["info", "success", "warning", "danger"];
function boundedText(value: unknown, label: string, bytes: number): asserts value is string {
  if (typeof value !== "string" || new TextEncoder().encode(value).length > bytes) throw new ClientError("validation", `${label} must be a string of at most ${bytes} bytes.`);
}
/** Display-only ribbon; the server view declaration is added. */
export function renderRibbon(props: RibbonProps): HTMLElement {
  if (props === null || typeof props !== "object" || Object.keys(props).some(key => !["text", "tooltip", "background", "backgroundClass"].includes(key))) throw new ClientError("validation", "Invalid ribbon declaration.");
  boundedText(props.text, "Ribbon text", 4096);
  if (props.tooltip !== undefined) boundedText(props.tooltip, "Ribbon tooltip", 4096);
  if (props.background !== undefined && !LEVELS.includes(props.background)) throw new ClientError("validation", "Invalid ribbon background.");
  if (props.backgroundClass !== undefined && (typeof props.backgroundClass !== "string" || !LEVELS.map(level => `bg-${level}`).includes(props.backgroundClass))) throw new ClientError("validation", `Unsupported ribbon background class: ${String(props.backgroundClass)}.`);
  if (props.background !== undefined && props.backgroundClass !== undefined && props.backgroundClass !== `bg-${props.background}`) throw new ClientError("validation", "Ribbon background and background class disagree.");
  const level = props.background ?? (props.backgroundClass === undefined ? "info" : props.backgroundClass.slice(3));
  const ribbon = el("div", props.text, "message-ribbon"); ribbon.dataset["level"] = level;
  if (props.tooltip !== undefined) ribbon.title = props.tooltip;
  if (Array.from(props.text).length > 20) ribbon.classList.add("message-ribbon-compact");
  return ribbon;
}
/** Presentation only: never requests or changes the browser's notification permission. */
export function renderNotificationPermissionAlert(props: PermissionAlertProps): HTMLElement {
  if (props === null || typeof props !== "object" || Object.keys(props).some(key => !["permission", "open"].includes(key))) throw new ClientError("validation", "Invalid notification permission declaration.");
  if (!["denied", "granted", "default", "unsupported"].includes(props.permission)) throw new ClientError("validation", "Invalid notification permission state.");
  if (typeof props.open !== "function") throw new ClientError("validation", "Notification permission guidance requires an open callback.");
  const host = el("div", undefined, "notification-permission-alert");
  host.hidden = props.permission !== "denied";
  if (props.permission === "denied") host.append(button("Browser notifications are blocked. Learn how to enable them.", props.open));
  return host;
}
/** Invoke only for a successfully loaded empty view; loading and failure have their own surfaces. */
export function noContentHelp(title?: string, description?: string): HTMLElement {
  if (title !== undefined) boundedText(title, "No-content title", 4096);
  if (description !== undefined) boundedText(description, "No-content description", 8192);
  const host = el("div", undefined, "empty-state message-no-content"); host.setAttribute("role", "alert");
  host.append(el("h2", title === undefined ? "No records yet" : title), el("p", description === undefined ? "Create a record to begin working here." : description));
  return host;
}
export class Ribbon extends Component<RibbonProps> { static override template = "web.MessageRibbon"; }
export class NotificationPermissionAlert extends Component<PermissionAlertProps> { static override template = "web.NotificationPermissionAlert"; }
/** Trusted TypeScript render functions, usable by component fixtures and addon mounts without runtime XML compilation. */
export const messageWidgetTemplates: readonly TemplateModule[] = [
  { name: "web.MessageRibbon", render: self => renderRibbon((self as Ribbon).props) },
  { name: "web.NotificationPermissionAlert", render: self => renderNotificationPermissionAlert((self as NotificationPermissionAlert).props) },
];

/** Record-view permission guidance; all observation belongs to this disposable mount. */
export function notificationPermissionWidget(env: {
  open(): void; report(error: unknown): void;
}): {node: HTMLElement; dispose():void} {
  const node=el('div');let dead=false, current:PermissionAlertProps['permission']|undefined;
  let status:PermissionStatus|undefined;
  const update=():void=>{
    if(dead)return;
    const permission=typeof Notification==='undefined'?'unsupported':Notification.permission;
    if(permission===current)return;
    const next=renderNotificationPermissionAlert({permission,open:env.open});
    current=permission;node.replaceChildren(next);
  };
  const refresh=():void=>{try{update();}catch(error){env.report(error);}};
  update();window.addEventListener('focus',refresh);document.addEventListener('visibilitychange',refresh);
  if(typeof Notification!=='undefined' && navigator.permissions?.query) {
    void navigator.permissions.query({name:'notifications'}).then(value=>{if(dead)return;status=value;status.addEventListener('change',refresh);refresh();},error=>{if(!dead)env.report(error);});
  }
  return {node,dispose:()=>{if(dead)return;dead=true;window.removeEventListener('focus',refresh);document.removeEventListener('visibilitychange',refresh);status?.removeEventListener('change',refresh);}};
}
