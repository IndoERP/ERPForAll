/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
import { expect, test, vi } from "vitest";
import { RecordForm } from "../src/client/record-form.js";
import { RecordState } from "../src/client/record.js";
import { RpcClient } from "../src/client/transport.js";
import { viewButton, disposeViewButton } from "../src/client/view-button.js";
import type { Field, Node, View } from "../src/client/metadata.js";

/// (partial: record renderer uses real loaded numeric values including zero)
test("statinfo reads current record values and rejects malformed server values", () => {
  const rpc = new RpcClient(async () => { throw new Error("Unexpected RPC"); }, () => {});
  const field: Field = {name:"line_count",kind:"integer",label:"Lines",required:false,readonly:true,selection:null,relation:null,relationField:null};
  const state = new RecordState(rpc,"test.document",[field],{});
  const nodes: Node[] = [{kind:"root",children:[{kind:"sheet",children:[{kind:"button_box",children:[{kind:"stat_button",name:"open_lines",label:"Lines",variant:"secondary",confirm:null,invisible:null,children:[{kind:"field",field,label:"Lines",widget:"statinfo"}]}]}]}]}];
  const view: View = {type:"form",mode:"record",nodes,fields:[field]};
  const form = document.createElement("form");
  const rendered = new RecordForm({form,state,view,rpc,context:{},
    run:()=>{},report:()=>{},openRecord:async()=>{},change:()=>{},
    button:(_node,parent)=>{const control=document.createElement("button");control.type="button";parent.append(control);return control;}
  });
  state.values["line_count"]=0;rendered.render();
  expect(form.querySelector(".form-stat-value")?.textContent).toBe("0");
  expect(form.querySelector(".form-stat-button")?.getAttribute("aria-label")).toBe("0 Lines");
  state.values["line_count"]=17;rendered.render();
  expect(form.querySelector(".form-stat-value")?.textContent).toBe("17");
  expect(form.querySelector(".form-stat-button")?.getAttribute("aria-label")).toBe("17 Lines");
  expect(form.querySelector("input")).toBeNull();
  delete state.values["line_count"];rendered.render();
  expect(form.querySelector(".form-stat-value")?.textContent).toBe("—");
  state.values["line_count"]="<fake>";
  expect(()=>rendered.render()).toThrow("Invalid statistic value for line_count.");
  rendered.dispose();
});


/// (partial: RecordForm icon/value/name composition through rerender and overflow)
test("a declared statistic icon and live count remain on one action after rerender and a phone-width resize", () => {
  const width = vi.spyOn(window, "innerWidth", "get").mockReturnValue(1200);
  const action = vi.fn(), controls: HTMLButtonElement[] = [];
  const rpc = new RpcClient(async () => { throw new Error("Unexpected RPC"); }, () => {});
  const field: Field = {name:"line_count",kind:"integer",label:"Lines",required:false,readonly:true,selection:null,relation:null,relationField:null};
  const state = new RecordState(rpc,"test.document",[field],{});
  const nodes: Node[] = [{kind:"root",children:[{kind:"sheet",children:[{kind:"button_box",children:[{kind:"stat_button",name:"open_lines",label:"",variant:"secondary",confirm:null,invisible:null,details:{size:"normal",circle:false,classes:[],debounce_ms:0,block_ui:false,icon:"lucide:refresh-cw",title:"Related records"},children:[{kind:"field",field,label:"Lines",widget:"statinfo"}]}]}]}]}];
  const view: View = {type:"form",mode:"record",nodes,fields:[field]};
  const form = document.createElement("form"); document.body.append(form);
  const rendered = new RecordForm({form,state,view,rpc,context:{},run:()=>{},report:()=>{},openRecord:async()=>{},change:()=>{},button:(node,parent)=>{
    const control=viewButton(node,{model:"test.document",activate:action,report:error=>{throw error;}});
    controls.push(control); parent.append(control); return control;
  }});
  try {
    state.values["line_count"]=0; rendered.render();
    let target=form.querySelector<HTMLButtonElement>(".form-stat-button")!;
    expect(target.querySelectorAll('svg[data-icon="refresh-cw"]')).toHaveLength(1);
    expect(target.getAttribute("aria-label")).toBe("0 Lines Related records"); expect(target.querySelector("button")).toBeNull();
    state.values["line_count"]=17; rendered.render();
    target=form.querySelector<HTMLButtonElement>(".form-stat-button")!;
    expect(target.querySelector(".form-stat-value")?.textContent).toBe("17");
    expect(target.getAttribute("aria-label")).toBe("17 Lines Related records");
    expect(target.querySelectorAll('svg[data-icon="refresh-cw"]')).toHaveLength(1);
    width.mockReturnValue(390); window.dispatchEvent(new Event("resize"));
    // A phone keeps up to four statistics in the box: the single action stays direct, with no More menu.
    expect(form.querySelector(".form-button-box > .form-stat-button")).toBe(target);
    expect(form.querySelector(".form-overflow-toggle")).toBeNull();
    target.click();
    expect(action).toHaveBeenCalledTimes(1);
    expect(target.getAttribute("aria-label")).toBe("17 Lines Related records");
    expect(target.querySelectorAll('svg[data-icon="refresh-cw"]')).toHaveLength(1);
  } finally { rendered.dispose(); controls.forEach(disposeViewButton); form.remove(); width.mockRestore(); }
});
