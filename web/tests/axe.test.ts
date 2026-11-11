/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
import { afterEach, expect, test } from "vitest";
import { auditComponent, assertAxeResults, auditPage } from "../testing/axe.js";
afterEach(()=>document.body.replaceChildren());
test("real axe rejects an unlabelled button and accepts a labelled button", async () => {
  const root = document.createElement("main"), button = document.createElement("button"); root.append(button); document.body.append(root);
  await expect(auditComponent(root)).rejects.toThrow(/button-name/);
  button.textContent = "Save"; const results = await auditComponent(root); expect(results.violations).toEqual([]);
});
test("threshold rejects serious and critical; moderate remains available for review", () => {
  const violation = {id:"example",impact:"moderate",nodes:[{target:["#bad"]}]} as const;
  expect(()=>assertAxeResults({violations:[violation]})).not.toThrow();
  expect(()=>assertAxeResults({violations:[{...violation,impact:"serious"}]})).toThrow(/example.*#bad/);
  expect(()=>assertAxeResults({violations:[{...violation,impact:"critical"}]})).toThrow(/critical/);
});
test("browser injection failure propagates rather than recording a pass", async () => {
  await expect(auditPage({addScriptTag:async()=>{throw new Error("CSP blocked axe");},evaluate:async()=>{throw new Error("must not run");}})).rejects.toThrow("CSP blocked axe");
});
