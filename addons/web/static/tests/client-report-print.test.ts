/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
import { afterEach, beforeEach, expect, test, vi } from "vitest";
import { DialogService } from "../src/client/dialog-service.js";
import { loadPrintReports, printDropdown, printRecords } from "../src/client/report-print.js";
import { RpcClient } from "../src/client/transport.js";

let dialogs: DialogService;
beforeEach(() => {
  Object.defineProperty(HTMLDialogElement.prototype, "showModal", { configurable: true, value(this: HTMLDialogElement) { this.open = true; } });
  Object.defineProperty(HTMLDialogElement.prototype, "close", { configurable: true, value(this: HTMLDialogElement) { this.open = false; } });
  dialogs = new DialogService(() => {});
});
afterEach(() => { dialogs.dispose(); vi.restoreAllMocks(); document.body.replaceChildren(); });

type Call = { model: string; method: string; args: Record<string, unknown> };
function fakeRpc(answer: (call: Call) => unknown): { rpc: RpcClient; calls: Call[] } {
  const calls: Call[] = [];
  const rpc = { call: vi.fn(async (model: string, method: string, args: Record<string, unknown>) => { const call = { model, method, args }; calls.push(call); return answer(call); }) };
  return { rpc: rpc as unknown as RpcClient, calls };
}
const order = { report: "sale.order.document", name: "Order" };

test("a small print downloads the synchronous address the server gives", async () => {
  const { rpc, calls } = fakeRpc(() => ({ mode: "sync", url: "/report/pdf/sale.order.document/1,2" }));
  const download = vi.fn();
  expect(await printRecords({ rpc, dialogs, download }, order, "sale.order", [1, 2])).toBeUndefined();
  expect(calls).toEqual([{ model: "report.job", method: "erp_print", args: { report: "sale.order.document", res_model: "sale.order", res_ids: [1, 2] } }]);
  expect(download).toHaveBeenCalledWith("/report/pdf/sale.order.document/1,2");
});

test("a small print the server fails to render is reported to the user, not saved as a broken file", async () => {
  const fetcher = vi.fn(async () => new Response("The template of Order has no field total.", { status: 422, headers: { "Content-Type": "text/plain" } }));
  const files = new RpcClient(fetcher, () => undefined);
  const { rpc } = fakeRpc(() => ({ mode: "sync", url: "/erp/report/pdf/sale.order.document/1" }));
  const both = Object.assign(rpc, { getFile: files.getFile.bind(files) });
  await expect(printRecords({ rpc: both, dialogs }, order, "sale.order", [1])).rejects.toThrow("The template of Order has no field total.");
  expect(fetcher).toHaveBeenCalledWith("/erp/report/pdf/sale.order.document/1", expect.objectContaining({ method: "GET", headers: expect.objectContaining({ Accept: "application/pdf" }) }));
  expect(document.querySelector("a[download]")).toBeNull();
});

test("an address outside this server is refused", async () => {
  const { rpc } = fakeRpc(() => ({ mode: "sync", url: "//elsewhere.example/x.pdf" }));
  const download = vi.fn();
  await expect(printRecords({ rpc, dialogs, download }, order, "sale.order", [1])).rejects.toThrow("outside this server");
  expect(download).not.toHaveBeenCalled();
});

