/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
import { afterEach, beforeEach, expect, test, vi } from 'vitest';
import { MessageServices } from '../src/client/messages.js';
import { createModelFieldSelector } from '../src/client/model-field-selector.js';
import { auditComponent } from '../../../../web/testing/axe.js';
import type { Field } from '../src/client/metadata.js';
let messages: MessageServices | undefined;
beforeEach(()=>{Object.defineProperty(HTMLDialogElement.prototype,'showModal',{configurable:true,value(this:HTMLDialogElement){this.open=true;}});Object.defineProperty(HTMLDialogElement.prototype,'close',{configurable:true,value(this:HTMLDialogElement){this.open=false;}});});
afterEach(()=>{messages?.dispose();document.body.replaceChildren();});
const field=(name:string,label:string,relation:string|null=null):Field=>({name,label,relation,kind:relation?'many2one':'char',required:false,readonly:false,selection:null,relationField:null});
const maps=new Map([['sale.order',new Map([['name',field('name','Reference')],['partner_id',field('partner_id','Contact','res.partner')]])],['res.partner',new Map([['name',field('name','Name')],['secret',field('secret','Hidden')]])]]);
function fixture(){messages=new MessageServices(document.body,vi.fn());return {messages,report:vi.fn()};}
test('labelled paths preserve identity, traversal policies, caret navigation, clear and sheet',async()=>{
 const environment=fixture(),change=vi.fn();const picker=createModelFieldSelector({environment,label:'Field',model:'sale.order',path:'partner_id.name',fields:async(model:string)=>maps.get(model)!,allowEmpty:true,filter:(value:Field)=>value.name!=='secret',sort:(left:Field,right:Field)=>left.name.localeCompare(right.name),change,touch:true});document.body.append(picker.node);await picker.ready;
 expect(picker.node.textContent).toContain('Contact / Name');expect(picker.node.dataset['state']).toBe('valid');
 picker.node.querySelector<HTMLButtonElement>('[data-field-open]')!.click();await vi.waitFor(()=>expect(document.querySelector('dialog')).not.toBeNull());
 await vi.waitFor(()=>expect(document.querySelector('[data-field-name=partner_id]')).not.toBeNull()); const input=document.querySelector<HTMLInputElement>('.editor-field-search')!;input.value='';input.setSelectionRange(0,0);input.dispatchEvent(new Event('input'));
 input.dispatchEvent(new KeyboardEvent('keydown',{key:'ArrowDown',bubbles:true}));input.dispatchEvent(new KeyboardEvent('keydown',{key:'ArrowDown',bubbles:true}));input.dispatchEvent(new KeyboardEvent('keydown',{key:'ArrowRight',bubbles:true}));
 await vi.waitFor(()=>expect(document.querySelector('[data-field-name="name"]')?.textContent).toBe('Name'));
 expect(document.querySelector('[data-field-name="secret"]')).toBeNull();input.dispatchEvent(new KeyboardEvent('keydown',{key:'ArrowDown',bubbles:true}));input.dispatchEvent(new KeyboardEvent('keydown',{key:'Enter',bubbles:true}));expect(change).toHaveBeenLastCalledWith('partner_id.name'); await vi.waitFor(()=>expect(document.querySelector('dialog')).toBeNull());
 picker.node.querySelector<HTMLButtonElement>('[data-field-clear]')!.click();expect(change).toHaveBeenLastCalledWith('');await vi.waitFor(()=>expect(picker.node.dataset['state']).toBe('empty'));picker.dispose();

 {
 const environment=fixture(),change=vi.fn();const invalid=createModelFieldSelector({environment,label:'Field',model:'sale.order',path:'partner_id.absent',fields:(model:string)=>maps.get(model)!,readonly:true,change});document.body.append(invalid.node);await invalid.ready;expect(invalid.node.dataset['state']).toBe('invalid');expect(invalid.node.querySelector('[data-field-open]')).toBeNull();invalid.dispose();
 const empty=createModelFieldSelector({environment,label:'Field',model:'sale.order',path:'',fields:(model:string)=>maps.get(model)!,allowEmpty:true,change});await empty.ready;expect(empty.node.dataset['state']).toBe('empty');empty.dispose();
 const missing=createModelFieldSelector({environment,label:'Field',model:'sale.order',path:'name',fields:()=>Promise.reject(new Error('Metadata unavailable')),change});document.body.append(missing.node);await missing.ready;expect(missing.node.dataset['state']).toBe('error');expect(missing.node.textContent).toContain('Metadata unavailable');expect(environment.report).toHaveBeenCalled();missing.dispose();
 }
});
test('owner disposal suppresses late metadata and path depth is bounded',async()=>{
 const environment=fixture(),change=vi.fn();let resolve!:(value:ReadonlyMap<string,Field>)=>void;const promise=new Promise<ReadonlyMap<string,Field>>(yes=>{resolve=yes;});const picker=createModelFieldSelector({environment,label:'Field',model:'sale.order',path:'name',fields:()=>promise,change});picker.dispose();resolve(maps.get('sale.order')!);await picker.ready;expect(change).not.toHaveBeenCalled();
 expect(()=>createModelFieldSelector({environment,label:'Field',model:'sale.order',path:Array(33).fill('partner_id').join('.'),fields:(model:string)=>maps.get(model)!,change})).toThrowError(expect.objectContaining({kind:'validation',message:'Field path must contain at most 32 segments and 1024 bytes.'}));
});

