/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
import { expect, test } from "vitest";
import examples from "../../../../testdata/corpus/search-panel-v1.examples.json";
import { describeViews } from "../src/client/metadata.js";
import { ClientError } from "../src/client/transport.js";

function description() {
  const field = (type: string, relation: string | null = null) => ({type,label:"Field",required:false,readonly:false,selection:null,relation,relation_field:null});
  return {wire_major:1,registry_version:"r1",description_version:"d1",model:"panel.item",notices:[],fields:{"panel.item":{
    name:field("char"),category_id:field("many2one","panel.category"),tag_ids:field("many2many","panel.tag")
  }},views:[{identity:{kind:"stored",id:1},type:"list",schema_major:1,renderer:"web.list_sections_v1",root:0,nodes:[{kind:"list",children:[],search_panel:structuredClone(examples.positive[0]!.panel)}]}]};
}
function failure(run:()=>unknown, kind: string, message: string): void {
  let error: unknown;
  try { run(); } catch(caught) { error=caught; }
  expect(error).toBeInstanceOf(ClientError);
  expect((error as ClientError).kind).toBe(kind);
  expect((error as ClientError).message).toBe(message);
}
/// (partial: strict client admission of ordered Rust wire section fixtures)
test("client admits section metadata without activating a renderer",()=>{
  const result=describeViews(description(),"panel.item","r1");
  expect(result.views.get("list")!.searchPanel).toMatchObject({kind:"sections",version:1,sections:[{kind:"category",id:1},{kind:"filter",id:2}]});
});
/// (partial: unknown section versions refuse before content use)
test("unknown panel version has a fixed compatibility error",()=>{
  const source=description();
  Object.assign(source.views[0]!.nodes[0]!.search_panel,{version:2});
  failure(()=>describeViews(source,"panel.item","r1"),"unsupported","The application changed. Reload the page to continue.");
});
/// (partial: client refuses a domain model that differs from the root relation)
test("client refuses contradictory comodel scopes without echoing data",()=>{
  const source=description();
  Object.assign(source.views[0]!.nodes[0]!.search_panel.sections[1]!.domain!,{model:"secret.model"});
  failure(()=>describeViews(source,"panel.item","r1"),"server","Invalid search panel section 2 domain model.");
});
/// (partial: client label UTF-8 byte limit, not UTF-16 length)
test("client checks exact label bytes and one beyond",()=>{
  const source=description();
  source.views[0]!.nodes[0]!.search_panel.sections[0]!.label="é".repeat(128);
  expect(describeViews(source,"panel.item","r1").views.get("list")!.searchPanel?.kind).toBe("sections");
  source.views[0]!.nodes[0]!.search_panel.sections[0]!.label+="a";
  failure(()=>describeViews(source,"panel.item","r1"),"server","Search panel section 1 exceeds label byte limit.");
});

