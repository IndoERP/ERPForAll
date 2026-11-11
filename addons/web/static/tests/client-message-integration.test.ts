/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
import { expect, test } from 'vitest';
import { RecordState } from '../src/client/record.js';
import { RpcClient } from '../src/client/transport.js';
import type { Field } from '../src/client/metadata.js';
const fields: Field[] = [{ name:'name',kind:'char',label:'Name',readonly:false,required:true,selection:null,relation:null,relationField:null }];
const json = (value:unknown):Response => new Response(JSON.stringify(value),{headers:{'Content-Type':'application/json'}});
/// (partial: structured current-producer warning and value application; class/sticky await)
test('onchange retains title and kind, applies values, and clears an old warning', async()=>{
  let warned = true;
  const state = new RecordState(new RpcClient(async path=>String(path).endsWith('onchange') ? json({value:{name:'Adjusted'},...(warned?{warning:{title:'Check this',message:'Value adjusted',type:'dialog'}}:{})}) : json({name:'Draft'}),()=>{}),'test.note',fields,{});
  await state.load();
  expect(state.values['name']).toBe('Adjusted');
  expect(state.warning).toBe('Value adjusted');
  expect(state.warningDetails).toEqual({title:'Check this',message:'Value adjusted',type:'dialog'});
  warned=false; await state.onchange('name');
  expect(state.warningDetails).toBeUndefined(); expect(state.warning).toBe('');
});
test('malformed warning metadata is refused by kind and message',async()=>{
  const state = new RecordState(new RpcClient(async path=>String(path).endsWith('onchange')?json({value:{},warning:{title:42,message:'Do not coerce',type:'dialog'}}):json({name:'Draft'}),()=>{}),'test.note',fields,{});
  await expect(state.load()).rejects.toMatchObject({kind:'server',message:'Invalid onchange warning.'});
});

import { vi, afterEach } from 'vitest';
import '../src/client/fields.js';
import { RecordForm } from '../src/client/record-form.js';
import { describeViews } from '../src/client/metadata.js';
import { MessageServices } from '../src/client/messages.js';
import { ErrorDialog } from '../src/client/error-dialog.js';
afterEach(()=>{document.body.replaceChildren();vi.restoreAllMocks();});
/// (partial: validation tracking and silent notify hook; mounted danger presentation is app-owned)
test('validation tracks invalid fields and notifies only when requested',()=>{
  const rpc = new RpcClient(async()=>json({}),()=>{}), state = new RecordState(rpc,'test.note',fields,{});
  state.values={name:''};
  const description=describeViews({wire_major:1,registry_version:'r1',description_version:'d1',model:'test.note',fields:{'test.note':{name:{type:'char',label:'Name',required:true,readonly:false,relation:null,relation_field:null,selection:null}}},notices:[],views:[{identity:{kind:'stored',id:1},type:'form',schema_major:1,renderer:'web.form',root:0,nodes:[{kind:'form',record_mode:'record',children:[1]},{kind:'field',force_save:false,field:{model:'test.note',name:'name'},widget:'char',options:{},modifiers:{readonly:null,required:null,invisible:null},label:'Name'}]}]},'test.note','r1');
  const form=document.createElement('form');document.body.append(form);
  const notify=vi.fn(), surface=new RecordForm({form,state,view:description.views.get('form')!,rpc,context:{},run:()=>{},report:()=>{},notify,openRecord:async()=>{},change:()=>{},button:()=>{}});
  surface.render(); expect(surface.valid(false)).toBe(false); expect(notify).not.toHaveBeenCalled();
  expect(form.querySelector('input')?.getAttribute('aria-invalid')).toBe('true');
  expect(surface.valid()).toBe(false); expect(notify).toHaveBeenCalledExactlyOnceWith('Complete the required fields before saving.');
  form.querySelector('input')!.value='Valid';expect(surface.valid()).toBe(true);expect(notify).toHaveBeenCalledTimes(1);
  expect(form.querySelector('input')?.hasAttribute('aria-invalid')).toBe(false);surface.dispose();
});

