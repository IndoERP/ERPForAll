/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
// Placing built blocks in the DOM. A dynamic position is a parent
// and an end anchor: the compiler's `<!>` marker, or `null` for the last child of an element (append).
// Helpers always insert relative to the anchor's current parent, so a root position of a multi-root
// block keeps working after its fragment has been emptied into the document.

import { UIError } from "./errors.js";
import { MountScope, type Owner, dispose, handleError, mountTree, setOwner } from "./owner.js";
import { untrack } from "./reactive.js";

const FRAGMENT = 11;
const COMMENT = 8;

/**
 * The end anchor of a position: the marker, or for a `null` marker under a `DocumentFragment` (which
 * the compiler never emits, but which would otherwise append to an emptied fragment later) an empty
 * text node appended now; `null` (append to the element) otherwise.
 */
export function endAnchor(parent: Node, marker: Node | null): Node | null {
  if (marker !== null) return marker;
  if (parent.nodeType === FRAGMENT) return parent.appendChild(document.createTextNode(""));
  return null;
}

/** Inserts `node` before `end` (or appends to `parent` when `end` is null). */
export function insertBefore(parent: Node, end: Node | null, node: Node): void {
  containerOf(parent, end).insertBefore(node, end);
}

/**
 * The container of a position now: the anchor's parent, or `parent` for an element's last position. An
 * anchor that code outside the runtime removed from the DOM is an error naming that, not a null access.
 */
export function containerOf(parent: Node, end: Node | null): Node {
  if (end === null) return parent;
  const container = end.parentNode;
  if (container === null) {
    throw new UIError("the anchor of a dynamic position was removed from the DOM by code outside the runtime", "detached");
  }
  return container;
}

/**
 * The first and last node a built block will occupy once inserted, or null for an empty block. A
 * fragment whose first child is a marker gets an empty text node in front, so content its first root
 * position inserts later stays inside the range (§7.2).
 */
export function blockRange(node: Node | null): [Node, Node] | null {
  if (node === null) return null;
  if (node.nodeType !== FRAGMENT) return [node, node];
  let first = node.firstChild;
  if (first === null) return null;
  if (first.nodeType === COMMENT) first = node.insertBefore(document.createTextNode(""), first);
  return [first, node.lastChild!];
}

/**
 * Removes the nodes from `first` to `last` (siblings, in order). A range that code outside the runtime
 * broke (browser translation, an extension or a widget moved or removed its nodes) is an error naming
 * that, never a partial removal that leaves old content on screen (review round 2, silent-failure 6): the
 * range is checked before anything is removed.
 */
export function removeRange(first: Node, last: Node): void {
  const parent = first.parentNode;
  if (parent === null) throw detached();
  if (first !== last) {
    let n = first.nextSibling;
    while (n !== last) {
      if (n === null) throw detached();
      n = n.nextSibling;
    }
  }
  let n: Node = first;
  for (;;) {
    const next = n.nextSibling;
    parent.removeChild(n);
    if (n === last) return;
    n = next!;
  }
}

/** The error of a block whose nodes code outside the runtime moved or removed. */
export function detached(): UIError {
  return new UIError("the nodes of a block were moved or removed from the DOM by code outside the runtime", "detached");
}

/**
 * Starts building content under `parent` (an owner whose child owners the build creates). Once the
 * parent's mount scope has mounted, components created by the build must register in a fresh scope,
 * which `endBuild` mounts when the content is in the DOM (§7.6): this returns it, and the caller gives
 * it to the child owners it creates. While the scope has not mounted yet, they simply join it (null).
 */
export function beginBuild(parent: Owner): MountScope | null {
  return parent.scope.mounted ? new MountScope() : null;
}

/** Runs `onMounted` for the components a `beginBuild` scope collected (post-order). */
export function endBuild(scope: MountScope | null): void {
  if (scope !== null) mountTree(scope);
}

/**
 * One replaceable block at a position (the branch of `cond`, the block of `keyed`, the fallback of
 * `out`, a slot's content): its own owner and its node range.
 */
export class Region {
  private owner: Owner | null = null;
  private first: Node | null = null;
  private last: Node | null = null;

  constructor(
    private readonly parent: Node,
    private readonly end: Node | null,
  ) {}

  /** Builds `fn` in a new child owner of `parentOwner` (untracked), inserts it and mounts it. */
  build(fn: () => Node | null, parentOwner: Owner): void {
    endBuild(this.buildHeld(fn, parentOwner));
  }

  /**
   * As `build`, but returns the mount scope of the components the build created instead of mounting
   * them (null when they joined a scope that has not mounted yet): the caller runs `endBuild` once it
   * knows the content stays (an error boundary does not mount content that failed while it was built).
   */
  buildHeld(fn: () => Node | null, parentOwner: Owner): MountScope | null {
    const owner = parentOwner.child();
    this.owner = owner;
    const scope = beginBuild(parentOwner);
    if (scope !== null) owner.scope = scope;
    const prev = setOwner(owner);
    let node: Node | null = null;
    try {
      node = untrack(fn);
    } catch (e) {
      handleError(owner, e);
    } finally {
      setOwner(prev);
    }
    if (owner.disposed) return null;
    const range = blockRange(node);
    if (range !== null) {
      this.first = range[0];
      this.last = range[1];
      insertBefore(this.parent, this.end, node!);
    }
    return scope;
  }

  /** Disposes the block's owner, then removes its nodes. */
  clear(): void {
    this.dispose();
    const first = this.first;
    const last = this.last;
    this.first = this.last = null;
    if (first !== null) removeRange(first, last!);
  }

  /** Disposes the block's owner only (an enclosing structure removes the DOM). */
  dispose(): void {
    const owner = this.owner;
    this.owner = null;
    if (owner !== null) dispose(owner);
  }
}