/// (partial: old server remains admitted, extended payload under old key refuses)
test("legacy keys never admit a sections payload",()=>{
  const source=description();source.views[0]!.renderer="web.list";
  failure(()=>describeViews(source,"panel.item","r1"),"unsupported","The application changed. Reload the page to continue.");
  Object.assign(source.views[0]!.nodes[0]!,{search_panel:{kind:"unsupported",message:"Requires another slice."}});
  expect(describeViews(source,"panel.item","r1").views.get("list")!.searchPanel?.kind).toBe("unsupported");
});
/// (partial: client icon, view-key components, section/default count boundaries)
test("client admits exact scalar and collection ceilings and refuses one beyond",()=>{
  const icon=description();icon.views[0]!.nodes[0]!.search_panel.sections[0]!.icon="a".repeat(64);
  expect(describeViews(icon,"panel.item","r1").views.size).toBe(1);
  icon.views[0]!.nodes[0]!.search_panel.sections[0]!.icon+="a";
  failure(()=>describeViews(icon,"panel.item","r1"),"server","Search panel section 1 exceeds icon byte limit.");
  const keys=description();keys.views[0]!.nodes[0]!.search_panel.view_types=["list",...Array.from({length:63},(_,i)=>`a${i}`)];
  expect(describeViews(keys,"panel.item","r1").views.size).toBe(1);
  keys.views[0]!.nodes[0]!.search_panel.view_types.push("b");
  failure(()=>describeViews(keys,"panel.item","r1"),"server","Invalid search panel section 0 view key count.");
  const defaults=description();Object.assign(defaults.views[0]!.nodes[0]!.search_panel.sections[1]!,{default_values:Array.from({length:64},(_,i)=>i+1)});
  expect(describeViews(defaults,"panel.item","r1").views.size).toBe(1);
  Object.assign(defaults.views[0]!.nodes[0]!.search_panel.sections[1]!,{default_values:Array.from({length:65},(_,i)=>i+1)});
  failure(()=>describeViews(defaults,"panel.item","r1"),"server","Invalid search panel section 2 default count.");
});
/// (partial: exact section count, view-key components and UTF-8 default-key bytes)
test("client applies independent component and collection bounds",()=>{
  const count=description();const sections=count.views[0]!.nodes[0]!.search_panel.sections;
  for(let id=3;id<=64;id++)sections.push({...structuredClone(sections[1]!),id});
  expect(describeViews(count,"panel.item","r1").views.size).toBe(1);
  sections.push({...structuredClone(sections[1]!),id:65});
  failure(()=>describeViews(count,"panel.item","r1"),"server","Invalid search panel section 0 section count.");
  const keys=description();keys.views[0]!.nodes[0]!.search_panel.view_types=["list","a".repeat(64)];
  expect(describeViews(keys,"panel.item","r1").views.size).toBe(1);
  keys.views[0]!.nodes[0]!.search_panel.view_types=["list","a".repeat(65)];
  failure(()=>describeViews(keys,"panel.item","r1"),"server","Invalid search panel section 0 view key.");
  const selection=(key:string)=>{
    const source=description();
    Object.assign(source.fields["panel.item"].category_id,{type:"selection",relation:null,selection:[[key,"Choice"]]});
    Object.assign(source.views[0]!.nodes[0]!.search_panel.sections[0]!,{default_values:[key]});
    return source;
  };
  expect(describeViews(selection("é".repeat(128)),"panel.item","r1").views.size).toBe(1);
  failure(()=>describeViews(selection("é".repeat(128)+"a"),"panel.item","r1"),"server","Search panel section 1 exceeds selection key byte limit.");
});
/// (partial: shared negative corpus rejects scoped/model/grammar contradictions)
/// (partial: shared unsafe-default and unknown-member fixtures)
test("client rejects every shared negative contract fixture",()=>{
  const expected:Readonly<Record<string,readonly [string,string]>>={
    "unknown section version":["unsupported","The application changed. Reload the page to continue."],
    "forged domain model":["server","Invalid search panel section 2 domain model."],
    "undeclared RHS variable":["server","Invalid search panel section 2 category dependency."],
    "wrong grouping scope":["server","Invalid search panel section 2 field scope."],
    "unsafe default record ID":["server","Invalid search panel section 1 default identity."],
    "unknown section member":["server","Invalid search panel section 2 members."],
    "missing binary prefix operand":["server","Invalid search panel section 2 domain prefix arity."],
    "unknown criterion operator":["server","Invalid search panel section 2 domain criterion."],
    "nested relational predicate":["server","Invalid search panel section 2 domain criterion."],
    "undeclared execution global":["server","Invalid search panel section 2 category dependency."]
  };
  const document=(value:unknown):unknown=>{
    const nodes:unknown[]=[];
    const encode=(raw:unknown):number=>{
      let node:unknown;
      if(Array.isArray(raw))node={kind:"list",items:raw.map(encode)};
      else if(typeof raw==="string")node={kind:"literal",value:{kind:"str",value:raw}};
      else if(typeof raw==="boolean")node={kind:"literal",value:{kind:"bool",value:raw}};
      else if(typeof raw==="number"&&Number.isSafeInteger(raw))node={kind:"literal",value:{kind:"int",value:String(raw)}};
      else throw new Error("Unsupported trusted fixture value");
      nodes.push(node);return nodes.length-1;
    };
    encode(value);return {schema:1,nodes};
  };
  for(const fixture of examples.negative){
    const source=description();const panel=source.views[0]!.nodes[0]!.search_panel;
    if("patch" in fixture)Object.assign(panel,fixture.patch);
    if("path" in fixture&&fixture.path){
      const parts=fixture.path.replaceAll("[",".").replaceAll("]","").split(".");
      let target:unknown=panel;
      for(const key of parts.slice(0,-1))target=(target as Record<string,unknown>)[key];
      (target as Record<string,unknown>)[parts.at(-1)!]=fixture.value;
    }
    if("domain_source" in fixture){
      if(typeof fixture.domain_source==="string")Object.assign(panel.sections[1]!.domain!.ast.nodes[2]!,{name:"uid"});
      else Object.assign(panel.sections[1]!.domain!,{ast:document(fixture.domain_source)});
    }
    const result=expected[fixture.name];expect(result,fixture.name).toBeDefined();
    failure(()=>describeViews(source,"panel.item","r1"),result![0],result![1]);
  }
});

