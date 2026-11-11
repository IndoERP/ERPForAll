/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
import { MockRpc } from "../src/index.js";
import type { Company } from "../../../../addons/web/static/src/generated/human-ui.js";
import type { TestOnlyResponse } from "../../../src/generated/TestOnlyResponse.js";
type Calls = { company: { request: { id: number }; response: Company }; schema: { request: { key: string }; response: TestOnlyResponse } };
const rpc = new MockRpc<Calls>();
rpc.on("company", ({ id }) => ({ id, name: "Company", parent_id: null }));
const company: Promise<Company> = rpc.call("company", { id: 1 });
void company;
// @ts-expect-error generated response's id is numeric
rpc.on("company", () => ({ id: "bad", name: "Company", parent_id: null }));
// @ts-expect-error request shape is operation-dependent
void rpc.call("company", { key: "wrong" });
// @ts-expect-error operations are explicit
void rpc.call("missing", {});
// @ts-expect-error nested generated DTO cannot be replaced with an unrelated record
rpc.on("schema", () => ({ other: true }));

// The addon-guide scoped patch example is compiled here.
import { withTestScope } from "@erpforall/elang-test";
import { registerAddons } from "../../../src/runtime/addons.js";
import { extensionTestAddon } from "./fixtures/extension-test-addon.js";
const model = { label() { return "original"; } };
void withTestScope(async scope => {
  scope.patch(model, sup => ({ label() { return `${sup.label.call(this)} patched`; } }));
});

class TemplateHeading { label(): string { return "Base"; } }
void withTestScope(async scope => {
  scope.manage(() => {
    scope.registries.define("extensionTemplate", (value): value is string => typeof value === "string");
  });
  scope.manage(() => registerAddons([
    extensionTestAddon({ id: "first", label: "First", target: TemplateHeading.prototype }),
    extensionTestAddon({ id: "second", label: "Second", target: TemplateHeading.prototype }),
  ], { registry: scope.registries }));
});

void withTestScope(async scope => {
  // @ts-expect-error async functions cannot be managed synchronously
  scope.manage(async () => {});
  // @ts-expect-error ordinary callbacks returning promises are also forbidden
  scope.manage(() => Promise.resolve(1));
});

void withTestScope(async scope => {
  const maybeAsync: () => number | Promise<number> = () => Promise.resolve(1);
  // @ts-expect-error a callback that may return a promise cannot be owned synchronously
  scope.manage(maybeAsync);
});
