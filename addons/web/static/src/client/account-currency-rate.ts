/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
import { button, el } from "./dom.js";
import { icon } from "./icons.js";
import { ClientError } from "./transport.js";
import type { FieldControl, FieldControlOptions } from "./fields.js";

/** Invert rate with decimal precision; zero or negative rates are rejected. */
export function invertRate(rate: number): number {
  if (!Number.isFinite(rate) || rate <= 0) {
    throw new ClientError("validation", "Rate must be positive.");
  }
  // Decimal reciprocal rounding avoids binary floating point drift on exact reciprocals (e.g. 1/4 = 0.25).
  const inverted = 1 / rate;
  return Number(inverted.toFixed(12));
}

/** Formats a rate for 6-decimal UI display, returning '0.000000' for empty/zero rates. */
export function formatRateDisplay(rate: unknown): string {
  if (rate === false || rate === null || rate === undefined || typeof rate !== "number" || !Number.isFinite(rate) || rate <= 0) {
    return "0.000000";
  }
  return rate.toFixed(6);
}

export type ParseRateResult =
  | { valid: true; value: number }
  | { valid: false; reason: "not_a_number" | "not_positive"; message: string };

/**
 * Strict rate parser for invoice rate inputs.
 * Rejects non-numbers, negative values and zero with explicit reason messages.
 */
export function parseRateInput(rawInput: string): ParseRateResult {
  const trimmed = rawInput.trim().replace(",", ".");
  if (!trimmed || !/^-?(\d+(\.\d*)?|\.\d+)$/.test(trimmed)) {
    return { valid: false, reason: "not_a_number", message: "Rate must be a number." };
  }
  const parsed = Number(trimmed);
  if (isNaN(parsed) || !Number.isFinite(parsed)) {
    return { valid: false, reason: "not_a_number", message: "Rate must be a number." };
  }
  if (parsed <= 0) {
    return { valid: false, reason: "not_positive", message: "Rate must be positive." };
  }
  return { valid: true, value: parsed };
}

/** Stored user preference for rate swap direction (user preference, not browser storage). */
let persistedSwapPreference = false;

export function getCurrencySwapPreference(): boolean {
  return persistedSwapPreference;
}

export function setCurrencySwapPreference(swapped: boolean): void {
  persistedSwapPreference = swapped;
}

function extractId(val: unknown): number | null {
  if (typeof val === "number") return val;
  if (val && typeof val === "object" && "id" in val && typeof (val as { id: unknown }).id === "number") {
    return (val as { id: number }).id;
  }
  return null;
}

/** The currency's own name from the record values; a currency without one is an error, never a made-up code. */
function currencyName(val: unknown, which: string): string {
  if (val && typeof val === "object") {
    if ("display_name" in val && typeof (val as { display_name: unknown }).display_name === "string") return (val as { display_name: string }).display_name;
    if ("name" in val && typeof (val as { name: unknown }).name === "string") return (val as { name: string }).name;
  }
  throw new ClientError("validation", `The exchange rate widget needs the ${which} currency name.`);
}

export interface CurrencyExchangeRateHandle extends FieldControl {
  readonly isSwapped: boolean;
  swap(): void;
  setInputRate(text: string): void;
  getDisplayRate(): string;
  getError(): string | null;
}

/**
 * `account_currency_exchange_rate` widget.
 * Formats rate as "1 <from> ⇄ <rate> <to>" with 6 decimals.
 * Supports swapping direction, strict input validation, and readonly mode.
 */
