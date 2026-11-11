/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
import { expect, test } from "vitest";
import type { Node } from "../src/client/metadata.js";
import { renderFormLayout } from "../src/client/form-layout.js";

const field = (name: string) => ({ kind: "field", force_save: false, field: { name }, label: name, widget: "char" });
const tree = () => [{ kind: "root", children: [{ kind: "sheet", children: [
  { kind: "group", label: "Details", columns: 4, colspan: 2, children: [field("name"), field("owner")] },
  { kind: "notebook", children: [
    { kind: "page", name: "body", label: "Description", children: [field("body")] },
    { kind: "page", name: "memo", label: "Internal memo", children: [field("memo")] }
  ] }
] }] }] as unknown as Node[];
const renderField = (node: Extract<Node, {kind: "field"}>, parent: HTMLElement): void => {
  const input = document.createElement("input"); input.name = node.field.name; parent.append(input);
};

/// (partial: typed layout DOM and hierarchy; server XML covered separately)
test("root stays transparent while declared groups and sheets retain their hierarchy", () => {
  const form = document.createElement("form");
  renderFormLayout(form, tree(), renderField, new Map());
  expect(form.children).toHaveLength(1);
  expect(form.firstElementChild?.classList.contains("form-sheet-layout")).toBe(true);
  expect(form.querySelector(".form-group-title")?.textContent).toBe("Details");
  expect(form.querySelectorAll(".form-group-grid > input")).toHaveLength(2);
  expect(form.querySelectorAll("input")).toHaveLength(4);
});

/// (partial: click/keyboard tab selection and rerender state; no live RPC)
test("tab keyboard moves one active panel and preserves its choice across onchange-style rerender", () => {
  const form = document.createElement("form"); document.body.append(form);
  const nodes = tree(), selected = new Map<Node, number>();
  renderFormLayout(form, nodes, renderField, selected);
  const tabs = Array.from(form.querySelectorAll<HTMLButtonElement>('[role="tab"]'));
  tabs[0]!.focus();tabs[0]!.dispatchEvent(new KeyboardEvent("keydown",{key:"ArrowRight",bubbles:true}));
  expect(document.activeElement).toBe(tabs[1]);
  expect(tabs.map(t=>t.getAttribute("aria-selected"))).toEqual(["false","true"]);
  expect(tabs.map(t=>t.tabIndex)).toEqual([-1,0]);
  expect(Array.from(form.querySelectorAll<HTMLElement>('[role="tabpanel"]')).map(p=>p.hidden)).toEqual([true,false]);
  form.replaceChildren();renderFormLayout(form,nodes,renderField,selected);
  expect(form.querySelector('[role="tab"][aria-selected="true"]')?.textContent).toBe("Internal memo");
  const last=form.querySelector<HTMLButtonElement>('[role="tab"][aria-selected="true"]')!;
  last.dispatchEvent(new KeyboardEvent("keydown",{key:"Home",bubbles:true}));
  expect(form.querySelector('[role="tab"][aria-selected="true"]')?.textContent).toBe("Description");
  form.remove();
});

/// (partial: invalid field's containing page becomes visible before native validity)
test("validation reveals a field on another page without replacing inputs or edits", () => {
  const form=document.createElement("form"), selected=new Map<Node,number>();
  const layout=renderFormLayout(form,tree(),renderField,selected);
  const memo=form.querySelector<HTMLInputElement>('input[name="memo"]')!;memo.value="Keep this draft";
  expect(memo.closest<HTMLElement>('[role="tabpanel"]')?.hidden).toBe(true);
  layout.revealField(memo);
  expect(memo.closest<HTMLElement>('[role="tabpanel"]')?.hidden).toBe(false);
  expect(form.querySelector('input[name="memo"]')).toBe(memo);
  expect(memo.value).toBe("Keep this draft");
});

/// (partial: lifted notebook navigation retains page ownership and invalid-field reveal)
test("optional sheet navigation host moves tabs without moving page contents or losing reveal", () => {
  const form = document.createElement("form"), tabsHost = document.createElement("div");
  const layout = renderFormLayout(form, tree(), renderField, new Map(), undefined, undefined, { notebookTabsHost: tabsHost });
  expect(tabsHost.querySelectorAll('[role="tab"]')).toHaveLength(2);
  expect(form.querySelector('[role="tablist"]')).toBeNull();
  const memo = form.querySelector<HTMLInputElement>('input[name="memo"]')!;
  const panel = memo.closest<HTMLElement>('[role="tabpanel"]')!;
  expect(panel.hidden).toBe(true);
  layout.revealField(memo);
  expect(panel.hidden).toBe(false);
  expect(tabsHost.querySelector('[aria-selected="true"]')?.getAttribute("aria-controls")).toBe(panel.id);
});

