/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
import { afterEach, expect, test, vi } from 'vitest';
import { describeViews, type ButtonNode } from '../src/client/metadata.js';
import { runObjectButton, type ButtonEnvironment } from '../src/client/buttons.js';
import { isFalsy } from '../src/client/app.js';

const mods = {readonly:null,required:null,invisible:null};
const details = (extra: Record<string,unknown> = {}) => ({size:'normal',circle:false,classes:[],debounce_ms:0,block_ui:false,...extra});
const objectButton = (extra: Record<string,unknown> = {}) => ({kind:'button',name:'submit',button_type:'object',label:'Submit',variant:'secondary',confirm:null,modifiers:mods,...extra});
function decode(children: Record<string,unknown>[], header = true): unknown[] {
 const nodes = header ? [{kind:'form',record_mode:'record',children:[1]},{kind:'header',children:children.map((_,i)=>i+2)},...children] : [{kind:'form',record_mode:'record',children:children.map((_,i)=>i+1)},...children];
 const description=describeViews({wire_major:1,registry_version:'r1',description_version:'d1',model:'test.note',fields:{'test.note':{}},notices:[],views:[{identity:{kind:'stored',id:1},type:'form',schema_major:1,renderer:'web.form',root:0,nodes}]},'test.note','r1');
 const root = description.views.get('form')!.nodes[0] as {children: {children:unknown[]}[]};
 return header ? root.children[0]!.children : root.children;
}
afterEach(()=>{vi.useRealTimers();document.body.replaceChildren();});

/// (partial: consumer preserves seven served ranks and independent presentation options)
test('served ranks and presentation details survive strict consumer admission',()=>{
 for(const variant of ['primary','secondary','link','success','info','warning','danger','unstyled']){
  const [node]=decode([objectButton({variant,details:details({size:'large',circle:true,classes:['addon-highlight'],icon:'lucide:refresh-cw',title:'Check',help:'Read this'})})]);
  expect(node).toMatchObject({variant,details:{size:'large',circle:true,classes:['addon-highlight'],icon:'lucide:refresh-cw',title:'Check',help:'Read this'}});
 }
});
/// (partial: client discriminates served action/URL/inert identities without RPC method names)
test('typed non-object targets remain distinct after decoding',()=>{
 for(const target of [{kind:'action',id:7},{kind:'url',href:'https://example.test'},{kind:'inert'}]){
  const [node]=decode([{kind:'action_button',target,label:'Explicit target',variant:'secondary',confirm:null,modifiers:mods}]);
  expect(node).toMatchObject({kind:'button',target,label:'Explicit target'});
 }
});
/// (partial: served ribbon declaration; rendering and native producer tested separately)
/// (partial: served permission alert carries no server permission state)
test('record display leaves are separate from service widgets',()=>{
 expect(decode([{kind:'ribbon',text:'Ready',tooltip:'Display only',background:'success'},{kind:'notification_alert'}],false)).toEqual([{kind:'ribbon',text:'Ready',tooltip:'Display only',background:'success'},{kind:'notification_alert'}]);
});
test.each([
 [{size:'huge'},'Invalid button size.'],
 [{hotkey:'ctrl+x'},'Invalid button hotkey.'],
 [{debounce_ms:-1},'Button debounce must be an integer from 0 to 60000.'],
 [{debounce_ms:60_001},'Button debounce must be an integer from 0 to 60000.'],
 [{block_ui:'true'},'Button block_ui must be boolean.'],
 [{icon:'lucide:no_such_icon'},'Unknown button icon: lucide:no_such_icon.'],
])('malformed button details %j refuse explicitly',(extra,message)=>{
 expect(()=>decode([objectButton({details:details(extra)})])).toThrowError(expect.objectContaining({kind:'server',message}));
});

