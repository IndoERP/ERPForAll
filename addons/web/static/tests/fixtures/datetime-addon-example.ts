/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
// Compiled by addons/web/static/tsconfig.json. Mirrors the public addon-guide example;
// the public alias is resolved by TypeScript and the human-client asset producer.
import { createDateTimeEditor, registerFieldWidget } from "@erpforall/client";

registerFieldWidget("example.delivery_date", {
  kinds: ["date"],
  render(environment) {
    if (!environment.editorLocale || !environment.messages) {
      throw new Error("This date widget requires editor locale and message services.");
    }
    const value = environment.value === null || environment.value === undefined
      ? false : environment.value;
    if (value !== false && typeof value !== "string") {
      throw new Error("Expected an ISO date or an empty value.");
    }
    return createDateTimeEditor({
      kind: "date", name: environment.field.name, label: environment.field.label,
      value, locale: environment.editorLocale, popovers: environment.messages.popovers,
      readonly: environment.field.readonly, required: environment.field.required,
      ...(environment.signal ? { signal: environment.signal } : {}),
      onApply: environment.change, report: environment.report,
    });
  },
});
