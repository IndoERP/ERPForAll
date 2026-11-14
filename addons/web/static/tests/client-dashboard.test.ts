/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
import { afterEach, expect, test, vi } from "vitest";
import { dropAbsentDashboardFilters, readDashboard, mountDashboard } from "../src/client/dashboard.js";
import { SearchModel } from "../src/client/search/search-model.js";
import type { Field } from "../src/client/metadata.js";
const field: Field = {name:"name",kind:"char",label:"Name",required:false,readonly:false,selection:null,relation:null,relationField:null};
const config = {version:1,revision:"a".repeat(64),items:[{kind:"count",name:"open",label:"Open",domain:null,filter:"open",color:"info"}]};
const response = {version:1,hidden:false,revision:"a".repeat(64),period:"week",periods:[{id:"week",label:"Week",kind:"rolling",days:7}],values:[{name:"open",value:"2",domain:[["name","=","Open"]]}]};
afterEach(()=>document.body.replaceChildren());
/// (partial: independent client dashboard vocabulary admission)
test("dashboard decoder refuses incompatible or unknown declarations",()=>{
  expect(readDashboard(config,new Map([["name",field]]),"demo").items[0]?.name).toBe("open");
  for(const raw of [{...config,version:2},{...config,extra:1},{...config,items:[...config.items,...config.items]}]){
    try {readDashboard(raw,new Map(),"demo");throw new Error("Expected rejection");}catch(error){expect(error).toMatchObject({kind:"server",message:"Invalid dashboard declaration."});}
  }
});
/// (partial: declared dashboard filters survive strict URL staging and preserve unrelated facets)
test("dashboard filters restore and toggle without clearing text facets",()=>{
  const filters=[{id:"dashboard_open",description:"Open",domain:[["name","=","Open"]],isActive:false}];
  const options={model:"demo",fields:new Map([["name",field]]),filters};
  const model=new SearchModel(options);model.addFieldFacet("name","needle");model.toggleFilter("dashboard_open");
  const params=new URLSearchParams();model.serializeToUrlParams(params);
  const restored=new SearchModel(options);restored.loadFromUrlParams(params);
  expect(restored.getDomain()).toEqual([["name","ilike","needle"],["name","=","Open"]]);
  restored.toggleFilter("dashboard_open");expect(restored.getDomain()).toEqual([["name","ilike","needle"]]);
});
/// (partial: stale successes/errors, retry and disposal at the renderer boundary)
test("dashboard only publishes the latest answer and exposes retry",async()=>{
  const pending:{resolve:(value:unknown)=>void;reject:(reason:unknown)=>void}[]=[];
  const dashboard=mountDashboard({config:readDashboard(config,new Map(),"demo"),read:()=>new Promise((resolve,reject)=>pending.push({resolve,reject})),apply:()=>{},selected:()=>false});
  document.body.append(dashboard.element);
  const first=dashboard.refresh([]),second=dashboard.refresh([["name","=","new"]]);
  pending[1]!.resolve(response);await second;pending[0]!.reject(new Error("old"));await first;
  expect(dashboard.element.textContent).toContain("2");expect(dashboard.element.textContent).not.toContain("old");
  const third=dashboard.refresh([["name","=","broken"]]);pending[2]!.reject(new Error("Failed read"));await third;
  expect(dashboard.element.getAttribute("aria-busy")).toBe("false");expect(dashboard.element.querySelector("[role=alert]")?.textContent).toContain("Failed read");
  dashboard.dispose();
});
/// (partial: semantic controls and count domains supplied to the host)
test("dashboard card uses a native button and applies its admitted returned domain",async()=>{
  const calls:unknown[]=[];
  const dashboard=mountDashboard({config:readDashboard(config,new Map(),"demo"),read:async()=>response,apply:(name,domain)=>calls.push([name,domain]),selected:()=>true});
  document.body.append(dashboard.element);await dashboard.refresh([]);
  const button=dashboard.element.querySelector<HTMLButtonElement>("button[data-dashboard-item]")!;button.focus();button.click();
  expect(calls).toEqual([["open",[["name","=","Open"]]]]);expect(button.getAttribute("aria-pressed")).toBe("true");expect(document.activeElement).toBe(button);await dashboard.refresh([],true);expect(document.activeElement).toBe(button);
  expect(dashboard.element.querySelector("select")?.getAttribute("aria-label")).toBe("Dashboard period");dashboard.dispose();
});

