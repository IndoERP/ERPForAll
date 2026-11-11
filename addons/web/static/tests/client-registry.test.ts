/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
import { expect, test } from "vitest";
import { Registry } from "../src/client/registry.js";
test("addon extension registries reject duplicate and unknown identities", () => {
  const registry = new Registry<{ label: string }>("test widget");
  registry.add("notes.title", { label: "Title" });
  expect(registry.get("notes.title")).toEqual({ label: "Title" });
  expect(() => registry.add("notes.title", { label: "Replacement" })).toThrow("Duplicate test widget: notes.title.");
  expect(() => registry.get("notes.unknown")).toThrow("Unknown test widget: notes.unknown.");
});

/// (partial: mixed-form addon widget disposal cannot report an owned mount cancellation)
test("disposing a compiled field during mount settles quietly and is idempotent", async () => {
  const { Component } = await import("@erpforall/elang");
  const { configureClientTemplates, elangFieldWidget } = await import("../src/client/registry.js");
  class FieldComponent extends Component { static override template = "test.DisposableField"; }
  configureClientTemplates([{ name: "test.DisposableField", render: () => document.createElement("span") }]);
  const errors: unknown[] = [];
  const handle = elangFieldWidget(FieldComponent)({ field: { name: "title", kind: "char", label: "Title", required: false, readonly: false, selection: null, relation: null, relationField: null }, value: "Title", rpc: {} as import("../src/client/transport.js").RpcClient, context: {}, change: () => {}, run: () => {}, report: error => errors.push(error) });
  document.body.append(handle.node); handle.dispose();
  expect(() => handle.dispose()).not.toThrow();
  await new Promise(resolve => setTimeout(resolve, 0));
  expect(errors).toEqual([]); expect(handle.node.querySelector("span")).toBeNull(); handle.node.remove();
});

/// (partial: genuine mixed-form addon mount failures remain reported)
test("compiled field mount failures keep their precise runtime error", async () => {
  const { Component, UIError } = await import("@erpforall/elang");
  const { configureClientTemplates, elangFieldWidget } = await import("../src/client/registry.js");
  class BrokenField extends Component { static override template = "test.BrokenField"; }
  const failure = new UIError("Fixture widget failed.", "template");
  configureClientTemplates([{ name: "test.BrokenField", render: () => { throw failure; } }]);
  const errors: unknown[] = [];
  const handle = elangFieldWidget(BrokenField)({ field: { name: "title", kind: "char", label: "Title", required: false, readonly: false, selection: null, relation: null, relationField: null }, value: "Title", rpc: {} as import("../src/client/transport.js").RpcClient, context: {}, change: () => {}, run: () => {}, report: error => errors.push(error) });
  document.body.append(handle.node); await new Promise(resolve => setTimeout(resolve, 0));
  expect(errors[0]).toBeInstanceOf(UIError); expect(errors[0]).toMatchObject({ code: "template", message: "Fixture widget failed." });
  handle.dispose(); handle.node.remove();
});
