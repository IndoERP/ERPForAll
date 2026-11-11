/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
import { expect, test, vi } from "vitest";
import { createFileEditor, createDropzone, MessageServices } from "../src/client/public.js";
import type { WidgetEnvironment, UploadTransport } from "../src/client/public.js";
import { RpcClient } from "../src/client/transport.js";

// The guide's example consumes only the addon-facing public entry, under the real typecheck.
function fileControl(environment: WidgetEnvironment, transport: UploadTransport<string>) {
  if (!environment.messages) throw new Error("This host does not supply message services.");
  return createFileEditor({ label: "Supporting file", limits: { maxFiles: 1, maxFileBytes: 1_048_576, maxTotalBytes: 1_048_576 },
    messages: environment.messages, signal: environment.signal, report: environment.report, transport,
    onComplete: result => environment.change(result) });
}
test("trusted addon public entry composes a caller-owned result without selecting an RPC route", async () => {
  const host = document.createElement("div"); document.body.append(host);
  const report = vi.fn(), change = vi.fn(), fetcher = vi.fn(fetch), messages = new MessageServices(host, report);
  const control = fileControl({ field: { name: "reference", label: "Reference", kind: "char", readonly: false, required: false, relation: null, relationField: null, selection: null }, value: false, change, messages, rpc: new RpcClient(fetcher, () => {}), context: {}, run: job => { void job(); }, report }, async () => "caller-reference");
  host.append(control.node);
  const drop = createDropzone({ target: control.node, onDrop: () => {} }); host.append(drop.node);
  try {
    await control.select([new File(["data"], "record.txt")]);
    expect(change).toHaveBeenCalledExactlyOnceWith("caller-reference");
    expect(fetcher).not.toHaveBeenCalled(); expect(report).not.toHaveBeenCalled();
  } finally { drop.dispose(); control.dispose(); messages.dispose(); host.remove(); }
});