test('relation policy refuses traversal and pending page does not select stale fields',async()=>{
 const environment=fixture(),change=vi.fn(); let release!:(value:ReadonlyMap<string,Field>)=>void;
 const picker=createModelFieldSelector({environment,label:'Field',model:'sale.order',path:'',allowEmpty:true,fields:(model:string)=>model==='sale.order'?maps.get(model)!:new Promise<ReadonlyMap<string,Field>>(resolve=>{release=resolve;}),sort:(left:Field,right:Field)=>left.name.localeCompare(right.name),change});document.body.append(picker.node);await picker.ready;
 picker.node.querySelector<HTMLButtonElement>('[data-field-open]')!.click();await vi.waitFor(()=>expect(document.querySelector('[data-field-name=partner_id]')).not.toBeNull());const input=document.querySelector<HTMLInputElement>('.editor-field-search')!;
 input.dispatchEvent(new KeyboardEvent('keydown',{key:'ArrowDown',bubbles:true}));input.dispatchEvent(new KeyboardEvent('keydown',{key:'ArrowDown',bubbles:true}));input.dispatchEvent(new KeyboardEvent('keydown',{key:'ArrowRight',bubbles:true}));input.dispatchEvent(new KeyboardEvent('keydown',{key:'Enter',bubbles:true}));expect(change).not.toHaveBeenCalled();expect(document.querySelectorAll('[role=option]')).toHaveLength(0);
 release(maps.get('res.partner')!);await vi.waitFor(()=>expect(document.querySelector('[data-field-name=name]')?.textContent).toBe('Name'));input.dispatchEvent(new KeyboardEvent('keydown',{key:'ArrowLeft',bubbles:true}));await vi.waitFor(()=>expect(document.querySelector('[data-field-name=partner_id]')).not.toBeNull());input.dispatchEvent(new KeyboardEvent('keydown',{key:'Escape',bubbles:true}));expect(document.querySelector('.editor-popover')).toBeNull();picker.dispose();
 const restricted=createModelFieldSelector({environment,label:'Field',model:'sale.order',path:'partner_id.name',followRelation:false,fields:(model:string)=>maps.get(model)!,change});await restricted.ready;expect(restricted.node.dataset['state']).toBe('invalid');expect(restricted.node.textContent).toContain('The field path cannot follow this relation.');restricted.dispose();
});

test('pending field selection is serialized and disposal suppresses its late completion',async()=>{
 const environment=fixture();let resolve!:()=>void;const change=vi.fn(()=>new Promise<void>(yes=>{resolve=yes;}));const picker=createModelFieldSelector({environment,label:'Field',model:'sale.order',path:'',allowEmpty:true,fields:(model:string)=>maps.get(model)!,change});document.body.append(picker.node);await picker.ready;
 picker.node.querySelector<HTMLButtonElement>('[data-field-open]')!.click();await vi.waitFor(()=>expect(document.querySelector('[data-field-name=name]')).not.toBeNull());const option=document.querySelector<HTMLElement>('[data-field-name=name]')!;option.click();option.click();expect(change).toHaveBeenCalledTimes(1);expect(picker.node.getAttribute('aria-busy')).toBe('true');picker.dispose();resolve();await Promise.resolve();expect(document.querySelector('.editor-popover')).toBeNull();
});


test('field listbox and relation controls have valid accessible ownership',async()=>{
 const environment=fixture();const picker=createModelFieldSelector({environment,label:'Field',model:'sale.order',path:'',allowEmpty:true,fields:(model:string)=>maps.get(model)!,change:vi.fn()});document.body.append(picker.node);await picker.ready;
 picker.node.querySelector<HTMLButtonElement>('[data-field-open]')!.click();await vi.waitFor(()=>expect(document.querySelector('[data-field-name=partner_id]')).not.toBeNull());
 const overlay=document.querySelector<HTMLElement>('.editor-popover')!;await expect(auditComponent(overlay)).resolves.toBeDefined();picker.dispose();
});

