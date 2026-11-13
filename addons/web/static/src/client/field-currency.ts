/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
import type { WidgetDraft } from "./registry.js";
import type { FieldControl, FieldControlOptions } from "./fields.js";
import { scalarControl } from "./field-scalars.js";
import { fieldShell } from "./field-shell.js";
import { array, integer, object, text } from "./metadata.js";
import { el } from "./dom.js";
import { ClientError, type RpcClient } from "./transport.js";
const unsafe = (): never => { throw new ClientError("unsupported", "This amount cannot round-trip safely. Lossless monetary transport is not available."); };
/** Admit exact decimal scale using integer arithmetic; binary multiplication never decides safety. */
export function validateMonetaryAmount(value: number | false, digits: number): void {
  if (!Number.isInteger(digits) || digits < 0 || digits > 20) throw new ClientError("server", "Invalid currency decimal places.");
  if (value === false) return;
  if (!Number.isFinite(value)) unsafe();
  const match = /^(\d+)(?:\.(\d+))?(?:e([+-]?\d+))?$/.exec(String(Math.abs(value)));
  if (!match) return unsafe();
  const fraction = match[2] ?? "", coefficient = (match[1]! + fraction).replace(/^0+/, "");
  if (!coefficient) return;
  if (coefficient.replace(/0+$/, "").length > 15) unsafe();
  const shift = digits + Number(match[3] ?? "0") - fraction.length;
  if (shift > 16 || shift < -384) unsafe();
  let scaled = BigInt(coefficient);
  if (shift >= 0) scaled *= 10n ** BigInt(shift);
  else { const divisor = 10n ** BigInt(-shift); if (scaled % divisor !== 0n) unsafe(); scaled /= divisor; }
  if (scaled > BigInt(Number.MAX_SAFE_INTEGER)) unsafe();
}
interface Currency { symbol: string; position: "before" | "after"; digits: number }
/** Ephemeral per-view authorized currency reads, bounded and shared across its field occurrences. */
export class CurrencyDisplayCache {
  private readonly values = new Map<string, Promise<Currency>>();
  /** A successful record reload/discard starts a fresh presentation snapshot. */
  clear(): void { this.values.clear(); }
  get(id: number, rpc: RpcClient, context: Record<string, unknown>, signal?: AbortSignal): Promise<Currency> {
    const serialized = JSON.stringify(context);
    if (serialized.length > 4096) throw new ClientError("validation", "Currency context exceeds 4096 characters.");
    const key = `${id} ${serialized}`;
    const cached = this.values.get(key); if (cached) return cached;
    if (this.values.size >= 128) throw new ClientError("validation", "Currency presentation exceeds 128 distinct view entries.");
    const pending: Promise<Currency> = rpc.call("res.currency", "web_read", { ids: [id], specification: { symbol: {}, position: {}, decimal_places: {} }, context }, { signal }).then<Currency>(raw => {
      const rows = array(raw, "currency records"); if (rows.length !== 1) throw new ClientError("permission", "Currency presentation is unavailable for this record.");
      const row = object(rows[0], "currency record");
      if (integer(row["id"], "currency identity") !== id) throw new ClientError("server", "Currency response identity does not match the amount.");
      const position = row["position"]; if (position !== "before" && position !== "after") throw new ClientError("server", "Invalid currency symbol position.");
      const symbol = text(row["symbol"], "currency symbol"); if (new TextEncoder().encode(symbol).length > 256) throw new ClientError("server", "Currency symbol exceeds 256 bytes.");
      const digits = integer(row["decimal_places"], "currency decimal places"); if (digits > 20) throw new ClientError("server", "Invalid currency decimal places.");
      return { symbol, position, digits };
    }).catch(error => { if (this.values.get(key) === pending) this.values.delete(key); throw error; });
    this.values.set(key, pending); return pending;
  }
}
/** A bounded asynchronous adapter; callbacks and metadata reads stay owned by the current view. */
export function monetaryControl(options: FieldControlOptions): FieldControl {
  const { field } = options, config = field.options ?? {}, host = el(options.viewContext === "list" ? "span" : "div", undefined, "field-monetary");
  // Loading, a missing currency and a refused read are presentation states, never form validity: the placeholder
  // is a status element, not an input, so it cannot block saving an amount nobody edited (lead r1 M2).
  const pending = el("span", "Loading currency presentation…", "field-monetary-status");
  pending.setAttribute("aria-busy", "true");
  const shell = fieldShell(field, pending, options.viewContext);
  pending.setAttribute("role", "status");
  host.append(shell.node);
  let restored: WidgetDraft | undefined;
  let live = true, child: FieldControl | undefined;
  const scope = new AbortController(); const abort = (): void => scope.abort();
  const parentSignal = options.editor?.signal; parentSignal?.addEventListener("abort", abort, { once: true }); if (parentSignal?.aborted) scope.abort();
  const fail = (error: unknown): void => {
    if (!live || scope.signal.aborted) return;
    pending.textContent = "";
    shell.invalid(error instanceof Error ? error.message : String(error)); pending.removeAttribute("aria-busy");
  };
  try {
    if (!config.currency_field) throw new ClientError("unsupported", "Monetary fields require an authorized currency_field declaration.");
    const raw = options.values?.[config.currency_field.name];
    const id = typeof raw === "number" ? integer(raw, "currency identity") : raw && typeof raw === "object" ? integer(object(raw)["id"], "currency identity") : 0;
    if (!id) {
      child = scalarControl({ ...options, change: value => {
        if (value !== false && typeof value !== "number") throw new ClientError("server", "Invalid monetary amount.");
        options.change(value);
      } });
      host.replaceChildren(child.node);
      return { node: host, captureDraft: () => child?.captureDraft?.(), restoreDraft: draft => child?.restoreDraft?.(draft), dispose: () => { if (!live) return; live = false; scope.abort(); parentSignal?.removeEventListener("abort", abort); child?.dispose(); } };
    }
    const cache = options.currencies ?? new CurrencyDisplayCache();
    // Shared cache requests use the outer owner, so disposing one occurrence cannot abort siblings.
    void cache.get(id, options.rpc, options.context, parentSignal).then(currency => {
      if (!live || scope.signal.aborted) return;
      const amount = options.value === null || options.value === undefined ? false : options.value;
      if (amount !== false && typeof amount !== "number") throw new ClientError("server", "Invalid monetary amount.");
      validateMonetaryAmount(amount, currency.digits);
      if (config.field_digits && !config.digits) throw new ClientError("unsupported", "field_digits requires an explicit digits declaration. The model precision cannot be discovered here.");
      const digits = config.field_digits ? config.digits?.[1] : currency.digits;
      child = scalarControl({ ...options, field: { ...field, options: { ...config, digits: [20, digits ?? currency.digits] } }, change: value => {
        if (value !== false && typeof value !== "number") throw new ClientError("server", "Invalid monetary amount.");
        validateMonetaryAmount(value, currency.digits); options.change(value);
      } });
      const value = child.node.querySelector<HTMLElement>("input,.field-read-value");
      if (value && !config.no_symbol && amount !== false) {
        const symbol = el("span", currency.symbol, "field-currency-symbol"); symbol.setAttribute("aria-hidden", "true");
        const presentation = el("span", undefined, "field-currency-value"); value.before(presentation);
        if (currency.position === "before") presentation.append(symbol, value); else presentation.append(value, symbol);
      }
      if (restored) { child.restoreDraft?.(restored); restored = undefined; }
      host.replaceChildren(child.node);
    }).catch(fail);
  } catch (error) { fail(error); }
  return { node: host, captureDraft: () => child ? child.captureDraft?.() : restored, restoreDraft: draft => { if (child) child.restoreDraft?.(draft); else restored = draft; }, dispose: () => { if (!live) return; live = false; scope.abort(); parentSignal?.removeEventListener("abort", abort); child?.dispose(); } };
}