import { NetworkStatus } from '../src/client/network-status.js';
/// (partial: transport observer contract; mounted app acceptance is in client-app.test.ts)
test('transport loss and actual safe-read recovery drive offline, sync and error without replaying writes',async()=>{
  const host=document.createElement('div');document.body.append(host);const status=new NetworkStatus(host);
  let calls=0, finish:((value:Response)=>void)|undefined;
  const rpc=new RpcClient(async()=>{calls++;if(calls===1)throw new TypeError('lost');return new Promise<Response>(resolve=>{finish=resolve;});},()=>{},undefined,undefined,(event,options)=>status.observe(event,options));
  await expect(rpc.call('test.note','web_save',{}, {write:true})).rejects.toMatchObject({kind:'ambiguous',message:'The connection was interrupted. The change may have been saved. Check the record before trying again.'});
  expect(calls).toBe(1);expect(status.node.dataset['state']).toBe('offline');
  const failedRead=rpc.call('test.note','web_read',{});expect(status.node.dataset['state']).toBe('syncing');
  finish!(new Response('{}',{status:503,headers:{'Content-Type':'application/json'}}));await expect(failedRead).rejects.toMatchObject({kind:'server'});expect(status.node.dataset['state']).toBe('error');
  const read=rpc.call('test.note','web_read',{});expect(status.node.dataset['state']).toBe('syncing');finish!(json([]));await read;
  expect(status.node.dataset['state']).toBe('online');expect(calls).toBe(3);status.dispose();
});

test.each(['dialog', undefined])('onchange warning dialog at the 8192-byte bound displays applied values (%s)', async kind=>{
  Object.defineProperty(HTMLDialogElement.prototype,'showModal',{configurable:true,value(this:HTMLDialogElement){this.open=true;}});
  Object.defineProperty(HTMLDialogElement.prototype,'close',{configurable:true,value(this:HTMLDialogElement){this.open=false;}});
  const host=document.createElement('div');document.body.append(host);const report=vi.fn(),messages=new MessageServices(host,report);
  const body='<b>'+ 'x'.repeat(8185)+'</b>';
  const rpc=new RpcClient(async path=>String(path).endsWith('onchange')?json({value:{name:'Adjusted at boundary'},warning:{message:body,...(kind?{type:kind}:{})}}):json([{id:7,name:'Before'}]),()=>{});
  const state=new RecordState(rpc,'test.note',fields,{});
  try {
    await state.load(7);state.change('name','Entered');await state.onchange('name');
    messages.warning(state.warningDetails);
    expect(state.values['name']).toBe('Adjusted at boundary');
    expect(state.warningDetails).toEqual({title:'Warning',message:body,type:'dialog'});
    expect(document.querySelector('.message-dialog-body')?.textContent).toBe(body);
    expect(document.querySelector('.message-dialog-body b')).toBeNull();expect(report).not.toHaveBeenCalled();
  } finally {messages.dispose();}
});

test.each(['dialog', undefined])('oversized onchange warning refuses explicitly without losing applied values (%s)', async kind=>{
  Object.defineProperty(HTMLDialogElement.prototype,'showModal',{configurable:true,value(this:HTMLDialogElement){this.open=true;}});
  Object.defineProperty(HTMLDialogElement.prototype,'close',{configurable:true,value(this:HTMLDialogElement){this.open=false;}});
  const host=document.createElement('div');document.body.append(host);const errors=new ErrorDialog(),reported:unknown[]=[];
  const report=(error:unknown):void=>{reported.push(error);errors.show(error);};
  const messages=new MessageServices(host,report);
  let oversized=false;
  const rpc=new RpcClient(async path=>String(path).endsWith('onchange')?json({value:{name:oversized?'Adjusted despite refused warning':'First adjustment'},warning:{message:oversized?'x'.repeat(8193):'Earlier warning',...(kind?{type:kind}:{})}}):json([{id:7,name:'Before'}]),()=>{});
  const state=new RecordState(rpc,'test.note',fields,{});
  try {
    await state.load(7);await state.onchange('name');expect(state.warning).toBe('Earlier warning');oversized=true;
    try {await state.onchange('name');messages.warning(state.warningDetails);} catch(error){report(error);}
    expect(reported).toHaveLength(1);
    expect(reported[0]).toMatchObject({kind:'server',message:'Onchange warning message must be text of at most 8192 bytes.'});
    expect(state.values['name']).toBe('Adjusted despite refused warning');expect(state.dirty).toBe(true);
    expect(state.warningDetails).toBeUndefined();expect(state.warning).toBe('');
    expect(document.querySelector('.client-error-dialog')?.textContent).toContain('Onchange warning message must be text of at most 8192 bytes.');
  } finally {errors.dispose();messages.dispose();}
});
