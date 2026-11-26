/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
import type { TemplateModule } from "../../src/index.js";

// Controller fixtures, not proof of compiled XML. The real capture tests template integration.
export const templates: readonly TemplateModule[] = [
  { name: "web.HumanLogin", render: value => {
    const main = document.createElement("main"); main.className = "login-screen";
    const title = document.createElement("h1"); title.textContent = "Sign in"; main.append(title);
    const self = value as { props: { page: { external_label?: string; can_link_external?: boolean; csrf: string } }; onExternalSubmit(event: Event): Promise<void> };
    const page = self.props.page;
    if (page.external_label) {
      const card = document.createElement("div"); card.className = "login-card"; main.append(card);
      const error = document.createElement("p"); error.dataset["externalLoginError"] = ""; error.hidden = true; card.append(error);
      const modes = page.can_link_external ? ["Sign in with", "Connect", "Disconnect"] : ["Sign in with"];
      for (const mode of modes) {
        const form = document.createElement("form");
        form.setAttribute("action", mode === "Disconnect" ? "/erp/session/external/revoke" : "/erp/login/external");
        const fields = mode === "Connect" ? { csrf_token: page.csrf, link: "1" } : { csrf_token: page.csrf };
        for (const [name, value] of Object.entries(fields)) {
          const field = document.createElement("input"); field.type = "hidden"; field.name = name; field.value = value; form.append(field);
        }
        const button = document.createElement("button"); button.textContent = `${mode} ${page.external_label}`; form.append(button);
        if (mode !== "Disconnect") form.addEventListener("submit", event => { void self.onExternalSubmit(event); });
        card.append(form);
      }
    }
    return main;
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
