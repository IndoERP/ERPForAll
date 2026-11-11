/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
import { reportHotkeyError } from "../../../../../web/src/runtime/hotkeys.js";
import { button, el, errorMessage } from './dom.js';
import { DialogService, type DialogHandle } from './dialog-service.js';
import { NotificationService, type NotificationOptions } from './notifications.js';
import { BlockUi } from './block-ui.js';
import { NetworkStatus } from './network-status.js';
import { EffectService } from './effects.js';
import { PopoverService } from './popover.js';
import { TooltipService } from './tooltip.js';
import type { OnchangeWarning } from './record.js';
import { ClientError } from './transport.js';

/** One application's transient message ownership. Server authorization is never carried by this handle. */
export class MessageServices {
  readonly notifications: NotificationService;
  readonly dialogs: DialogService;
  readonly block: BlockUi;
  readonly network: NetworkStatus;
  readonly effects: EffectService;
  readonly popovers: PopoverService;
  readonly tooltips: TooltipService;
  private legacyNotice: (()=>void)|undefined;
  private dead = false;
  private disposing = false;
  private readonly teardownErrors: unknown[] = [];
  constructor(host: HTMLElement, report: (error:unknown)=>void) {
    const scopedReport = (error: unknown): void => { if (this.disposing) this.teardownErrors.push(error); else if (this.dead) reportHotkeyError(error); else report(error); };
    this.notifications = new NotificationService(host, scopedReport);
    this.block = new BlockUi(this.notifications.node);
    this.network = new NetworkStatus(host);
    this.effects = new EffectService(this.notifications.node, this.notifications, scopedReport);
    this.popovers = new PopoverService(host, scopedReport);
    this.tooltips = new TooltipService(this.popovers, scopedReport);
    this.dialogs = new DialogService(scopedReport, dialog => {
      if (!this.dead) (dialog ?? host).append(this.notifications.node);
    });
  }
  notice(message: string, options: NotificationOptions = {}): void {
    if (this.dead) throw new ClientError('validation','Message services are disposed.');
    if (!message) { this.legacyNotice?.(); this.legacyNotice=undefined; return; }
    this.legacyNotice=this.notifications.add(message,options);
  }
  warning(warning: OnchangeWarning | undefined): void {
    if (!warning) return;
    if (this.dead) throw new ClientError('validation','Message services are disposed.');
    if (warning.type === 'dialog') this.dialogs.alert({title:warning.title,message:warning.message,contentClass:'message-warning'});
    else this.notifications.add(warning.message,{title:warning.title,level:'warning',...(warning.className === undefined ? {} : {className:warning.className}),...(warning.sticky === undefined ? {} : {sticky:warning.sticky})});
  }
  /** A navigation failure decision. Uncertain writes keep their stricter record guard. */
  async saveFailure(error:unknown, options:{discard:()=>void|Promise<void>;redirect?:()=>void|Promise<void>}):Promise<boolean> {
    if (error instanceof ClientError && (error.kind==='ambiguous' || error.kind==='session')) throw error;
    if (this.dead) throw new ClientError('validation','Message services are disposed.');
    return new Promise((resolve,reject)=>{
      let handle:DialogHandle|undefined, settled=false, pending=false;
      const finish=(leave:boolean):void=>{if(settled)return;settled=true;handle?.close();resolve(leave);};
      const footer=el('div',undefined,'actions');
      const perform=(job:()=>void|Promise<void>,leave:boolean):void=>{
        if(pending||settled||this.dead)return;pending=true;
        for(const control of footer.querySelectorAll('button'))control.disabled=true;
        void Promise.resolve().then(() => { if (!settled && !this.dead) return job(); }).then(()=>finish(leave),failure=>{if(!settled){settled=true;handle?.dispose();reject(failure);}}).finally(()=>{
          pending=false;if(!settled)for(const control of footer.querySelectorAll('button'))control.disabled=false;
        });
      };
      footer.append(button('Remain',()=>{if(!pending)finish(false);},'primary'),button('Discard',()=>perform(options.discard,true)));
      if(options.redirect)footer.append(button('Open related record',()=>perform(options.redirect!,false)));
      handle=this.dialogs.open({title:'Changes could not be saved',body:errorMessage(error),size:'sm',footer,dismiss:()=>{if(pending)return false;finish(false);return true;},onClose:()=>finish(false)});
    });
  }
  navigation():void { this.popovers.navigation(); }
  dispose():void {
    if(this.dead)return;this.dead=true;this.disposing=true;
    const errors = this.teardownErrors;
    for (const service of [this.tooltips, this.popovers, this.effects, this.dialogs, this.block, this.network, this.notifications]) {
      try { service.dispose(); } catch (error) { errors.push(error); }
    }
    this.disposing=false;
    if (errors.length) throw new AggregateError(errors, "Message service cleanup failed.");
  }
}
