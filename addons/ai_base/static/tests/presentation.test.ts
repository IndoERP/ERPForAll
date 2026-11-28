/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
import { expect, test, vi } from "vitest";
import { renderReply, renderChatReply, workspacePrompt } from "../src/presentation.js";
function show(props: object) {
  const node=document.createElement("div"), openDraft=vi.fn().mockResolvedValue(undefined);
  const text=["```erp-view",JSON.stringify({version:1,title:"Report",...props}),"```"].join("\n");
  renderReply(node,text,{openDraft,run:job=>{void job();}});return {node,openDraft,text};
}
test("table text stays literal, including labels that look like HTML",()=>{
  const {node}=show({type:"table",columns:["Name","Amount"],rows:[["<img src=x>",0],["Missing",null]]});
  expect(node.querySelector("img")).toBeNull();expect(node.querySelector("td")?.textContent).toBe("<img src=x>");
  expect(node.querySelectorAll("td")[3]?.textContent).toBe("—");
});
test("negative and positive values share one chart scale",()=>{
  const {node}=show({type:"chart",series:[{label:"Prior",value:-50},{label:"Now",value:100}]});
  const bars=node.querySelectorAll<HTMLElement>(".ai-answer-bar");
  expect([bars[0]?.style.width,bars[0]?.style.left,bars[1]?.style.width]).toEqual(["25%","25%","50%"]);
});
test("slides work with keyboard and have a readable position",()=>{
  const {node}=show({type:"slides",slides:[{heading:"One",points:[]},{heading:"Two",points:["Detail"]}]});
  node.querySelector(".ai-answer-deck")?.dispatchEvent(new KeyboardEvent("keydown",{key:"ArrowRight"}));
  expect(node.querySelector("h4")?.textContent).toBe("Two");expect(node.textContent).toContain("Slide 2 of 2");
});
test("proposed defaults need a click and keep the ordinary form save boundary",()=>{
  const {node,openDraft}=show({type:"draft",form:"rfq",fields:{origin:"Store replenishment"}});
  expect(openDraft).not.toHaveBeenCalled();node.querySelector("button")!.click();
  expect(openDraft).toHaveBeenCalledWith("purchase.purchase_rfq",{origin:"Store replenishment"});
  const invalid=show({type:"draft",form:"rfq",fields:{company_id:"2",state:"done"}});
  expect(invalid.node.querySelector("button")).toBeNull();expect(invalid.node.textContent).toBe(invalid.text);
});
test("the workspace prompt only attaches the format guide to the operator's question",()=>{
  expect(workspacePrompt("Show monthly totals")).toContain("User question:\nShow monthly totals");
});
test("an existing-document answer cannot navigate until its named link is activated", () => {
  const node = document.createElement("div"), open = vi.fn().mockResolvedValue(undefined);
  const lookup = vi.fn(() => open);
  const reply = ["```erp-view", JSON.stringify({version:1,type:"record",title:"Purchase order",model:"purchase.order",reference:"PO00001"}), "```"].join("\n");
  renderReply(node, reply, {run:job=>{void job();}}, lookup);
  expect(lookup).toHaveBeenCalledWith("purchase.order","PO00001");
  expect(open).not.toHaveBeenCalled();
  const button = node.querySelector("button")!;
  expect(button.textContent).toBe("Open PO00001");
  button.click();
  expect(open).toHaveBeenCalledTimes(1);
  renderReply(node, reply, {run:job=>{void job();}}, () => undefined);
  expect(node.querySelector("button")).toBeNull();
});

test("chat renders financial figures through a sourced canvas", () => {
  const host = document.createElement("div");
  const proposal = {version:1,title:"Revenue",type:"table",columns:["Revenue"],rows:[[10000]]};
  renderChatReply(host,["```erp-view",JSON.stringify(proposal),"```"].join("\n"));
  expect(host.querySelector("table")).toBeNull();
  expect(host.textContent).toContain("live report");
});
