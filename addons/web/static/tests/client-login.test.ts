/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
import { beforeEach, expect, test } from "vitest";
import { guardSubmit, setSubmitBusy, togglePasswordReveal, updateCapsLockHint } from "../src/client/login.js";

// The markup of the web.HumanLogin form that the behaviour reads.
beforeEach(() => {
  document.body.innerHTML = `
    <form class="login-form"><div class="login-field"><div class="login-password">
      <input id="login-password" type="password"/>
      <button type="button" class="login-reveal" aria-pressed="false">Show</button></div>
      <p data-login-capslock="" hidden>Caps Lock is on.</p></div>
      <button type="submit" data-login-submit="">Sign in</button></form>`;
});
const form = (): HTMLFormElement => document.querySelector("form")!;
const submit = (): HTMLButtonElement => document.querySelector("[data-login-submit]")!;
const fire = (target: Element, event: Event, handler: (event: never) => void): Event => {
  target.addEventListener(event.type, handler as EventListener, { once: true });
  target.dispatchEvent(event);
  return event;
};

test("the first submit goes through and marks the button busy; a second one is cancelled", () => {
  const first = fire(form(), new Event("submit", { cancelable: true }), guardSubmit);
  expect(first.defaultPrevented).toBe(false);
  expect(submit().textContent).toBe("Signing in…");
  expect(submit().getAttribute("aria-busy")).toBe("true");
  const second = fire(form(), new Event("submit", { cancelable: true }), guardSubmit);
  expect(second.defaultPrevented).toBe(true);
  setSubmitBusy(form(), false);
  expect(submit().textContent).toBe("Sign in");
  expect(fire(form(), new Event("submit", { cancelable: true }), guardSubmit).defaultPrevented).toBe(false);
});

test("the reveal toggle shows and hides the password and reports its state", () => {
  const toggle = document.querySelector<HTMLButtonElement>(".login-reveal")!;
  const input = document.querySelector<HTMLInputElement>("#login-password")!;
  fire(toggle, new MouseEvent("click"), togglePasswordReveal);
  expect([input.type, toggle.textContent, toggle.getAttribute("aria-pressed")]).toEqual(["text", "Hide", "true"]);
  expect(document.activeElement).toBe(input);
  fire(toggle, new MouseEvent("click"), togglePasswordReveal);
  expect([input.type, toggle.textContent, toggle.getAttribute("aria-pressed")]).toEqual(["password", "Show", "false"]);
});

test("the Caps Lock hint follows the key state", () => {
  const input = document.querySelector<HTMLInputElement>("#login-password")!;
  const hint = document.querySelector<HTMLElement>("[data-login-capslock]")!;
  fire(input, new KeyboardEvent("keyup", { modifierCapsLock: true }), updateCapsLockHint);
  expect(hint.hidden).toBe(false);
  fire(input, new KeyboardEvent("keyup", { modifierCapsLock: false }), updateCapsLockHint);
  expect(hint.hidden).toBe(true);
});

test("markup without the expected parts fails loudly", () => {
  expect(() => guardSubmit(new Event("submit"))).toThrow("Expected the sign-in form.");
  document.body.innerHTML = `<form></form>`;
  expect(() => setSubmitBusy(form(), true)).toThrow("Expected the sign-in button.");
});
