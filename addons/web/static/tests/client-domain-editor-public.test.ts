/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
import { expect, test } from "vitest";
import * as client from "../src/client/public.js";

test("trusted addons can compose the reusable structured editor factories", () => {
  expect(client).toHaveProperty("createTreeEditor", expect.any(Function));
  expect(client).toHaveProperty("createDomainEditor", expect.any(Function));
  expect(client).toHaveProperty("createExpressionEditor", expect.any(Function));
});

// The developer guide's public composition pattern is typechecked and exercised.
test("the addon guide composition keeps owner replacements separate from user edits", async () => {
  const { MessageServices, createDomainEditor } = client;
  const messages = new MessageServices(document.body, () => {});
  const environment = {
    messages,
    report: (error: unknown): never => {
      throw error;
    },
  };
  const fields = new Map([["name", {
    name: "name",
    label: "Name",
    kind: "char" as const,
    required: false,
    readonly: false,
    relation: null,
    relationField: null,
    selection: null,
  }]]);
  let draftSource = "[]";
  let draftStatus: "supported" | "unsupported" | "invalid" = "supported";
  const editor = createDomainEditor({
    environment,
    label: "Record rules",
    model: "res.partner",
    value: "[]",
    fields: () => fields,
    change(result) {
      draftSource = result.source;
      draftStatus = result.status;
    },
  });
  document.body.append(editor.node);
  try {
    await editor.ready;
    await editor.setValue("[('name', '=', 'Owner')]");
    expect(draftSource).toBe("[]");
    expect(draftStatus).toBe("supported");
    expect(editor.node.dataset["state"]).toBe("supported");
  } finally {
    editor.dispose();
    messages.dispose();
    document.body.replaceChildren();
  }
});
