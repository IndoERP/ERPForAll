/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
import { expect, test } from "vitest";
import { MethodActions } from "../src/client/method-actions.js";
import { parseRoute, routeUrl } from "../src/client/route.js";

/// (partial: bounded storage and closed route identity)
/// (partial: real page-local identity survives route transport and expires on a fresh page)
test("history carries a bounded identity; absent or evicted state never loses the filter silently", () => {
  const actions = new MethodActions();
  const entry = {result:{kind:"current_window" as const,name:"Related",model:"test.book",views:[{id:null,type:"list" as const}],domain:[["id","=",7]],context:{}},context:{allowed_company_ids:[1]},origin:{action:"20"},originLabel:"Mouse"};
  const first = actions.add(entry);
  entry.result.domain[0]![2] = 99;
  expect(actions.get(first).result.domain).toEqual([["id","=",7]]);
  const route = {action:"20",method_action:first,view_type:"list" as const};
  const url = routeUrl(route);
  expect(url).not.toContain("domain");
  expect(parseRoute(new URL(url,"https://example.invalid"))).toEqual(route);
  const anotherPage = new MethodActions();
  const otherId = anotherPage.add(entry);
  expect(otherId).not.toBe(first);
  expect(()=>anotherPage.get(first)).toThrowError(expect.objectContaining({kind:"validation",message:"This related action has expired. Open its originating menu again."}));
  expect(()=>parseRoute(new URL("/erp/action-20?method_action=1","https://example.invalid"))).toThrowError(expect.objectContaining({kind:"validation",message:"Invalid navigation parameter: method_action."}));
  for(let i=0;i<64;i++) actions.add(entry);
  expect(()=>actions.get(first)).toThrowError(expect.objectContaining({kind:"validation",message:"This related action has expired. Open its originating menu again."}));
  actions.clear();
  expect(()=>actions.get("65")).toThrowError(expect.objectContaining({kind:"validation",message:"This related action has expired. Open its originating menu again."}));
});