test("a large print shows progress, the failed document, the warning and then the download", async () => {
  let reads = 0;
  const { rpc } = fakeRpc(({ method }) => {
    if (method === "erp_print") return { mode: "job", id: 7, poll_ms: 1 };
    reads += 1;
    return reads === 1
      ? [{ state: "running", total: 51, processed: 20, failed: 0, failures: "[]", warnings: "[]", error: false, attachment_id: false }]
      : [{ state: "done", total: 51, processed: 51, failed: 1, failures: JSON.stringify([{ id: 13, error: "The document did not finish within the render deadline." }]), warnings: JSON.stringify(["The terms field printed without its image."]), error: false, attachment_id: [90, "Order.pdf"] }];
  });
  const download = vi.fn();
  const handle = (await printRecords({ rpc, dialogs, download }, order, "sale.order", Array.from({ length: 51 }, (_, i) => i + 1)))!;
  const status = handle.dialog.querySelector(".print-job-status")!;
  await vi.waitFor(() => expect(status.textContent).toBe("Printed 50 of 51 documents; 1 could not be printed."));
  expect(handle.dialog.querySelector(".print-job-failures")!.textContent).toBe("Record 13: The document did not finish within the render deadline.");
  expect(handle.dialog.querySelector(".print-job-warnings")!.textContent).toBe("The terms field printed without its image.");
  const button = [...handle.dialog.querySelectorAll<HTMLButtonElement>("button")].find(item => item.textContent === "Download PDF")!;
  expect(button.hidden).toBe(false);
  button.click();
  expect(download).toHaveBeenCalledWith("/erp/content/90?download=true");
  expect(reads).toBe(2);
});

test("a failed print shows its reason and stops reading", async () => {
  let reads = 0;
  const { rpc } = fakeRpc(({ method }) => {
    if (method === "erp_print") return { mode: "job", id: 8, poll_ms: 1 };
    reads += 1;
    return [{ state: "failed", total: 60, processed: 60, failed: 60, failures: "[]", warnings: "[]", error: "None of the 60 documents could be printed.", attachment_id: false }];
  });
  const handle = (await printRecords({ rpc, dialogs }, order, "sale.order", Array.from({ length: 60 }, (_, i) => i + 1)))!;
  await vi.waitFor(() => expect(handle.dialog.querySelector("[role='alert']")!.textContent).toBe("None of the 60 documents could be printed."));
  await new Promise(resolve => setTimeout(resolve, 10));
  expect(reads).toBe(1);
});

test("a report list that is not a list is refused", async () => {
  const { rpc } = fakeRpc(() => ({}));
  await expect(loadPrintReports(rpc, "sale.order")).rejects.toThrow("did not answer the report list with a list");
});

test("the Print button shows a printer icon, loads its reports at once and stays hidden until they arrive", async () => {
  let release!: (value: unknown) => void;
  const calls: unknown[] = [];
  const rpc = { call: vi.fn((model: string, method: string, args: unknown) => { calls.push({ model, method, args }); return new Promise(resolve => { release = resolve; }); }) } as unknown as RpcClient;
  const pick = vi.fn();
  const print = printDropdown(rpc, "sale.order", pick, () => {});
  document.body.append(print.node);
  const toggle = print.node.querySelector<HTMLButtonElement>(".print-toggle-btn")!;
  expect(toggle.textContent).toBe("Print");
  expect(toggle.querySelector("svg")?.getAttribute("data-icon")).toBe("printer");
  expect(print.node.hidden).toBe(true);
  expect(calls).toEqual([{ model: "report.job", method: "erp_print_reports", args: { res_model: "sale.order" } }]);
  expect(rpc.call).toHaveBeenCalledWith("report.job", "erp_print_reports", { res_model: "sale.order" }, { background: true });
  release([order, { report: "sale.order.quote", name: "Quotation" }]);
  await vi.waitFor(() => expect(print.node.hidden).toBe(false));
  toggle.click();
  expect([...print.node.querySelectorAll(".print-report-btn")].map(item => item.textContent)).toEqual(["Order", "Quotation"]);
  expect(print.node.querySelector(".print-report-btn")!.getAttribute("aria-label")).toBe("Print Order");
  expect(calls).toHaveLength(1);
  print.node.querySelector<HTMLButtonElement>(".print-report-btn")!.click();
  expect(pick).toHaveBeenCalledWith(order);
  print.dispose();
});

