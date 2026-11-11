/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
import { createHotkeyController, type HotkeyBinding } from "@erpforall/elang";
import { createButtonControl } from './button-control.js';
import { knownMenuIcon } from './icons.js';
import type { ButtonNode, CancelNode } from './metadata.js';
import type { MessageServices } from './messages.js';
import { el } from './dom.js';
import { ClientError } from './transport.js';

const disposers = new WeakMap<HTMLElement,()=>void>();
export function disposeViewButton(control:HTMLElement):void { disposers.get(control)?.(); disposers.delete(control); }
/** Presentation for one admitted control; the owning pipeline still decides when it can run. */
export function viewButton(node:ButtonNode|CancelNode, env:{activate():void;className?:string;model:string;debug?:boolean;messages?:MessageServices;report(error:unknown):void}):HTMLButtonElement {
 const details=node.details;
 const icon=details?.icon===undefined?undefined:knownMenuIcon(details.icon);
 if(icon===null)throw new ClientError('validation',`Unknown button icon: ${details?.icon}.`);
 const control=createButtonControl({label:node.label,rank:node.variant,...(details?{size:details.size,circle:details.circle,...(icon?{icon}:{}),...(details.title!==undefined?{title:details.title}:{})}:{}),className:[env.className,...(details?.classes??[])].filter(Boolean).join(' '),...(node.kind==='cancel_button'||node.target?.kind!=='inert'?{action:env.activate}:{})});
 if(details?.hotkey)control.dataset['hotkey']=details.hotkey;
 let dead=false, detach:(()=>void)|undefined, observer:MutationObserver|undefined;
 const detailed=details?.help!==undefined || env.debug===true;
 if(env.messages && (detailed || details?.title)) {
  const attach=():void=>{
   if(dead || !control.isConnected)return;
   observer?.disconnect();observer=undefined;
   const options=detailed?{content:()=>{
    const box=el('div',undefined,'button-help-tooltip');
    if(node.label)box.append(el('strong',node.label));
    if(details?.title)box.append(el('p',details.title));
    if(details?.help)box.append(el('p',details.help));
    if(env.debug)box.append(el('p',`Model: ${env.model}; control: ${node.kind==='cancel_button'?'cancel':node.name}`));
    return box;
   }}:{text:details!.title!};
   detach=env.messages!.tooltips.attach(control,options);
  };
  queueMicrotask(()=>{
   if(dead)return;
   try {if(control.isConnected)attach();else {observer=new MutationObserver(()=>{try{attach();}catch(error){env.report(error);}});observer.observe(control.ownerDocument.documentElement,{subtree:true,childList:true});}}
   catch(error){env.report(error);}
  });
 }
 const focus=():void=>{control.dispatchEvent(new MouseEvent('mouseenter'));};
 const blur=():void=>{control.dispatchEvent(new MouseEvent('mouseleave'));};
 control.addEventListener('focus',focus);control.addEventListener('blur',blur);
 disposers.set(control,()=>{if(dead)return;dead=true;observer?.disconnect();detach?.();control.removeEventListener('focus',focus);control.removeEventListener('blur',blur);});
 return control;
}

/** Owner-local quiet windows survive a form re-render and never schedule a trailing action. */
export class ButtonDebouncer {
 private dead=false;
 private readonly timers=new Map<object,ReturnType<typeof setTimeout>>();
 run(key:object,delay:number,action:()=>void):void {
  if(this.dead)throw new ClientError('validation','Button execution owner is disposed.');
  if(!Number.isSafeInteger(delay)||delay<0||delay>60000)throw new ClientError('validation','Button debounce must be an integer from 0 to 60000.');
  if(delay===0){action();return;}
  const prior=this.timers.get(key);clearTimeout(prior);
  this.timers.set(key,setTimeout(()=>this.timers.delete(key),delay));
  if(prior===undefined)action();
 }
 dispose():void{if(this.dead)return;this.dead=true;for(const timer of this.timers.values())clearTimeout(timer);this.timers.clear();}
}

/** Compatibility adapter: all view owners share runtime arbitration. */
export class ButtonHotkeys {
 private readonly controller: ReturnType<typeof createHotkeyController>;
 constructor(controls:()=>readonly HTMLButtonElement[], env:{blocked():boolean;report(error:unknown):void}) {
  this.controller=createHotkeyController(document.body,{blocked:env.blocked,report:env.report,bindings:()=>controls().filter(control=>Boolean(control.dataset['hotkey'])).map((target):HotkeyBinding=>({kind:'dom',chord:`alt+${target.dataset['hotkey']!}`,target}))});
 }
 refresh():void { this.controller.refresh(); }
 dispose():void { this.controller.dispose(); }
}
