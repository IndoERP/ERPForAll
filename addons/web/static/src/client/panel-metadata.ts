/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
import type { PanelSection as WireSection } from "../generated/human-ui.js";
import type { Field } from "./metadata.js";
import { ClientError } from "./transport.js";
import { PanelCompatibilityError } from "./panel-recovery.js";
import type { ValidatedAst } from "../../../../../web/packages/expr/src/index.js";

type Category = Omit<Extract<WireSection,{kind:"category"}>,"field"> & {field:Field};
type Filter = Omit<Extract<WireSection,{kind:"filter"}>,"field"|"domain"> & {field:Field;domain:{model:string;ast:ValidatedAst}|null};
export type AdmittedSection = Category | Filter;
export interface SectionsPanel {kind:"sections";version:1;viewTypes:readonly string[];sections:readonly AdmittedSection[];notice:string|null}
export const unavailablePanelMessage="Search panel categories and filters require a later release.";
const operators=new Set(["=","!=","<","<=",">",">=","=?","=like","=ilike","like","not like","ilike","not ilike","in","not in","child_of","parent_of"]);
const path=/^[A-Za-z_][A-Za-z0-9_$]{0,62}(?:\.[A-Za-z_][A-Za-z0-9_$]{0,62})*$/;
const viewKey=/^[a-z][a-z0-9_]{0,63}(?:\.[a-z][a-z0-9_]{0,63})?$/;
function bad(id:number, reason:string):never {throw new ClientError("server",`Invalid search panel section ${id} ${reason}.`);}
function object(raw:unknown,id:number):Record<string,unknown> {
  if(raw===null || typeof raw!=="object" || Array.isArray(raw))bad(id,"object");
  return raw as Record<string,unknown>;
}
function closed(raw:Record<string,unknown>,keys:readonly string[],id:number):void {
  if(Object.keys(raw).length!==keys.length || keys.some(key=>!Object.hasOwn(raw,key)) || Object.keys(raw).some(key=>!keys.includes(key)))bad(id,"members");
}
function list(raw:unknown,id:number):unknown[] {if(!Array.isArray(raw))bad(id,"array");return raw;}
function number(raw:unknown,id:number,kind:string):number {if(typeof raw!=="number" || !Number.isSafeInteger(raw) || raw<0)bad(id,kind);return raw;}
function flag(raw:unknown,id:number):boolean {if(typeof raw!=="boolean")bad(id,"boolean");return raw;}
function text(raw:unknown,max:number,id:number,kind:string):string {
  if(typeof raw!=="string")bad(id,kind);
  if(raw.length>max || new TextEncoder().encode(raw).length>max)throw new ClientError("server",`Search panel section ${id} exceeds ${kind} byte limit.`);
  if(!raw.trim())bad(id,kind);
  return raw;
}
/** Selection keys are opaque declared identities; whitespace is not display-text trimming. */
function selectionKey(raw:unknown,id:number):string {
  if(typeof raw!=="string" || raw.length===0)bad(id,"selection key");
  if(raw.length>256 || new TextEncoder().encode(raw).length>256)throw new ClientError("server",`Search panel section ${id} exceeds selection key byte limit.`);
  return raw;
}
function fieldHandle(raw:unknown,model:string,id:number):{model:string;name:string} {
  const source=object(raw,id);closed(source,["model","name"],id);
  const name=text(source["name"],4096,id,"field path");
  if(source["model"]!==model || !path.test(name))bad(id,"field scope");
  return {model,name};
}
/** Independent closed grammar admission; it never evaluates the domain or category variables. */
function domainDependencies(ast:ValidatedAst,id:number):Set<string> {
  const dependencies=new Set<string>();
  const node=(index:number)=>{const value=ast.nodes[index];if(!value)bad(id,"domain tree");return value;};
  const string=(index:number):string|undefined=>{const value=node(index);return value.kind==="literal"&&value.value.kind==="str"?value.value.value:undefined;};
  const numeric=(index:number):boolean=>{const value=node(index);return value.kind==="literal"&&(value.value.kind==="int" || value.value.kind==="float");};
  const rhs=(index:number,depth:number):void=>{
    if(depth>100)bad(id,"domain depth");
    const value=node(index);
    if(value.kind==="name") {dependencies.add(value.name);return;}
    if(value.kind==="literal") {
      if(value.value.kind==="bytes")bad(id,"domain literal");
      if(value.value.kind==="float" && ((BigInt(`0x${value.value.value}`)>>52n)&0x7ffn)===0x7ffn)bad(id,"domain finite literal");
      return;
    }
    if(value.kind==="list"||value.kind==="tuple") {for(const child of value.items)rhs(child,depth+1);return;}
    if(value.kind==="unary" && (value.op==="u_add"||value.op==="u_sub") && numeric(value.operand)){rhs(value.operand,depth+1);return;}
    bad(id,"domain grammar");
  };
  const root=ast.nodes.at(-1);
  if(!root || (root.kind!=="list"&&root.kind!=="tuple"))bad(id,"domain root");
  let needed=0;
  for(const index of root.items) {
    if(needed===0)needed=1; // Adjacent complete expressions use the specified implicit AND.
    const logical=string(index);
    if(logical==="!" || logical==="&" || logical==="|"){if(logical!=="!")needed++;continue;}
    const criterion=node(index);
    if((criterion.kind!=="list"&&criterion.kind!=="tuple")||criterion.items.length!==3)bad(id,"domain criterion");
    const lhs=string(criterion.items[0]!),op=string(criterion.items[1]!);
    if(lhs===undefined || lhs.length>4096 || !path.test(lhs) || op===undefined || !operators.has(op))bad(id,"domain criterion");
    rhs(criterion.items[2]!,1);needed--;
  }
  if(needed!==0)bad(id,"domain prefix arity");
  return dependencies;
}

