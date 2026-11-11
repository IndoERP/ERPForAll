/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
import { UIError } from "./errors.js";
import { htmlEscape, isMarkup, markup, markupHtml } from "./markup.js";
import type { Markup } from "./markup.js";

/** B0 dictionaries are pinned to the authenticated user's language by the caller. */
export interface TranslationCatalogue {
  readonly global?: Readonly<Record<string,string>>;
  readonly modules?: Readonly<Record<string,Readonly<Record<string,string>>>>;
}
/** Translator construction is per session; no process-global language or mutable dictionary. */
export interface TranslationOptions { readonly language: string; readonly catalogue?: TranslationCatalogue }
/** Positional %s, named %(name)s and literal %% formatting. Missing arguments fail loudly. */
export type Translate = (source: string, ...substitutions: unknown[]) => string | Markup;

/** Snapshot a bounded catalogue; source terms remain usable before later extraction tooling ships. */
export function createTranslator(options: TranslationOptions): { readonly language: string; readonly _t: Translate; forModule(module: string): Translate } {
  if (options.language !== "en_US") throw new UIError(`Language ${options.language} is not installed; additional catalogues require a later release`,"value");
  const record = (value: unknown): boolean => {
    if (!value || typeof value !== "object" || Array.isArray(value)) return false;
    const prototype: unknown = Object.getPrototypeOf(value);
    return prototype === null || prototype === Object.prototype;
  };
  let terms = 0, bytes = 0;
  const charge = (amount: number): void => {
    bytes += amount;
    if (bytes > 16_777_216) throw new UIError("Translation catalogue exceeds 16 MiB admission budget", "value");
  };
  const moduleName = (name: string): void => {
    if (typeof name !== "string" || name.length === 0 || name.length > 256) throw new UIError("Translation module must be a nonempty string of at most 256 characters", "value");
  };
  const dictionary = (input: Readonly<Record<string,string>> | undefined): ReadonlyMap<string,string> => {
    if (input === undefined) return new Map();
    if (!record(input)) throw new UIError("Translation catalogue must be a dictionary of strings", "value");
    const result = new Map<string,string>();
    for (const key in input) {
      if (!Object.hasOwn(input,key)) continue;
      const value = input[key];
      if (typeof value !== "string") throw new UIError("Translation catalogue terms must be strings", "value");
      terms++;
      if (terms > 100_000) throw new UIError("Translation catalogue exceeds 100000 terms", "value");
      charge(64 + 2*(key.length+value.length));
      result.set(key,value);
    }
    return result;
  };
  const catalogue = options.catalogue;
  if (catalogue !== undefined && (!record(catalogue))) throw new UIError("Translation catalogue must be an object", "value");
  if (catalogue) for (const key in catalogue) {
    if (key !== "global" && key !== "modules") throw new UIError("Unknown translation catalogue key", "value");
  }
  const global = dictionary(catalogue?.global), modules = new Map<string,ReadonlyMap<string,string>>();
  if (catalogue?.modules !== undefined) {
    if (!record(catalogue.modules)) throw new UIError("Translation modules must be dictionaries", "value");
    for (const name in catalogue.modules) {
      const terms = catalogue.modules[name];
      if (terms === undefined) throw new UIError("Translation module catalogue must be a dictionary of strings", "value");
      moduleName(name);
      if (modules.size >= 1024) throw new UIError("Translation catalogue exceeds 1024 modules", "value");
      charge(128 + 2*name.length);
      modules.set(name,dictionary(terms));
    }
  }
  const bind = (module?: string): Translate => (source,...values) => {
    if (typeof source !== "string" || source.length > 65_536) throw new UIError("Translation source must be a string of at most 65536 characters", "value");
    const text = (module === undefined ? undefined : modules.get(module)?.get(source)) ?? global.get(source) ?? source;
    const named = values.length === 1 && typeof values[0] === "object" && values[0] !== null && !isMarkup(values[0]) ? values[0] as Record<string,unknown> : undefined;
    const substitutions = named ? Object.values(named) : values;
    const safe = substitutions.some(isMarkup); let position = 0;
    // Tokenize first so escaped percent signs and escaped text are not reinterpreted.
    let output = "", end = 0;
    for (const match of text.matchAll(/%%|%s|%\(([^)]+)\)s/g)) {
      const literal = text.slice(end,match.index);
      output += safe ? markupHtml(htmlEscape(literal))! : literal;
      if (match[0] === "%%") output += "%";
      else {
        const key = match[1]; let value: unknown;
        if (key !== undefined) {
          if (!named || !Object.hasOwn(named,key)) throw new UIError(`Missing translation substitution ${key}`,"value");
          value = named[key];
        } else {
          if (position >= values.length) throw new UIError(`Missing translation substitution ${position+1}`,"value");
          value = values[position++];
        }
        output += safe ? markupHtml(htmlEscape(value))! : String(value);
      }
      end = match.index + match[0].length;
      if (output.length > 1_048_576) throw new UIError("Translation output exceeds 1048576 characters", "value");
    }
    const tail = text.slice(end); output += safe ? markupHtml(htmlEscape(tail))! : tail;
    if (output.length > 1_048_576) throw new UIError("Translation output exceeds 1048576 characters", "value");
    return safe ? markup(output) : output;
  };
  return Object.freeze({language:options.language,_t:bind(),forModule(module: string) {
    moduleName(module);
    return bind(module);
  }});
}
