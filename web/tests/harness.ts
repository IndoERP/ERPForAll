/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
// Test harness: mounts components whose templates are written the way the compiler writes them (the
// same ABI calls; tests/contract.test.ts runs real compiler output), and cleans up after each test.

import { afterEach } from "vitest";
import {
  Component,
  type ComponentClass,
  type MountOptions,
  type PropsOf,
  type TemplateModule,
  flushSync,
  mount,
  unmount,
} from "../src/index.js";

/** The `$self` of a test template: a component with free-form fields set in `setup`. */
export type Self = Component<Record<string, unknown>> & Record<string, unknown>;

const mounted: Element[] = [];
const unexpected: unknown[] = [];

afterEach(() => {
  const errors = unexpected.splice(0);
  for (const target of mounted.splice(0)) {
    try {
      unmount(target);
    } catch {
      // the test unmounted it itself
    }
    target.remove();
  }
  flushSync();
  if (errors.length > 0) throw errors[0];
});

/** A fresh mount target attached to the document. */
export function target(): HTMLElement {
  const el = document.createElement("div");
  document.body.appendChild(el);
  mounted.push(el);
  return el;
}

/** A template module from a render function. */
export function tpl(name: string, render: (self: Self) => Node | null): TemplateModule {
  return { name, render: render as (self: object) => Node | null };
}

/** Mounts `Root` on a new target with `templates`; errors go to `errors` unless `onError` is given. */
export async function mountApp<C extends ComponentClass>(
  Root: C,
  templates: TemplateModule[],
  options: Omit<MountOptions, "templates"> & { errors?: unknown[] } = {},
): Promise<{ root: HTMLElement; instance: InstanceType<C> }> {
  const root = target();
  const errors = options.errors;
  const mountOptions: MountOptions<Record<string, unknown>> = {
    templates,
    onError: options.onError ?? ((e) => (errors ?? unexpected).push(e)),
  };
  if (options.props !== undefined) mountOptions.props = options.props;
  if (options.dev !== undefined) mountOptions.dev = options.dev;
  const instance = await mount(Root, root, mountOptions as MountOptions<PropsOf<C>>);
  return { root, instance };
}

/** A component class with one inline template (`name` defaults to the class name). */
export function comp(
  name: string,
  render: (self: Self) => Node | null,
  extra: { setup?: (self: Self) => void; components?: Record<string, ComponentClass> } = {},
): { C: ComponentClass; t: TemplateModule } {
  const C = class extends Component<Record<string, unknown>> {
    static override template = name;
    static override components = extra.components ?? {};
    override setup(): void {
      extra.setup?.(this as unknown as Self);
    }
  };
  Object.defineProperty(C, "name", { value: name });
  return { C, t: tpl(name, render) };
}

/** The element's HTML with the runtime's empty anchor text nodes invisible (they serialise as ""). */
export function html(el: Element): string {
  return el.innerHTML;
}

/** Settles pending promises and runs the effect queue. */
export async function settle(): Promise<void> {
  for (let i = 0; i < 5; i++) await Promise.resolve();
  flushSync();
}