/** Admit a versioned structural panel without granting a data method or activating its renderer. */
export function readSectionsPanel(raw:unknown,model:string,fields:ReadonlyMap<string,Field>,admitAst:(raw:unknown)=>ValidatedAst):SectionsPanel {
  const panel=object(raw,0);
  if(panel["version"]!==1)throw new PanelCompatibilityError();
  closed(panel,["kind","version","view_types","sections","notice"],0);
  const types=list(panel["view_types"],0);
  if(types.length<1 || types.length>64)bad(0,"view key count");
  const viewTypes=types.map(key=>text(key,129,0,"view key"));
  if(new Set(viewTypes).size!==viewTypes.length || viewTypes.some(key=>!viewKey.test(key)))bad(0,"view key");
  const source=list(panel["sections"],0);
  if(source.length<1 || source.length>64)bad(0,"section count");
  const seen=new Set<number>(),categories=new Set<string>(),dependencies=new Map<number,Set<string>>();
  const sections:AdmittedSection[]=source.map(value=>{
    const section=object(value,0),id=number(section["id"],0,"identity");
    if(id===0 || id>4294967295 || seen.has(id))bad(id,"identity");seen.add(id);
    const kind=section["kind"];
    if(kind!=="category"&&kind!=="filter")bad(id,"kind");
    closed(section,["kind","id","field","label","icon","enable_counters","expand","limit","default_values",...(kind==="category"?["hierarchize"]:["domain","group_by"])],id);
    const handle=fieldHandle(section["field"],model,id),field=fields.get(handle.name);
    if(!field || !["selection","many2one","many2many"].includes(field.kind))bad(id,"field");
    if(field.kind==="selection") {
      if(!field.selection?.length)bad(id,"static selection");
      const keys=new Set<string>();
      for(const [rawKey] of field.selection) {
        if(!rawKey)bad(id,"static selection");
        const key=selectionKey(rawKey,id);
        if(keys.has(key))bad(id,"static selection");
        keys.add(key);
      }
    }
    const label=text(section["label"],256,id,"label"),icon=section["icon"]===null?null:text(section["icon"],64,id,"icon");
    if(icon!==null&&!/^[A-Za-z][A-Za-z0-9_-]{0,63}$/.test(icon))bad(id,"icon token");
    const defaults=list(section["default_values"],id);
    if(defaults.length>(kind==="category"?1:64))bad(id,"default count");
    const default_values=defaults.map(value=>{
      if(field.kind==="selection") {
        const key=selectionKey(value,id);
        if(!field.selection?.some(([candidate])=>candidate===key))bad(id,"default selection");return key;
      }
      const record=number(value,id,"default identity");if(record===0)bad(id,"default identity");return record;
    });
    if(new Set(default_values).size!==default_values.length)bad(id,"duplicate defaults");
    const common={id,field,label,icon,enable_counters:flag(section["enable_counters"],id),expand:flag(section["expand"],id),limit:number(section["limit"],id,"display limit"),default_values};
    if(kind==="category") {
      if(categories.has(field.name))bad(id,"category binding");categories.add(field.name);
      return {kind,...common,hierarchize:flag(section["hierarchize"],id)};
    }
    const group_by=section["group_by"]===null?null:fieldHandle(section["group_by"],field.relation??"",id);
    let domain:Filter["domain"]=null;
    if(section["domain"]!==null) {
      const scoped=object(section["domain"],id);closed(scoped,["model","ast"],id);
      if(field.kind==="selection" || !field.relation || scoped["model"]!==field.relation)bad(id,"domain model");
      let ast:ValidatedAst;
      try {ast=admitAst(scoped["ast"]);} catch {bad(id,"domain AST");}
      dependencies.set(id,domainDependencies(ast,id));
      domain={model:field.relation,ast};
    }
    if(field.kind==="selection"&&group_by!==null)bad(id,"grouping field");
    return {kind,...common,domain,group_by};
  });
  for(const [id,names] of dependencies)for(const name of names)if(!categories.has(name))bad(id,"category dependency");
  if(sections.some(section=>section.kind==="filter"&&section.domain!==null)&&sections.some(section=>section.kind==="category"&&section.enable_counters))bad(0,"counter flags");
  const notice=panel["notice"]===null?null:text(panel["notice"],4096,0,"notice");
  return {kind:"sections",version:1,viewTypes,sections,notice};
}
