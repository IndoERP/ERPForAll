/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
import { button, choose, el, labelled } from "./dom.js";
import { saveFile } from "./file-save.js";
import { array, integer, object, text } from "./metadata.js";
import { registerClientAction } from "./registry.js";
import { ClientError } from "./transport.js";

registerClientAction("base.translation_catalog", {
  async create({ host, rpc, run, notice }) {
    const lifetime = new AbortController();
    const response = await rpc.call("res.lang", "search_read", {
      fields: ["code", "name"], domain: [["active", "=", true], ["code", "!=", "en_US"]], limit: 500,
    });
    const form = el("form", undefined, "technical-sessions translation-catalog");
    const module = el("input"), model = el("input"), language = el("select"), file = el("input");
    module.required = model.required = language.required = true;
    module.maxLength = model.maxLength = 128;
    module.autocomplete = model.autocomplete = "off";
    file.type = "file";
    file.accept = ".po,text/x-gettext-translation";
    const placeholder = el("option", "Choose an active language");
    placeholder.value = "";
    language.append(placeholder);
    for (const item of array(response, "active languages")) {
      const row = object(item, "active language");
      const option = el("option", text(row["name"], "language name"));
      option.value = text(row["code"], "language code");
      language.append(option);
    }
    const readScope = () => ({ module: module.value.trim(), model: model.value.trim(), language: language.value });
    let running = false;
    const execute = (task: (scope: ReturnType<typeof readScope>) => Promise<void>): void => run(async () => {
      if (running || !form.reportValidity()) return;
      const scope = readScope();
      running = true;
      for (const control of [download, upload, module, model, language, file]) control.disabled = true;
      try { await task(scope); }
      finally {
        running = false;
        for (const control of [download, upload, module, model, language, file]) control.disabled = false;
      }
    });
    const download = button("Export PO", () => execute(async scope => {
      const content = text(await rpc.call("res.lang", "erp_export_translation_catalog", scope), "gettext catalog");
      if (!lifetime.signal.aborted) {
        saveFile(new Blob([content], { type: "text/x-gettext-translation;charset=utf-8" }), `${scope.module}-${scope.model}-${scope.language}.po`);
      }
    }), "translation-export");
    const upload = button("Import PO", () => execute(async scope => {
      const source = file.files?.[0];
      if (!source || source.size === 0 || source.size > 512 * 1024) {
        throw new ClientError("validation", "Choose a gettext PO file up to 512 KiB.");
      }
      if (await choose("Import translations?", `Apply the translations to ${scope.model} records in ${scope.language}?`, ["Import", "Cancel"], lifetime.signal) !== "Import") return;
      const content = await source.text();
      if (lifetime.signal.aborted) return;
      const count = integer(await rpc.call("res.lang", "erp_import_translation_catalog", { ...scope, content }, { write: true }), "imported term count");
      if (!lifetime.signal.aborted) {
        file.value = "";
        notice(`Imported ${count} translation terms.`);
      }
    }), "primary");
    const controls = el("div", undefined, "technical-session-controls");
    controls.append(download, upload);
    form.addEventListener("submit", event => event.preventDefault());
    form.append(labelled("Addon technical name", module), labelled("Model technical name", model),
      labelled("Language", language), labelled("PO file to import", file), controls);
    host.append(form);
    return { dispose() { lifetime.abort(); } };
  },
});
