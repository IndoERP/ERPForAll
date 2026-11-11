/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
import { expect, test } from "vitest";
import { createTranslator, isMarkup, markup } from "../src/index.js";
import { uiError } from "./error-contract.js";
/// (partial: runtime catalogue and substitution; not server catalogue delivery)
test("language catalogue lookup and module overrides are session scoped", () => {
  const catalogue = { global: { "Hello %s": "Welcome %s" }, modules: { sale: { Table: "Desk" } } };
  const t = createTranslator({ language: "en_US", catalogue });
  expect(t._t("Hello %s", "Ada")).toBe("Welcome Ada"); expect(t.forModule("sale")("Table")).toBe("Desk");
  expect(t._t("Table")).toBe("Table"); catalogue.global["Hello %s"] = "mutated";
  expect(t._t("Hello %s", "Ada")).toBe("Welcome Ada");
  expect(createTranslator({language:"en_US"})._t("Hello %s", "Ada")).toBe("Hello Ada");
});
test("named placeholders, percent literals, and markup never promote plain input to HTML", () => {
  const {_t} = createTranslator({language:"en_US"});
  expect(_t("%(name)s: 100%%",{name:"<Ada>"})).toBe("<Ada>: 100%");
  const result = _t("<b>%s %s</b>", markup("<i>safe</i>"), "<img>");
  expect(isMarkup(result)).toBe(true); expect(String(result)).toBe("&lt;b&gt;<i>safe</i> &lt;img&gt;&lt;/b&gt;");
});
test("missing substitutions, invalid catalogues and unsupported languages fail with kind and message", () => {
  const {_t} = createTranslator({language:"en_US"});
  for (const [fn,message] of [
    [()=>_t("Hello %s"),"Missing translation substitution 1"],
    [()=>createTranslator({language:"id_ID"}),"Language id_ID is not installed; additional catalogues require a later release"],
    [()=>createTranslator({language:"en_US",catalogue:{global:{Bad:42 as unknown as string}}}),"Translation catalogue terms must be strings"],
  ] as const) { uiError(fn, "value", message); }
});
test("catalogue bounds include empty modules, module names, terms and aggregate bytes", () => {
  const modules=Object.fromEntries(Array.from({length:1024},(_,i)=>[`module${i}`,{}]));
  expect(createTranslator({language:"en_US",catalogue:{modules}})._t("fallback")).toBe("fallback");
  uiError(()=>createTranslator({language:"en_US",catalogue:{modules:{...modules,overflow:{}}}}),"value","Translation catalogue exceeds 1024 modules");
  for (const name of ["","x".repeat(257)]) uiError(()=>createTranslator({language:"en_US",catalogue:{modules:{[name]:{}}}}),"value","Translation module must be a nonempty string of at most 256 characters");
  uiError(()=>createTranslator({language:"en_US",catalogue:{global:{x:"x".repeat(8_388_608)}}}),"value","Translation catalogue exceeds 16 MiB admission budget");
  const global=Object.fromEntries(Array.from({length:100_001},(_,i)=>[String(i),""]));
  uiError(()=>createTranslator({language:"en_US",catalogue:{global}}),"value","Translation catalogue exceeds 100000 terms");
});
test("named, source and output failure boundaries retain typed messages", () => {
  const translator=createTranslator({language:"en_US"}); const {_t}=translator;
  uiError(()=>_t("%(name)s",{}),"value","Missing translation substitution name");
  uiError(()=>_t("x".repeat(65537)),"value","Translation source must be a string of at most 65536 characters");
  uiError(()=>_t("%s","x".repeat(1_048_577)),"value","Translation output exceeds 1048576 characters");
  uiError(()=>translator.forModule(""),"value","Translation module must be a nonempty string of at most 256 characters");
});
test("wrong dictionary shapes and unknown catalogue keys fail rather than using source defaults", () => {
  uiError(()=>createTranslator({language:"en_US",catalogue:{modules:{sale:undefined} as never}}),"value","Translation module catalogue must be a dictionary of strings");
  for (const global of [new Map(),[],null]) {
    uiError(()=>createTranslator({language:"en_US",catalogue:{global:global as unknown as Record<string,string>}}),"value","Translation catalogue must be a dictionary of strings");
  }
  uiError(()=>createTranslator({language:"en_US",catalogue:{unexpected:true} as never}),"value","Unknown translation catalogue key");
  expect(createTranslator({language:"en_US",catalogue:{global:JSON.parse('{"__proto__":"literal term"}')}})._t("__proto__")).toBe("literal term");
});
test("unicode and CRLF source strings are preserved through fallback formatting", () => {
  const {_t}=createTranslator({language:"en_US"});
  expect(_t("Halo 🌏\r\n%s", "Zoë")).toBe("Halo 🌏\r\nZoë");
});