export function currencyExchangeRateControl(options: FieldControlOptions): CurrencyExchangeRateHandle {
  const values = options.values ?? {};
  const fieldOpts = (options.field.options ?? {}) as Record<string, unknown>;
  const rateField = typeof fieldOpts["rate_field"] === "string" ? fieldOpts["rate_field"] : "invoice_currency_rate";
  const currencyId = extractId(values["currency_id"]);
  const companyCurrencyId = extractId(values["company_currency_id"]);
  const moveType = values["move_type"] as string | undefined;
  const isEntry = moveType === "entry";
  const isSameCurrency = currencyId !== null && companyCurrencyId !== null && currencyId === companyCurrencyId;
  const isHidden = isEntry || isSameCurrency;

  const host = el("div", undefined, "field-currency-exchange-rate d-flex gap-1 align-items-center flex-nowrap text-muted");
  if (isHidden) {
    host.hidden = true;
    host.style.display = "none";
  }

  let swapped = getCurrencySwapPreference();
  let currentRate = typeof options.value === "number" ? options.value : typeof values[rateField] === "number" ? (values[rateField] as number) : 0;
  const isDraft = (values["state"] as string | undefined) === "draft";
  const isEditable = isDraft && !options.field.readonly;

  const container = el("div", undefined, "exchange-rate-controls d-flex gap-1 align-items-center flex-nowrap");
  const errorEl = el("span", undefined, "field-error text-danger ms-2");
  errorEl.setAttribute("role", "alert");
  errorEl.hidden = true;

  host.append(container, errorEl);

  let inputEl: HTMLInputElement | null = null;
  let textEl: HTMLElement | null = null;

  const getEffectiveRate = (): number => {
    if (!currentRate || currentRate <= 0) return 0;
    return swapped ? invertRate(currentRate) : currentRate;
  };

  const renderContent = (): void => {
    container.replaceChildren();

    const fromCurr = swapped ? values["currency_id"] : values["company_currency_id"];
    const toCurr = swapped ? values["company_currency_id"] : values["currency_id"];

    const oneSpan = el("span", "1", "rate-unit");
    const fromSpan = el("span", isHidden ? "" : currencyName(fromCurr, swapped ? "document" : "company"), "rate-from-currency");

    const swapBtn = button("", () => {
      swapped = !swapped;
      setCurrencySwapPreference(swapped);
      renderContent();
    }, "btn btn-link text-dark p-0 mx-1 swap-currency-btn");
    swapBtn.type = "button";
    swapBtn.title = "Click to swap currencies";
    swapBtn.append(icon("rotate-ccw"));

    const displayRate = formatRateDisplay(getEffectiveRate());

    if (isEditable) {
      inputEl = el("input", undefined, "o_input field-rate-input") as HTMLInputElement;
      inputEl.type = "text";
      inputEl.value = displayRate;
      inputEl.style.maxWidth = "21ch";
      inputEl.addEventListener("change", () => {
        applyInput(inputEl!.value);
      });
      textEl = null;
    } else {
      textEl = el("span", displayRate, "field-read-value rate-display-value");
      inputEl = null;
    }

    const toSpan = el("span", isHidden ? "" : currencyName(toCurr, swapped ? "company" : "document"), "rate-to-currency");

    container.append(oneSpan, fromSpan, swapBtn, inputEl ?? textEl!, toSpan);
  };

  const applyInput = (val: string): void => {
    const parsed = parseRateInput(val);
    if (!parsed.valid) {
      if (inputEl) inputEl.value = formatRateDisplay(getEffectiveRate());
      errorEl.textContent = parsed.message;
      errorEl.hidden = false;
      return;
    }
    errorEl.hidden = true;
    errorEl.textContent = "";

    const newRate = swapped ? invertRate(parsed.value) : parsed.value;
    currentRate = newRate;
    if (inputEl) inputEl.value = formatRateDisplay(getEffectiveRate());
    options.change(newRate);
    if (options.changeValues) {
      options.changeValues({ [rateField]: newRate, rate_source: "document_entered" });
    }
  };

  renderContent();

  return {
    node: host,
    get isSwapped() { return swapped; },
    swap() {
      swapped = !swapped;
      setCurrencySwapPreference(swapped);
      renderContent();
    },
    setInputRate(text: string) {
      if (inputEl) {
        inputEl.value = text;
        applyInput(text);
      }
    },
    getDisplayRate() {
      return formatRateDisplay(getEffectiveRate());
    },
    getError() {
      return errorEl.hidden ? null : errorEl.textContent;
    },
    dispose() {
      host.replaceChildren();
    },
  };
}

export interface CurrencyDatePickerHandle extends FieldControl {
  pickDate(dateStr: string): Promise<void>;
  getNotice(): string | null;
  getError(): string | null;
}

