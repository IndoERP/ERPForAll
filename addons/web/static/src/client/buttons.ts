/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
import { FALSE, NONE, boolVal, createContext, decodeAst, dictVal, evalAst, floatVal, intVal, isTruthy, listVal, parseBoundedJson, strVal, type ExprValue, type ValidatedAst } from "../../../../../web/packages/expr/src/index.js";
import type { ButtonNode, CancelNode, Field } from "./metadata.js";
import { passwordResubmitted, type Resubmission } from "./password-flow.js";
import { ClientError } from "./transport.js";

/** Lock a caller-selected surface for one serialized invocation, retaining its prior disabled state. */
export function lockButtonControls(controls: readonly HTMLButtonElement[]): () => void {
  const enabled = new Set(controls.filter(control => !control.disabled));
  for (const control of enabled) control.disabled = true;
  let released = false;
  return () => {
    if (released) return;
    released = true;
    for (const control of enabled) control.disabled = false;
    enabled.clear();
  };
}

export function recordExpressionValue(field: Field, value: unknown): ExprValue {
  if (value === undefined || value === null || value === false && field.kind !== "boolean") return FALSE;
  if (typeof value === "boolean") return boolVal(value);
  if (typeof value === "string") return strVal(value);
  if (typeof value === "number") return field.kind === "float" || !Number.isInteger(value) ? floatVal(value) : intVal(value);
  if (field.kind === "many2one" && typeof value === "object" && !Array.isArray(value) && typeof (value as { id?: unknown }).id === "number") return intVal((value as { id: number }).id);
  if ((field.kind === "one2many" || field.kind === "many2many") && Array.isArray(value)) {
    return listVal(value.map(item => lineId(field, item)));
  }
  throw new ClientError("unsupported", `Field ${field.name} cannot be used in a button expression.`);
}

/** A line's id as expressions see it. */
function lineId(field: Field, item: unknown): ExprValue {
  if (typeof item === "number") return intVal(item);
  const line = item as { id?: unknown; __key?: unknown } | null;
  if (typeof line?.id === "number") return intVal(line.id);
  if (typeof line?.__key === "string") return strVal(line.__key);
  throw new ClientError("unsupported", `Field ${field.name} holds a line without an id or a client key.`);
}

/** Whether `node` is hidden for the record's current values. A failing expression throws; the caller shows it. */
export function buttonHidden(node: ButtonNode | CancelNode, fields: readonly Field[], values: Record<string, unknown>): boolean {
  if (node.invisible === null) return false;
  const names: Record<string, ExprValue> = {};
  for (const field of fields) names[field.name] = recordExpressionValue(field, values[field.name]);
  return isTruthy(evalAst(node.invisible, createContext({ names })));
}

/** What a header button needs from the form it sits in. */
export interface ButtonEnvironment {
  readonly model: string;
  readonly context: Record<string, unknown>;
  /** The open record's id, undefined while it is new. */
  id(): number | undefined;
  dirty(): boolean;
  /** Same path as the Save button. Resolves without saving when native validation refuses. */
  save(): Promise<void>;
  call(model: string, method: string, args: Record<string, unknown>): Promise<unknown>;
  reload(): Promise<void>;
  /**
    Consume a method result instead of the legacy caller reload. `resubmit` is present for a password
   *  operation that the identity confirmation sends again.
   */
  result?(value: unknown, context?: Record<string,unknown>, resubmit?: Resubmission): Promise<void>;
  /**
    Password submissions: the operation parameters with the typed secrets, `null` when the form's
   *  own validation refused, undefined for an ordinary button. A submission is never saved first. 
   */
  secretArguments?(method: string, context: () => Record<string,unknown>): Promise<Record<string,unknown> | null | undefined>;
  /** Sends a kept password operation again after the source surface may be gone; defaults to `call`. */
  resubmitCall?(model: string, method: string, args: Record<string, unknown>): Promise<unknown>;
  confirm(title: string, message: string, accept: string, cancel?: string): Promise<boolean>;
  readonly fields?: readonly Field[];
  readonly uid?: number;
  values?(): Record<string,unknown>;
  action?(id: number, context: Record<string,unknown>): Promise<void>;
  url?(href: string): Promise<void>;
  block?(): () => void;
}

