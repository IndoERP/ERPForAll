/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
import { afterEach, describe, expect, test, vi } from "vitest";
import { openCommandPalette, type Command } from "../src/client/command-palette.js";
import { ActionMenus, executeActionMenuItem, loadAvailablePrintItems } from "../src/client/action-menus.js";
import { renderCogMenu } from "../src/client/cog-menu.js";
import { Pager } from "../src/client/pager.js";
import { BreadcrumbTrail, renderBreadcrumbs } from "../src/client/navigation.js";
import { HomeMenu } from "../src/client/home-menu.js";
import { renderDebugMenu } from "../src/client/debug-menu.js";
import { renderUserMenuItems } from "../src/client/user-menu.js";
import { deliverFailure } from "../src/client/failure-owner.js";
import { ClientError, type RpcClient } from "../src/client/transport.js";

afterEach(() => {
  vi.restoreAllMocks();
  document.body.replaceChildren();
});

const enter = (target: HTMLElement, init: KeyboardEventInit = {}): void => {
  target.dispatchEvent(new KeyboardEvent("keydown", { key: "Enter", ...init }));
};
const paletteInput = (node: HTMLElement): HTMLInputElement => node.querySelector<HTMLInputElement>(".command-palette-input")!;

describe("command palette failures", () => {
  /// (partial: a failing provider is reported and the others still render)
  test("each provider settles on its own and a failure is reported", async () => {
    const report = vi.fn();
    const good: Command[] = [{ name: "Open partners", action: () => {} }];
    const palette = openCommandPalette({
      providers: [
        { provide: () => good },
        { provide: () => { throw new Error("sync provider"); } },
        { provide: async () => { throw new Error("async provider"); } }
      ],
      report
    });
    await vi.waitFor(() => expect(palette.node.querySelectorAll(".command-palette-item")).toHaveLength(1));
    await vi.waitFor(() => expect(report).toHaveBeenCalledTimes(2));
    expect(report.mock.calls.map(([error]) => (error as Error).message).sort()).toEqual(["async provider", "sync provider"]);
    palette.close();
  });

  /// (partial: a broken provider is reported once per palette session, not on every keystroke)
  test("a failing provider is reported once however many searches follow", async () => {
    const report = vi.fn();
    const palette = openCommandPalette({ providers: [{ provide: async () => { throw new Error("broken provider"); } }], report });
    await vi.waitFor(() => expect(report).toHaveBeenCalledTimes(1));
    const input = paletteInput(palette.node);
    for (const text of ["a", "ab", "abc"]) {
      input.value = text;
      input.dispatchEvent(new Event("input"));
      await vi.waitFor(() => expect(palette.node.querySelector(".command-palette-empty")?.getAttribute("style") ?? "").toContain("block"));
    }
    expect(report).toHaveBeenCalledTimes(1);
    palette.close();
  });

  /// (partial: command palette focus-trap failure is reported, not swallowed)
  test("focus-trap failure in command palette is reported via config.report", async () => {
    const elang = await import("@erpforall/elang");
    const trapSpy = vi.spyOn(elang, "createFocusTrap").mockImplementationOnce(() => {
      throw new Error("Focus trap initialization failed");
    });
    const report = vi.fn();
    const palette = openCommandPalette({ report });
    expect(report).toHaveBeenCalledWith(expect.objectContaining({ message: "Focus trap initialization failed" }));
    palette.close();
    trapSpy.mockRestore();
  });

  /// (partial: a failure without a report surfaces as an Error, even when a non-Error was thrown)
  test("a non-Error failure without a report is wrapped in an Error for the global handler", () => {
    vi.useFakeTimers();
    try {
      deliverFailure({}, "plain text");
      expect(() => vi.runAllTimers()).toThrow(new Error("plain text"));
    } finally { vi.useRealTimers(); }
  });

  /// (partial: commands start through the host's run, after the palette closed)
  test("a command runs through run once the palette is closed and its failure is reported", async () => {
    const report = vi.fn();
    const runs: string[] = [];
    let openAtAction: boolean | undefined;
    let palette!: ReturnType<typeof openCommandPalette>;
    const failing = new Error("command failed");
    palette = openCommandPalette({
      providers: [{ provide: () => [{ name: "Boom", action: async () => { openAtAction = palette.isOpen; throw failing; } }] }],
      run: job => { runs.push("run"); job().catch(report); }
    });
    await vi.waitFor(() => expect(palette.node.querySelectorAll(".command-palette-item")).toHaveLength(1));
    enter(paletteInput(palette.node));
    await vi.waitFor(() => expect(report).toHaveBeenCalledWith(failing));
    expect(runs).toEqual(["run"]);
    expect(openAtAction).toBe(false);
  });

  test("a rejecting command without a host run still reaches report", async () => {
    const report = vi.fn();
    const failing = new Error("no run");
    const palette = openCommandPalette({ providers: [{ provide: () => [{ name: "Boom", action: () => Promise.reject(failing) }] }], report });
    await vi.waitFor(() => expect(palette.node.querySelectorAll(".command-palette-item")).toHaveLength(1));
    enter(paletteInput(palette.node));
    await vi.waitFor(() => expect(report).toHaveBeenCalledWith(failing));
  });

  /// (partial: a second Enter while the search is pending runs the command once)
  test("a second Enter while the search is pending does not run the command twice", async () => {
    let release!: () => void;
    const gate = new Promise<void>(resolve => { release = resolve; });
    const action = vi.fn();
    const palette = openCommandPalette({ providers: [{ provide: async () => { await gate; return [{ name: "Once", action }]; } }] });
    const input = paletteInput(palette.node);
    enter(input);
    enter(input);
    release();
    await vi.waitFor(() => expect(palette.isOpen).toBe(false));
    await new Promise(resolve => setTimeout(resolve, 20));
    expect(action).toHaveBeenCalledTimes(1);
  });

  /// (partial: provider links are validated and opened without opener access)
  test("provider links refuse a script scheme and open new tabs with noopener", async () => {
    const open = vi.spyOn(window, "open").mockReturnValue(null);
    const report = vi.fn();
    const palette = openCommandPalette({
      providers: [{ provide: () => [{ name: "Script", href: "javascript:alert(1)" }, { name: "Docs", href: "https://example.com/docs" }] }],
      report
    });
    await vi.waitFor(() => expect(palette.node.querySelectorAll(".command-palette-item")).toHaveLength(2));
    enter(paletteInput(palette.node));
    await vi.waitFor(() => expect(report).toHaveBeenCalledWith(expect.objectContaining({ kind: "validation" })));
    expect(open).not.toHaveBeenCalled();

    const second = openCommandPalette({ providers: [{ provide: () => [{ name: "Docs", href: "https://example.com/docs" }] }], report });
    await vi.waitFor(() => expect(second.node.querySelectorAll(".command-palette-item")).toHaveLength(1));
    enter(paletteInput(second.node), { ctrlKey: true });
    await vi.waitFor(() => expect(open).toHaveBeenCalledWith("https://example.com/docs", "_blank", "noopener,noreferrer"));
  });

  test("a same-origin link goes through the host router", async () => {
    const navigate = vi.fn(async () => {});
    const palette = openCommandPalette({ providers: [{ provide: () => [{ name: "Partners", href: "/erp/action-20" }] }], navigate });
    await vi.waitFor(() => expect(palette.node.querySelectorAll(".command-palette-item")).toHaveLength(1));
    enter(paletteInput(palette.node));
    await vi.waitFor(() => expect(navigate).toHaveBeenCalledWith(expect.objectContaining({ action: "20" })));
  });
});

