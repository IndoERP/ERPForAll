/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
/**
 * Translations of the client's own texts (empty states, command buttons), in the user's language.
 *
 * The English text is the key: code writes `t("No records yet")` or `t("Create your first {name} record with New.",
 * { name })`, and `web/scripts/extract-client-strings.mjs` lists every such literal for translators. An addon ships a
 * catalog for a language (an object from English source to translation) and registers it from its client entry with
 * `registerTranslations(lang, catalog)`. The application selects the user's language (`locale.lang` of the bootstrap)
 * with `setClientLanguage`. A text without a translation in that language, or in its base language (`id` for
 * `id_ID`), stays English. A malformed catalog, a placeholder a translation drops or adds, and two addons translating
 * one text differently are refused by name.
 */

export type TranslationValues = Readonly<Record<string, string | number>>;

const LANGUAGE = /^[a-z]{2,3}(?:_[A-Za-z0-9]{2,8}){0,2}(?:@[a-z]{2,16})?$/;
const PLACEHOLDER = /\{([a-z_][a-z0-9_]*)\}/g;
const MAX_TEXT = 4096;

const catalogs = new Map<string, Map<string, string>>();
let language = "en_US";

function placeholders(text: string): string[] {
  return [...new Set([...text.matchAll(PLACEHOLDER)].map(match => match[1]!))].sort();
}

function checkLanguage(lang: unknown): string {
  if (typeof lang !== "string" || !LANGUAGE.test(lang)) throw new Error(`Invalid translation language: ${JSON.stringify(lang)?.slice(0, 64)}.`);
  return lang;
}

/**
 * Add `catalog` (English source to translation) for `lang`. Every entry is checked before any is kept: a catalog that
 * is not a plain object, an empty or overlong text, a non-text translation, a translation whose placeholders differ
 * from its source's, or a source another catalog already translates differently is refused with the offending text.
 */
export function registerTranslations(lang: string, catalog: unknown): void {
  const target = checkLanguage(lang);
  if (catalog === null || typeof catalog !== "object" || Array.isArray(catalog) || Object.getPrototypeOf(catalog) !== Object.prototype) {
    throw new Error(`The ${target} translation catalog must be an object of English texts to translations.`);
  }
  const known = catalogs.get(target);
  const accepted: [string, string][] = [];
  for (const [source, translated] of Object.entries(catalog as Record<string, unknown>)) {
    if (!source || source.length > MAX_TEXT) throw new Error(`The ${target} translation catalog has an empty or overlong source text.`);
    if (typeof translated !== "string" || !translated || translated.length > MAX_TEXT) {
      throw new Error(`The ${target} translation of "${source.slice(0, 80)}" must be a non-empty text.`);
    }
    if (placeholders(source).join() !== placeholders(translated).join()) {
      throw new Error(`The ${target} translation of "${source.slice(0, 80)}" must keep the placeholders ${placeholders(source).map(name => `{${name}}`).join(", ") || "(none)"}.`);
    }
    const previous = known?.get(source);
    if (previous !== undefined && previous !== translated) {
      throw new Error(`"${source.slice(0, 80)}" already has another ${target} translation.`);
    }
    accepted.push([source, translated]);
  }
  const entries = known ?? new Map<string, string>();
  for (const [source, translated] of accepted) entries.set(source, translated);
  catalogs.set(target, entries);
}

/** Select the language `t` translates into: the user's `locale.lang`. */
export function setClientLanguage(lang: string): void {
  language = checkLanguage(lang);
}

/** The language `t` translates into. */
export function clientLanguage(): string {
  return language;
}

/**
 * `source` in the selected language, or in English when no catalog translates it; `{name}` placeholders take
 * `values[name]`. A placeholder without a value is refused: it is a mistake in the calling code.
 */
export function t(source: string, values: TranslationValues = {}): string {
  const base = language.split(/[_@]/)[0]!;
  const text = catalogs.get(language)?.get(source) ?? catalogs.get(base)?.get(source) ?? source;
  return text.replace(PLACEHOLDER, (_whole, name: string) => {
    if (!Object.hasOwn(values, name)) throw new Error(`The text "${source.slice(0, 80)}" needs a value for {${name}}.`);
    return String(values[name]);
  });
}

/** Tests only: forget every catalog and return to English. */
export function resetTranslations(): void {
  catalogs.clear();
  language = "en_US";
}