function env(extra:Record<string,unknown>={}) {return {model:'test.note',context:{lang:'en_US',allowed_company_ids:[1]},id:()=>7,dirty:()=>false,save:vi.fn(async()=>{}),call:vi.fn(async()=>null),reload:vi.fn(async()=>{}),confirm:vi.fn(async()=>true),...extra};}
/// (partial: execution receives all declared confirmation labels)
test('confirmation options precede execution and cancellation calls nothing',async()=>{
 const node={kind:'button',name:'submit',label:'Submit',variant:'secondary',confirm:'Proceed?',invisible:null,details:details({confirm_title:'Review',confirm_label:'Approve',cancel_label:'Keep editing'})} as unknown as ButtonNode;
 const owner=env();await runObjectButton(node,owner as ButtonEnvironment);
 expect(owner.confirm).toHaveBeenCalledWith('Review','Proceed?','Approve','Keep editing');expect(owner.call).toHaveBeenCalledTimes(1);
 owner.confirm.mockResolvedValue(false);owner.call.mockClear();await runObjectButton(node,owner as ButtonEnvironment);expect(owner.call).not.toHaveBeenCalled();
});
test('title-only buttons use their admitted accessible identity for default confirmation presentation',async()=>{
 const owner=env();
 const node={kind:'button',name:'submit',label:'',variant:'secondary',confirm:'Proceed?',invisible:null,details:details({title:'Archive record'})} as unknown as ButtonNode;
 await runObjectButton(node,owner as ButtonEnvironment);
 expect(owner.confirm).toHaveBeenCalledWith('Archive record','Proceed?','Archive record');
});
test('button context cannot replace saved active identities while legacy context remains intact',async()=>{
 const doc=(key:string)=>({schema:1,nodes:[{kind:'literal',value:{kind:'str',value:key}},{kind:'name',name:'count'},{kind:'dict',items:[[0,1]]}]});
 for(const key of ['active_model','active_id','active_ids']){
  const owner=env({fields:[{name:'count',kind:'integer'}],values:()=>({count:99})});
  const node={kind:'button',name:'submit',label:'Submit',variant:'secondary',confirm:null,invisible:null,details:{...details(),context:decodeAst(parseBoundedJson(JSON.stringify(doc(key))))}} as unknown as ButtonNode;
  await expect(runObjectButton(node,owner as ButtonEnvironment)).rejects.toMatchObject({kind:'validation',message:`Button context cannot replace protected key '${key}'.`});expect(owner.call).not.toHaveBeenCalled();
 }
 const context={active_model:'parent.model',active_id:123,active_ids:[123]},owner=env({context});
 await runObjectButton({kind:'button',name:'submit',label:'Submit',variant:'secondary',confirm:null,invisible:null},owner as ButtonEnvironment);
 expect(owner.call).toHaveBeenCalledWith('test.note','submit',{ids:[7],context});
});
/// (partial: an action target calls its action owner after saving and never invokes a model method)
test('typed action and inert targets cannot become object RPC methods',async()=>{
 const action=vi.fn(async()=>{}),owner=env({action});
 const base={kind:'button',name:'action:7',label:'Open',variant:'secondary',confirm:null,invisible:null};
 await runObjectButton({...base,target:{kind:'action',id:7}} as unknown as ButtonNode,owner as ButtonEnvironment);
 expect(action).toHaveBeenCalledWith(7,owner.context);expect(owner.call).not.toHaveBeenCalled();
 await runObjectButton({...base,target:{kind:'inert'}} as unknown as ButtonNode,owner as ButtonEnvironment);
 expect(owner.call).not.toHaveBeenCalled();expect(action).toHaveBeenCalledTimes(1);
});
/// (partial: declared UI block is released on a failing invocation)
test('declared execution block balances success and failure',async()=>{
 const release=vi.fn(),block=vi.fn(()=>release),failure=new Error('method failed');
 const node={kind:'button',name:'submit',label:'Submit',variant:'secondary',confirm:null,invisible:null,details:details({block_ui:true})} as unknown as ButtonNode;
 const owner=env({block});owner.call.mockRejectedValue(failure as never);
 await expect(runObjectButton(node,owner as ButtonEnvironment)).rejects.toBe(failure);expect(block).toHaveBeenCalledTimes(1);expect(release).toHaveBeenCalledTimes(1);
});


