/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
import {
  ClientError, createAutocomplete, el, fieldShell, registerFieldWidget, resolveFieldWidget, t,
  type WidgetEnvironment, type WidgetHandle,
} from "@erpforall/client";

export function methodField(env: WidgetEnvironment): WidgetHandle {
  if (env.mode === "read" || env.field.readonly) {
    const value = env.value === undefined || env.value === null || env.value === false || env.value === "" ? t("All methods") : env.value;
    return resolveFieldWidget("char", "form").render({ ...env, value, field: { ...env.field, widget: "char", readonly: true } });
  }
  const relation = env.values?.["model_id"];
  const modelId = Array.isArray(relation) ? relation[0]
    : typeof relation === "object" && relation !== null && "id" in relation ? relation.id : relation;
  const effect = env.values?.["effect"];
  const input = el("input");
  input.name = env.field.name;
  input.autocomplete = "off";
  input.setAttribute("aria-label", env.field.label);
  input.placeholder = t("All methods");
  const owner = fieldShell(env.field, input).node;
  let selected = typeof env.value === "string" ? env.value : "";
  input.value = selected;
  if (typeof modelId !== "number" || !Number.isSafeInteger(modelId) || modelId <= 0 || !effect) {
    input.disabled = true;
    return { node: owner, dispose() {} };
  }
  if (!env.messages) throw new ClientError("unsupported", t("Method selection requires the form's autocomplete service."));
  let model: string | undefined;
  function select(value: string): void {
    selected = value;
    input.value = value;
    env.change(value || false);
  }
  const complete = createAutocomplete({
    input, owner, autoSelect: true, representedValue: () => selected,
    environment: { messages: env.messages, report: env.report, ...(env.signal ? { signal: env.signal } : {}) },
    sources: [{ async options(query, signal) {
      const opts = { signal, background: true };
      if (model === undefined) {
        const result = await env.rpc.call("ir.model", "read", { ids: [modelId], fields: ["model"], context: env.context }, opts);
        if (!Array.isArray(result) || result.length !== 1 || typeof result[0]?.model !== "string") {
          throw new ClientError("validation", t("The selected model is unavailable."));
        }
        model = result[0].model as string;
      }
      const methods = await env.rpc.call("ir.approval.policy", "erp_workflow_methods", { model, effect, query, context: env.context }, opts);
      if (!Array.isArray(methods) || !methods.every(method => typeof method === "string")) {
        throw new ClientError("server", t("The server returned invalid method choices."));
      }
      const choices = methods.map((method: string) => ({ kind: "option" as const, label: method, select: () => select(method) }));
      if (!query) choices.unshift({ kind: "option", label: t("All methods"), select: () => select("") });
      return choices;
    } }],
  });
  input.addEventListener("click", () => complete.toggle(input.value === selected ? "" : input.value));
  return { node: owner, dispose: complete.dispose };
}

registerFieldWidget("approvals.method", { kinds: ["char"], render: methodField });
