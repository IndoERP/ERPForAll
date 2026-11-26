/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
import { clearProtectedTabState } from "./session-boundary.js";
import { Component, mount, onMounted, onWillDestroy } from "@erpforall/elang";
import { templates } from "@erpforall/human-templates";
import { parseBoundedJson } from "../../../../../web/packages/expr/src/index.js";
import { configureClientTemplates, systrayItems } from "./registry.js";
import { registerBuiltinSystrayItems } from "./activity-menu.js";
import { HumanApp } from "./app.js";
import "./technical-sessions.js";
import "./translation-catalog.js";
import { el, errorMessage } from "./dom.js";
import { guardSubmit, setSubmitBusy, togglePasswordReveal, updateCapsLockHint } from "./login.js";
import { object, text } from "./metadata.js";
import type { LoginPage as Login } from "../generated/human-ui.js";
import { prepareClientMount } from "./mount-point.js";
import { productName } from "./product-name.js";
class LoginPage extends Component<{ page: Login; productName: string }> {
  static override template = "web.HumanLogin";
  private form: HTMLFormElement | null = null;
  override setup(): void {
    // A page restored from the back/forward cache must accept a new sign-in.
    const restore = (event: PageTransitionEvent): void => { if (event.persisted && this.form) setSubmitBusy(this.form, false); };
    onMounted(() => window.addEventListener("pageshow", restore));
    onWillDestroy(() => window.removeEventListener("pageshow", restore));
  }
  onSubmit(event: Event): void {
    if (event.currentTarget instanceof HTMLFormElement) this.form = event.currentTarget;
    guardSubmit(event);
  }
  onReveal(event: Event): void { togglePasswordReveal(event); }
  onPasswordKey(event: Event): void { updateCapsLockHint(event); }
  async onExternalSubmit(event: Event): Promise<void> {
    event.preventDefault();
    const form = event.currentTarget;
    if (!(form instanceof HTMLFormElement) || form.dataset["busy"] === "true") return;
    const error = form.closest(".login-card")?.querySelector("[data-external-login-error]");
    form.dataset["busy"] = "true";
    try {
      // Navigate separately from the form request so the OAuth hop never needs an exception to
      // the login CSP's form-action directive.
      const fields = new URLSearchParams();
      for (const [key, value] of new FormData(form)) if (typeof value === "string") fields.set(key, value);
      const reply = await fetch(form.action, { method: "POST", headers: { accept: "application/json" }, body: fields, redirect: "error" });
      if (!reply.ok) throw new Error("External sign-in is unavailable.");
      const { location } = object(await reply.json(), "external sign-in response");
      const next = new URL(text(location, "identity provider URL"));
      if (next.protocol !== "https:" && !(next.protocol === "http:" && (next.hostname === "127.0.0.1" || next.hostname === "[::1]"))) {
        throw new Error("External sign-in is unavailable.");
      }
      window.location.assign(next.href);
    } catch {
      if (error instanceof HTMLElement) {
        error.textContent = "External sign-in is unavailable.";
        error.hidden = false;
      }
    } finally { form.dataset["busy"] = "false"; }
  }
}
class Shell extends Component<{ host: HTMLElement; bootstrap: unknown }> {
  static override template = "web.HumanClient";
  override setup(): void {
    let app: HumanApp | undefined;
    onMounted(() => { app = new HumanApp(this.props.host, this.props.bootstrap); app.run(() => app!.start(true)); });
    onWillDestroy(() => app?.dispose());
  }
}
export async function boot(documentRoot: Document = document): Promise<void> {
  configureClientTemplates(templates);
  const host = prepareClientMount(documentRoot);
  const fail = (error: unknown): void => {
    const main = el("main"), alert = el("p", errorMessage(error), "error");
    alert.setAttribute("role", "alert"); main.append(el("h1", "Unable to load the application"), alert); host.replaceChildren(main);
  };
  try {
    const login = documentRoot.getElementById("erp-login"), bootstrap = documentRoot.getElementById("erp-bootstrap");
    if (login && bootstrap || !login && !bootstrap) throw new Error("Invalid application page data.");
    const value = parseBoundedJson((login ?? bootstrap)!.textContent ?? "");
    if (login) {
      clearProtectedTabState();
      const data = object(value, "login page"), redirect = text(data["redirect"], "redirect");
      if (!redirect.startsWith("/") || redirect.startsWith("//")) throw new Error("Invalid sign-in redirect.");
      const target = new URL(redirect, window.location.origin);
      if (target.origin !== window.location.origin) throw new Error("Invalid sign-in redirect origin.");
      const page: Login = { csrf: text(data["csrf"], "CSRF token"), database: text(data["database"], "database"), redirect, error: data["error"] === null ? null : text(data["error"], "login error") };
      const externalLabel = data["external_label"], canLink = data["can_link_external"];
      if (externalLabel !== undefined) page.external_label = text(externalLabel, "external sign-in provider");
      if (canLink !== undefined) {
        if (typeof canLink !== "boolean" || !page.external_label) throw new Error("Invalid external sign-in policy.");
        page.can_link_external = canLink;
      }
      await mount(LoginPage, host, { templates, props: { page, productName: productName(documentRoot) }, dev: false, onError: fail });
    } else {
      registerBuiltinSystrayItems(systrayItems);
      await mount(Shell, host, { templates, props: { host, bootstrap: value }, dev: false, onError: fail });
    }
  } catch (error) { fail(error); }
}
