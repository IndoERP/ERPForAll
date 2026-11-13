/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
import { expect, test } from "vitest";
import { validateMonetaryAmount } from "../src/client/field-currency.js";
/// (partial: safe amount scale admission refuses unroundtrippable amounts)
test("monetary safety validates decimal scale without binary multiplication rounding", () => {
  expect(() => validateMonetaryAmount(0.29, 2)).not.toThrow();
  expect(() => validateMonetaryAmount(0, 2)).not.toThrow();
  expect(() => validateMonetaryAmount(false, 2)).not.toThrow();
  for (const [value, digits] of [[90071992547409.92, 2], [0.125, 2], [1234567890123456, 0], [Infinity, 2]] as const) {
    expect(() => validateMonetaryAmount(value, digits)).toThrowError(expect.objectContaining({ kind: "unsupported", message: "This amount cannot round-trip safely. Lossless monetary transport is not available." }));
  }
});

/// (partial: successful unsafe numeric writes retain ambiguous-outcome recovery)
test("unsafe record numbers are refused before field use and writes remain ambiguous", async () => {
  const { RpcClient } = await import("../src/client/transport.js");
  const rpc = new RpcClient(async () => new Response('[{"amount":1.00000000000000001}]', { headers: { "Content-Type": "application/json" } }), () => {});
  await expect(rpc.call("fixture.record", "web_read", {})).rejects.toMatchObject({ kind: "unsupported", message: "This record contains a number that cannot round-trip safely. Lossless monetary transport is not available." });
  await expect(rpc.call("fixture.record", "web_save", {}, { write: true })).rejects.toMatchObject({ kind: "ambiguous", message: "The change may have been saved, but its numeric response cannot round-trip safely. Check the record before another write. Lossless monetary transport is not available." });
});

/// (partial: authorized currency reads share requests and reject identity mismatches)
test("currency presentation caches only validated authorized metadata", async () => {
  const { CurrencyDisplayCache } = await import("../src/client/field-currency.js");
  const { RpcClient } = await import("../src/client/transport.js");
  const requests: unknown[] = [];
  let wrong = true;
  const rpc = new RpcClient(async (_path, init) => {
    requests.push(JSON.parse(String(init?.body)));
    return new Response(JSON.stringify([{ id: wrong ? 2 : 1, symbol: "€", position: "after", decimal_places: 2 }]), { headers: { "Content-Type": "application/json" } });
  }, () => {});
  const cache = new CurrencyDisplayCache();
  await expect(cache.get(1, rpc, { lang: "en_GB" })).rejects.toMatchObject({ kind: "server", message: "Currency response identity does not match the amount." });
  wrong = false;
  const first = cache.get(1, rpc, { lang: "en_GB" }), second = cache.get(1, rpc, { lang: "en_GB" });
  expect(first).toBe(second); expect(await first).toEqual({ symbol: "€", position: "after", digits: 2 });
  expect(requests).toHaveLength(2);
  expect(requests[1]).toMatchObject({ ids: [1], specification: { symbol: {}, position: {}, decimal_places: {} }, context: { lang: "en_GB" } });
});

/// (partial: a pending amount is a status, not an invalid input, and disposal suppresses late rendering)
test("monetary presentation uses the returned symbol position and suppresses disposed responses", async () => {
  const { createFieldControl } = await import("../src/client/fields.js");
  const { RpcClient } = await import("../src/client/transport.js");
  let resolve!: (response: Response) => void;
  const rpc = new RpcClient(() => new Promise<Response>(done => { resolve = done; }), () => {});
  const options = { field: { name: "amount", label: "Amount", kind: "monetary" as const, widget: "monetary", readonly: true, required: false, relation: null, relationField: null, selection: null, options: { currency_field: { model: "fixture.record", name: "currency_id" } } }, value: 0.29, values: { currency_id: { id: 1 } }, numericLocale: { decimal_point: ",", thousands_sep: ".", grouping: [3, 0] }, rpc, context: {}, change: () => { throw new Error("Unexpected amount mutation"); }, run: async (job: () => Promise<void>) => job() };
  const handle = createFieldControl(options);
  expect(handle.node.querySelector("input")).toBeNull();
  expect(handle.node.querySelector("[role=status]")?.textContent).toBe("Loading currency presentation…");
  resolve(new Response('[{"id":1,"symbol":"€","position":"after","decimal_places":2}]', { headers: { "Content-Type": "application/json" } }));
  await new Promise(done => setTimeout(done, 0));
  expect(handle.node.querySelector(".field-read-value")?.nextElementSibling?.textContent).toBe("€");
  expect(handle.node.textContent).toContain("0,29"); handle.dispose();
  const disposed = createFieldControl(options); disposed.dispose();
  resolve(new Response('[{"id":1,"symbol":"€","position":"after","decimal_places":2}]', { headers: { "Content-Type": "application/json" } }));
  await new Promise(done => setTimeout(done, 0));
  expect(disposed.node.querySelector(".field-currency-symbol")).toBeNull();
});