test.each(['pointer','keyboard'])('R7 relation policy failure is visible and reported for %s traversal',async method=>{
 const environment=fixture(),failure=new Error('Relation policy refused');let refuse=false;const picker=createModelFieldSelector({environment,label:'Field',model:'sale.order',path:'',allowEmpty:true,fields:(model:string)=>maps.get(model)!,sort:(left:Field,right:Field)=>left.name.localeCompare(right.name),followRelation:()=>{if(refuse)throw failure;return true;},change:vi.fn()});document.body.append(picker.node);await picker.ready;picker.node.querySelector<HTMLButtonElement>('[data-field-open]')!.click();await vi.waitFor(()=>expect(document.querySelector('[data-field-name=partner_id]')).not.toBeNull());refuse=true;
 if(method==='pointer')document.querySelector<HTMLButtonElement>('.editor-field-follow')!.click();else {const input=document.querySelector<HTMLInputElement>('.editor-field-search')!;input.dispatchEvent(new KeyboardEvent('keydown',{key:'ArrowDown',bubbles:true}));input.dispatchEvent(new KeyboardEvent('keydown',{key:'ArrowDown',bubbles:true}));input.dispatchEvent(new KeyboardEvent('keydown',{key:'ArrowRight',bubbles:true}));}
 await vi.waitFor(()=>expect(environment.report).toHaveBeenCalledWith(failure));expect(document.querySelector('.editor-field-message')?.textContent).toBe('Relation policy refused');picker.dispose();
});


test.each([false,true])('empty field list has accessible nonselectable placeholder, filtered=%s',async filtered=>{
 const environment=fixture(),change=vi.fn();const picker=createModelFieldSelector({environment,label:'Field',model:'sale.order',path:'',allowEmpty:true,fields:(model:string)=>filtered?maps.get(model)!:new Map<string,Field>(),change});document.body.append(picker.node);await picker.ready;picker.node.querySelector<HTMLButtonElement>('[data-field-open]')!.click();const input=document.querySelector<HTMLInputElement>('.editor-field-search')!;await vi.waitFor(()=>expect(input.hasAttribute('aria-busy')).toBe(false));if(filtered){input.value='no-such-field';input.dispatchEvent(new Event('input'));}
 const overlay=document.querySelector<HTMLElement>('.editor-popover')!;await expect(auditComponent(overlay)).resolves.toBeDefined();const placeholder=overlay.querySelector<HTMLElement>('[role=option]')!;expect(placeholder.getAttribute('aria-disabled')).toBe('true');expect(placeholder.getAttribute('aria-selected')).toBe('false');input.dispatchEvent(new KeyboardEvent('keydown',{key:'ArrowDown',bubbles:true}));input.dispatchEvent(new KeyboardEvent('keydown',{key:'Enter',bubbles:true}));placeholder.click();expect(change).not.toHaveBeenCalled();picker.dispose();
});
test('field keyboard choice survives mouseenter and touch until actual mouse or pen movement',async()=>{
 const environment=fixture(),change=vi.fn();const picker=createModelFieldSelector({environment,label:'Field',model:'sale.order',path:'',allowEmpty:true,fields:(model:string)=>maps.get(model)!,sort:(left:Field,right:Field)=>left.name.localeCompare(right.name),change});document.body.append(picker.node);await picker.ready;picker.node.querySelector<HTMLButtonElement>('[data-field-open]')!.click();await vi.waitFor(()=>expect(document.querySelector('[data-field-name=partner_id]')).not.toBeNull());const input=document.querySelector<HTMLInputElement>('.editor-field-search')!;
 input.dispatchEvent(new KeyboardEvent('keydown',{key:'ArrowDown',bubbles:true}));input.dispatchEvent(new KeyboardEvent('keydown',{key:'ArrowDown',bubbles:true}));const selected=input.getAttribute('aria-activedescendant'),first=document.querySelector<HTMLElement>('[data-field-name=name]')!;first.dispatchEvent(new MouseEvent('mouseenter'));expect(input.getAttribute('aria-activedescendant')).toBe(selected);const move=(pointerType:string,movementX:number,movementY=0):void=>{const event=new Event('pointermove',{bubbles:true});Object.defineProperties(event,{pointerType:{value:pointerType},movementX:{value:movementX},movementY:{value:movementY}});first.dispatchEvent(event);};move('touch',1);expect(input.getAttribute('aria-activedescendant')).toBe(selected);move('mouse',0);expect(input.getAttribute('aria-activedescendant')).toBe(selected);move('mouse',1);expect(input.getAttribute('aria-activedescendant')).toBe(first.id);input.dispatchEvent(new KeyboardEvent('keydown',{key:'ArrowDown',bubbles:true}));move('pen',0,1);expect(input.getAttribute('aria-activedescendant')).toBe(first.id);document.querySelector<HTMLElement>('[data-field-name=partner_id]')!.click();expect(change).toHaveBeenLastCalledWith('partner_id');picker.dispose();
});

test.each(['x'.repeat(257),'é'.repeat(129)])('field relation identity is byte-bounded before pattern matching and aggregate encoding',async relation=>{
 const environment=fixture();const fields=new Map([['partner_id',field('partner_id','Contact',relation)]]);const picker=createModelFieldSelector({environment,label:'Field',model:'sale.order',path:'partner_id',fields:()=>fields,change:vi.fn()});document.body.append(picker.node);await picker.ready;expect(picker.node.dataset['state']).toBe('error');expect(environment.report).toHaveBeenCalledWith(expect.objectContaining({kind:'validation',message:'Field relation identity must be text of at most 256 bytes.'}));picker.dispose();
});
