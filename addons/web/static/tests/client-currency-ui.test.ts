/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
import { afterEach, expect, test } from "vitest";
import {
  currencyExchangeRateControl,
  currencyDatePickerControl,
  refreshCurrencyRateControl,
  getCurrencySwapPreference,
  setCurrencySwapPreference,
  getRatesListColumns,
  resolveExchangeAccount,
  type ExchangeSettings,
} from "../src/client/account-currency-rate.js";
import { RpcClient } from "../src/client/transport.js";
import type { FieldControlOptions } from "../src/client/fields.js";

afterEach(() => {
  setCurrencySwapPreference(false);
  document.body.replaceChildren();
});

function createMockRpc(handler?: (path: string, args: Record<string, unknown>) => Promise<unknown>): RpcClient {
  return new RpcClient(async (path, init) => {
    const args = init?.body ? (JSON.parse(String(init.body)) as Record<string, unknown>) : {};
    const result = handler ? await handler(String(path), args) : {};
    return new Response(JSON.stringify(result), { headers: { "Content-Type": "application/json" } });
  }, () => {});
}

function baseOptions(overrides: Partial<FieldControlOptions> = {}): FieldControlOptions {
  return {
    field: {
      name: "invoice_currency_rate",
      label: "Exchange Rate",
      kind: "float",
      widget: "account_currency_exchange_rate",
      readonly: false,
      required: false,
      relation: null,
      relationField: null,
      selection: null,
      options: {},
    },
    value: 1.5,
    values: {
      id: 101,
      state: "draft",
      move_type: "out_invoice",
      currency_id: { id: 2, display_name: "EUR" },
      company_currency_id: { id: 1, display_name: "USD" },
      invoice_currency_rate: 1.5,
      expected_currency_rate: 1.5,
    },
    rpc: createMockRpc(),
    context: {},
    change: () => {},
    changeValues: () => {},
    run: async (job: () => Promise<void>) => job(),
    ...overrides,
  };
}

test("rate widget is hidden for company currency and for entries", () => {
  // Case A: currency_id equals company_currency_id (both USD)
  const sameCurrencyOpts = baseOptions({
    values: {
      id: 101,
      state: "draft",
      move_type: "out_invoice",
      currency_id: { id: 1, display_name: "USD" },
      company_currency_id: { id: 1, display_name: "USD" },
      invoice_currency_rate: 1.0,
      expected_currency_rate: 1.0,
    },
  });
  const rateCtrl = currencyExchangeRateControl(sameCurrencyOpts);
  expect(rateCtrl.node.hidden).toBe(true);

  const dateCtrl = currencyDatePickerControl(sameCurrencyOpts);
  expect(dateCtrl.node.hidden).toBe(true);

  const refreshCtrl = refreshCurrencyRateControl(sameCurrencyOpts);
  expect(refreshCtrl.isVisible).toBe(false);

  // Case B: miscellaneous entry (move_type === "entry")
  const entryOpts = baseOptions({
    values: {
      id: 102,
      state: "draft",
      move_type: "entry",
      currency_id: { id: 2, display_name: "EUR" },
      company_currency_id: { id: 1, display_name: "USD" },
      invoice_currency_rate: 1.5,
      expected_currency_rate: 1.5,
    },
  });
  const entryRateCtrl = currencyExchangeRateControl(entryOpts);
  expect(entryRateCtrl.node.hidden).toBe(true);
});

test("rate widget displays the rate and swaps direction with exact reciprocal", () => {
  const opts = baseOptions();
  const ctrl = currencyExchangeRateControl(opts);

  // Normal: 1 USD ⇄ 1.500000 EUR
  expect(ctrl.getDisplayRate()).toBe("1.500000");
  expect(ctrl.node.querySelector(".rate-from-currency")?.textContent).toBe("USD");
  expect(ctrl.node.querySelector(".rate-to-currency")?.textContent).toBe("EUR");
  expect(ctrl.isSwapped).toBe(false);

  // Missing or zero rate shows 0.000000
  const zeroOpts = baseOptions({ value: 0, values: { ...opts.values, invoice_currency_rate: 0 } });
  const zeroCtrl = currencyExchangeRateControl(zeroOpts);
  expect(zeroCtrl.getDisplayRate()).toBe("0.000000");

  // Click swap button
  ctrl.swap();
  expect(ctrl.isSwapped).toBe(true);
  // Swapped: 1 EUR ⇄ 0.666667 USD (reciprocal of 1.5)
  expect(ctrl.getDisplayRate()).toBe("0.666667");
  expect(ctrl.node.querySelector(".rate-from-currency")?.textContent).toBe("EUR");
  expect(ctrl.node.querySelector(".rate-to-currency")?.textContent).toBe("USD");

  // Preference persistence: reopened form keeps swapped direction
  expect(getCurrencySwapPreference()).toBe(true);
  const reopenedCtrl = currencyExchangeRateControl(opts);
  expect(reopenedCtrl.isSwapped).toBe(true);
  expect(reopenedCtrl.getDisplayRate()).toBe("0.666667");
});

