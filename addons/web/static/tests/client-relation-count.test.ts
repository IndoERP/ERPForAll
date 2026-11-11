/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
import { expect, test, vi } from "vitest";
import "../src/client/fields.js";
import { describeViews, type Field, type View } from "../src/client/metadata.js";
import { RecordState } from "../src/client/record.js";
import { RecordForm } from "../src/client/record-form.js";
import { RpcClient } from "../src/client/transport.js";
const field: Field = { name: "loan_ids", kind: "one2many", label: "Loans", readonly: true, required: false, relation: "library.loan", relationField: "book_id", selection: null };
const description = () => ({ wire_major:1, registry_version:"r", description_version:"d", model:"library.book", fields:{"library.book":{loan_ids:{type:"one2many",label:"Loans",readonly:false,required:false,relation:"library.loan",relation_field:"book_id",selection:null}}}, views:[{identity:{kind:"stored",id:1},type:"form",renderer:"web.form",schema_major:1,root:0,nodes:[{kind:"form",record_mode:"record",children:[1]},{kind:"sheet",children:[2]},{kind:"button_box",children:[3]},{kind:"stat_button",name:"open_loans",button_type:"object",label:"Loans",variant:"secondary",confirm:null,modifiers:{readonly:null,required:null,invisible:null},children:[4]},{kind:"field",field:{model:"library.book",name:"loan_ids"},widget:"relation_count",options:{},label:"Loans",force_save:false,modifiers:{readonly:null,required:null,invisible:null}}]}],notices:[] });
function wire() { return description(); }
function fixture(fetcher: ConstructorParameters<typeof RpcClient>[0], id: number | undefined = 4) {
  const rpc = new RpcClient(fetcher, () => {}), state = new RecordState(rpc,"library.book",[],{}); state.id=id;
  const view: View = {type:"form",mode:"record",fields:[],nodes:[{kind:"root",children:[{kind:"sheet",children:[{kind:"button_box",children:[{kind:"stat_button",name:"open_loans",label:"Loans",variant:"secondary",confirm:null,invisible:null,children:[{kind:"field",field,label:"Loans",widget:"relation_count"}]}]}]}]}]};
  const form = document.createElement("form"), report=vi.fn();
  const surface = new RecordForm({form,state,view,rpc,context:{allowed_company_ids:[1]},run:job=>{void job();},report,openRecord:async()=>{},change:()=>{},button:(node,parent)=>{const b=document.createElement("button");b.type="button";b.textContent=node.label;parent.append(b);return b;}});
  return {form,state,surface,report};
}
const answer = (value: unknown, status=200) => new Response(JSON.stringify(value),{status,headers:{"Content-Type":"application/json"}});
const settle = async () => { for(let i=0;i<10;i++) await Promise.resolve(); };
/// (partial: consumer excludes count-only relation from read/default/onchange/write fields)
test("count-only XML does not materialize a one2many or alter standard statinfo",()=>{
  const d=wire(); d.fields["library.book"].loan_ids.readonly=true;
  const view=describeViews(d,"library.book","r").views.get("form")!;
  expect(view.fields).toEqual([]);
  const bad=wire(); bad.fields["library.book"].loan_ids.readonly=true; bad.views[0]!.nodes[4]!.widget="statinfo";
  expect(()=>describeViews(bad,"library.book","r")).toThrow("Smart-button statistics");
});
/// (partial: exact owner RPC, true zero and reload refresh)
test("relation count reads real RPC and refetches after form reload/render",async()=>{
  let count=0; const calls:unknown[]=[];
  const f=fixture(async(path,init)=>{calls.push([path,JSON.parse(String(init?.body))]);return answer(count);});
  f.surface.render(); await settle();
  expect(f.form.querySelector(".form-stat-value")?.textContent).toBe("0");
  expect(calls[0]).toEqual(["/erp/json/2/library.book/relation_count",{ids:[4],field:"loan_ids",context:{allowed_company_ids:[1]}}]);
  expect(f.form.querySelector("button")?.getAttribute("aria-label")).toContain("0");
  count=13; f.surface.render(); await settle();
  expect(f.form.querySelector(".form-stat-value")?.textContent).toBe("13");
  f.surface.dispose();
});
/// (partial: new/no-record forms do not invent a persisted zero)
test("new forms do not count and disable their related-record statistic",async()=>{
  const fetcher=vi.fn(async()=>answer(88)); const f=fixture(fetcher,undefined);
  // Undefined default argument uses4, so explicitly set the unsaved state.
  f.state.id=undefined; f.surface.render(); await settle();
  expect(fetcher).not.toHaveBeenCalled(); expect(f.form.querySelector("button")?.disabled).toBe(true);
  expect(f.form.querySelector(".form-stat-value")?.textContent).toBe("—"); f.surface.dispose();
});
/// (partial: ACL/network/malformed values never become numeric zero)
test("denials and malformed count values remain visible errors",async()=>{
  for(const [value,status] of [[{name:"erp.exceptions.AccessError",message:"Loans denied"},403],[-1,200],[1.5,200],["7",200],[Number.MAX_SAFE_INTEGER+1,200]] as const){
    const f=fixture(async()=>answer(value,status));f.state.values={loan_ids:[{id:1},{id:2}]};f.surface.render();await settle();
    expect(f.form.querySelector(".form-stat-value")?.textContent).toBe("Unavailable");
    expect(f.report).toHaveBeenCalledTimes(1);f.surface.dispose();
  }
});
/// (partial: stale reads cannot overwrite a later render or mutate after disposal)
test("out-of-order count replies are ignored after render/navigation disposal",async()=>{
  const replies:((response:Response)=>void)[]=[];
  const f=fixture(async()=>new Promise<Response>(resolve=>replies.push(resolve)));
  f.surface.render();const old=f.form.querySelector(".form-stat-value")!;
  f.state.id=8;f.surface.render();replies[1]!(answer(23));await settle();
  expect(f.form.querySelector(".form-stat-value")?.textContent).toBe("23");
  replies[0]!(answer(999));await settle();expect(f.form.querySelector(".form-stat-value")?.textContent).toBe("23");expect(old.textContent).not.toBe("999");
  f.surface.render();f.surface.dispose();replies[2]!(answer(456));await settle();
  expect(f.form.querySelector(".form-stat-value")?.textContent).not.toBe("456");
});