/// (partial: admitted aside content is explicit, singular and omitted when fully hidden)
test("aside presentation keeps field order and never creates an empty rail", () => {
  const form = document.createElement("form"), asideHost = document.createElement("aside");
  const hidden = { ...field("private"), field: { name: "private", invisible: true } };
  const nodes = [{ kind: "root", children: [{ kind: "sheet", children: [field("body")] },
    { kind: "aside", children: [{ kind: "group", label: "Record information", columns: 2, colspan: 2, children: [hidden] }] }
  ] }] as unknown as Node[];
  renderFormLayout(form, nodes, renderField, new Map(), undefined, undefined, { asideHost });
  expect(asideHost.children).toHaveLength(0);
  expect([...form.querySelectorAll("input")].map(input => input.name)).toEqual(["body"]);
  const root = nodes[0]!;
  if (root.kind !== "root") throw new Error("Fixture requires a root.");
  const aside = root.children[1]!;
  if (aside.kind !== "aside") throw new Error("Fixture requires an aside.");
  aside.children.push(field("created_on") as unknown as Node);
  form.replaceChildren();
  renderFormLayout(form, nodes, renderField, new Map(), undefined, undefined, { asideHost });
  expect([...asideHost.querySelectorAll("input")].map(input => input.name)).toEqual(["created_on"]);
  expect(asideHost.textContent).not.toContain("Record information");
});

/// (partial: slot only where a chatter host is passed; flags carried as data)
test("chatter slot mounts only in the main-route host and renders nothing elsewhere", () => {
  const chatter: Extract<Node, { kind: "chatter" }> = { kind: "chatter", reloadOnPost: true, reloadOnAttachment: false, reloadOnActivity: false, reloadOnFollower: true, openAttachments: false, hasActivities: true };
  const sheet = { kind: "sheet", children: [field("body")] } as unknown as Node;
  const nodes: Node[] = [{ kind: "root", children: [sheet, chatter] }];
  const form = document.createElement("form"), chatterHost = document.createElement("aside");
  renderFormLayout(form, nodes, renderField, new Map(), undefined, undefined, { chatterHost });
  const slot = chatterHost.querySelector<HTMLElement>(".form-chatter-slot")!;
  expect(chatterHost.children).toHaveLength(1);
  expect(slot.childElementCount).toBe(0);
  expect({ ...slot.dataset }).toEqual({ reloadOnPost: "true", reloadOnAttachment: "false", reloadOnActivity: "false", reloadOnFollower: "true", openAttachments: "false", hasActivities: "true" });
  expect(form.querySelector(".form-chatter-slot")).toBeNull();
  const dialog = document.createElement("form");
  renderFormLayout(dialog, nodes, renderField, new Map());
  expect(dialog.querySelector(".form-chatter-slot")).toBeNull();
  expect([...dialog.querySelectorAll("input")].map(input => input.name)).toEqual(["body"]);
});

/// (partial: typed title placement and nested page ownership)
test("explicit title uses arbitrary declared fields and only lifts the always-visible title", () => {
  const form = document.createElement("form"), titleHost = document.createElement("div");
  const nodes = [{ kind: "root", children: [{ kind: "sheet", children: [
    { kind: "title", children: [{ kind: "heading", level: 1, children: [field("display_label")] }] },
    { kind: "notebook", children: [{ kind: "page", name: "extra", label: "Extra", children: [
      { kind: "title", children: [{ kind: "heading", level: 2, children: [field("caption")] }] }
    ] }] }
  ] }] }] as unknown as Node[];
  renderFormLayout(form, nodes, renderField, new Map(), undefined, undefined, { titleHost });
  expect(titleHost.querySelector("h1 input")?.getAttribute("name")).toBe("display_label");
  expect(form.querySelector('[role="tabpanel"] h2 input')?.getAttribute("name")).toBe("caption");
  expect(titleHost.querySelectorAll("input")).toHaveLength(1);
});

/// (partial: actual statistic DOM inside an actionable control, empty box omitted)
test("XML smart buttons stay within sheet, preserve zero and never create editable nested controls", () => {
  const statField = { ...field("count"), widget: "statinfo", label: "Lines", field: { name:"count", readonly:true } };
  const action = {kind:"stat_button",name:"open_lines",label:"Lines",variant:"secondary",confirm:null,invisible:null,children:[statField]};
  const nodes = [{kind:"root",children:[{kind:"sheet",children:[{kind:"button_box",children:[action]}]}]}] as unknown as Node[];
  const host = document.createElement("form");
  let clicks = 0;
  renderFormLayout(host, nodes, (_node,parent) => { const span=document.createElement("span");span.textContent="0 Lines";parent.append(span); }, new Map(), (node,parent) => {
    const control=document.createElement("button");control.type="button";control.textContent=node.label;control.onclick=()=>{clicks++;};parent.append(control);return control;
  });
  const control=host.querySelector<HTMLButtonElement>(".form-sheet-layout > .form-button-box > .form-stat-button")!;
  expect(control.textContent).toBe("0 Lines");expect(control.getAttribute("aria-label")).toBe("0 Lines");
  control.click(); expect(clicks).toBe(1);expect(control.querySelector("input")).toBeNull();
  const empty=document.createElement("form");
  renderFormLayout(empty,nodes,renderField,new Map(),()=>undefined);
  expect(empty.querySelector(".form-button-box")).toBeNull();
});

/// (partial: pruned/hidden title children create no empty external region)
test("a title with no authorized visible fields creates no empty heading", () => {
  const form=document.createElement("form"), titleHost=document.createElement("div");
  const nodes=[{kind:"root",children:[{kind:"title",children:[{kind:"heading",level:1,children:[]}]}]}] as Node[];
  renderFormLayout(form,nodes,renderField,new Map(),undefined,undefined,{titleHost});
  expect(titleHost.children).toHaveLength(0);
});
