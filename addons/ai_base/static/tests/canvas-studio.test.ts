/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
import { afterEach, expect, test, vi } from "vitest";
import { createCanvasStage } from "../src/canvas/canvas-stage.js";

afterEach(() => document.body.replaceChildren());
const doc = (title: string, text = title) => ({ canvas_type:"document", title,
  data: { sections:[{kind:"heading",text:"Decisions"},{kind:"paragraph",text}] } });

test("each output is an addressable document and the compact picker selects the same artifact as the outline", () => {
  const stage = createCanvasStage();
  document.body.append(stage.node);
  const one = stage.renderArtifact(doc("Cutover notes", "<script>literal</script>"));
  const two = stage.renderArtifact({ canvas_type:"timeline",title:"Implementation",data:{items:[{when:"Week 1",label:"Sign off",detail:"Controller"}]} });
  const outline = stage.node.querySelector<HTMLElement>('[role=tablist][aria-label="Artifacts"]')!;
  const tabs = [...outline.querySelectorAll<HTMLButtonElement>('[role=tab]')];
  expect(tabs).toHaveLength(2);
  expect(tabs[1]!.getAttribute("aria-selected")).toBe("true");
  const picker = stage.node.querySelector<HTMLSelectElement>('select[aria-label="Choose artifact"]')!;
  picker.value = one;
  picker.dispatchEvent(new Event("change"));
  expect(tabs[0]!.getAttribute("aria-selected")).toBe("true");
  const active = stage.node.querySelector<HTMLElement>('[role=tabpanel]:not([hidden])')!;
  expect(active.textContent).toContain("<script>literal</script>");
  expect(active.querySelector("script")).toBeNull();
  expect(tabs[0]!.getAttribute("aria-controls")).toBe(active.id);
  tabs[0]!.focus();
  tabs[0]!.dispatchEvent(new KeyboardEvent("keydown",{key:"ArrowRight",bubbles:true}));
  expect(document.activeElement).toBe(tabs[1]);
  expect(picker.value).toBe(two);
  expect(stage.selectArtifact("unknown")).toBe(false);
});
test("a newly received result preserves earlier reading focus and slide progress", () => {
  const stage = createCanvasStage();
  document.body.append(stage.node);
  const deck = stage.renderArtifact({canvas_type:"slides",title:"Review",data:{slides:[
    {heading:"Plan",points:["Methods"]},{heading:"Risks",points:["Thresholds"]}]}});
  const slide = stage.node.querySelector<HTMLElement>(".ai-answer-deck")!;
  slide.focus();
  slide.dispatchEvent(new KeyboardEvent("keydown",{key:"ArrowRight"}));
  stage.renderArtifact(doc("Follow-up actions"),{activate:false});
  expect(stage.node.querySelector<HTMLElement>('[role=tabpanel]:not([hidden])')?.contains(slide)).toBe(true);
  expect(slide.textContent).toContain("Slide 2 of 2");
  expect(document.activeElement).toBe(slide);
  expect(stage.selectArtifact(deck)).toBe(true);
});
test("mixed sections, checklist statuses, and timelines use literal semantic markup", () => {
  const stage = createCanvasStage();
  stage.renderArtifact({ canvas_type:"brief",title:"Implementation decision",data:{ sections:[
    {kind:"paragraph",text:"Choose the initial warehouse."},
    {kind:"list",items:["Primary warehouse","Transit warehouse"]},{kind:"callout",text:"Confirm the cutoff"},
    {kind:"checklist",items:[{label:"Review scope",state:"completed"},{label:"Training",state:"pending"}]},
    {kind:"timeline",items:[{when:"Friday",label:"Dry run"}]}
  ] } });
  const panel = stage.node.querySelector<HTMLElement>('[role=tabpanel]:not([hidden])')!;
  expect(panel.textContent).toContain("Confirm the cutoff");
  expect(panel.textContent).toContain("Completed");
  expect(panel.querySelectorAll("li").length).toBe(5);
  expect(panel.querySelector("input")).toBeNull();
});
test("invalid sections explain the error without dropping existing work and exclusive formats do not offer chart controls", () => {
  const stage = createCanvasStage(vi.fn());
  const first = stage.renderArtifact(doc("Valid"));
  stage.renderArtifact({canvas_type:"document",title:"Malformed",data:{sections:[{kind:"html",text:"<b>hello</b>"}]}});
  expect(stage.node.querySelector('[role=alert]')?.textContent).toMatch(/section/i);
  expect(stage.selectArtifact(first)).toBe(true);
  const panel = stage.node.querySelector<HTMLElement>('[role=tabpanel]:not([hidden])')!;
  expect(panel.querySelector('.canvas-view-switcher:not([hidden])')).toBeNull();
  expect(panel.textContent).toContain("Decisions");
});

