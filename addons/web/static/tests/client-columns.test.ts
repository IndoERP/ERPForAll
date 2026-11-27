/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
import {afterEach, expect, test, vi} from "vitest";
import {fitColumns,resizeColumns} from "../src/client/columns.js";
afterEach(()=>{document.body.replaceChildren();vi.restoreAllMocks();});
function table(widths:Map<string,number>):HTMLTableElement {
 const table=document.createElement("table");table.innerHTML='<thead><tr><th></th><th>Name</th><th>Login</th></tr></thead><tbody><tr><td></td><td>A name</td><td>A login</td></tr></tbody>';
 document.body.append(table);
 for(const th of table.querySelectorAll("th"))vi.spyOn(th,"getBoundingClientRect").mockReturnValue({width:200} as DOMRect);
 resizeColumns(table,[{name:"name",label:"Name"},{name:"login",label:"Login"}],widths);return table;
}
test("keyboard resize preserves column identity and bounds across list redraws",()=>{
 const widths=new Map<string,number>(),first=table(widths),handle=first.querySelector<HTMLElement>('[aria-label="Resize Name column"]')!;
 handle.dispatchEvent(new KeyboardEvent("keydown",{key:"ArrowRight",bubbles:true}));
 expect(widths.get("name")).toBe(210);expect(widths.get("login")).toBe(200);expect(handle.getAttribute("aria-valuenow")).toBe("210");
 const next=table(widths);expect(next.querySelectorAll("col")[1]!.style.width).toBe("210px");
 for(let i=0;i<500;i++)handle.dispatchEvent(new KeyboardEvent("keydown",{key:"ArrowLeft",bubbles:true}));expect(widths.get("name")).toBe(80);
 for(let i=0;i<500;i++)handle.dispatchEvent(new KeyboardEvent("keydown",{key:"ArrowRight",bubbles:true}));expect(widths.get("name")).toBe(4096);
});
test("pointer resize clamps width, ignores another pointer and stops on cancellation",()=>{
 const widths=new Map<string,number>(),first=table(widths),handle=first.querySelector<HTMLElement>('[aria-label="Resize Name column"]')!;
 const pointer=(name:string,id:number,x:number)=>{const event=new MouseEvent(name,{button:0,clientX:x,bubbles:true});Object.defineProperty(event,"pointerId",{value:id});handle.dispatchEvent(event);};
 // A press, and another pointer's move, change nothing: the column is pinned only once its own drag moves it.
 pointer("pointerdown",1,100);pointer("pointermove",2,180);expect(widths.get("name")).toBeUndefined();
 pointer("pointermove",1,150);expect(widths.get("name")).toBe(250);pointer("pointercancel",1,150);pointer("pointermove",1,300);expect(widths.get("name")).toBe(250);
});
test("first draw sizes columns to their content, then freezes them with the spare room shared to fill the container",async()=>{
 const widths=new Map<string,number>(),host=document.createElement("div");document.body.append(host);
 Object.defineProperty(host,"clientWidth",{configurable:true,value:840});
 const t=document.createElement("table");t.innerHTML='<thead><tr><th></th><th>Name</th><th>Login</th></tr></thead><tbody></tbody>';host.append(t);
 const [,name,login]=[...t.querySelectorAll("th")];
 vi.spyOn(name!,"getBoundingClientRect").mockReturnValue({width:300} as DOMRect);vi.spyOn(login!,"getBoundingClientRect").mockReturnValue({width:100} as DOMRect);
 resizeColumns(t,[{name:"name",label:"Name"},{name:"login",label:"Login"}],widths);
 expect(t.classList.contains("list-auto-layout")).toBe(true);expect(widths.size).toBe(0);
 await new Promise(resolve=>requestAnimationFrame(()=>resolve(undefined)));
 // 800 px for two flexible columns measured 300 and 100: the 400 px left over is shared evenly (500 and 300), the
 // selector stays 40 px.
 expect(t.classList.contains("list-auto-layout")).toBe(false);expect(widths.get("name")).toBe(500);expect(widths.get("login")).toBe(300);
 expect(t.style.tableLayout).toBe("fixed");expect(t.style.width).toBe("840px");
});
test("a width the user already set is kept and the list draws frozen at once",()=>{
 const widths=new Map<string,number>([["name",333],["login",222]]),t=table(widths);
 expect(t.classList.contains("list-auto-layout")).toBe(false);expect(t.querySelectorAll("col")[1]!.style.width).toBe("333px");expect(t.style.width).toBe("595px");
});
// Live 2026-11-07 (deploy 7b11f0b18): the class came off before the measurement, so the fixed layout measured six equal
// columns; and a list is built off-screen, so the first frame can see an unmounted table.
const frame=()=>new Promise(resolve=>requestAnimationFrame(()=>resolve(undefined)));
test("columns are measured while the content layout is in force, after the list is mounted",async()=>{
 const widths=new Map<string,number>(),host=document.createElement("div");
 Object.defineProperty(host,"clientWidth",{configurable:true,value:440});
 const t=document.createElement("table");t.innerHTML='<thead><tr><th></th><th>Name</th><th>Login</th></tr></thead><tbody></tbody>';host.append(t);
 const [,name,login]=[...t.querySelectorAll("th")];
 // Content widths only while the auto layout is on; the fixed layout would split evenly.
 const sized=(content:number)=>({width:t.classList.contains("list-auto-layout")?content:200} as DOMRect);
 vi.spyOn(name!,"getBoundingClientRect").mockImplementation(()=>sized(300));vi.spyOn(login!,"getBoundingClientRect").mockImplementation(()=>sized(100));
 resizeColumns(t,[{name:"name",label:"Name"},{name:"login",label:"Login"}],widths);
 await frame();await frame();
 expect(widths.size).toBe(0);expect(t.classList.contains("list-auto-layout")).toBe(true);
 document.body.append(host);
 await frame();
 expect(widths.get("name")).toBe(300);expect(widths.get("login")).toBe(100);
 expect(t.classList.contains("list-auto-layout")).toBe(false);expect(t.style.width).toBe("440px");
});