/// (partial: section-local static selection catalogue admission without defaults)
test("selection sections validate the entire catalogue even without defaults",()=>{
  const selected=(choices:readonly (readonly [string,string])[])=>{
    const source=description();
    Object.assign(source.fields["panel.item"].category_id,{type:"selection",relation:null,selection:choices});
    Object.assign(source.views[0]!.nodes[0]!.search_panel.sections[0]!,{default_values:[]});
    return source;
  };
  for(const choices of [[],[["","Empty"]],[["same","A"],["same","B"]]] as const){
    failure(()=>describeViews(selected(choices),"panel.item","r1"),"server","Invalid search panel section 1 static selection.");
  }
  expect(describeViews(selected([["é".repeat(128),"Choice"]]),"panel.item","r1").views.size).toBe(1);
  failure(()=>describeViews(selected([["é".repeat(128)+"a","Choice"]]),"panel.item","r1"),"server","Search panel section 1 exceeds selection key byte limit.");
  expect(describeViews(selected(Array.from({length:33},(_,i)=>[`key${i}`,"Choice"] as const)),"panel.item","r1").views.size).toBe(1);
});

/// (partial: retained empty AST still means an explicitly declared filter domain)
test("declared empty domain preserves counter compatibility while null means absence",()=>{
  const declared=description(),panel=declared.views[0]!.nodes[0]!.search_panel;
  Object.assign(panel.sections[1]!.domain!,{ast:{schema:1,nodes:[{kind:"list",items:[]}]}});
  Object.assign(panel.sections[0]!,{enable_counters:false});
  expect(describeViews(declared,"panel.item","r1").views.size).toBe(1);
  Object.assign(panel.sections[0]!,{enable_counters:true});
  failure(()=>describeViews(declared,"panel.item","r1"),"server","Invalid search panel section 0 counter flags.");
  Object.assign(panel.sections[1]!,{domain:null});
  expect(describeViews(declared,"panel.item","r1").views.size).toBe(1);
});

/// (partial: declared nonempty keys are identities, not trimmed display text)
test("static selection keys preserve whitespace identities",()=>{
  const source=description();
  Object.assign(source.fields["panel.item"].category_id,{type:"selection",relation:null,selection:[[" ","Space key"]]});
  Object.assign(source.views[0]!.nodes[0]!.search_panel.sections[0]!,{default_values:[" "]});
  const panel=describeViews(source,"panel.item","r1").views.get("list")!.searchPanel;
  expect(panel?.kind).toBe("sections");
  if(panel?.kind!=="sections")throw new Error("Expected sections fixture");
  expect(panel.sections[0]!.default_values).toEqual([" "]);
});
