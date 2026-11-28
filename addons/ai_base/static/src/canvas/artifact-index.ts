/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
import { icon } from "@erpforall/client";

type Glyph = Parameters<typeof icon>[0];
const formats: Record<string,[string,Glyph]> = {
  chart:["Chart","chart-column"], table:["Table","list"], pivot:["Pivot","layout-grid"],
  dashboard:["Dashboard","layout-grid"], flow:["Workflow","route"], simulation:["Scenario","sliders-horizontal"],
  document:["Document","file-text"], brief:["Brief","book-open"], slides:["Presentation","presentation"],
  checklist:["Checklist","clipboard-list"], timeline:["Timeline","calendar-days"],
};
export function artifactFormat(kind: string): [string,Glyph] {
  return Object.hasOwn(formats,kind) ? formats[kind]! : ["Artifact","file-text"];
}
let nextStage = 0;

/** Keep each panel mounted so selection retains slide position, filters, and scroll. */
export function artifactIndex(panels: HTMLElement, empty: HTMLElement, changed: (count: number) => void) {
  const prefix = `canvas-${++nextStage}`;
  let serial = 0, selected = "", dragging = "";
  const entries = new Map<string,{panel:HTMLElement; tab:HTMLButtonElement; option:HTMLOptionElement}>();
  const rail = document.createElement("aside"); rail.className = "studio-index";
  const heading = document.createElement("h3"); heading.textContent = "Artifacts";
  const tabs = document.createElement("div"); tabs.className = "studio-tabs";
  tabs.setAttribute("role","tablist"); tabs.setAttribute("aria-label","Artifacts"); tabs.setAttribute("aria-orientation","vertical");
  rail.append(heading,tabs);
  const picker = document.createElement("select"); picker.className = "studio-picker";
  picker.setAttribute("aria-label","Choose artifact");
  picker.onchange = () => select(picker.value);
  const arrange = document.createElement("details"); arrange.className = "studio-arrange";
  const summary = document.createElement("summary"); summary.textContent = "Reorder";
  const earlier = document.createElement("button"), later = document.createElement("button");
  const moveBar = document.createElement("div");
  earlier.type = later.type = "button"; earlier.textContent = "Move earlier"; later.textContent = "Move later";
  earlier.onclick = () => shift(-1); later.onclick = () => shift(1);
  moveBar.append(earlier,later); arrange.append(summary,moveBar);
  const ordered = () => Array.from(tabs.children, el => (el as HTMLButtonElement).value);

  function updateOrder() {
    const order = ordered(), position = order.indexOf(selected);
    earlier.disabled = position <= 0; later.disabled = position < 0 || position >= order.length - 1;
    arrange.hidden = order.length < 2;
    if (arrange.hidden) arrange.open = false;
  }
  function shift(delta: number) {
    const order = ordered(), position = order.indexOf(selected), target = order[position + delta];
    if (target) { move(selected,target,delta > 0); summary.focus(); }
  }
  function updateCount() {
    updateOrder();
    rail.hidden = picker.hidden = !entries.size;
    changed(entries.size);
  }
  function select(id: string, focus = false) {
    const next = entries.get(id);
    if (!next) return false;
    const old = entries.get(selected);
    if (old && old !== next) {
      old.panel.hidden = true; old.tab.tabIndex = -1; old.tab.setAttribute("aria-selected","false");
    }
    selected = id; picker.value = id; next.panel.hidden = false;
    next.tab.tabIndex = 0; next.tab.setAttribute("aria-selected","true");
    updateOrder();
    if (focus) next.tab.focus();
    return true;
  }
  function move(id: string, target: string, after: boolean) {
    const from = entries.get(id), to = entries.get(target);
    if (!from || !to || from === to) return;
    tabs.insertBefore(from.tab, after ? to.tab.nextSibling : to.tab);
    picker.insertBefore(from.option, after ? to.option.nextSibling : to.option);
    panels.insertBefore(from.panel, after ? to.panel.nextSibling : to.panel);
    picker.value = selected; updateOrder();
  }
  function add(panel: HTMLElement, kind: string, title: string, activate = true) {
    const id = `${prefix}-${++serial}`, tab = document.createElement("button"), option = document.createElement("option");
    const [label,glyph] = artifactFormat(kind);
    const copy = document.createElement("span"), type = document.createElement("small"), name = document.createElement("span");
    name.textContent = title; type.textContent = label; copy.append(name,type);
    tab.type = "button"; tab.value = id; tab.className = "studio-tab"; tab.id = `${id}-tab`;
    tab.setAttribute("role","tab"); tab.setAttribute("aria-controls",id); tab.setAttribute("aria-selected","false");
    tab.tabIndex = -1; tab.append(icon(glyph), copy); tab.onclick = () => select(id);
    tab.draggable = true;
    tab.ondragstart = event => { dragging = id; event.dataTransfer?.setData("text/plain",id); };
    tab.ondragend = () => { dragging = ""; };
    tab.ondragover = event => { if (dragging) event.preventDefault(); };
    tab.ondrop = event => {
      if (!dragging) return;
      event.preventDefault();
      move(dragging,id,event.clientY > tab.getBoundingClientRect().top + tab.offsetHeight / 2);
      dragging = "";
    };
    panel.id = id; panel.hidden = true; panel.dataset["artifactType"] = kind;
    panel.setAttribute("role","tabpanel"); panel.setAttribute("aria-labelledby",tab.id); panel.tabIndex = 0;
    option.value = id; option.textContent = `${title} · ${label}`;
    entries.set(id,{panel,tab,option}); tabs.append(tab); picker.append(option);
    empty.remove(); panels.append(panel);
    if (!selected || activate) select(id);
    updateCount();
    return id;
  }
  function remove(id: string) {
    const entry = entries.get(id);
    if (!entry) return;
    const order = ordered(), index = order.indexOf(id);
    entry.panel.remove(); entry.tab.remove(); entry.option.remove(); entries.delete(id);
    if (selected === id) {
      selected = "";
      const following = order[index+1] ?? order[index-1];
      if (following) {
        select(following);
        if (picker.getClientRects().length) picker.focus();
        else entries.get(following)?.tab.focus();
      } else { panels.append(empty); empty.focus(); }
    }
    updateCount();
  }
  function clear() {
    entries.clear(); tabs.replaceChildren(); picker.replaceChildren(); selected = dragging = "";
    panels.replaceChildren(empty); updateCount();
  }
  tabs.onkeydown = event => {
    if (!(event.target instanceof HTMLButtonElement)) return;
    const order = ordered(), current = order.indexOf(event.target.value);
    const delta = ["ArrowDown","ArrowRight"].includes(event.key) ? 1 : ["ArrowUp","ArrowLeft"].includes(event.key) ? -1 : 0;
    if (!delta && event.key !== "Home" && event.key !== "End") return;
    event.preventDefault();
    const target = order[event.key === "Home" ? 0 : event.key === "End" ? order.length-1 : (current+delta+order.length)%order.length]!;
    if (event.altKey && delta) { shift(delta); event.target.focus(); }
    else select(target,true);
  };
  clear();
  return {rail,picker,arrange,add,remove,clear,select};
}