test("a model without a report has no Print button", async () => {
  const { rpc, calls } = fakeRpc(() => []);
  const print = printDropdown(rpc, "res.partner", () => {}, () => {});
  document.body.append(print.node);
  await vi.waitFor(() => expect(calls).toHaveLength(1));
  await new Promise(resolve => setTimeout(resolve, 0));
  expect(print.node.hidden).toBe(true);
  expect(print.node.querySelector(".print-report-btn, .print-report-none")).toBeNull();
  print.dispose();
});

test("a failed background read shows the button; opening it reads again and reports a failure", async () => {
  let fail = true;
  const { rpc, calls } = fakeRpc(() => { if (fail) throw new Error("offline"); return [order]; });
  const errors: unknown[] = [];
  const print = printDropdown(rpc, "sale.order", () => {}, error => errors.push(error));
  document.body.append(print.node);
  await vi.waitFor(() => expect(print.node.hidden).toBe(false));
  expect(errors).toHaveLength(0);
  const toggle = print.node.querySelector<HTMLButtonElement>(".print-toggle-btn")!;
  toggle.click();
  await vi.waitFor(() => expect(errors).toHaveLength(1));
  toggle.click(); // closes
  fail = false;
  toggle.click();
  await vi.waitFor(() => expect(print.node.querySelector(".print-report-btn")?.textContent).toBe("Order"));
  expect(calls).toHaveLength(3);
  print.dispose();
});

test("the reports may come from a shared source, read once for several buttons", async () => {
  const { rpc, calls } = fakeRpc(() => { throw new Error("not called"); });
  const shared = Promise.resolve([order]);
  const first = printDropdown(rpc, "sale.order", () => {}, () => {}, () => shared);
  const second = printDropdown(rpc, "sale.order", () => {}, () => {}, () => shared);
  await vi.waitFor(() => expect([first.node.hidden, second.node.hidden]).toEqual([false, false]));
  expect(calls).toHaveLength(0);
  first.dispose(); second.dispose();
});

test("picking a report closes the Print menu and prints with that report", async () => {
  const { rpc } = fakeRpc(() => [order]);
  const pick = vi.fn();
  const print = printDropdown(rpc, "sale.order", pick, () => {});
  document.body.append(print.node);
  const toggle = print.node.querySelector<HTMLButtonElement>(".print-toggle-btn")!;
  toggle.click();
  await vi.waitFor(() => expect(print.node.querySelector(".print-report-btn")).not.toBeNull());
  print.node.querySelector<HTMLButtonElement>(".print-report-btn")!.click();
  expect(pick).toHaveBeenCalledWith(order);
  expect(print.node.querySelector(".print-menu")!.classList.contains("open")).toBe(false);
  print.dispose();
});

test("every Print entry is a menu item with its own accessible name", async () => {
  const { rpc } = fakeRpc(() => [order, { report: "sale.order.quote", name: "Quotation" }]);
  const print = printDropdown(rpc, "sale.order", () => {}, () => {});
  document.body.append(print.node);
  print.node.querySelector<HTMLButtonElement>(".print-toggle-btn")!.click();
  await vi.waitFor(() => expect(print.node.querySelectorAll(".print-report-btn")).toHaveLength(2));
  expect([...print.node.querySelectorAll(".print-report-btn")].map(item => item.getAttribute("role"))).toEqual(["menuitem", "menuitem"]);
  expect([...print.node.querySelectorAll(".print-report-btn")].map(item => item.getAttribute("aria-label"))).toEqual(["Print Order", "Print Quotation"]);
  print.dispose();
});

test("a Print button disposed before the reports arrive adds nothing", async () => {
  let release!: (value: unknown) => void;
  const rpc = { call: vi.fn(() => new Promise(resolve => { release = resolve; })) } as unknown as RpcClient;
  const print = printDropdown(rpc, "sale.order", () => {}, () => {});
  document.body.append(print.node);
  print.dispose();
  release([order]);
  await new Promise(resolve => setTimeout(resolve, 10));
  expect(print.node.querySelector(".print-report-btn")).toBeNull();
  expect(print.node.hidden).toBe(true);
});
