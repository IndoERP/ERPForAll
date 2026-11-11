/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
import {afterEach, expect, test, vi} from "vitest";
import {resizeColumns} from "../src/client/columns.js";
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
 pointer("pointerdown",1,100);pointer("pointermove",2,180);expect(widths.get("name")).toBe(200);
 pointer("pointermove",1,150);expect(widths.get("name")).toBe(250);pointer("pointercancel",1,150);pointer("pointermove",1,300);expect(widths.get("name")).toBe(250);
});
test("first draw sizes columns to their content, then freezes them scaled to fill the container",async()=>{
 const widths=new Map<string,number>(),host=document.createElement("div");document.body.append(host);
 Object.defineProperty(host,"clientWidth",{configurable:true,value:840});
 const t=document.createElement("table");t.innerHTML='<thead><tr><th></th><th>Name</th><th>Login</th></tr></thead><tbody></tbody>';host.append(t);
 const [,name,login]=[...t.querySelectorAll("th")];
 vi.spyOn(name!,"getBoundingClientRect").mockReturnValue({width:300} as DOMRect);vi.spyOn(login!,"getBoundingClientRect").mockReturnValue({width:100} as DOMRect);
 resizeColumns(t,[{name:"name",label:"Name"},{name:"login",label:"Login"}],widths);
 expect(t.classList.contains("list-auto-layout")).toBe(true);expect(widths.size).toBe(0);
 await new Promise(resolve=>requestAnimationFrame(()=>resolve(undefined)));
 // 800 px for two columns measured 300 and 100: the content ratio is kept (600 and 200), the selector stays 40 px.
 expect(t.classList.contains("list-auto-layout")).toBe(false);expect(widths.get("name")).toBe(600);expect(widths.get("login")).toBe(200);
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
 // A phone (390 px host): a 480 px long column and a 100 px one fit 350 px: 80 + 400 × 190/420 ≈ 261, 80 + 20 × 190/420 ≈ 89.
 expect(await fit(390,[600,100])).toEqual({ widths: [261, 89], table: "390px" });
 // Too narrow for the minimum: every column keeps 80 px (the list scrolls, as before).
 expect(await fit(200,[300,300,300])).toEqual({ widths: [80, 80, 80], table: "280px" });
 // A wide host still scales up to fill it.
 expect(await fit(840,[300,100])).toEqual({ widths: [600, 200], table: "840px" });
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
