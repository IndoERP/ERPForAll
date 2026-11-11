/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
import type { TemplateModule } from "../../src/index.js";

// Controller fixtures, not proof of compiled XML. The real capture tests template integration.
export const templates: readonly TemplateModule[] = [
  { name: "web.HumanLogin", render: () => {
    const main = document.createElement("main"); main.className = "login-screen";
    const title = document.createElement("h1"); title.textContent = "Sign in"; main.append(title); return main;
  } },
  { name: "web.HumanClient", render: () => {
    const root = document.createElement("div");
    const skip = document.createElement("button"); skip.dataset["skipContent"] = ""; skip.textContent = "Skip to content"; root.append(skip);
    for (const [region, tag] of [["header", "header"], ["navigation", "nav"], ["notice", "div"], ["content", "main"]] as const) {
      const node = document.createElement(tag); node.dataset[`client${region[0]!.toUpperCase()}${region.slice(1)}`] = "";
      if (region === "content") { node.id = "erp-content"; node.tabIndex = -1; }
      if (region === "navigation") node.setAttribute("aria-label", "Application navigation");
      root.append(node);
    }
    return root;
  } },
  { name: "web.HumanControlPanel", render: self => {
    const root = document.createElement("section"); root.dataset["controlPanel"] = "";
    for (const key of ["breadcrumbs", "search", "actions", "extension", "tools"]) {
      const node = document.createElement(key === "breadcrumbs" ? "nav" : "div"); node.setAttribute(`data-control-${key}`, "");
      if (key === "breadcrumbs") {
        node.setAttribute("aria-label", "Breadcrumbs");
        const title = document.createElement("h1"); title.textContent = (self as { props: { title: string } }).props.title; node.append(title);
      }
      root.append(node);
    }
    return root;
  } }
];