/**
 * Click behaviour of an object button: confirm, save a dirty or new record first (stopping if
 * that did not save), call the method on the record, reload. Errors propagate to the caller.
 */
export async function runObjectButton(node: ButtonNode, env: ButtonEnvironment): Promise<void> {
  if(node.target?.kind === "inert") return;
  const details=node.details;
  if(node.confirm !== null) {
    const identity=node.label.trim()?node.label:details?.title;
    if(!identity?.trim())throw new ClientError("validation","Button confirmation requires an accessible label.");
    const accepted=details && (details.confirm_title!==undefined || details.confirm_label!==undefined || details.cancel_label!==undefined)
      ? await env.confirm(details.confirm_title??identity,node.confirm,details.confirm_label??identity,details.cancel_label??"Cancel")
      : await env.confirm(identity,node.confirm,identity);
    if(!accepted) return;
  }
  if(details?.block_ui && !env.block) throw new ClientError("unsupported","This button owner does not supply UI blocking.");
  const release=details?.block_ui?env.block!():undefined;
  try {
    if(node.target === undefined && env.secretArguments) {
      // The button context is evaluated only for a submission, so an ordinary button keeps save-then-evaluate.
      let context: Record<string, unknown> | undefined;
      const effective=():Record<string,unknown>=>context??=details?.context?buttonContext(details.context,env):env.context;
      const args=await env.secretArguments(node.name,effective);
      if(args === null) return;
      if(args !== undefined) { await invoke(node, env, args, details?.context ? effective() : undefined); return; }
    }
    if (env.dirty() || env.id() === undefined) {
      await env.save();
      if (env.dirty() || env.id() === undefined) return;
    }
    const id=env.id();if(id===undefined)return;
    const context=details?.context?buttonContext(details.context,env):env.context;
    if(node.target?.kind === "url") {
      if(!env.url) throw new ClientError("unsupported","This button owner does not supply URL navigation.");
      await env.url(node.target.href); return;
    }

    if(node.target?.kind === "action") {
      if(!env.action) throw new ClientError("unsupported","This button owner does not supply action loading.");
      await env.action(node.target.id,context); return;
    }
    await invoke(node, env, {ids:[id],context}, details?.context ? context : undefined);
  } finally { release?.(); }
}

/** Call the button's method and hand its result on; a password operation keeps itself for resubmission. */
async function invoke(node: ButtonNode, env: ButtonEnvironment, args: Record<string, unknown>, effective: Record<string, unknown> | undefined): Promise<void> {
  const result=await env.call(env.model,node.name,args);
  if(!env.result) { await env.reload(); return; }
  const model=env.model, method=node.name;
  const again=(m:string,name:string,a:Record<string,unknown>):Promise<unknown>=>env.resubmitCall ? env.resubmitCall(m,name,a) : env.call(m,name,a);
  const resubmit:Resubmission|undefined=passwordResubmitted(model,method)?{model,method,call:()=>again(model,method,args)}:undefined;
  if(resubmit) await env.result(result,effective,resubmit);
  else if(effective) await env.result(result,effective);
  else await env.result(result);
}