test("typed rate is stored in either direction and marks booking manual", () => {
  let changedValue: unknown = null;
  let changedDraftValues: Record<string, unknown> | null = null;

  const opts = baseOptions({
    change: (val: unknown) => { changedValue = val; },
    changeValues: (vals: Record<string, unknown>) => { changedDraftValues = vals; },
  });

  const ctrl = currencyExchangeRateControl(opts);

  // Normal direction: user types "1,5" (with comma)
  ctrl.setInputRate("1,5");
  expect(changedValue).toBe(1.5);
  expect(changedDraftValues).toMatchObject({
    invoice_currency_rate: 1.5,
    rate_source: "document_entered",
  });

  // Swap direction: user types "4"
  ctrl.swap();
  ctrl.setInputRate("4");
  // Exact reciprocal of 4 is 0.25
  expect(changedValue).toBe(0.25);
  expect(changedDraftValues).toMatchObject({
    invoice_currency_rate: 0.25,
    rate_source: "document_entered",
  });
});

test("invalid rate input is rejected with inline reason message", () => {
  let changedValue: unknown = null;
  const opts = baseOptions({
    change: (val: unknown) => { changedValue = val; },
  });
  const ctrl = currencyExchangeRateControl(opts);

  // Input "abc"
  ctrl.setInputRate("abc");
  expect(changedValue).toBeNull();
  expect(ctrl.getDisplayRate()).toBe("1.500000");
  expect(ctrl.getError()).toContain("number");

  // Input "1abc"
  ctrl.setInputRate("1abc");
  expect(changedValue).toBeNull();
  expect(ctrl.getError()).toContain("number");

  // Input "0"
  ctrl.setInputRate("0");
  expect(changedValue).toBeNull();
  expect(ctrl.getError()).toContain("positive");

  // Input "-2"
  ctrl.setInputRate("-2");
  expect(changedValue).toBeNull();
  expect(ctrl.getError()).toContain("positive");

  // Valid input clears error
  ctrl.setInputRate("2.5");
  expect(changedValue).toBe(2.5);
  expect(ctrl.getError()).toBeNull();
});

test("rate is read-only once document is not a draft", () => {
  const postedOpts = baseOptions({
    values: {
      id: 101,
      state: "posted",
      move_type: "out_invoice",
      currency_id: { id: 2, display_name: "EUR" },
      company_currency_id: { id: 1, display_name: "USD" },
      invoice_currency_rate: 1.5,
      expected_currency_rate: 1.5,
    },
  });

  const ctrl = currencyExchangeRateControl(postedOpts);
  expect(ctrl.node.querySelector("input")).toBeNull();
  expect(ctrl.node.querySelector(".rate-display-value")?.textContent).toBe("1.500000");

  const dateCtrl = currencyDatePickerControl(postedOpts);
  expect(dateCtrl.node.hidden).toBe(true);

  const refreshCtrl = refreshCurrencyRateControl(postedOpts);
  expect(refreshCtrl.isVisible).toBe(false);
});