import { ButtonDebouncer, ButtonHotkeys, viewButton, disposeViewButton } from '../src/client/view-button.js';
import { notificationPermissionWidget } from '../src/client/message-widgets.js';
import { decodeAst, parseBoundedJson } from '../../../../web/packages/expr/src/index.js';
import { parseRoute, routeUrl } from '../src/client/route.js';
import { renderFormLayout } from '../src/client/form-layout.js';
function arenaView(nodes:unknown[],fields:Record<string,unknown>={}) {
 return describeViews({wire_major:1,registry_version:'r1',description_version:'d1',model:'test.note',fields:{'test.note':fields},notices:[],views:[{identity:{kind:'stored',id:1},type:'form',schema_major:1,renderer:'web.form',root:0,nodes}]},'test.note','r1').views.get('form')!;
}
test('a title-only cancel control retains its admitted accessible name',()=>{
 const view=arenaView([{kind:'form',record_mode:'record',children:[1]},{kind:'footer',children:[2]},{kind:'cancel_button',label:'',variant:'secondary',modifiers:mods,details:details({title:'Close dialog'})}]);
 const footer=view.nodes[0] as {children:{children:import('../src/client/metadata.js').CancelNode[]}[]};
 const control=viewButton(footer.children[0]!.children[0]!,{activate:vi.fn(),model:'test.note',report:error=>{throw error;}});
 expect(control.getAttribute('aria-label')).toBe('Close dialog');disposeViewButton(control);
});
test('statistic content rendering preserves a declared icon on the single action target',()=>{
 const view=arenaView([{kind:'form',record_mode:'record',children:[1]},{kind:'sheet',children:[2]},{kind:'button_box',children:[3]},{kind:'stat_button',name:'related',button_type:'object',label:'Related',variant:'secondary',confirm:null,modifiers:mods,details:details({icon:'lucide:refresh-cw'}),children:[4]},{kind:'field',field:{model:'test.note',name:'count'},widget:'statinfo',options:{},modifiers:mods,label:'Count',force_save:false}],{count:{type:'integer',label:'Count',readonly:true,required:false,selection:null,relation:null,relation_field:null}});
 const form=document.createElement('form');document.body.append(form);const controls:HTMLElement[]=[];
 const layout=renderFormLayout(form,view.nodes,(_node,parent)=>parent.append(document.createTextNode('3')),new Map(),(node,parent)=>{const control=viewButton(node,{activate:vi.fn(),model:'test.note',report:error=>{throw error;}});parent.append(control);controls.push(control);return control;});
 try{expect(form.querySelectorAll('button')).toHaveLength(1);expect(form.querySelector('button svg')?.getAttribute('data-icon')).toBe('refresh-cw');expect(form.querySelector('.form-stat-content')).not.toBeNull();}
 finally{layout.dispose();controls.forEach(disposeViewButton);}
});
test('leading debounce resets a quiet window without a trailing action and disposes pending timers',()=>{
 vi.useFakeTimers();const owner=new ButtonDebouncer(),key={},call=vi.fn();
 owner.run(key,100,call);expect(call).toHaveBeenCalledTimes(1);
 vi.advanceTimersByTime(90);owner.run(key,100,call);vi.advanceTimersByTime(90);owner.run(key,100,call);expect(call).toHaveBeenCalledTimes(1);
 vi.advanceTimersByTime(100);expect(call).toHaveBeenCalledTimes(1);owner.run(key,100,call);expect(call).toHaveBeenCalledTimes(2);
 owner.dispose();expect(vi.getTimerCount()).toBe(0);
 expect(()=>owner.run(key,100,call)).toThrowError(expect.objectContaining({kind:'validation',message:'Button execution owner is disposed.'}));
});
/// (partial: owner eligibility, overlay, repeat, editable targets and modal dispatch; rendered proof is separate)
test('declared access keys respect active modal ownership and unavailable controls',()=>{
 const call=vi.fn(),report=vi.fn(),control=document.createElement('button');control.dataset['hotkey']='x';control.addEventListener('click',call);document.body.append(control);
 const owner=new ButtonHotkeys(()=>[control],{blocked:()=>false,report});
 const key=(value:string,extra:KeyboardEventInit={})=>document.dispatchEvent(new KeyboardEvent('keydown',{key:value,bubbles:true,cancelable:true,altKey:true,...extra}));
 try{
  key('Alt');expect(document.querySelectorAll('.button-hotkey-hint')).toHaveLength(1);
  key('x');expect(call).toHaveBeenCalledTimes(1);key('x',{repeat:true});expect(call).toHaveBeenCalledTimes(1);
  control.disabled=true;key('Alt');expect(document.querySelectorAll('.button-hotkey-hint')).toHaveLength(0);key('x');expect(call).toHaveBeenCalledTimes(1);control.disabled=false;
  const input=document.createElement('input');document.body.append(input);input.dispatchEvent(new KeyboardEvent('keydown',{key:'x',altKey:true,bubbles:true}));expect(call).toHaveBeenCalledTimes(1);
  const dialog=document.createElement('dialog');dialog.open=true;document.body.append(dialog);key('x');expect(call).toHaveBeenCalledTimes(1);
  dialog.append(control);key('x');expect(call).toHaveBeenCalledTimes(2);dialog.remove();key('x');expect(call).toHaveBeenCalledTimes(2);
  expect(report).not.toHaveBeenCalled();
 }finally{owner.dispose();}
 key('Alt');expect(document.querySelectorAll('.button-hotkey-hint')).toHaveLength(0);
});

