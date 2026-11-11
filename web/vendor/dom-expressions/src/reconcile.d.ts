/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
// ERPForAll's own declaration for the vendored reconcile.js (listed under `local` in UPSTREAM.toml, not
// an upstream file). reconcileArrays moves the DOM nodes of `a` (currently attached under `parentNode`,
// in that order, `a` non-empty) so that they read as `b`; nodes of `a` absent from `b` are removed, nodes
// of `b` absent from `a` are inserted. It may write into `a`.
export default function reconcileArrays(parentNode: Node, a: ChildNode[], b: ChildNode[]): void;
