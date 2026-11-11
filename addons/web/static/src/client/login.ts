/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
// Sign-in page behaviour on the markup of the web.HumanLogin template.

/** One request per sign-in: a submit while the first is in flight is cancelled. */
export function guardSubmit(event: Event): void {
  const form = event.currentTarget;
  if (!(form instanceof HTMLFormElement)) throw new Error("Expected the sign-in form.");
  if (form.dataset["busy"] === "true") { event.preventDefault(); return; }
  setSubmitBusy(form, true);
}

/** Marks the sign-in button busy or ready again (a page restored from the back/forward cache). */
export function setSubmitBusy(form: HTMLFormElement, on: boolean): void {
  const submit = form.querySelector("[data-login-submit]");
  if (!(submit instanceof HTMLButtonElement)) throw new Error("Expected the sign-in button.");
  form.dataset["busy"] = String(on);
  submit.setAttribute("aria-busy", String(on));
  submit.textContent = on ? "Signing in…" : "Sign in";
}

/** Shows or hides the password; the toggle names the next action and reports its state. */
export function togglePasswordReveal(event: Event): void {
  const toggle = event.currentTarget;
  const input = toggle instanceof Element ? toggle.closest(".login-password")?.querySelector("input") : null;
  if (!(toggle instanceof HTMLButtonElement) || !(input instanceof HTMLInputElement)) throw new Error("Expected the password field.");
  const reveal = input.type === "password";
  input.type = reveal ? "text" : "password";
  toggle.textContent = reveal ? "Hide" : "Show";
  toggle.setAttribute("aria-pressed", String(reveal));
  input.focus();
}

/** Shows the Caps Lock hint of the password field while Caps Lock is on. */
export function updateCapsLockHint(event: KeyboardEvent): void {
  const field = event.currentTarget instanceof Element ? event.currentTarget.closest(".login-field") : null;
  const hint = field?.querySelector("[data-login-capslock]");
  if (!(hint instanceof HTMLElement)) throw new Error("Expected the Caps Lock hint.");
  hint.hidden = !event.getModifierState("CapsLock");
}
