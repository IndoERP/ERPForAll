/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
import { afterEach, beforeEach, expect, test, vi } from 'vitest';
import { MessageServices } from '../src/client/messages.js';
import { ClientError } from '../src/client/transport.js';
let messages:MessageServices|undefined;
const showDescriptor=Object.getOwnPropertyDescriptor(HTMLDialogElement.prototype,'showModal');
const closeDescriptor=Object.getOwnPropertyDescriptor(HTMLDialogElement.prototype,'close');
beforeEach(()=>{
 Object.defineProperty(HTMLDialogElement.prototype,'showModal',{configurable:true,value(this:HTMLDialogElement){this.open=true;}});
 Object.defineProperty(HTMLDialogElement.prototype,'close',{configurable:true,value(this:HTMLDialogElement){this.open=false;}});
});
afterEach(()=>{
 messages?.dispose();messages=undefined;vi.restoreAllMocks();document.body.replaceChildren();
 if(showDescriptor)Object.defineProperty(HTMLDialogElement.prototype,'showModal',showDescriptor);else Reflect.deleteProperty(HTMLDialogElement.prototype,'showModal');
 if(closeDescriptor)Object.defineProperty(HTMLDialogElement.prototype,'close',closeDescriptor);else Reflect.deleteProperty(HTMLDialogElement.prototype,'close');
});
const setup=()=>{const root=document.createElement('div');document.body.append(root);const report=vi.fn();messages=new MessageServices(root,report);return {root,report,service:messages};};
/// (partial: presentation of current warning title/message/type; class/sticky await)
test('structured warning selects dialog or notification and never rejects updated values',()=>{
 const {service}=setup();service.warning({title:'Check value',message:'Changed',type:'dialog'});
 expect(document.querySelector('dialog h2')?.textContent).toBe('Check value');
 expect(document.querySelector('dialog')?.textContent).toContain('Changed');
 service.dialogs.active()!.querySelector<HTMLButtonElement>('.message-dialog-footer button')!.click();
 service.warning({title:'Check value',message:'Adjusted',type:'notification'});
 expect(document.querySelector('.client-notification[data-level="warning"]')?.textContent).toContain('Adjusted');
});
test('failed navigation save offers remain, discard and conditional trusted redirect with serialized callbacks',async()=>{
 const {service}=setup(), discard=vi.fn(async()=>{}), redirect=vi.fn(async()=>{});
 const remain=service.saveFailure(new ClientError('validation','Check the name'),{discard});
 expect(document.querySelector('dialog')?.textContent).not.toContain('Open related record');
 const click=(label:string)=>[...document.querySelectorAll<HTMLButtonElement>('dialog button')].find(b=>b.textContent===label)!.click();
 click('Remain');await expect(remain).resolves.toBe(false);expect(discard).not.toHaveBeenCalled();
 const leave=service.saveFailure(new ClientError('validation','Check the name'),{discard});click('Discard');await expect(leave).resolves.toBe(true);expect(discard).toHaveBeenCalledTimes(1);
 const redirected=service.saveFailure(new ClientError('validation','Related record'),{discard,redirect});click('Open related record');await expect(redirected).resolves.toBe(false);expect(redirect).toHaveBeenCalledTimes(1);
 const uncertain=service.saveFailure(new ClientError('ambiguous','Check whether saved'),{discard});await expect(uncertain).rejects.toMatchObject({kind:'ambiguous',message:'Check whether saved'});expect(discard).toHaveBeenCalledTimes(1);
});
test('notifications follow active modal and teardown prevents late resurrection',()=>{
 const {root,service}=setup();const parent=service.dialogs.open({title:'Parent',body:'Body'});service.notice('Inside');
 expect(parent.dialog.querySelector('.client-notification')?.textContent).toContain('Inside');
 parent.close();expect(root.querySelector('.client-notifications')).not.toBeNull();service.dispose();
 expect(document.querySelector('.client-notifications')).toBeNull();expect(()=>service.notice('Late')).toThrowError(expect.objectContaining({kind:'validation'}));
});

test('throwing overlay cleanup cannot leave other protected message scopes alive',()=>{
 const {root,service}=setup();const anchor=document.createElement('button');anchor.textContent='Anchor';root.append(anchor);
 service.notice('Protected notice');service.block.begin();service.dialogs.open({title:'Protected dialog',body:'Private content'});
 service.popovers.open(anchor,document.createElement('div'),{onClose:()=>{throw new Error('cleanup failure');}});
 expect(()=>service.dispose()).toThrow('Message service cleanup failed.');
 expect(document.querySelector('dialog')).toBeNull();expect(document.querySelector('.client-notifications')).toBeNull();expect(document.querySelector('.client-block-ui')).toBeNull();
});

import '../src/client/fields.js';
import { fieldWidgets } from '../src/client/registry.js';
import { RpcClient } from '../src/client/transport.js';
test('editable relation fields open the owned readonly picker and dispose its scope',async()=>{
 const {root,service}=setup(),change=vi.fn();
 const rpc=new RpcClient(async()=>new Response('[{"id":8,"display_name":"Chosen record"}]',{headers:{'Content-Type':'application/json'}}),()=>{});
 const handle=fieldWidgets.get('many2one').render({field:{name:'partner_id',kind:'many2one',label:'Partner',readonly:false,required:false,selection:null,relation:'res.partner',relationField:null},value:false,rpc,context:{},messages:service,change,run:job=>{void job();},report:()=>{}});
 root.append(handle.node);const more=handle.node.querySelector<HTMLButtonElement>('.relation-search-more');expect(more).not.toBeNull();
 // An icon control inside the field: no visible text, the accessible name and tooltip still name the picker.
 expect(more!.querySelector('svg')).not.toBeNull();expect(more!.textContent).toBe('');expect(more!.getAttribute('aria-label')).toBe('Search more Partner');expect(more!.title).toBe('Search more Partner…');
 more!.click();
 await vi.waitFor(()=>expect(document.querySelector('.select-create-results')?.textContent).toContain('Chosen record'));
 handle.dispose();expect(document.querySelector('dialog')).toBeNull();expect(change).not.toHaveBeenCalled();
});