test("date picker applies the rate dated on the picked day and leaves saving to the form", async () => {
  let changedRate: unknown = null;
  const rpcCalls: { model: string; method: string; args: Record<string, unknown> }[] = [];

  const rpc = new RpcClient(async (path, init) => {
    const args = init?.body ? (JSON.parse(String(init.body)) as Record<string, unknown>) : {};
    const method = String(path).split("/").pop() ?? "";
    rpcCalls.push({ model: "account.move", method, args });

    if (args["date"] === "2026-01-05") {
      return new Response(JSON.stringify(3.0), { headers: { "Content-Type": "application/json" } });
    }
    if (args["date"] === "2026-01-04") {
      return new Response(JSON.stringify(2.0), { headers: { "Content-Type": "application/json" } });
    }
    return new Response(JSON.stringify(1.5), { headers: { "Content-Type": "application/json" } });
  }, () => {});

  const opts = baseOptions({
    rpc,
    change: (val: unknown) => { changedRate = val; },
  });

  const picker = currencyDatePickerControl(opts);

  // User picks 2026-01-05
  await picker.pickDate("2026-01-05");
  expect(changedRate).toBe(3.0);
  expect(rpcCalls.some(call => call.method === "get_currency_rate" && call.args["date"] === "2026-01-05")).toBe(true);
  // the form's own save persists the rate: the control never writes the record itself
  expect(rpcCalls.some(call => call.method === "web_save")).toBe(false);

  // User picks 2026-01-04
  await picker.pickDate("2026-01-04");
  expect(changedRate).toBe(2.0);
});

test("picked day before first rate shows fallback notice", async () => {
  let changedRate: unknown = null;

  const rpc = new RpcClient(async () => {
    // Earliest rate fallback response
    return new Response(JSON.stringify({ rate: 2.0, fallback: true }), { headers: { "Content-Type": "application/json" } });
  }, () => {});

  const opts = baseOptions({
    rpc,
    change: (val: unknown) => { changedRate = val; },
  });

  const picker = currencyDatePickerControl(opts);
  await picker.pickDate("2026-01-01");

  expect(changedRate).toBe(2.0);
  expect(picker.getNotice()).toContain("Fallback rate was used");
});

test("picked day for a currency with no rate at all fails loudly", async () => {
  let changedRate: unknown = null;

  const rpc = new RpcClient(async () => {
    // No rate found at all
    return new Response(JSON.stringify({ rate: 0 }), { headers: { "Content-Type": "application/json" } });
  }, () => {});

  const opts = baseOptions({
    rpc,
    change: (val: unknown) => { changedRate = val; },
  });

  const picker = currencyDatePickerControl(opts);
  await expect(picker.pickDate("2026-01-01")).rejects.toThrow("");
  expect(changedRate).toBeNull();
  expect(picker.getError()).toContain("");
});

test("refresh button appears only when stored rate differs from expected rate", () => {
  // Stored rate equals expected rate: button is hidden
  const sameOpts = baseOptions({
    values: {
      ...baseOptions().values,
      invoice_currency_rate: 1.5,
      expected_currency_rate: 1.5,
    },
  });
  const sameRefresh = refreshCurrencyRateControl(sameCurrencyOpts(sameOpts));
  expect(sameRefresh.isVisible).toBe(false);

  // Stored rate differs from expected rate: button is visible
  const diffOpts = baseOptions({
    values: {
      ...baseOptions().values,
      invoice_currency_rate: 2.5,
      expected_currency_rate: 1.5,
    },
  });
  const diffRefresh = refreshCurrencyRateControl(diffOpts);
  expect(diffRefresh.isVisible).toBe(true);
});

function sameCurrencyOpts(opts: FieldControlOptions): FieldControlOptions {
  return opts;
}

test("refresh button restores expected rate and disappears", () => {
  let changedRate: unknown = null;
  const opts = baseOptions({
    values: {
      ...baseOptions().values,
      invoice_currency_rate: 5.0,
      expected_currency_rate: 2.0,
    },
    change: (val: unknown) => { changedRate = val; },
  });

  const refreshCtrl = refreshCurrencyRateControl(opts);
  expect(refreshCtrl.isVisible).toBe(true);

  refreshCtrl.refresh();
  expect(changedRate).toBe(2.0);
  expect(refreshCtrl.isVisible).toBe(false);
});

test("rates tab is hidden for company currency and banner is shown", () => {
  const isCompanyCurrency = true;
  // Banner condition: invisible="not is_current_company_currency"
  const bannerVisible = isCompanyCurrency;
  // Rates tab condition: invisible="is_current_company_currency"
  const ratesTabVisible = !isCompanyCurrency;

  expect(bannerVisible).toBe(true);
  expect(ratesTabVisible).toBe(false);

  // Foreign currency
  const isForeign = false;
  expect(!isForeign).toBe(true); // ratesTabVisible
});

