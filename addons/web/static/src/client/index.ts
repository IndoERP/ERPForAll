/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
import { Component, mount, onMounted, onWillDestroy } from "@erpforall/elang";
import { templates } from "@erpforall/human-templates";
import { parseBoundedJson } from "../../../../../web/packages/expr/src/index.js";
import { configureClientTemplates, systrayItems } from "./registry.js";
import { registerBuiltinSystrayItems } from "./activity-menu.js";
import { HumanApp } from "./app.js";
import { el, errorMessage } from "./dom.js";
import { guardSubmit, setSubmitBusy, togglePasswordReveal, updateCapsLockHint } from "./login.js";
import { object, text } from "./metadata.js";
import type { LoginPage as Login } from "../generated/human-ui.js";
import { prepareClientMount } from "./mount-point.js";
class LoginPage extends Component<{ page: Login }> {
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
  onPasswordKey(event: KeyboardEvent): void { updateCapsLockHint(event); }
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
      const data = object(value, "login page"), redirect = text(data["redirect"], "redirect");
      if (!redirect.startsWith("/") || redirect.startsWith("//")) throw new Error("Invalid sign-in redirect.");
      const target = new URL(redirect, window.location.origin);
      if (target.origin !== window.location.origin) throw new Error("Invalid sign-in redirect origin.");
      const page: Login = { csrf: text(data["csrf"], "CSRF token"), database: text(data["database"], "database"), redirect, error: data["error"] === null ? null : text(data["error"], "login error") };
      await mount(LoginPage, host, { templates, props: { page }, dev: false, onError: fail });
    } else {
      registerBuiltinSystrayItems(systrayItems);
      await mount(Shell, host, { templates, props: { host, bootstrap: value }, dev: false, onError: fail });
    }
  } catch (error) { fail(error); }
}
