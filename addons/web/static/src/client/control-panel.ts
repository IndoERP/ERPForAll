/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
import { Component, mount, unmount } from "@erpforall/elang";
import { clientTemplates, controlPanelExtensions } from "./registry.js";
export interface ControlPanelProps { title: string; model: string }
class ControlPanel extends Component<ControlPanelProps> { static override template = "web.HumanControlPanel"; }
export async function mountControlPanel(host: HTMLElement, props: ControlPanelProps, report: (error: unknown) => void): Promise<{ panel: HTMLElement; actions: HTMLElement; dispose(): void }> {
  const root = document.createElement("div"); host.append(root);
  const mounted: HTMLElement[] = [root];
  try {
    await mount(ControlPanel, root, { templates: clientTemplates(), props, dev: false, onError: report });
    const panel = root.querySelector<HTMLElement>("[data-control-panel]"), actions = root.querySelector<HTMLElement>("[data-control-actions]"), anchor = root.querySelector<HTMLElement>("[data-control-extension]");
    if (!panel || !actions || !anchor) throw new Error("Compiled control panel is missing a public extension anchor.");
    for (const [, extension] of controlPanelExtensions.entries()) {
      if (extension.model !== props.model) continue;
      const target = document.createElement("div"); anchor.append(target); mounted.push(target);
      await mount(extension.component, target, { templates: clientTemplates(), props: { ...props }, dev: false, onError: report });
    }
    return { panel, actions, dispose: () => { for (const target of mounted.reverse()) unmount(target); } };
  } catch (error) { for (const target of mounted.reverse()) unmount(target); throw error; }
}