test("in a container narrower than the content the columns shrink to fit, never below 80 px",async()=>{
 const fit=async(container:number,contents:number[])=>{
  const widths=new Map<string,number>(),host=document.createElement("div");
  Object.defineProperty(host,"clientWidth",{configurable:true,value:container});
  const t=document.createElement("table");
  t.innerHTML=`<thead><tr><th></th>${contents.map((_,i)=>`<th>C${i}</th>`).join("")}</tr></thead><tbody></tbody>`;host.append(t);document.body.append(host);
  [...t.querySelectorAll("th")].slice(1).forEach((th,i)=>vi.spyOn(th,"getBoundingClientRect").mockImplementation(()=>({width:t.classList.contains("list-auto-layout")?contents[i]!:200} as DOMRect)));
  resizeColumns(t,contents.map((_,i)=>({name:`c${i}`,label:`C${i}`})),widths);
  await frame();
  return { widths: contents.map((_,i)=>widths.get(`c${i}`)), table: t.style.width };
 };
 // A phone (390 px host): a 480 px long column and a 100 px one fit 350 px. Both give up the excess evenly; the short
 // one stops at 80 px and the long one gives the rest: 80 and 270.
 expect(await fit(390,[600,100])).toEqual({ widths: [270, 80], table: "390px" });
 // Too narrow for the minimum: every column keeps 80 px (the list scrolls, as before).
 expect(await fit(200,[300,300,300])).toEqual({ widths: [80, 80, 80], table: "280px" });
 // A wide host is still filled.
 expect(await fit(840,[300,100])).toEqual({ widths: [500, 300], table: "840px" });
});

test("the last sortable column leaves room for its sort button, touch chooser and resize handle", () => {
  const host = document.createElement("div"), t = document.createElement("table");
  Object.defineProperty(host, "clientWidth", { value: 200 });
  t.innerHTML = '<thead><tr><th></th><th>Name</th><th class="sortable-column column-choices-anchor">Status</th></tr></thead><tbody></tbody>';
  document.body.append(host); host.append(t);
  const widths = new Map<string, number>();
  const fields = [{ name: "name", label: "Name", kind: "char" }, { name: "state", label: "Status", kind: "selection" }];
  resizeColumns(t, fields, widths)();
  const handle = t.querySelector<HTMLElement>('[aria-label="Resize Status column"]')!;
  const minimum = Number(handle.getAttribute("aria-valuemin"));
  // 12px cell padding, 44px touch chooser, 16px resize gutter and 6px between controls.
  const controls = 12 + 44 + 16 + 6;
  expect(widths.get("state")! - controls).toBeGreaterThanOrEqual(44);
  handle.dispatchEvent(new KeyboardEvent("keydown", { key: "ArrowLeft", bubbles: true }));
  expect(widths.get("state")).toBe(minimum);
  expect(minimum - controls).toBeGreaterThanOrEqual(44);
});