/// (partial: physical-key matching for shifted digits and macOS Option-composed letters)
test('declared access keys match the physical key when the produced character differs',()=>{
 const report=vi.fn(),calls:string[]=[];
 const control=(hotkey:string)=>{const node=document.createElement('button');node.dataset['hotkey']=hotkey;node.addEventListener('click',()=>calls.push(hotkey));document.body.append(node);return node;};
 const controls=[control('shift+1'),control('x'),control('7')];
 const owner=new ButtonHotkeys(()=>controls,{blocked:()=>false,report});
 const key=(init:KeyboardEventInit)=>document.dispatchEvent(new KeyboardEvent('keydown',{bubbles:true,cancelable:true,altKey:true,...init}));
 try{
  key({key:'!',code:'Digit1',shiftKey:true});expect(calls).toEqual(['shift+1']);
  key({key:'≈',code:'KeyX'});expect(calls).toEqual(['shift+1','x']);
  key({key:'7',code:'Numpad7'});expect(calls).toEqual(['shift+1','x','7']);
  key({key:'!',code:'Digit1'});expect(calls).toEqual(['shift+1','x','7']);
  expect(report).not.toHaveBeenCalled();
 }finally{owner.dispose();for(const node of controls)node.remove();}
});

test('button context evaluates current saved values and refuses protected context keys before an RPC',async()=>{
 const doc=(key:string)=>({schema:1,nodes:[{kind:'literal',value:{kind:'str',value:key}},{kind:'name',name:'name'},{kind:'dict',items:[[0,1]]}]});
 const context=decodeAst(parseBoundedJson(JSON.stringify(doc('default_name'))));
 const node={kind:'button',name:'submit',label:'Submit',variant:'secondary',confirm:null,invisible:null,details:{...details(),context}} as unknown as ButtonNode;
 const owner=env({fields:[{name:'name',kind:'char'}],values:()=>({name:'Saved value'})});
 await runObjectButton(node,owner as ButtonEnvironment);expect(owner.call).toHaveBeenCalledWith('test.note','submit',{ids:[7],context:{...owner.context,default_name:'Saved value'}});
 owner.call.mockClear();
 const bad={...node,details:{...details(),context:decodeAst(parseBoundedJson(JSON.stringify(doc('allowed_company_ids'))))}} as unknown as ButtonNode;
 await expect(runObjectButton(bad,owner as ButtonEnvironment)).rejects.toMatchObject({kind:'validation',message:"Button context cannot replace protected key 'allowed_company_ids'."});expect(owner.call).not.toHaveBeenCalled();
});

test('a context-only authorized field is loaded as readonly hidden state without rendering a new occurrence',()=>{
 const context={schema:1,nodes:[{kind:'literal',value:{kind:'str',value:'default_count'}},{kind:'name',name:'count'},{kind:'dict',items:[[0,1]]}]};
 const nodes=[{kind:'form',record_mode:'record',children:[1]},{kind:'header',children:[2]},objectButton({details:details({context})})];
 const description=describeViews({wire_major:1,registry_version:'r1',description_version:'d1',model:'test.note',fields:{'test.note':{count:{type:'integer',label:'Count',required:true,readonly:false,selection:null,relation:null,relation_field:null}}},notices:[],views:[{identity:{kind:'stored',id:1},type:'form',schema_major:1,renderer:'web.form',root:0,nodes}]},'test.note','r1');
 const view=description.views.get('form')!;
 expect(view.fields).toEqual([expect.objectContaining({name:'count',readonly:true,required:false,invisible:true,forceSave:false})]);
 expect(JSON.stringify(view.nodes)).not.toContain('"kind":"field"');
});