const protectedContextKeys=new Set(["allowed_company_ids","active_company_ids","company_id","uid","sudo","su","session_id","csrf","lang","tz","active_model","active_id","active_ids","__proto__","constructor","prototype"]);
/** Evaluate through the existing expression engine, then admit a bounded JSON literal result. */
function expressionValue(ast: ValidatedAst, env: ButtonEnvironment): unknown {
  let nodes=0,bytes=0;
  const charge=(depth:number):void=>{if(depth>8 || ++nodes>256)throw new ClientError("validation","Button context exceeds 8 levels or 256 values.");};
  const text=(value:string):string=>{bytes+=new TextEncoder().encode(value).length;if(bytes>65536)throw new ClientError("validation","Button context exceeds 65536 UTF-8 bytes.");return value;};
  const fromJson=(value:unknown,depth:number):ExprValue=>{
    charge(depth);
    if(value===null || value===undefined)return NONE;
    if(typeof value==="boolean")return boolVal(value);
    if(typeof value==="string")return strVal(text(value));
    if(typeof value==="number" && Number.isFinite(value))return Number.isSafeInteger(value)?intVal(value):floatVal(value);
    if(Array.isArray(value))return listVal(value.map(item=>fromJson(item,depth+1)));
    if(value!==null && typeof value==="object" && (Object.getPrototypeOf(value)===Object.prototype || Object.getPrototypeOf(value)===null))return dictVal(Object.entries(value).map(([key,item])=>[strVal(text(key)),fromJson(item,depth+1)] as const));
    throw new ClientError("validation","Button context contains an unsupported value.");
  };
  const names:Record<string,ExprValue>=Object.create(null) as Record<string,ExprValue>;
  if(env.uid!==undefined)names["uid"]=intVal(env.uid);
  names["context"]=fromJson(env.context,0);names["id"]=env.id()===undefined?FALSE:intVal(env.id()!);
  names["active_id"]=names["id"];names["active_model"]=strVal(env.model);names["active_ids"]=listVal(env.id()===undefined?[]:[intVal(env.id()!)]);
  const requested=new Set(ast.nodes.filter(node=>node.kind==="name").map(node=>node.name));
  for(const field of env.fields??[]) if(requested.has(field.name)) names[field.name]=recordExpressionValue(field,env.values?.()[field.name]);
  const evaluated=evalAst(ast,createContext({names}));
  nodes=0;bytes=0;
  const plain=(value:ExprValue,depth:number):unknown=>{
    charge(depth);
    switch(value.kind){
      case "none":return null;
      case "bool":return value.value;
      case "str":return text(value.value);
      case "int":{const number=Number(value.value);if(!Number.isSafeInteger(number))throw new ClientError("validation","Button context integer is outside the JSON-safe range.");return number;}
      case "float":if(!Number.isFinite(value.value))throw new ClientError("validation","Button context number must be finite.");return value.value;
      case "list":case "tuple":return value.items.map(item=>plain(item,depth+1));
      case "dict":{
        if(value.entries.length>64)throw new ClientError("validation","Button context has more than 64 keys.");
        const result:Record<string,unknown>=Object.create(null) as Record<string,unknown>;
        for(const [key,item] of value.entries){if(key.kind!=="str" || key.value.length>128 || ["__proto__","constructor","prototype"].includes(key.value))throw new ClientError("validation","Button context requires safe string keys of at most 128 characters.");result[text(key.value)]=plain(item,depth+1);}
        return result;
      }
      default:throw new ClientError("validation","Button context result must contain only JSON literal values.");
    }
  };
  return plain(evaluated,0);
}
/** Action metadata uses the same bounded expression vocabulary as button context. */
export function buttonExpression(document:unknown,env:ButtonEnvironment):unknown {
  return expressionValue(decodeAst(parseBoundedJson(JSON.stringify(document))),env);
}
function buttonContext(ast:ValidatedAst,env:ButtonEnvironment):Record<string,unknown>{
  const value=expressionValue(ast,env);
  if(value===null || typeof value!=="object" || Array.isArray(value))throw new ClientError("validation","Button context must evaluate to a dictionary.");
  for(const key of Object.keys(value))if(protectedContextKeys.has(key))throw new ClientError("validation",`Button context cannot replace protected key '${key}'.`);
  return {...env.context,...value};
}
