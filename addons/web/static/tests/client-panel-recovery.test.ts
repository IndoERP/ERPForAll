/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
import { expect, test, vi } from "vitest";
import { attemptPanelRecovery, PanelRecoverySafety } from "../src/client/panel-recovery.js";

function storage() {
  const values=new Map<string,string>();
  return {getItem:(key:string)=>values.get(key)??null,setItem:(key:string,value:string)=>{values.set(key,value);}};
}
/// (partial: a consumed recovery guard is not re-armed by metadata success)
test("metadata recovery reloads once for the entire tab session",()=>{
  const state=storage(),reload=vi.fn();
  expect(attemptPanelRecovery(()=>state,true,reload)).toBe(true);
  // Compatible/unrelated metadata has no reset operation; a later failure remains terminal.
  expect(attemptPanelRecovery(()=>state,true,reload)).toBe(false);
  expect(reload).toHaveBeenCalledTimes(1);
});
/// (partial: dirty/pending/uncertain state and storage failures suppress reload)
test("unsafe or unguarded recovery remains manual",()=>{
  const reload=vi.fn();
  expect(attemptPanelRecovery(()=>storage(),false,reload)).toBe(false);
  expect(attemptPanelRecovery(()=>{throw new Error("unavailable");},true,reload)).toBe(false);
  expect(attemptPanelRecovery(()=>({getItem:()=>null,setItem:()=>{throw new Error("quota");}}),true,reload)).toBe(false);
  expect(reload).not.toHaveBeenCalled();
});

/// (partial: all transport writes interlock metadata reload, including concurrent writes)
test("pending and uncertain transport writes keep recovery manual",()=>{
  const safety=new PanelRecoverySafety();
  expect(safety.safe).toBe(true);
  safety.observe("request",{write:true});safety.observe("request",{write:true});
  expect(safety.safe).toBe(false);
  safety.observe("online",{write:true});expect(safety.safe).toBe(false);
  safety.observe("error",{write:true});expect(safety.safe).toBe(true);
  safety.observe("request",{write:true});safety.observe("offline",{write:true});
  expect(safety.safe).toBe(false);
  safety.observe("request",{});safety.observe("online",{});
  expect(safety.safe).toBe(false);
});