describe("action menus", () => {
  const rpcOf = (call: (model: string, method: string, params: unknown) => unknown): RpcClient => ({ call: vi.fn(async (model: string, method: string, params: unknown) => call(model, method, params)) }) as unknown as RpcClient;

  /// (partial: a failed availability call is not an empty report list)
  test("a failed report availability call rejects instead of showing no report", async () => {
    await expect(loadAvailablePrintItems({
      items: [{ id: 1, description: "Invoice", domain: [] }],
      resModel: "res.partner",
      activeIds: [1],
      fetchValidReportIds: async () => { throw new Error("availability rpc down"); }
    })).rejects.toThrow("availability rpc down");
  });

  test("a failing item is reported through the environment", async () => {
    const report = vi.fn();
    const failing = new Error("doAction failed");
    const menus = new ActionMenus([{ description: "Go", action: 5 }], { resModel: "res.partner", getActiveIds: () => [1], doAction: async () => { throw failing; }, report });
    document.body.append(menus.render());
    document.querySelector<HTMLButtonElement>(".action-menus-trigger")!.click();
    document.querySelector<HTMLButtonElement>(".action-menu-item")!.click();
    await vi.waitFor(() => expect(report).toHaveBeenCalledWith(failing));
    menus.dispose();
  });

  /// (partial: select-all fails loudly)
  test("select-all fails on a non-list answer and on more records than the cap", async () => {
    const doAction = vi.fn();
    const base = { resModel: "res.partner", getActiveIds: () => [1], isDomainSelected: true, domain: [], doAction };
    await expect(executeActionMenuItem({ action: 1 }, { ...base, rpc: rpcOf(() => ({})) })).rejects.toBeInstanceOf(ClientError);
    const rpc = rpcOf((_model, method) => method === "search" ? Array.from({ length: 5000 }, (_, i) => i + 1) : 12000);
    await expect(executeActionMenuItem({ action: 1 }, { ...base, rpc })).rejects.toThrow(/12000 records match/);
    expect(doAction).not.toHaveBeenCalled();
  });

  test("an item URL is validated before it is opened", async () => {
    const open = vi.spyOn(window, "open").mockReturnValue(null);
    await expect(executeActionMenuItem({ url: "javascript:alert(1)" }, { resModel: "res.partner", getActiveIds: () => [] })).rejects.toMatchObject({ kind: "validation" });
    expect(open).not.toHaveBeenCalled();
  });
});

