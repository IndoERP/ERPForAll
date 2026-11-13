/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
import { relationQuery } from './field-relations.js';
import { button } from './dom.js';
import { icon } from './icons.js';
import { selectCreateDialog } from './select-create-dialog.js';
import { editRelation } from './relation-editor.js';
import type { FieldControl } from './fields.js';
import type { WidgetEnvironment } from './registry.js';
import type { DialogHandle } from './dialog-service.js';

/** Adds the full picker beside the existing autocomplete without changing its search semantics. */
export function withRelationPicker(control:FieldControl, env:WidgetEnvironment):FieldControl {
  const {messages,field}=env;
  if (!messages || field.kind!=='many2one' || field.readonly || !field.relation) return control;
  const effective=relationQuery(field,env.values ?? {},env.context,env.fields);
  const scope=new AbortController();let picker:DialogHandle|undefined, dead=false;
  const abort=():void=>scope.abort();env.signal?.addEventListener('abort',abort,{once:true});
  const more=button('Search more…',()=>{
    const query=more.dataset['query']??'';delete more.dataset['query'];
    if(dead||scope.signal.aborted||picker?.dialog.isConnected)return;
    picker=selectCreateDialog({dialogs:messages.dialogs,rpc:env.rpc,model:field.relation!,title:`Choose ${field.label}`,query,domain:effective.domain,context:effective.context,
      select:choice=>{if(!dead&&!scope.signal.aborted)env.change(choice);},
      ...(field.options?.no_create || field.options?.no_create_edit ? {} : { createEdit:(input: { context: Record<string, unknown> })=>editRelation({dialogs:messages.dialogs,messages,editorLocale:env.editorLocale,numericLocale:env.numericLocale,rpc:env.rpc,model:field.relation!,context:input.context,report:env.report,signal:scope.signal}) }),
      report:env.report,signal:scope.signal});
  },'relation-search-more');
  // An icon control keeps the field quiet; the accessible name and tooltip still say what it opens.
  more.replaceChildren(icon('search'));
  more.setAttribute('aria-label',`Search more ${field.label}`);
  more.title=`Search more ${field.label}…`;
  (control.node.querySelector('.many2one-buttons') ?? control.node).append(more);
  if(env.signal?.aborted)scope.abort();
  return {node:control.node,dispose:()=>{
    if(dead)return;dead=true;env.signal?.removeEventListener('abort',abort);scope.abort();picker?.dispose();control.dispose();
  }};
}
