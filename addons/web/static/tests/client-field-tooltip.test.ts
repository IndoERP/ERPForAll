/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
import { afterEach, expect, test, vi } from "vitest";
import "../src/client/fields.js";
import { RecordForm } from "../src/client/record-form.js";
import { RecordState } from "../src/client/record.js";
import { MessageServices } from "../src/client/messages.js";
import type { Field, View } from "../src/client/metadata.js";
import { RpcClient } from "../src/client/transport.js";
afterEach(() => { document.body.replaceChildren(); vi.restoreAllMocks(); });
/// (partial: staged forms attach authorized help only after mounting and retain full debug text)
test("detached form staging retains tooltip metadata through mount and disposal", async () => {
  const field: Field = { name: "name", kind: "char", label: "L".repeat(4096), options: { help: "H".repeat(4096) }, readonly: false, required: false, relation: null, relationField: null, selection: null };
  const rpc = new RpcClient(async () => { throw new Error("Unexpected RPC"); }, () => {}), state = new RecordState(rpc, "fixture.record", [field], {});
  state.values = { name: "Value" };
  const form = document.createElement("form"), messages = new MessageServices(document.body, error => { throw error; });
  const attach = vi.spyOn(messages.tooltips, "attach");
  const view: View = { type: "form", mode: "record", fields: [field], nodes: [{ kind: "root", children: [{ kind: "field", field, label: field.label, widget: "char" }] }] };
  const surface = new RecordForm({ form, state, view, rpc, context: { debug: true }, messages, run: () => {}, report: error => { throw error; }, openRecord: async () => {}, change: () => {}, button: () => {} });
  surface.render(); surface.render(); expect(attach).not.toHaveBeenCalled();
  document.body.append(form); await Promise.resolve(); await Promise.resolve();
  expect(attach).toHaveBeenCalledOnce();
  const options = attach.mock.calls[0]![1];
  expect(options.content?.().textContent).toContain("H".repeat(4096)); expect(options.content?.().textContent).toContain("Type: char");
  surface.dispose(); messages.dispose();
});

async function monetaryTooltipMount(mounted: boolean): Promise<void> {
  const field: Field = { name: "amount", kind: "monetary", widget: "monetary", label: "Amount", options: { help: "Authorized currency help", currency_field: { model: "fixture.record", name: "currency_id" } }, readonly: false, required: false, relation: null, relationField: null, selection: null };
  let deliver!: (response: Response) => void;
  const rpc = new RpcClient(() => new Promise<Response>(resolve => { deliver = resolve; }), () => {}), state = new RecordState(rpc, "fixture.record", [field], {});
  state.values = { amount: 0.29, currency_id: { id: 1 } };
  const form = document.createElement("form"), messages = new MessageServices(document.body, error => { throw error; });
  const attach = vi.spyOn(messages.tooltips, "attach"), detached = vi.fn();
  attach.mockImplementation(() => detached);
  if (mounted) document.body.append(form);
  const view: View = { type: "form", mode: "record", fields: [field], nodes: [{ kind: "root", children: [{ kind: "field", field, label: field.label, widget: "monetary" }] }] };
  const surface = new RecordForm({ form, state, view, rpc, context: { debug: true }, numericLocale: { decimal_point: ",", thousands_sep: ".", grouping: [3, 0] }, messages, run: () => {}, report: error => { throw error; }, openRecord: async () => {}, change: () => {}, button: () => {} });
  surface.render();
  deliver(new Response('[{"id":1,"symbol":"€","position":"after","decimal_places":2}]', { headers: { "Content-Type": "application/json" } }));
  await new Promise(resolve => setTimeout(resolve, 0));
  if (!mounted) { document.body.append(form); await Promise.resolve(); await Promise.resolve(); }
  expect(attach).toHaveBeenCalledOnce();
  const anchor = attach.mock.calls[0]![0];
  expect(anchor.isConnected).toBe(true); expect(anchor.contains(form.querySelector('input[name="amount"]'))).toBe(true);
  const described = anchor.getAttribute("aria-describedby")!.split(" ").map(id => document.getElementById(id)?.textContent).join(" ");
  expect(described).toContain("Authorized currency help"); expect(described).toContain("Type: monetary");
  surface.dispose(); expect(detached).toHaveBeenCalledOnce(); messages.dispose();
 }
/// (partial: connected asynchronous monetary mount retains help/debug association and disposal)
test("connected monetary tooltip survives asynchronous child replacement", () => monetaryTooltipMount(true));
/// (partial: detached asynchronous monetary mount retains help/debug association and disposal)
test("detached monetary tooltip survives asynchronous child replacement", () => monetaryTooltipMount(false));