/**
 * `account_pick_currency_date` widget.
 * Calendar button beside currency on draft foreign-currency documents.
 * Fetches currency rate for chosen date from server and applies it.
 */
export function currencyDatePickerControl(options: FieldControlOptions): CurrencyDatePickerHandle {
  const values = options.values ?? {};
  const currencyId = extractId(values["currency_id"]);
  const companyCurrencyId = extractId(values["company_currency_id"]);
  const isDraft = (values["state"] as string | undefined) === "draft";
  const isSameCurrency = currencyId !== null && companyCurrencyId !== null && currencyId === companyCurrencyId;
  const isHidden = !isDraft || isSameCurrency;

  const host = el("div", undefined, "field-pick-currency-date-wrapper d-inline-flex align-items-center");
  if (isHidden) {
    host.hidden = true;
    host.style.display = "none";
  }

  const btn = button("", () => {
    // Open date picker when interactive
    dateInput.click();
  }, "btn btn-link text-dark p-0 pick-rate-date-btn");
  btn.type = "button";
  btn.title = "Pick the rate on a certain date";
  btn.append(icon("calendar"));

  const dateInput = el("input", undefined, "pick-rate-date-hidden-input") as HTMLInputElement;
  dateInput.type = "date";
  dateInput.style.display = "none";

  const noticeEl = el("span", undefined, "fallback-rate-notice field-status text-info ms-2");
  noticeEl.hidden = true;

  const errorEl = el("span", undefined, "rate-date-error text-danger ms-2");
  errorEl.setAttribute("role", "alert");
  errorEl.hidden = true;

  host.append(btn, dateInput, noticeEl, errorEl);

  const applyPickedDate = async (dateStr: string): Promise<void> => {
    noticeEl.hidden = true;
    noticeEl.textContent = "";
    errorEl.hidden = true;
    errorEl.textContent = "";

    try {
      const moveId = values["id"];
      if (typeof moveId !== "number" || !Number.isSafeInteger(moveId) || moveId < 1) {
        throw new ClientError("validation", "Save the document before picking a rate date.");
      }
      if (currencyId === null || companyCurrencyId === null) {
        throw new ClientError("validation", "The document and company currencies are needed to look up a rate.");
      }
      const model = (options as unknown as { model?: string }).model || "account.move";
      const resp = await options.rpc.call(model, "get_currency_rate", {
        id: moveId,
        company_id: companyCurrencyId,
        currency_id: currencyId,
        date: dateStr,
      });

      let rate = 0;
      let isFallback = false;
      if (typeof resp === "number") {
        rate = resp;
      } else if (resp && typeof resp === "object") {
        const obj = resp as Record<string, unknown>;
        if (typeof obj["rate"] === "number") rate = obj["rate"];
        if (obj["fallback"] === true) isFallback = true;
      }

      if (rate <= 0) {
        throw new ClientError("validation", `No exchange rate found for currency ${currencyId} on company ${companyCurrencyId} at date ${dateStr}.`);
      }

      if (isFallback) {
        noticeEl.textContent = "Fallback rate was used: date is before the earliest recorded rate.";
        noticeEl.hidden = false;
      }

      options.change(rate);
      if (options.changeValues) {
        options.changeValues({ invoice_currency_rate: rate });
      }
      // The form's own save persists the rate; this control never writes the record itself.
    } catch (err) {
      const msg = err instanceof Error ? err.message : String(err);
      errorEl.textContent = msg;
      errorEl.hidden = false;
      throw err;
    }
  };

  dateInput.addEventListener("change", () => {
    if (dateInput.value) {
      void applyPickedDate(dateInput.value);
    }
  });

  return {
    node: host,
    async pickDate(dateStr: string) {
      await applyPickedDate(dateStr);
    },
    getNotice() {
      return noticeEl.hidden ? null : noticeEl.textContent;
    },
    getError() {
      return errorEl.hidden ? null : errorEl.textContent;
    },
    dispose() {
      host.replaceChildren();
    },
  };
}

export interface RefreshCurrencyRateHandle extends FieldControl {
  refresh(): void;
  readonly isVisible: boolean;
}

