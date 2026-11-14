/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
import type { ActionResult, ActionView } from "../generated/human-ui.js";
import { array, integer, object, text } from "./metadata.js";
import { ClientError } from "./transport.js";

function closed(value: Record<string, unknown>, keys: string[]): void {
  for (const key of Object.keys(value)) if (!keys.includes(key)) throw new ClientError("unsupported", `Unknown action result member: ${key}.`);
  for (const key of keys) if (!Object.hasOwn(value, key)) throw new ClientError("server", `Missing action result member: ${key}.`);
}
function bounded(value: unknown, label: string, max: number): string {
  const result = text(value, label);
  if (new TextEncoder().encode(result).length > max) throw new ClientError("server", `Invalid ${label}: too long.`);
  return result;
}
/** Decode the server's closed result vocabulary before mounting any UI. */
export function actionResult(raw: unknown): ActionResult {
  const envelope = object(raw, "action result envelope"); closed(envelope, ["wire_major", "result"]);
  if (new TextEncoder().encode(JSON.stringify(envelope)).length > 16384) throw new ClientError("server", "Action result is too large.");
  if (envelope["wire_major"] !== 1) throw new ClientError("unsupported", "Unsupported action result version. Reload the page.");
  const value = object(envelope["result"], "action result");
  switch (value["kind"]) {
    case "current_window": {
      closed(value, ["kind", "name", "model", "views", "domain", "context"]);
      const name = bounded(value["name"], "current-window title", 256), model = bounded(value["model"], "current-window model", 256);
      if (!name || !/^[a-z][a-z0-9_]*(\.[a-z][a-z0-9_]*)*$/.test(model)) throw new ClientError("server", "Invalid current-window title or model.");
      const rawViews = array(value["views"], "current-window views");
      if (rawViews.length < 1 || rawViews.length > 2) throw new ClientError("unsupported", "A current window requires list, optionally followed by form.");
      const views = rawViews.map((rawView, index): ActionView => {
        const view = object(rawView, "current-window view"); closed(view, ["id", "type"]);
        const type = view["type"];
        if (!(index === 0 && type === "list" || index === 1 && type === "form")) throw new ClientError("unsupported", "A current window requires list, optionally followed by form.");
        const id = view["id"] === null ? null : integer(view["id"], "current-window view id");
        if (id === 0) throw new ClientError("server", "Invalid current-window view id.");
        return { id, type };
      });
      const domain = value["domain"];
      if (!Array.isArray(domain)) throw new ClientError("server", "Invalid current-window domain.");
      let terms = 0;
      for (const token of [...domain].reverse()) {
        if (token === "!" || token === "&" || token === "|") {
          const arity = token === "!" ? 1 : 2;
          if (terms < arity) throw new ClientError("server", "Invalid current-window domain.");
          terms -= arity - 1;
        } else if (Array.isArray(token) && token.length === 3 &&
          (typeof token[0] === "string" && typeof token[1] === "string" ||
           (token[0] === 0 || token[0] === 1) && token[1] === "=" && token[2] === 1)) terms++;
        else throw new ClientError("server", "Invalid current-window domain.");
      }
      const context = object(value["context"], "current-window context");
      if (Object.keys(context).length > 64) throw new ClientError("server", "Too many current-window defaults.");
      for (const [key, value] of Object.entries(context)) {
        if (key === "active_test" || key === "create") {
          // Same rule as the server's normalizer (`addons/web/src/action_result.rs`): these two keys are
          // booleans on a current window only; a dialog refuses them.
          if (typeof value !== "boolean") throw new ClientError("server", `Current-window ${key} must be a boolean.`);
        } else if (model === "product.uom" && key === "product_id") {
          if (typeof value !== "number" || !Number.isSafeInteger(value) || value <= 0) throw new ClientError("server", "Packaging product_id must be a positive safe integer.");
        } else if (model === "product.uom" && key === "product_ids") {
          if (!Array.isArray(value) || !value.every(id => typeof id === "number" && Number.isSafeInteger(id) && id > 0)) throw new ClientError("server", "Packaging product_ids must be a list of positive safe integers.");
        } else if (model === "product.uom" && key === "active_model") {
          if (value !== "product.template" && value !== "product.product" && value !== "uom.uom") throw new ClientError("server", "Packaging active_model must name a product or unit model.");
        } else if (!/^default_[a-z_][a-z0-9_]*$/.test(key)) throw new ClientError("unsupported", `Unsupported current-window context key: ${key}.`);
      }
      // The server owns domain syntax validation; independently bound the entire decoded
      // literal tree before retaining it in the page-lifetime action stack.
      const check = (item: unknown, depth: number): void => {
        if (depth > 6) throw new ClientError("server", "Current-window literals are too deeply nested.");
        if (typeof item === "number" && Number.isInteger(item) && !Number.isSafeInteger(item)) throw new ClientError("server", "Current-window integer literal exceeds the browser's safe range.");
        if (item === null || typeof item === "string" || typeof item === "boolean" || typeof item === "number" && Number.isFinite(item)) return;
        if (Array.isArray(item)) { item.forEach(child => check(child, depth + 1)); return; }
        for (const child of Object.values(object(item, "current-window literal"))) check(child, depth + 1);
      };
      check(domain, 0); check(context, 0);
      return { kind: "current_window", name, model, views, domain, context };
    }
    case "none": case "close": closed(value, ["kind"]); return { kind: value["kind"] };
    case "window": {
      closed(value, ["kind", "name", "model", "res_id", "views", "context"]);
      const name = bounded(value["name"], "dialog title", 256), model = bounded(value["model"], "dialog model", 256);
      if (!name || !/^[a-z][a-z0-9_]*(\.[a-z][a-z0-9_]*)*$/.test(model)) throw new ClientError("server", "Invalid dialog title or model.");
      const res_id = value["res_id"] === null ? null : integer(value["res_id"], "dialog record id");
      if (res_id === 0) throw new ClientError("server", "Invalid dialog record id.");
      const views = array(value["views"], "dialog views");
      if (views.length !== 1) throw new ClientError("unsupported", "A wizard requires exactly one form view.");
      const view = object(views[0], "dialog view"); closed(view, ["id", "type"]);
      if (view["type"] !== "form") throw new ClientError("unsupported", "A wizard requires a form view.");
      const id = view["id"] === null ? null : integer(view["id"], "dialog view id");
      if (id === 0) throw new ClientError("server", "Invalid dialog view id.");
      const context = object(value["context"], "dialog context");
      if (Object.keys(context).length > 64) throw new ClientError("server", "Too many dialog defaults.");
      for (const key of Object.keys(context)) if (!/^default_[a-z_][a-z0-9_]*$/.test(key)) throw new ClientError("unsupported", `Unsupported dialog context key: ${key}.`);
      // JSON originates from the bounded transport; defend this boundary against an incompatible producer too.
      const check = (item: unknown, depth: number): void => {
        if (depth > 6) throw new ClientError("server", "Dialog defaults are too deeply nested.");
        if (item === null || typeof item === "string" || typeof item === "boolean" || typeof item === "number" && Number.isFinite(item)) return;
        if (Array.isArray(item)) { item.forEach(child => check(child, depth + 1)); return; }
        for (const child of Object.values(object(item, "dialog default"))) check(child, depth + 1);
      };
      check(context, 0);
      if (new TextEncoder().encode(JSON.stringify(context)).length > 16384) throw new ClientError("server", "Dialog defaults are too large.");
      return { kind: "window", name, model, res_id, views: [{ id, type: "form" }], context };
    }
    case "notification": {
      closed(value, ["kind", "title", "message", "level", "then_close"]);
      const title = value["title"] === null ? null : bounded(value["title"], "notification title", 256);
      const message = bounded(value["message"], "notification message", 4096), level = value["level"];
      if (level !== "info" && level !== "success" && level !== "warning" && level !== "danger") throw new ClientError("server", "Invalid notification level.");
      if (typeof value["then_close"] !== "boolean") throw new ClientError("server", "Invalid notification close flag.");
      return { kind: "notification", title, message, level, then_close: value["then_close"] };
    }
    default: throw new ClientError("unsupported", "Unsupported action result kind.");
  }
}
