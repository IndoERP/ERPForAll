/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
// Test-only entry point. Do not import from a production runtime or view.
import axe from "axe-core";
import type { AxeResults } from "axe-core";

/** Minimum result shape shared by component and browser audits. */
export interface AxeReport {
  readonly violations: readonly {
    readonly id: string;
    readonly impact?: string | null;
    readonly nodes: readonly { readonly target: readonly unknown[] }[];
  }[];
}
/** Fail for each serious/critical violation, retaining all other results for caller review. */
export function assertAxeResults(results: AxeReport): void {
  const failures = results.violations.filter(v=>v.impact === "serious" || v.impact === "critical");
  if (failures.length) throw new Error(`Accessibility audit failed: ${failures.map(v=>`${v.id} (${v.impact}): ${v.nodes.map(n=>n.target.join(" ")).join(", ")}`).join("; ")}`);
}
/** Audit mounted jsdom component semantics. Color contrast requires auditPage in a real browser. */
export async function auditComponent(root: HTMLElement): Promise<AxeResults> {
  if (!root.isConnected) throw new Error("Accessibility audit requires a connected component root");
  const results = await axe.run(root,{rules:{"color-contrast":{enabled:false}}});
  assertAxeResults(results); return results;
}
/** Structural subset of a Playwright Page: no Playwright dependency in production or component tests. */
export interface AxePage {
  addScriptTag(options: {content: string}): Promise<unknown>;
  evaluate<T>(fn: () => Promise<T>): Promise<T>;
}
/** Audit the whole rendered document, including open overlays; injection or engine failures propagate. */
export async function auditPage(page: AxePage): Promise<AxeResults> {
  await page.addScriptTag({content:axe.source});
  const results = await page.evaluate(async()=>{
    const engine = (window as unknown as {axe: typeof axe}).axe;
    if (!engine) throw new Error("Accessibility engine failed to load");
    return engine.run(document);
  });
  assertAxeResults(results); return results;
}