/**
 * Refresh rate button.
 * Shown only on draft invoice when stored rate differs from expected rate.
 * Clicking restores expected rate and disappears.
 */
export function refreshCurrencyRateControl(options: FieldControlOptions): RefreshCurrencyRateHandle {
  const values = options.values ?? {};
  const isDraft = (values["state"] as string | undefined) === "draft";
  const storedRate = typeof values["invoice_currency_rate"] === "number" ? (values["invoice_currency_rate"] as number) : 0;
  const expectedRate = typeof values["expected_currency_rate"] === "number" ? (values["expected_currency_rate"] as number) : 0;
  const rateDiffers = isDraft && storedRate > 0 && expectedRate > 0 && storedRate !== expectedRate;

  const btn = button("", () => {
    handleRefresh();
  }, "btn btn-link p-0 refresh-currency-rate-btn");
  btn.type = "button";
  btn.title = "Reset the currency rate to the default accordingly to the invoice date";
  btn.append(icon("refresh-cw"));

  if (!rateDiffers) {
    btn.hidden = true;
    btn.style.display = "none";
  }

  const handleRefresh = (): void => {
    if (expectedRate > 0) {
      options.change(expectedRate);
      if (options.changeValues) {
        options.changeValues({ invoice_currency_rate: expectedRate });
      }
      btn.hidden = true;
      btn.style.display = "none";
    }
  };

  return {
    node: btn,
    get isVisible() {
      return !btn.hidden && btn.style.display !== "none";
    },
    refresh() {
      handleRefresh();
    },
    dispose() {
      btn.remove();
    },
  };
}

export interface RatesListConfig {
  multiCompany: boolean;
  multipleRateTypes: boolean;
}

/**
 * Returns the column configuration for the Rates list.
 * Rate-type column appears only when more than one rate type is in use.
 */
export function getRatesListColumns(config: RatesListConfig): readonly string[] {
  const cols = ["name"];
  if (config.multiCompany) {
    cols.push("company_id");
  }
  cols.push("company_currency_id", "company_rate", "inverse_company_rate");
  if (config.multipleRateTypes) {
    cols.push("rate_type_id");
  }
  return cols;
}

export type ExchangeDiffKind = "realized_gain" | "realized_loss" | "unrealized_gain" | "unrealized_loss";

export interface CurrencyAccountOverride {
  currency_id: number;
  kind: ExchangeDiffKind;
  account_id: number;
}

export interface ExchangeSettings {
  group_multi_currency: boolean;
  currency_exchange_journal_id?: number | null;
  income_currency_exchange_account_id?: number | null;
  expense_currency_exchange_account_id?: number | null;
  unrealized_income_currency_exchange_account_id?: number | null;
  unrealized_expense_currency_exchange_account_id?: number | null;
  overrides?: readonly CurrencyAccountOverride[];
}

/**
 * Resolves the exchange difference account for a given currency and difference kind.
 * Returns the override account if present, else the company default account, else throws typed missing-config error.
 */
export function resolveExchangeAccount(
  settings: ExchangeSettings,
  currencyId: number,
  kind: ExchangeDiffKind
): number {
  const override = settings.overrides?.find(o => o.currency_id === currencyId && o.kind === kind);
  if (override && override.account_id) {
    return override.account_id;
  }
  let companyAccount: number | null | undefined;
  switch (kind) {
    case "realized_gain": companyAccount = settings.income_currency_exchange_account_id; break;
    case "realized_loss": companyAccount = settings.expense_currency_exchange_account_id; break;
    case "unrealized_gain": companyAccount = settings.unrealized_income_currency_exchange_account_id; break;
    case "unrealized_loss": companyAccount = settings.unrealized_expense_currency_exchange_account_id; break;
  }
  if (companyAccount) {
    return companyAccount;
  }
  throw new ClientError("validation", `Missing exchange difference account configuration for currency ${currencyId} and kind ${kind}.`);
}

// shortcut: no field widget is registered for these controls: the server method they call
// (`get_currency_rate`) does not exist yet, so a view naming them must fail as an unknown widget.
// Register them here once it does.
