/*! SPDX-License-Identifier: MIT OR Apache-2.0 */

/** The server's legacy main is only a mount point; the mounted page owns its main landmark. */
export function prepareClientMount(documentRoot: Document): HTMLElement {
  const host = documentRoot.getElementById("erp-root");
  if (!host) throw new Error("Application mount point is missing.");
  if (host.tagName !== "MAIN") return host;
  const container = documentRoot.createElement("div");
  for (const attribute of host.attributes) container.setAttribute(attribute.name, attribute.value);
  container.append(...host.childNodes);
  host.replaceWith(container);
  return container;
}
