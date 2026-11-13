/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
import { afterEach, beforeEach, describe, expect, test, vi } from "vitest";
import { attachMenuDropdown, type MenuDropdownController } from "../src/client/menu-dropdown.js";

describe("attachMenuDropdown", () => {
  let host: HTMLElement;
  let trigger: HTMLButtonElement;
  let menu: HTMLElement;
  let item1: HTMLButtonElement;
  let item2: HTMLButtonElement;
  let item3: HTMLButtonElement;
  let controller: MenuDropdownController | undefined;

  beforeEach(() => {
    document.body.replaceChildren();
    host = document.createElement("div");
    document.body.append(host);

    trigger = document.createElement("button");
    trigger.type = "button";
    trigger.textContent = "Toggle Menu";

    menu = document.createElement("div");
    menu.className = "test-menu-dropdown";

    item1 = document.createElement("button");
    item1.type = "button";
    item1.setAttribute("role", "menuitem");
    item1.textContent = "Item 1";

    item2 = document.createElement("button");
    item2.type = "button";
    item2.setAttribute("role", "menuitem");
    item2.textContent = "Item 2";

    item3 = document.createElement("button");
    item3.type = "button";
    item3.setAttribute("role", "menuitem");
    item3.textContent = "Item 3";

    menu.append(item1, item2, item3);
    host.append(trigger, menu);
  });

  afterEach(() => {
    controller?.dispose();
    controller = undefined;
    document.body.replaceChildren();
    vi.restoreAllMocks();
  });

  test("initializes accessibility attributes and hidden state in display mode", () => {
    controller = attachMenuDropdown({ trigger, menu, mode: "display" });

    expect(trigger.getAttribute("aria-haspopup")).toBe("true");
    expect(trigger.getAttribute("aria-expanded")).toBe("false");
    expect(menu.getAttribute("role")).toBe("menu");
    expect(menu.style.display).toBe("none");
    expect(controller.isOpen()).toBe(false);
  });

  test("initializes accessibility attributes and hidden state in class-open mode", () => {
    controller = attachMenuDropdown({ trigger, menu, mode: "class-open" });

    expect(trigger.getAttribute("aria-haspopup")).toBe("true");
    expect(trigger.getAttribute("aria-expanded")).toBe("false");
    expect(menu.getAttribute("role")).toBe("menu");
    expect(menu.classList.contains("open")).toBe(false);
    expect(controller.isOpen()).toBe(false);
  });

  test("opens on trigger click, sets aria-expanded true, and moves focus to first item", () => {
    controller = attachMenuDropdown({ trigger, menu, mode: "display" });

    trigger.click();

    expect(controller.isOpen()).toBe(true);
    expect(trigger.getAttribute("aria-expanded")).toBe("true");
    expect(menu.style.display).toBe("block");
    expect(document.activeElement).toBe(item1);
    expect(item1.tabIndex).toBe(0);
    expect(item2.tabIndex).toBe(-1);
    expect(item3.tabIndex).toBe(-1);
  });

  test("opens with ArrowDown on trigger and moves focus to first item", () => {
    controller = attachMenuDropdown({ trigger, menu, mode: "display" });
    trigger.focus();

    trigger.dispatchEvent(new KeyboardEvent("keydown", { key: "ArrowDown", bubbles: true }));

    expect(controller.isOpen()).toBe(true);
    expect(document.activeElement).toBe(item1);
  });

  test("opens with ArrowUp on trigger and moves focus to last item", () => {
    controller = attachMenuDropdown({ trigger, menu, mode: "display" });
    trigger.focus();

    trigger.dispatchEvent(new KeyboardEvent("keydown", { key: "ArrowUp", bubbles: true }));

    expect(controller.isOpen()).toBe(true);
    expect(document.activeElement).toBe(item3);
    expect(item3.tabIndex).toBe(0);
    expect(item1.tabIndex).toBe(-1);
  });

  test("roving focus inside menu with ArrowDown, ArrowUp, Home, and End", () => {
    controller = attachMenuDropdown({ trigger, menu, mode: "display" });
    controller.open("first");
    expect(document.activeElement).toBe(item1);

    menu.dispatchEvent(new KeyboardEvent("keydown", { key: "ArrowDown", bubbles: true }));
    expect(document.activeElement).toBe(item2);
    expect(item2.tabIndex).toBe(0);
    expect(item1.tabIndex).toBe(-1);

    menu.dispatchEvent(new KeyboardEvent("keydown", { key: "ArrowDown", bubbles: true }));
    expect(document.activeElement).toBe(item3);

    // Wraps to start
    menu.dispatchEvent(new KeyboardEvent("keydown", { key: "ArrowDown", bubbles: true }));
    expect(document.activeElement).toBe(item1);

    // Wraps to end
    menu.dispatchEvent(new KeyboardEvent("keydown", { key: "ArrowUp", bubbles: true }));
    expect(document.activeElement).toBe(item3);

    // Home goes to first item
    menu.dispatchEvent(new KeyboardEvent("keydown", { key: "Home", bubbles: true }));
    expect(document.activeElement).toBe(item1);

    // End goes to last item
    menu.dispatchEvent(new KeyboardEvent("keydown", { key: "End", bubbles: true }));
    expect(document.activeElement).toBe(item3);
  });

  test("skips disabled and aria-disabled items during roving focus", () => {
    item2.disabled = true;
    controller = attachMenuDropdown({ trigger, menu, mode: "display" });
    controller.open("first");
    expect(document.activeElement).toBe(item1);

    menu.dispatchEvent(new KeyboardEvent("keydown", { key: "ArrowDown", bubbles: true }));
    // Skips disabled item2 and moves straight to item3
    expect(document.activeElement).toBe(item3);
  });

  test("Escape inside menu closes menu and restores focus to trigger", () => {
    controller = attachMenuDropdown({ trigger, menu, mode: "display" });
    controller.open("first");
    expect(document.activeElement).toBe(item1);

    menu.dispatchEvent(new KeyboardEvent("keydown", { key: "Escape", bubbles: true }));

    expect(controller.isOpen()).toBe(false);
    expect(trigger.getAttribute("aria-expanded")).toBe("false");
    expect(menu.style.display).toBe("none");
    expect(document.activeElement).toBe(trigger);
  });

  test("Escape on trigger closes menu when open and keeps focus on trigger", () => {
    controller = attachMenuDropdown({ trigger, menu, mode: "display" });
    controller.open("first");
    trigger.focus();

    trigger.dispatchEvent(new KeyboardEvent("keydown", { key: "Escape", bubbles: true }));

    expect(controller.isOpen()).toBe(false);
    expect(trigger.getAttribute("aria-expanded")).toBe("false");
    expect(document.activeElement).toBe(trigger);
  });

  test("Tab inside menu closes menu without returning focus to trigger", () => {
    controller = attachMenuDropdown({ trigger, menu, mode: "display" });
    controller.open("first");

    menu.dispatchEvent(new KeyboardEvent("keydown", { key: "Tab", bubbles: true }));

    expect(controller.isOpen()).toBe(false);
    expect(trigger.getAttribute("aria-expanded")).toBe("false");
    expect(menu.style.display).toBe("none");
  });

  test("document pointerdown outside trigger and menu closes menu", () => {
    controller = attachMenuDropdown({ trigger, menu, mode: "display" });
    controller.open("first");

    const outside = document.createElement("div");
    document.body.append(outside);

    outside.dispatchEvent(new MouseEvent("pointerdown", { bubbles: true }));

    expect(controller.isOpen()).toBe(false);
    expect(trigger.getAttribute("aria-expanded")).toBe("false");
    expect(menu.style.display).toBe("none");
  });

  test("refresh updates items when new elements are added dynamically", () => {
    controller = attachMenuDropdown({ trigger, menu, mode: "display" });
    controller.open("first");

    const item4 = document.createElement("button");
    item4.type = "button";
    item4.setAttribute("role", "menuitem");
    item4.textContent = "Item 4";
    menu.append(item4);

    controller.refresh();
    menu.dispatchEvent(new KeyboardEvent("keydown", { key: "End", bubbles: true }));
    expect(document.activeElement).toBe(item4);
  });
  test("typing a letter moves focus to the next entry starting with it", () => {
    const zeta = document.createElement("button");
    zeta.type = "button";
    zeta.setAttribute("role", "menuitem");
    zeta.textContent = "Zeta";
    menu.append(zeta);
    controller = attachMenuDropdown({ trigger, menu, mode: "display" });
    controller.open("first");

    menu.dispatchEvent(new KeyboardEvent("keydown", { key: "z", bubbles: true }));
    expect(document.activeElement).toBe(zeta);
    menu.dispatchEvent(new KeyboardEvent("keydown", { key: "i", bubbles: true }));
    expect(document.activeElement).toBe(item1);
  });

  test("class-open mode follows an external removal of the open class", () => {
    controller = attachMenuDropdown({ trigger, menu, mode: "class-open" });
    trigger.click();
    expect(menu.classList.contains("open")).toBe(true);

    menu.classList.remove("open");
    trigger.click();

    expect(menu.classList.contains("open")).toBe(true);
    expect(trigger.getAttribute("aria-expanded")).toBe("true");
  });

  test("type-ahead leaves typed letters alone inside an editable field of the menu", () => {
    const input = document.createElement("input");
    menu.append(input);
    controller = attachMenuDropdown({ trigger, menu, mode: "class-open" });
    trigger.click();
    const typed = new KeyboardEvent("keydown", { key: "i", bubbles: true, cancelable: true });
    input.dispatchEvent(typed);
    expect(typed.defaultPrevented).toBe(false);
    const onItem = new KeyboardEvent("keydown", { key: "i", bubbles: true, cancelable: true });
    item1.dispatchEvent(onItem);
    expect(onItem.defaultPrevented).toBe(true);
  });
});