/// (partial: an ordinary occurrence of the same O2M retains its record-read policy)
test("count-only exclusion preserves a separately declared ordinary sublist",()=>{
  const d=wire();d.fields["library.book"].loan_ids.readonly=true;
  const nodes=d.views[0]!.nodes;const ordinary=structuredClone(nodes[4]!);Object.assign(ordinary,{widget:"one2many"});
  Object.assign(nodes[1]!,{children:[2,5]});nodes.push(ordinary);
  const view=describeViews(d,"library.book","r").views.get("form")!;
  expect(view.fields.map(f=>f.name)).toEqual(["loan_ids"]);
});
/// (partial: transport failure does not substitute a cached relation length)
test("network failure leaves count unavailable even with existing relation values",async()=>{
  const f=fixture(async()=>{throw new Error("offline");});f.state.values={loan_ids:[{id:1}]};f.surface.render();await settle();
  expect(f.form.querySelector(".form-stat-value")?.textContent).toBe("Unavailable");expect(f.report).toHaveBeenCalledTimes(1);f.surface.dispose();
});
test("a pending count shows a sized placeholder, not the word Loading, and the value replaces it",async()=>{
  let release: (value: Response) => void = () => {};
  const f=fixture(()=>new Promise<Response>(resolve=>{release=resolve;}));
  f.surface.render(); await settle();
  const value=f.form.querySelector<HTMLElement>(".form-stat-value")!;
  expect(value.textContent).toBe(""); expect(value.classList.contains("form-stat-loading")).toBe(true);
  expect(f.form.querySelector(".form-stat-field")?.getAttribute("aria-busy")).toBe("true");
  release(answer(3)); await settle();
  expect(value.textContent).toBe("3"); expect(value.classList.contains("form-stat-loading")).toBe(false);
  expect(f.form.querySelector(".form-stat-field")?.hasAttribute("aria-busy")).toBe(false);
  f.surface.dispose();
});
