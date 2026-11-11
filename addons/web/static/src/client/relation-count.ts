/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
import { el, errorMessage } from "./dom.js";
import { ClientError, type RpcClient } from "./transport.js";

/** Explicit ERPForAll statistic provider. The real O2M stays out of record-value reads. */
export function relationCount(env: {model: string; field: string; label: string; id: () => number | undefined; context: Record<string,unknown>; rpc: RpcClient; signal?: AbortSignal; report(error: unknown): void}): {node: HTMLElement; dispose(): void} {
  const node=el("span",undefined,"form-stat-field"), value=el("span","—","form-stat-value");
  node.append(value,document.createTextNode(" "),el("span",env.label,"form-stat-label"));
  value.setAttribute("aria-live","polite");
  const controller=new AbortController(), id=env.id(), context=JSON.stringify(env.context);
  let alive=true;
  const current=(): boolean => alive && !controller.signal.aborted && id === env.id() && context === JSON.stringify(env.context);
  const abort=(): void => controller.abort();
  env.signal?.addEventListener("abort",abort,{once:true});
  const dispose=(): void => {alive=false;controller.abort();env.signal?.removeEventListener("abort",abort);};
  if (id === undefined) {node.title="Save the record to count related records.";return {node,dispose};}
  if (!Number.isSafeInteger(id) || id <= 0) throw new ClientError("server","Invalid relation count owner.");
  if (env.signal?.aborted) {dispose();return {node,dispose};}
  // aria-busy tells assistive technology the value is pending.
  value.textContent="";value.classList.add("form-stat-loading");node.setAttribute("aria-busy","true");
  void (async()=>{
    try {
      const count=await env.rpc.call(env.model,"relation_count",{ids:[id],field:env.field,context:env.context},{signal:controller.signal,background:true});
      if (!current()) return;
      if(typeof count !== "number" || !Number.isSafeInteger(count) || count < 0) throw new ClientError("server",`Invalid relation count for ${env.field}.`);
      value.textContent=String(count);
    } catch(error) {
      if(!current()) return;
      value.textContent="Unavailable";node.title=errorMessage(error);node.dataset["countError"]="true";env.report(error);
    } finally {
      if(current()) {value.classList.remove("form-stat-loading");node.removeAttribute("aria-busy");node.dispatchEvent(new Event("statistic-change",{bubbles:true}));}
    }
  })();
  return {node,dispose};
}