/// (partial: same-scope read reuse, force refresh and hidden/disposed states)
test("dashboard avoids redundant reads and ignores disposal answers",async()=>{
  let calls=0,resolve:(value:unknown)=>void=()=>{};
  const dashboard=mountDashboard({config:readDashboard(config,new Map(),"demo"),read:async()=>{calls++;return response;},apply:()=>{},selected:()=>false});
  document.body.append(dashboard.element);await dashboard.refresh([]);await dashboard.refresh([]);expect(calls).toBe(1);await dashboard.refresh([],true);expect(calls).toBe(2);
  const late=mountDashboard({config:readDashboard(config,new Map(),"demo"),read:()=>new Promise(r=>{resolve=r;}),apply:()=>{},selected:()=>false});document.body.append(late.element);
  const pending=late.refresh([]);late.dispose();resolve(response);await pending;expect(late.element.textContent).not.toContain("2");dashboard.dispose();
});
/// (partial: wrong types in response members refuse instead of being ignored)
test("dashboard refuses malformed count, period and numeric response values",async()=>{
  for(const raw of [{...response,values:[{...response.values[0],value:"-1"}]},{...response,periods:[{id:"week",label:"Week",kind:"rolling",days:"7"}]},{...response,periods:[{id:"week",label:"Week",kind:["month"],days:null}]}]){
    const dashboard=mountDashboard({config:readDashboard(config,new Map(),"demo"),read:async()=>raw,apply:()=>{},selected:()=>false});document.body.append(dashboard.element);await dashboard.refresh([]);
    expect(dashboard.element.querySelector("[role=alert]")?.textContent).toContain("Dashboard unavailable:");expect(dashboard.domains().size).toBe(0);dashboard.dispose();
  }
});

/// (partial: cumulative declared-filter capacity cannot be bypassed by repeated registration)
test("dashboard filter definitions enforce their cumulative bound",()=>{
  const definitions=Array.from({length:32},(_,i)=>({id:`dashboard_${i}`,description:`Metric ${i}`,domain:[],isActive:false}));
  const model=new SearchModel({model:"demo",fields:new Map([["name",field]]),filters:definitions});
  try {model.addDeclaredFilters([{id:"overflow",description:"Overflow",domain:[],isActive:false}]);throw new Error("Expected rejection");}
  catch(error){expect(error).toMatchObject({kind:"validation",message:"Invalid Duplicate or excessive declared filters.."});}
  expect(model.getFilters()).toHaveLength(32);
});

/// (partial: a card kept in the address but not offered now is dropped, other saved filters stay for the search model)
test("an address keeps only the dashboard cards that are registered now", () => {
  const params = new URLSearchParams({ search_filters: JSON.stringify(["dashboard_open", "archived", "dashboard_late", "created:month"]) });
  dropAbsentDashboardFilters(params, new Set(["dashboard_late", "archived"]));
  expect(JSON.parse(params.get("search_filters")!)).toEqual(["archived", "dashboard_late", "created:month"]);
  const only = new URLSearchParams({ search_filters: '["dashboard_open"]', search: "x" });
  dropAbsentDashboardFilters(only, new Set());
  expect(only.has("search_filters")).toBe(false); expect(only.get("search")).toBe("x");
  const malformed = new URLSearchParams({ search_filters: "not json" });
  dropAbsentDashboardFilters(malformed, new Set());
  expect(malformed.get("search_filters")).toBe("not json"); // left for the search model's own refusal
});

/// (partial: focus after Retry)
test("dashboard moves focus to the first card after Retry succeeds, and to the new Retry when it fails again",async()=>{
  let mode:"fail"|"ok"="fail";
  const dashboard=mountDashboard({config:readDashboard(config,new Map(),"demo"),read:async()=>{if(mode==="fail")throw new Error("Failed read");return response;},apply:()=>{},selected:()=>false});
  document.body.append(dashboard.element);await dashboard.refresh([]);
  const retry=()=>[...dashboard.element.querySelectorAll<HTMLButtonElement>("[role=alert] button")].find(b=>b.textContent==="Retry")!;
  // Retry fails again: the old button is replaced, the new one takes the focus.
  const first=retry();first.focus();first.click();await vi.waitFor(()=>expect(retry()).not.toBe(first));
  expect(document.activeElement).toBe(retry());
  // Retry succeeds: the button is gone, the focus lands on the first enabled card.
  mode="ok";retry().focus();retry().click();
  await vi.waitFor(()=>expect(dashboard.element.querySelector("[role=alert]")).toBeNull());
  expect(document.activeElement).toBe(dashboard.element.querySelector("button[data-dashboard-item]"));
  dashboard.dispose();
});
