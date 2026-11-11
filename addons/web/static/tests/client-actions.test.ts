/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
import { expect, test } from "vitest";
import { actionResult } from "../src/client/actions.js";
import { ClientError } from "../src/client/transport.js";
const decode = (result: unknown) => actionResult({wire_major:1,result});
const windowResult = {kind:"window",name:"Wizard",model:"test.wizard",res_id:null,views:[{id:null,type:"form"}],context:{default_name:"Ada"}};
const currentResult = {kind:"current_window",name:"Related",model:"test.wizard",views:[{id:null,type:"list"},{id:null,type:"form"}],domain:[["name","=","Ada"]],context:{default_name:"Ada"}};
function refused(value: unknown, kind: string, message: string): void {
  try { decode(value); throw new Error("accepted invalid result"); }
  catch (error) { expect(error).toBeInstanceOf(ClientError); expect(error).toMatchObject({kind,message}); }
}
/// (partial: strict normalized client grammar)
test("related-list action retains a literal base filter and defaults", () => {
  expect(decode(currentResult)).toEqual(currentResult);
  expect(decode({...currentResult,domain:[[0,"=",1]]})).toEqual({...currentResult,domain:[[0,"=",1]]});
  refused({...currentResult,domain:"name = Ada"}, "server", "Invalid current-window domain.");
  refused({...currentResult,domain:["|",["name","=","Ada"]]}, "server", "Invalid current-window domain.");
  refused({...currentResult,domain:[["name"]]}, "server", "Invalid current-window domain.");
  refused({...currentResult,domain:[["id","=",9007199254740992]]}, "server", "Current-window integer literal exceeds the browser's safe range.");
  refused({...currentResult,context:{allowed_company_ids:[9]}}, "unsupported", "Unsupported current-window context key: allowed_company_ids.");
  refused({...currentResult,views:[{id:null,type:"form"}]}, "unsupported", "A current window requires list, optionally followed by form.");
  refused({...currentResult,res_id:7}, "unsupported", "Unknown action result member: res_id.");
});
/// (partial: normalized result consumer closes every variant and refuses malformed/hostile members)
test("closed action result variants and default context boundary", () => {
  expect(decode(windowResult)).toEqual(windowResult);
  expect(decode({kind:"none"})).toEqual({kind:"none"});
  expect(decode({kind:"close"})).toEqual({kind:"close"});
  refused({kind:"close",url:"https://example.com"}, "unsupported", "Unknown action result member: url.");
  refused({...windowResult,context:{allowed_company_ids:[9]}}, "unsupported", "Unsupported dialog context key: allowed_company_ids.");
  refused({...windowResult,res_id:0}, "server", "Invalid dialog record id.");
  refused({...windowResult,views:[]}, "unsupported", "A wizard requires exactly one form view.");
  refused({...windowResult,views:[{id:null,type:"list"}]}, "unsupported", "A wizard requires a form view.");
  refused({kind:"notification",title:null,message:"done",level:"neon",then_close:false}, "server", "Invalid notification level.");
  refused({...windowResult,context:{default_name:"x".repeat(17000)}}, "server", "Action result is too large.");
  refused({kind:"alien"}, "unsupported", "Unsupported action result kind.");
});


test("result version, nested depth and notification flags fail precisely", () => {
  expect(() => actionResult({wire_major:2,result:{kind:"none"}})).toThrowError(expect.objectContaining({kind:"unsupported",message:"Unsupported action result version. Reload the page."}));
  let deep: unknown = "leaf"; for (let i=0;i<8;i++) deep=[deep];
  refused({...windowResult,context:{default_name:deep}},"server","Dialog defaults are too deeply nested.");
  refused({kind:"notification",title:null,message:"done",level:"info",then_close:"yes"},"server","Invalid notification close flag.");
  refused({...windowResult,model:"../arbitrary"},"server","Invalid dialog title or model.");
});

/// (partial: strict client context grammar)
test("branch archive context admits booleans only on current windows", () => {
  for (const active_test of [true, false]) {
    const value = {...currentResult,context:{active_test}};
    expect(decode(value)).toEqual(value);
  }
  for (const active_test of [null,0,"false",[]]) refused({...currentResult,context:{active_test}}, "server", "Current-window active_test must be a boolean.");
  refused({...windowResult,context:{active_test:false}}, "unsupported", "Unsupported dialog context key: active_test.");
});

/// (partial: the client accepts the variants action's context)
test("current windows admit a boolean create key, as the product variants action sends it", () => {
  const value = {...currentResult,context:{create:false,default_product_tmpl_id:7}};
  expect(decode(value)).toEqual(value);
  for (const create of [null,0,"false",[]]) refused({...currentResult,context:{create}}, "server", "Current-window create must be a boolean.");
  refused({...windowResult,context:{create:false}}, "unsupported", "Unsupported dialog context key: create.");
});