/// (partial: a new record snapshot refreshes currency display facts instead of retaining stale digits)
test("currency presentation refreshes metadata when the owner starts a new snapshot", async () => {
  const { CurrencyDisplayCache } = await import("../src/client/field-currency.js");
  const { RpcClient } = await import("../src/client/transport.js");
  let symbol = "€";
  const rpc = new RpcClient(async () => new Response(JSON.stringify([{ id: 1, symbol, position: "before", decimal_places: 0 }]), { headers: { "Content-Type": "application/json" } }), () => {});
  const cache = new CurrencyDisplayCache(); expect((await cache.get(1, rpc, {})).symbol).toBe("€");
  symbol = "¥"; cache.clear(); expect((await cache.get(1, rpc, {})).symbol).toBe("¥");
});

/// (partial: integer-backed monetary display retains currency precision while edits retain integer admission)
test("integer monetary readonly form/list presentation and editable admission remain distinct", async () => {
  const { createFieldControl } = await import("../src/client/fields.js");
  const { RpcClient } = await import("../src/client/transport.js");
  const rpc = new RpcClient(async () => new Response('[{"id":1,"symbol":"€","position":"after","decimal_places":2}]', { headers: { "Content-Type": "application/json" } }), () => {});
  const changes: unknown[] = [];
  const base = { field: { name: "amount", label: "Amount", kind: "integer" as const, widget: "monetary", readonly: true, required: false, relation: null, relationField: null, selection: null, options: { currency_field: { model: "fixture.record", name: "currency_id" } } }, value: 12, values: { currency_id: { id: 1 } }, numericLocale: { decimal_point: ",", thousands_sep: ".", grouping: [3, 0] }, rpc, context: {}, change: (value: unknown) => { changes.push(value); }, run: async (job: () => Promise<void>) => job() };
  for (const viewContext of ["form", "list"] as const) {
    const handle = createFieldControl({ ...base, viewContext });
    await new Promise(resolve => setTimeout(resolve, 0));
    expect(handle.node.querySelector(".field-read-value")?.textContent).toBe("12,00");
    expect(handle.node.querySelector(".field-currency-symbol")?.textContent).toBe("€");
    expect(handle.node.querySelector(".field-invalid")).toBeNull(); handle.dispose();
  }
  const editable = createFieldControl({ ...base, field: { ...base.field, readonly: false } });
  await new Promise(resolve => setTimeout(resolve, 0));
  const input = editable.node.querySelector("input")!;
  input.value = "12,5"; input.dispatchEvent(new Event("change"));
  expect(input.validity.valid).toBe(false); expect(changes).toEqual([]);
  input.value = "13"; input.dispatchEvent(new Event("change"));
  expect(input.validity.valid).toBe(true); expect(changes).toEqual([13]); editable.dispose();
});

/// (partial: missing currency, loading and an unedited scale mismatch never block saving, lead r1 M2)
test("monetary states without a usable currency leave no invalid input behind", async () => {
  const { createFieldControl } = await import("../src/client/fields.js");
  const { RpcClient } = await import("../src/client/transport.js");
  const field = { name: "amount", label: "Amount", kind: "monetary" as const, widget: "monetary", readonly: false, required: false, relation: null, relationField: null, selection: null, options: { currency_field: { model: "fixture.record", name: "currency_id" } } };
  const locale = { decimal_point: ",", thousands_sep: ".", grouping: [3, 0] };
  const changes: unknown[] = [];
  const invalidInputs = (node: HTMLElement): number => [...node.querySelectorAll<HTMLInputElement>("input")].filter(input => !input.validity.valid).length;
  const never = new RpcClient(() => new Promise<Response>(() => {}), () => {});
  // No currency: shown and editable as a plain number, with no symbol.
  const bare = createFieldControl({ field, value: 7.5, values: { currency_id: false }, numericLocale: locale, rpc: never, context: {}, change: (value: unknown) => { changes.push(value); }, run: async (job: () => Promise<void>) => job() });
  const input = bare.node.querySelector<HTMLInputElement>("input")!;
  expect(input.value).toBe("7,5"); expect(bare.node.querySelector(".field-currency-symbol")).toBeNull(); expect(invalidInputs(bare.node)).toBe(0);
  input.value = "8"; input.dispatchEvent(new Event("change")); expect(changes).toEqual([8]); bare.dispose();
  // Readonly while the currency read is still pending.
  const loading = createFieldControl({ field: { ...field, readonly: true }, value: 7.5, values: { currency_id: 1 }, numericLocale: locale, rpc: never, context: {}, change: () => {}, run: async (job: () => Promise<void>) => job() });
  expect(invalidInputs(loading.node)).toBe(0); loading.dispose();
  // A stored amount with more decimals than the currency allows: the message shows, the record stays saveable.
  const rpc = new RpcClient(async () => new Response('[{"id":1,"symbol":"€","position":"after","decimal_places":2}]', { headers: { "Content-Type": "application/json" } }), () => {});
  const scale = createFieldControl({ field, value: 0.125, values: { currency_id: 1 }, numericLocale: locale, rpc, context: {}, change: () => {}, run: async (job: () => Promise<void>) => job() });
  await new Promise(resolve => setTimeout(resolve, 0));
  expect(scale.node.querySelector(".field-error")?.textContent).toContain("round-trip"); expect(invalidInputs(scale.node)).toBe(0); scale.dispose();
});
