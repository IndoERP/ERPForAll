/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
import { el } from "./dom.js";
import type { Field } from "./metadata.js";
import { array, object, text } from "./metadata.js";
import { fieldWidgets, registerFieldWidget, type WidgetEnvironment, type WidgetHandle } from "./registry.js";
import type { RpcClient } from "./transport.js";

type Choices = readonly (readonly [string, string])[];

/** Choices already fetched per client connection; a re-rendered dialog reuses them synchronously. */
const resolved = new WeakMap<RpcClient, Map<string, Choices>>();
const pending = new WeakMap<RpcClient, Map<string, Promise<Choices>>>();

function mapOf<T>(store: WeakMap<RpcClient, Map<string, T>>, rpc: RpcClient): Map<string, T> {
  let map = store.get(rpc);
  if (!map) { map = new Map(); store.set(rpc, map); }
  return map;
}

function fetchChoices(environment: WidgetEnvironment): Promise<Choices> {
  const { rpc, field, context } = environment;
  const shared = mapOf(pending, rpc);
  let request = shared.get(field.name);
  if (!request) {
    request = rpc.call("res.partner", "fields_get", { allfields: [field.name], attributes: ["selection"], context }).then(raw => {
      const description = object(object(raw, "partner field descriptions")[field.name], `partner field ${field.name}`);
      const choices = array(description["selection"], `${field.name} choices`).map(item => {
        const pair = array(item, "preference choice");
        return [text(pair[0], "choice value"), text(pair[1], "choice label")] as const;
      });
      mapOf(resolved, rpc).set(field.name, choices);
      return choices;
    });
    // A failed fetch is retried by the next render rather than cached.
    request.catch(() => shared.delete(field.name));
    shared.set(field.name, request);
  }
  return request;
}

/**
  `web.preference_choice`: a text field of the My Preferences dialog (`res.users.lang`, `res.users.tz`)
 *  shown as the ordinary selection control over the choices the server lists (`res.partner.fields_get`). The human client has
 *  no renderer for a dynamic selection field, so the model keeps the field as text; the server
 *  refuses a value outside the choices on write. Until the
 *  choices arrive the control is read-only and shows the current value only. 
 */
export function preferenceChoice(environment: WidgetEnvironment): WidgetHandle {
  const selection = fieldWidgets.get("selection");
  const current = typeof environment.value === "string" && environment.value !== "" ? environment.value : null;
  const build = (choices: Choices, locked: boolean): WidgetHandle => {
    // A stored value the server no longer lists (a language since deactivated, a retired zone name)
    // stays visible and selected rather than failing the form; saving it again is the server's call.
    const offered = current === null || choices.some(([key]) => key === current) ? choices : [...choices, [current, current] as const];
    const field: Field = { ...environment.field, kind: "selection", widget: "selection", selection: offered, readonly: environment.field.readonly || locked };
    return selection.render({ ...environment, field });
  };
  const cached = resolved.get(environment.rpc)?.get(environment.field.name);
  if (cached) return build(cached, false);
  const holder = el("div", undefined, "preference-choice");
  let inner = build(current === null ? [] : [[current, current]], true);
  holder.append(inner.node);
  let disposed = false;
  void fetchChoices(environment).then(choices => {
    if (disposed) return;
    const next = build(choices, false);
    inner.dispose();
    inner = next;
    holder.replaceChildren(next.node);
  }).catch(error => { if (!disposed) environment.report(error); });
  return { node: holder, dispose: () => { disposed = true; inner.dispose(); } };
}

registerFieldWidget("web.preference_choice", { kinds: ["char"], render: preferenceChoice });