test.each([
 [{confirm:''},'Button confirmation must be nonempty.'],
 [{confirm:'Proceed?',details:details({confirm_title:' '})},'Button confirm_title must be nonempty.'],
 [{confirm:'Proceed?',details:details({confirm_label:''})},'Button confirm_label must be nonempty.'],
 [{confirm:'Proceed?',details:details({cancel_label:' '})},'Button cancel_label must be nonempty.'],
])('consumer refuses empty confirmation text %j',(extra,message)=>{
 expect(()=>decode([objectButton(extra)])).toThrowError(expect.objectContaining({kind:'server',message}));
});

test('permission guidance observes browser state and disposal prevents a later refresh',()=>{
 const prior=Object.getOwnPropertyDescriptor(globalThis,'Notification');let permission:NotificationPermission='denied';const open=vi.fn(),report=vi.fn();
 Object.defineProperty(globalThis,'Notification',{configurable:true,value:{get permission(){return permission;}}});
 const widget=notificationPermissionWidget({open,report});document.body.append(widget.node);
 try{
  expect(widget.node.querySelector<HTMLElement>('.notification-permission-alert')?.hidden).toBe(false);widget.node.querySelector<HTMLButtonElement>('button')!.click();expect(open).toHaveBeenCalledTimes(1);
  permission='granted';window.dispatchEvent(new Event('focus'));expect(widget.node.querySelector<HTMLElement>('.notification-permission-alert')?.hidden).toBe(true);
  widget.dispose();permission='denied';window.dispatchEvent(new Event('focus'));expect(widget.node.querySelector<HTMLElement>('.notification-permission-alert')?.hidden).toBe(true);expect(report).not.toHaveBeenCalled();
 }finally{widget.dispose();if(prior)Object.defineProperty(globalThis,'Notification',prior);else Reflect.deleteProperty(globalThis,'Notification');}
});
test('presentation debug flag roundtrips without becoming an authorization context',()=>{
 expect(parseRoute(new URL('https://erp.test/erp?action=7&debug=1'))).toEqual({action:'7',debug:'1'});
 expect(routeUrl({action:'7',debug:'1'})).toBe('/erp/action-7?debug=1');
 expect(()=>parseRoute(new URL('https://erp.test/erp?debug=true'))).toThrowError(expect.objectContaining({kind:'validation',message:'Invalid navigation parameter: debug.'}));
});
test('an inert declared button remains a single disabled control',()=>{
 const node={kind:'button',name:'inert',target:{kind:'inert'},label:'Unavailable',variant:'secondary',confirm:null,invisible:null} as unknown as ButtonNode;
 const activate=vi.fn(),control=viewButton(node,{activate,model:'test.note',report:error=>{throw error;}});document.body.append(control);control.click();expect(activate).not.toHaveBeenCalled();expect(control.disabled).toBe(true);expect(control.querySelector('button')).toBeNull();disposeViewButton(control);
});

/// (partial: list arch create, edit, delete attribute parsing and active action control)
/// (partial: form arch create, edit, delete attribute parsing and active action control)
test('isFalsy helper recognises the reference ERP false/0 falsy representations', () => {
  expect(isFalsy(false)).toBe(true);
  expect(isFalsy(0)).toBe(true);
  expect(isFalsy("0")).toBe(true);
  expect(isFalsy("false")).toBe(true);
  expect(isFalsy("False")).toBe(true);
  expect(isFalsy("FALSE")).toBe(true);
  expect(isFalsy(true)).toBe(false);
  expect(isFalsy(1)).toBe(false);
  expect(isFalsy("1")).toBe(false);
  expect(isFalsy("true")).toBe(false);
  expect(isFalsy(null)).toBe(false);
  expect(isFalsy(undefined)).toBe(false);
});
