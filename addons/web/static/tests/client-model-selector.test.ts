/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
import { afterEach, expect, test, vi } from 'vitest';
import { MessageServices } from '../src/client/messages.js';
import { createModelSelector } from '../src/client/model-selector.js';
let messages: MessageServices | undefined;
afterEach(() => { messages?.dispose(); document.body.replaceChildren(); });
function fixture() { messages = new MessageServices(document.body, error => { throw error; }); return { messages, report: vi.fn() }; }
const models = [{technical:'res.partner',label:'Contacts'}, {technical:'sale.order',label:'Orders'}, {technical:'account.move',label:'Invoices'}];
test('model identities, initial subset, technical/display search and explicit empty state', async () => {
 const environment=fixture(), select=vi.fn(); const picker=createModelSelector({environment,label:'Model',models,visibleLimit:1,select}); document.body.append(picker.node);
 picker.input.click(); await vi.waitFor(()=>expect(document.querySelectorAll('[role=option]:not([aria-disabled=true])').length).toBe(1));
 expect(document.body.textContent).toContain('Type to find more models');
 picker.input.value='sale.'; picker.input.dispatchEvent(new Event('input')); await vi.waitFor(()=>expect(document.body.textContent).toContain('Orders'));
 document.querySelector<HTMLElement>('[role=option]:not([aria-disabled=true])')!.click(); expect(select).toHaveBeenLastCalledWith(models[1]); await vi.waitFor(()=>expect(picker.input.value).toBe('Orders'));
 picker.input.value='Contacts'; picker.input.dispatchEvent(new Event('input')); await vi.waitFor(()=>expect(document.querySelector('[role=option]:not([aria-disabled=true])')?.textContent).toBe('Contacts'));
 picker.input.dispatchEvent(new KeyboardEvent('keydown',{key:'ArrowDown',bubbles:true})); picker.input.dispatchEvent(new KeyboardEvent('keydown',{key:'Enter',bubbles:true})); expect(select).toHaveBeenLastCalledWith(models[0]); await vi.waitFor(()=>expect(picker.input.value).toBe('Contacts')); await vi.waitFor(()=>expect(document.querySelector('[role=listbox]')).toBeNull());
 picker.input.value='not-here'; picker.input.dispatchEvent(new Event('input')); await vi.waitFor(()=>expect(document.body.textContent).toContain('No matching models'));
 picker.dispose();
});
test('invalid identity differs from empty and readonly cannot open',()=>{
 const environment=fixture();const invalid=createModelSelector({environment,label:'Model',models,value:'missing.model',readonly:true,select:vi.fn()});document.body.append(invalid.node);
 expect(invalid.node.dataset['state']).toBe('invalid');expect(invalid.input.readOnly).toBe(true);invalid.input.click();expect(document.querySelector('[role=listbox]')).toBeNull();invalid.dispose();
 const empty=createModelSelector({environment,label:'Model',models,select:vi.fn()});expect(empty.node.dataset['state']).toBe('empty');empty.dispose();
});
test('duplicate model identity and invalid visible limit fail precisely',()=>{
 const environment=fixture();expect(()=>createModelSelector({environment,label:'Model',models:[models[0]!,models[0]!],select:vi.fn()})).toThrowError(expect.objectContaining({kind:'validation',message:'Duplicate model identity: res.partner.'}));
 expect(()=>createModelSelector({environment,label:'Model',models,visibleLimit:0,select:vi.fn()})).toThrowError(expect.objectContaining({kind:'validation',message:'Model visible limit must be an integer from 1 to 1000.'}));
});

test('R2 model selection serializes callbacks even when an attempted later selection resolves first',async()=>{
 const environment=fixture();let finishA!:()=>void,finishB!:()=>void;const pendingA=new Promise<void>(resolve=>{finishA=resolve;}),pendingB=new Promise<void>(resolve=>{finishB=resolve;});const select=vi.fn((model:{technical:string})=>model.technical==='res.partner'?pendingA:pendingB);
 const picker=createModelSelector({environment,label:'Model',models,select});document.body.append(picker.node);picker.input.click();await vi.waitFor(()=>expect(document.querySelectorAll('[role=option]:not([aria-disabled=true])')).toHaveLength(3));document.querySelector<HTMLElement>('[role=option]:not([aria-disabled=true])')!.click();expect(select).toHaveBeenCalledTimes(1);expect(picker.input.disabled).toBe(true);
 picker.input.value='Orders';picker.input.click();expect(document.querySelector('[role=listbox]')).toBeNull();finishB();await Promise.resolve();expect(select).toHaveBeenCalledTimes(1);finishA();await vi.waitFor(()=>expect(picker.input.value).toBe('Contacts'));expect(picker.input.disabled).toBe(false);picker.dispose();
});
test('R2 rejected model selection restores represented value and permits a later selection',async()=>{
 const environment=fixture();let refuse!:(error:Error)=>void;const failure=new Error('Model selection refused');const select=vi.fn().mockImplementationOnce(()=>new Promise<void>((_resolve,reject)=>{refuse=reject;})).mockResolvedValue(undefined);
 const picker=createModelSelector({environment,label:'Model',models,value:'res.partner',select});document.body.append(picker.node);picker.input.click();await vi.waitFor(()=>expect(document.querySelectorAll('[role=option]:not([aria-disabled=true])')).toHaveLength(3));document.querySelectorAll<HTMLElement>('[role=option]:not([aria-disabled=true])')[1]!.click();refuse(failure);await vi.waitFor(()=>expect(environment.report).toHaveBeenCalledWith(failure));expect(picker.input.value).toBe('Contacts');expect(picker.input.disabled).toBe(false);expect(document.body.textContent).toContain('Model selection refused');picker.input.click();picker.input.click();await vi.waitFor(()=>expect(document.querySelectorAll('[role=option]:not([aria-disabled=true])')).toHaveLength(3));document.querySelectorAll<HTMLElement>('[role=option]:not([aria-disabled=true])')[2]!.click();await vi.waitFor(()=>expect(picker.input.value).toBe('Invoices'));picker.dispose();
});
test('R4 model aggregate admission stops before reading later entries',()=>{
 const environment=fixture();const entries=Array.from({length:64},(_,index)=>({technical:`test.model_${index}`,label:'x'.repeat(4096)}));const later=vi.fn(()=>{throw new Error('Later model read');});Object.defineProperty(entries,64,{get:later});expect(()=>createModelSelector({environment,label:'Model',models:entries,select:vi.fn()})).toThrowError(expect.objectContaining({kind:'validation',message:'Model choice text exceeds 262144 bytes.'}));expect(later).not.toHaveBeenCalled();
});