test("settle after attaching sizes the columns in the same task, so the content layout is never painted",()=>{
 const widths=new Map<string,number>(),host=document.createElement("div");
 Object.defineProperty(host,"clientWidth",{configurable:true,value:440});
 const t=document.createElement("table");t.innerHTML='<thead><tr><th></th><th>Name</th><th>Login</th></tr></thead><tbody></tbody>';
 const [,name,login]=[...t.querySelectorAll("th")];
 const sized=(content:number)=>({width:t.classList.contains("list-auto-layout")?content:200} as DOMRect);
 vi.spyOn(name!,"getBoundingClientRect").mockImplementation(()=>sized(300));vi.spyOn(login!,"getBoundingClientRect").mockImplementation(()=>sized(100));
 const settle=resizeColumns(t,[{name:"name",label:"Name"},{name:"login",label:"Login"}],widths);
 // Built off-screen: nothing to measure yet, the content layout waits.
 settle(); expect(widths.size).toBe(0); expect(t.classList.contains("list-auto-layout")).toBe(true);
 host.append(t); document.body.append(host);
 settle();
 expect(widths.get("name")).toBe(300); expect(widths.get("login")).toBe(100);
 expect(t.classList.contains("list-auto-layout")).toBe(false); expect(t.style.tableLayout).toBe("fixed"); expect(t.style.width).toBe("440px");
 // A second call (or the pending frame) changes nothing.
 widths.set("name",250); settle(); expect(widths.get("name")).toBe(250);
});
test("a table wider than its list is flagged so it can scroll sideways; a fitted one is not", () => {
 const host=document.createElement("div");document.body.append(host);
 Object.defineProperty(host,"clientWidth",{configurable:true,value:500});
 const t=document.createElement("table");t.innerHTML='<thead><tr><th></th><th>Name</th><th>Login</th></tr></thead><tbody></tbody>';host.append(t);
 const widths=new Map<string,number>([["name",300],["login",300]]);
 Object.defineProperty(t,"offsetWidth",{configurable:true,value:640});
 resizeColumns(t,[{name:"name",label:"Name"},{name:"login",label:"Login"}],widths)();
 expect(host.hasAttribute("data-wide")).toBe(true);
 Object.defineProperty(t,"offsetWidth",{configurable:true,value:500});
 t.querySelector<HTMLElement>('[aria-label="Resize Name column"]')!.dispatchEvent(new KeyboardEvent("keydown",{key:"ArrowLeft",bubbles:true}));
 expect(host.hasAttribute("data-wide")).toBe(false);
});
// Review M1: the last column draws the spare width, so a resize starts from the drawn width and then fixes the column.
test("resizing the last column starts from its drawn width, fixes it, and hands the spare room to the previous column",()=>{
 const widths=new Map<string,number>([["name",300],["login",500]]),t=table(widths),cols=t.querySelectorAll("col");
 const handle=t.querySelector<HTMLElement>('[aria-label="Resize Login column"]')!;
 expect(cols[2]!.style.width).toBe("");expect(handle.getAttribute("aria-valuenow")).toBe("200");
 handle.dispatchEvent(new KeyboardEvent("keydown",{key:"ArrowLeft",bubbles:true}));
 expect(widths.get("login")).toBe(190);expect(cols[2]!.style.width).toBe("190px");expect(handle.getAttribute("aria-valuenow")).toBe("190");
 expect(cols[1]!.style.width).toBe("");expect(t.style.minWidth).toContain("var(--list-table-min-width");
 const next=table(widths);expect(next.querySelectorAll("col")[2]!.style.width).toBe("190px");
});
test("a one-column list resized by keyboard changes its width and no longer stretches to the surface",()=>{
 const widths=new Map<string,number>([["name",500]]),t=document.createElement("table");
 t.innerHTML='<thead><tr><th></th><th>Name</th></tr></thead><tbody></tbody>';document.body.append(t);
 for(const th of t.querySelectorAll("th"))vi.spyOn(th,"getBoundingClientRect").mockReturnValue({width:600} as DOMRect);
 resizeColumns(t,[{name:"name",label:"Name"}],widths);
 const handle=t.querySelector<HTMLElement>('[aria-label="Resize Name column"]')!;
 expect(handle.getAttribute("aria-valuenow")).toBe("600");
 handle.dispatchEvent(new KeyboardEvent("keydown",{key:"ArrowLeft",bubbles:true}));
 expect(widths.get("name")).toBe(590);expect(t.querySelectorAll("col")[1]!.style.width).toBe("590px");expect(t.style.minWidth).toBe("120px");expect(t.style.width).toBe("630px");
});
test("fitting keeps each column within its bounds, shares spare room to open columns and shrinks evenly to the minimum",()=>{
 // Spare room goes only to columns without an upper bound or below it; a fixed column (min = max) never moves.
 expect(fitColumns([{width:100,min:80},{width:64,min:64,max:64},{width:90,min:56,max:100}],454)).toEqual([290,64,100]);
 // Over the room: columns above their minimum give the excess up evenly.
 expect(fitColumns([{width:300,min:80},{width:200,min:80}],400)).toEqual([250,150]);
 // Minimums wider than the room: every column at its minimum (the list scrolls).
 expect(fitColumns([{width:300,min:160},{width:64,min:64,max:64},{width:100,min:56,max:100}],200)).toEqual([160,64,56]);
 // A measured width outside the bounds is clamped first; an unknown room leaves the clamped widths.
 expect(fitColumns([{width:20,min:80},{width:900,min:56,max:100}],0)).toEqual([80,100]);
});
test("column kinds pick the width rule: numbers stay at their content width, the name column stays readable, the spare room goes to a flexible column",async()=>{
 const draw=async(container:number)=>{
  const widths=new Map<string,number>(),host=document.createElement("div");
  Object.defineProperty(host,"clientWidth",{configurable:true,value:container});
  const t=document.createElement("table");
  t.innerHTML='<thead><tr><th></th><th>Name</th><th>Qty</th><th>Active</th></tr></thead><tbody></tbody>';host.append(t);document.body.append(host);
  const contents=[120,50,300];
  [...t.querySelectorAll("th")].slice(1).forEach((th,i)=>vi.spyOn(th,"getBoundingClientRect").mockImplementation(()=>({width:t.classList.contains("list-auto-layout")?contents[i]!:200} as DOMRect)));
  resizeColumns(t,[{name:"name",label:"Name",kind:"char"},{name:"qty",label:"Qty",kind:"integer"},{name:"active",label:"Active",kind:"boolean"}],widths);
  await frame();
  return { widths: ["name","qty","active"].map(n=>widths.get(n)), cols: [...t.querySelectorAll("col")].slice(1).map(c=>c.style.width), min: t.style.minWidth };
 };
 // Wide: the integer keeps 64 px (its floor), the check box 100 px (its cap), the name takes the rest and the spare.
 expect(await draw(1000)).toEqual({ widths: [796, 64, 100], cols: ["", "64px", "100px"], min: "max(320px, var(--list-table-min-width, 0px))" });
 // Narrow: every column at its floor (the name keeps 160 px) and the table is as wide as those floors, so it scrolls.
 expect(await draw(200)).toEqual({ widths: [160, 64, 56], cols: ["", "64px", "56px"], min: "max(320px, var(--list-table-min-width, 0px))" });
});
const listHost=(container:number,heads:string[],rows=0)=>{
 const host=document.createElement("div");let width=container;
 Object.defineProperty(host,"clientWidth",{configurable:true,get:()=>width});
 const t=document.createElement("table"),thead=t.createTHead(),tbody=t.createTBody(),head=thead.insertRow();
 head.append(document.createElement("th"));
 for(const label of heads){const th=document.createElement("th");th.textContent=label;head.append(th);}
 for(let i=0;i<rows;i++){const row=tbody.insertRow();row.dataset["id"]=String(i+1);row.insertCell();for(const _ of heads)row.insertCell().textContent="x";}
 host.append(t);document.body.append(host);
 return {host,t,resize:(next:number)=>{width=next;}};
};
test("an arch width is the content width plus the cell padding: fixed, a minimum, or a range",async()=>{
 const {t}=listHost(1000,["Name","Code","Notes"]);
 const contents=[120,300,700];
 [...t.querySelectorAll("th")].slice(1).forEach((th,i)=>vi.spyOn(th,"getBoundingClientRect").mockImplementation(()=>({width:t.classList.contains("list-auto-layout")?contents[i]!:200} as DOMRect)));
 const widths=new Map<string,number>();
 resizeColumns(t,[{name:"name",label:"Name",kind:"char",width:{min:200}},{name:"code",label:"Code",kind:"char",width:{min:60,max:60}},{name:"notes",label:"Notes",kind:"text",width:{min:100,max:400}}],widths);
 await frame();
 // code: fixed 60 + 24; notes: 700 capped at 400 + 24; name: at least 200 + 24, then the rest of the 960 px room.
 expect([widths.get("name"),widths.get("code"),widths.get("notes")]).toEqual([452,84,424]);
});
test("a list refits when its width changes, keeping a width the user set",async()=>{
 let observed:(()=>void)|undefined;
 vi.stubGlobal("ResizeObserver",class{constructor(callback:()=>void){observed=callback;}observe(){}disconnect(){}});
 const {t,resize}=listHost(840,["Name","Login","Qty"]);
 const contents=[300,100,50];
 [...t.querySelectorAll("th")].slice(1).forEach((th,i)=>vi.spyOn(th,"getBoundingClientRect").mockImplementation(()=>({width:t.classList.contains("list-auto-layout")?contents[i]!:200} as DOMRect)));
 const widths=new Map<string,number>();
 const settle=resizeColumns(t,[{name:"name",label:"Name"},{name:"login",label:"Login"},{name:"qty",label:"Qty",kind:"integer"}],widths);
 settle();
 // 800 px room: qty fixed at 64, the 336 px left over shared by name and login.
 expect([widths.get("name"),widths.get("login"),widths.get("qty")]).toEqual([468,268,64]);
 t.querySelector<HTMLElement>('[aria-label="Resize Name column"]')!.dispatchEvent(new KeyboardEvent("keydown",{key:"ArrowRight",bubbles:true}));
 expect(widths.get("name")).toBe(478);
 resize(1040);observed!();
 // 1000 px room: name keeps the user's 478 px, qty its 64 px, login (automatic) takes the rest.
 expect([widths.get("name"),widths.get("login"),widths.get("qty")]).toEqual([478,458,64]);
 vi.unstubAllGlobals();
});
test("double-click or Enter on a resize handle gives the column its automatic width back",async()=>{
 const {t}=listHost(840,["Name","Login"]);
 const contents=[300,100];
 [...t.querySelectorAll("th")].slice(1).forEach((th,i)=>vi.spyOn(th,"getBoundingClientRect").mockImplementation(()=>({width:t.classList.contains("list-auto-layout")?contents[i]!:200} as DOMRect)));
 const widths=new Map<string,number>();
 resizeColumns(t,[{name:"name",label:"Name"},{name:"login",label:"Login"}],widths)();
 const handle=t.querySelector<HTMLElement>('[aria-label="Resize Name column"]')!;
 handle.dispatchEvent(new KeyboardEvent("keydown",{key:"ArrowLeft",bubbles:true}));
 expect(widths.get("name")).toBe(490);
 handle.dispatchEvent(new MouseEvent("dblclick",{bubbles:true}));
 expect(widths.get("name")).toBe(500);
 handle.dispatchEvent(new KeyboardEvent("keydown",{key:"ArrowLeft",bubbles:true}));
 handle.dispatchEvent(new KeyboardEvent("keydown",{key:"Enter",bubbles:true}));
 expect(widths.get("name")).toBe(500);
});
test("a list first measured without records is measured again once records arrive",async()=>{
 const widths=new Map<string,number>();
 const first=listHost(440,["Name","Login"]);
 const contents=[[100,100],[300,100]];let pass=0;
 const spy=(t:HTMLTableElement)=>[...t.querySelectorAll("th")].slice(1).forEach((th,i)=>vi.spyOn(th,"getBoundingClientRect").mockImplementation(()=>({width:t.classList.contains("list-auto-layout")?contents[pass]![i]!:200} as DOMRect)));
 spy(first.t);
 resizeColumns(first.t,[{name:"name",label:"Name"},{name:"login",label:"Login"}],widths)();
 expect([widths.get("name"),widths.get("login")]).toEqual([200,200]);
 document.body.replaceChildren();pass=1;
 const next=listHost(440,["Name","Login"],2);spy(next.t);
 resizeColumns(next.t,[{name:"name",label:"Name"},{name:"login",label:"Login"}],widths)();
 expect([widths.get("name"),widths.get("login")]).toEqual([300,100]);
 // With records measured once, a later redraw keeps the frozen widths.
 document.body.replaceChildren();pass=0;
 const third=listHost(440,["Name","Login"],2);spy(third.t);
 resizeColumns(third.t,[{name:"name",label:"Name"},{name:"login",label:"Login"}],widths)();
 expect([widths.get("name"),widths.get("login")]).toEqual([300,100]);
});
test("a column measured on a placeholder is measured again when its cell content arrives",async()=>{
 // An amount cell shows a sentence until its currency is known: measured then, the column froze at its upper bound.
 const widths=new Map<string,number>(),host=document.createElement("div");document.body.append(host);
 Object.defineProperty(host,"clientWidth",{configurable:true,value:840});
 const t=document.createElement("table");t.innerHTML='<thead><tr><th>Name</th><th>Amount</th></tr></thead><tbody><tr class="x2many-row"><td>A line</td><td><span>Loading currency presentation…</span></td></tr></tbody>';host.append(t);
 const [name,amount]=[...t.querySelectorAll("th")];
 vi.spyOn(name!,"getBoundingClientRect").mockReturnValue({width:200} as DOMRect);
 const measured=vi.spyOn(amount!,"getBoundingClientRect").mockReturnValue({width:400} as DOMRect);
 resizeColumns(t,[{name:"name",label:"Name"},{name:"amount",label:"Amount",kind:"monetary"}],widths,{selector:false,actions:false});
 const frame=():Promise<void>=>new Promise(resolve=>requestAnimationFrame(()=>resolve()));
 await frame();expect(widths.get("amount")).toBe(180);
 measured.mockReturnValue({width:100} as DOMRect);t.querySelector("td span")!.textContent="$ 4,250.00";
 await frame();await frame();
 expect(widths.get("amount")).toBe(100);expect(t.style.tableLayout).toBe("fixed");
 // the same list drawn again within the time still follows its late content (a sub-list redraws with its form)
 const again=document.createElement("table");again.innerHTML=t.innerHTML;again.querySelector("colgroup")?.remove();for(const handle of again.querySelectorAll(".column-resize-handle"))handle.remove();host.replaceChildren(again);
 const [,amount2]=[...again.querySelectorAll("th")];vi.spyOn(again.querySelector("th")!,"getBoundingClientRect").mockReturnValue({width:200} as DOMRect);
 vi.spyOn(amount2!,"getBoundingClientRect").mockReturnValue({width:120} as DOMRect);
 resizeColumns(again,[{name:"name",label:"Name"},{name:"amount",label:"Amount",kind:"monetary"}],widths,{selector:false,actions:false});
 expect(widths.get("amount")).toBe(100);again.querySelector("td span")!.textContent="$ 14,250.00";
 await frame();await frame();expect(widths.get("amount")).toBe(120);
});
test("a measured content width is rounded up, never down",async()=>{
 const widths=new Map<string,number>(),host=document.createElement("div");document.body.append(host);
 Object.defineProperty(host,"clientWidth",{configurable:true,value:400});
 const t=document.createElement("table");t.innerHTML='<thead><tr><th>Name</th><th>Amount</th></tr></thead><tbody><tr class="x2many-row"><td>A line</td><td>$ 3,400.00</td></tr></tbody>';host.append(t);
 const [name,amount]=[...t.querySelectorAll("th")];
 vi.spyOn(name!,"getBoundingClientRect").mockReturnValue({width:200} as DOMRect);vi.spyOn(amount!,"getBoundingClientRect").mockReturnValue({width:100.4} as DOMRect);
 resizeColumns(t,[{name:"name",label:"Name"},{name:"amount",label:"Amount",kind:"monetary"}],widths,{selector:false,actions:false});
 await new Promise<void>(resolve=>requestAnimationFrame(()=>resolve()));
 expect(widths.get("amount")).toBe(101);
});