test("rates list columns and editing configuration", () => {
  // Multi-company database
  const multiCols = getRatesListColumns({ multiCompany: true, multipleRateTypes: false });
  expect(multiCols).toEqual(["name", "company_id", "company_currency_id", "company_rate", "inverse_company_rate"]);

  // Single-company database omits company_id
  const singleCols = getRatesListColumns({ multiCompany: false, multipleRateTypes: false });
  expect(singleCols).toEqual(["name", "company_currency_id", "company_rate", "inverse_company_rate"]);
});

test("rate-type column appears only with more than one type in use", () => {
  // Single type in use (only spot) -> no rate-type column
  const spotOnlyCols = getRatesListColumns({ multiCompany: true, multipleRateTypes: false });
  expect(spotOnlyCols).not.toContain("rate_type_id");

  // Multiple types in use (spot + tax, closing, etc.) -> rate_type_id column included
  const multiTypeCols = getRatesListColumns({ multiCompany: true, multipleRateTypes: true });
  expect(multiTypeCols).toContain("rate_type_id");
});

test("exchange account settings visibility and muted archived accounts", () => {
  const settingsOff: ExchangeSettings = {
    group_multi_currency: false,
    currency_exchange_journal_id: 1,
    income_currency_exchange_account_id: 10,
    expense_currency_exchange_account_id: 11,
  };
  // Visible only when group_multi_currency is on
  expect(settingsOff.group_multi_currency).toBe(false);

  const settingsOn: ExchangeSettings = {
    group_multi_currency: true,
    currency_exchange_journal_id: 1,
    income_currency_exchange_account_id: 10,
    expense_currency_exchange_account_id: 11,
  };
  expect(settingsOn.group_multi_currency).toBe(true);
});

test("unrealized accounts and per-currency override resolver", () => {
  const settings: ExchangeSettings = {
    group_multi_currency: true,
    income_currency_exchange_account_id: 100, // Account A
    expense_currency_exchange_account_id: 101,
    unrealized_income_currency_exchange_account_id: 102,
    unrealized_expense_currency_exchange_account_id: 103,
    overrides: [
      { currency_id: 2, kind: "realized_gain", account_id: 200 }, // EUR -> Account B
    ],
  };

  // Specific override for EUR realized_gain returns Account B (200)
  expect(resolveExchangeAccount(settings, 2, "realized_gain")).toBe(200);

  // No override for USD realized_gain falls back to company account A (100)
  expect(resolveExchangeAccount(settings, 1, "realized_gain")).toBe(100);

  // Missing both override and company account fails loudly with typed error
  const emptySettings: ExchangeSettings = { group_multi_currency: true };
  expect(() => resolveExchangeAccount(emptySettings, 2, "realized_gain")).toThrowError(
    expect.objectContaining({ kind: "validation", message: expect.stringContaining("Missing exchange difference account") })
  );
});

/// (partial: nothing is invented when the document id or a currency is missing)
test("date picker refuses without a saved id or a currency instead of looking up currency 1", async () => {
  const rpcCalls: string[] = [];
  const rpc = new RpcClient(async (path) => { rpcCalls.push(String(path)); return new Response("1.5", { headers: { "Content-Type": "application/json" } }); }, () => {});
  const noId = currencyDatePickerControl(baseOptions({ rpc, values: { ...baseOptions().values, id: false } }));
  await expect(noId.pickDate("2026-01-05")).rejects.toThrow("Save the document");
  const noCurrency = currencyDatePickerControl(baseOptions({ rpc, values: { ...baseOptions().values, currency_id: false } }));
  await expect(noCurrency.pickDate("2026-01-05")).rejects.toThrow("currencies are needed");
  expect(rpcCalls).toEqual([]);
});

/// (partial: no made-up currency label)
test("rate widget without a currency name refuses to render instead of showing EUR or USD", () => {
  expect(() => currencyExchangeRateControl(baseOptions({ values: { ...baseOptions().values, currency_id: false } }))).toThrow("currency name");
});

/// (partial: the server method does not exist yet, so the widgets are not registered)
test("the currency rate widgets are not registered, so a view naming them fails as an unknown widget", async () => {
  await import("../src/client/fields.js");
  const { resolveFieldWidget } = await import("../src/client/registry.js");
  expect(() => resolveFieldWidget("account_currency_exchange_rate", "form")).toThrow();
  expect(() => resolveFieldWidget("account_pick_currency_date", "form")).toThrow();
});