describe("cog, debug and user menus", () => {
  test("a rejecting display predicate is reported and a script link is refused", async () => {
    const report = vi.fn();
    const open = vi.spyOn(window, "open").mockReturnValue(null);
    const failing = new Error("isDisplayed failed");
    const handle = renderCogMenu([{ key: "a", label: "Broken", isDisplayed: async () => { throw failing; } }], { report });
    await vi.waitFor(() => expect(report).toHaveBeenCalledWith(failing));
    handle.dispose();

    const linkReport = vi.fn();
    const links = renderCogMenu([{ key: "b", label: "Script", href: "javascript:alert(1)" }], { report: linkReport });
    document.body.append(links.node);
    await vi.waitFor(() => expect(links.node.querySelector(".cog-menu-item")).not.toBeNull());
    links.node.querySelector<HTMLElement>(".cog-menu-item")!.click();
    await vi.waitFor(() => expect(linkReport).toHaveBeenCalledWith(expect.objectContaining({ kind: "validation" })));
    expect(open).not.toHaveBeenCalled();
    links.dispose();
  });

  test("debug and user menu links are validated, not followed by the browser", async () => {
    const report = vi.fn();
    const container = document.createElement("div");
    document.body.append(container);
    const handle = await renderDebugMenu(container, [() => [{ id: "x", description: "Script", href: "javascript:alert(1)" }]], { get: () => ({ label: "Tools", sequence: 1 }), has: () => true }, { report });
    const item = container.querySelector<HTMLElement>(".debug-menu-item")!;
    expect(item.tagName).toBe("BUTTON");
    item.click();
    await vi.waitFor(() => expect(report).toHaveBeenCalledWith(expect.objectContaining({ kind: "validation" })));
    handle.dispose();

    const userReport = vi.fn();
    const userBox = document.createElement("div");
    document.body.append(userBox);
    const user = renderUserMenuItems(userBox, [{ id: "u", description: "Script", href: "javascript:alert(1)" }], { report: userReport });
    userBox.querySelector<HTMLElement>(".user-menu-item")!.click();
    await vi.waitFor(() => expect(userReport).toHaveBeenCalledWith(expect.objectContaining({ kind: "validation" })));
    user.dispose();
  });
});

describe("pager, breadcrumbs and home menu", () => {
  test("a failed pager update reaches report", async () => {
    const report = vi.fn();
    const failing = new Error("page update failed");
    const pager = new Pager({ offset: 0, limit: 5, total: 20, onUpdate: async () => { throw failing; }, report });
    document.body.append(pager.render());
    document.querySelector<HTMLButtonElement>(".pager-next")!.click();
    await vi.waitFor(() => expect(report).toHaveBeenCalledWith(failing));
  });

  test("a failed breadcrumb navigation reaches report", async () => {
    const report = vi.fn();
    const failing = new Error("crumb failed");
    const trail = new BreadcrumbTrail();
    const route = {};
    trail.push({ id: "list", title: "List", route, url: "/erp", onSelected: async () => { throw failing; } });
    trail.push({ id: "form", title: "Form", route, url: "/erp", isFormView: true, onSelected: async () => {} });
    const container = document.createElement("div");
    document.body.append(container);
    renderBreadcrumbs(container, trail, { isSmall: false, report });
    container.querySelector<HTMLButtonElement>(".breadcrumb-back")!.click();
    await vi.waitFor(() => expect(report).toHaveBeenCalledWith(failing));
  });

  /// (partial: Escape closes once and focus returns to the opener)
  test("Escape closes the home menu once and gives focus back to the opener", () => {
    const opener = document.createElement("button");
    document.body.append(opener);
    opener.focus();
    const onClose = vi.fn();
    const home = new HomeMenu({
      container: document.body, apps: [{ id: 1, label: "Sales", actionId: 1 }], hasBackgroundAction: () => false, onOpenApp: () => {}, onClose
    });
    home.open();
    expect(document.activeElement).not.toBe(opener);
    window.dispatchEvent(new KeyboardEvent("keydown", { key: "Escape" }));
    expect(onClose).toHaveBeenCalledTimes(1);
    expect(document.activeElement).toBe(opener);
    home.dispose();
  });
});