test("a brief supports comparison matrices, selectable code and labelled facts as inert text", () => {
  const stage = createCanvasStage();
  stage.renderArtifact({canvas_type:"brief",title:"Data migration",data:{sections:[
    {kind:"table",columns:["Source","Target"],rows:[["Company","res.company"]]},
    {kind:"code",language:"sql",text:"SELECT '<button>'"},
    {kind:"facts",items:[{label:"Owner",value:"Finance"}]}
  ]}});
  const panel = stage.node.querySelector<HTMLElement>('[role=tabpanel]:not([hidden])')!;
  expect(panel.querySelectorAll("thead th")).toHaveLength(2);
  expect(panel.querySelector("td")?.textContent).toBe("Company");
  expect(panel.querySelector("pre code")?.textContent).toBe("SELECT '<button>'");
  expect(panel.querySelector("pre button")).toBeNull();
  expect(panel.querySelector("dl dt")?.textContent).toBe("Owner");
  expect(panel.querySelector("dl dd")?.textContent).toBe("Finance");
  for (const section of [
    {kind:"table",columns:["a","b"],rows:[["a"]]},
    {kind:"table",columns:Array(13).fill("a"),rows:[["a"]]},
    {kind:"code",text:"x",language:"x".repeat(33)},
    {kind:"facts",items:[{label:"Missing value"}]},
  ]) {
    stage.renderArtifact({canvas_type:"document",title:"Incomplete",data:{sections:[section]}});
    expect(stage.node.querySelector('[role=tabpanel]:not([hidden]) [role=alert]')).not.toBeNull();
  }
});

test("an ungrouped ERP dashboard shows every returned measure and retains the exact decimal", () => {
  const stage = createCanvasStage();
  stage.renderArtifact({canvas_type:"dashboard",title:"Order metrics",data:{
    source_model:"sale.order",dimensions:[],measures:[{key:"id:count",label:"id:count"},{key:"amount:avg",label:"Average amount"}],
    records:[{"id:count":17,"amount:avg":"2384.2300"}],series:[{label:"All matching records",value:17}]
  }});
  const metrics = stage.node.querySelector(".studio-metrics");
  expect(metrics).not.toBeNull();
  expect([...metrics!.querySelectorAll("dd")].map(item=>item.textContent)).toEqual(["17","2384.2300"]);
  expect([...metrics!.querySelectorAll("dt")].map(item=>item.textContent)).toEqual(["ID · Count","Average amount"]);
});

test("a single chart category is centered and chart tabs name their visualization panel", () => {
  const stage = createCanvasStage();
  stage.renderArtifact({canvas_type:"chart",title:"Open orders",data:{series:[{label:"Open",value:22}]}});
  const plot = stage.node.querySelector(".canvas-chart svg")!;
  const rect = plot.querySelector("rect")!;
  const width = Number(plot.getAttribute("viewBox")?.split(" ")[2]);
  expect(Number(rect.getAttribute("x")) + Number(rect.getAttribute("width")) / 2).toBeCloseTo(width / 2);
  const line = stage.node.querySelector<HTMLButtonElement>('button[data-mode=line]')!;
  line.click();
  const body = stage.node.querySelector(".canvas-card-body")!;
  expect(line.getAttribute("aria-controls")).toBe(body.id);
  expect(body.getAttribute("aria-labelledby")).toBe(line.id);
  expect(Number(body.querySelector("circle")?.getAttribute("cx"))).toBeCloseTo(width / 2);
});

test("reordering preserves selection and updates the compact picker and desktop outline together", () => {
  const stage = createCanvasStage();
  const first = stage.renderArtifact(doc("First")), second = stage.renderArtifact(doc("Second"));
  const earlier = Array.from(stage.node.querySelectorAll("button")).find(b=>b.textContent==="Move earlier")!;
  earlier.click();
  expect([...stage.node.querySelectorAll<HTMLOptionElement>(".studio-picker option")].map(option=>option.value)).toEqual([second,first]);
  expect([...stage.node.querySelectorAll<HTMLButtonElement>(".studio-tab")].map(tab=>tab.value)).toEqual([second,first]);
  expect(stage.node.querySelector<HTMLSelectElement>(".studio-picker")?.value).toBe(second);
  expect(earlier.disabled).toBe(true);
});

test("free-form assistant text cannot claim ERP provenance, source permission, or report-save actions", () => {
  const stage=createCanvasStage(undefined,vi.fn(),vi.fn());
  const artifact={canvas_type:"chart",title:"Unverified",data:{source_model:"sale.order",source_domain:[],analysis_definition:{panels:[]},series:[{label:"Sales",value:42}]}};
  expect(stage.detectAndRender(["```erp-view",JSON.stringify(artifact),"```"].join("\n"))).toBe(false);
  expect(stage.node.querySelector(".studio-tab")).toBeNull();
  const readonly=createCanvasStage();
  readonly.renderArtifact(artifact);
  expect([...readonly.node.querySelectorAll("button")].some(button=>["Save report","Open source records"].includes(button.textContent??""))).toBe(false);
});
