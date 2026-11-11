/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
// @vitest-environment jsdom
// Keyed list reconciliation property tests (permutations, inversions, additions, deletions, node identity).

import { describe, expect, test } from "vitest";
import reconcileArrays from "../vendor/dom-expressions/src/reconcile.js";

describe("Keyed reconciliation property tests", () => {
  interface Item {
    id: number;
    text: string;
  }

  function createNode(item: Item): ChildNode {
    const el = document.createElement("li");
    el.id = `item-${item.id}`;
    el.textContent = item.text;
    return el;
  }

  test("preserves DOM node identity across arbitrary permutations", () => {
    const parent = document.createElement("ul");
    const items: Item[] = [
      { id: 1, text: "A" },
      { id: 2, text: "B" },
      { id: 3, text: "C" },
      { id: 4, text: "D" },
      { id: 5, text: "E" },
    ];

    // Initial render
    let currentNodes: ChildNode[] = items.map(createNode);
    currentNodes.forEach((n) => parent.appendChild(n));
    const nodeMap = new Map<number, ChildNode>();
    items.forEach((it, idx) => nodeMap.set(it.id, currentNodes[idx]!));

    // Permutation 1: Reversed
    const reversed = [...items].reverse();
    const nextNodes1 = reversed.map((it) => nodeMap.get(it.id)!);
    reconcileArrays(parent, currentNodes, nextNodes1);
    currentNodes = nextNodes1;

    expect(parent.children.length).toBe(5);
    for (let i = 0; i < 5; i++) {
      const it = reversed[i]!;
      expect(parent.children[i]!.id).toBe(`item-${it.id}`);
      // Node identity preserved!
      expect(parent.children[i]).toBe(nodeMap.get(it.id));
    }

    // Permutation 2: Insertion at head, deletion at tail, middle swap
    const nextItems2: Item[] = [
      { id: 99, text: "New Head" },
      { id: 4, text: "D" },
      { id: 2, text: "B" },
    ];
    const node99 = createNode(nextItems2[0]!);
    nodeMap.set(99, node99);
    const nextNodes2: ChildNode[] = [node99, nodeMap.get(4)!, nodeMap.get(2)!];

    reconcileArrays(parent, currentNodes, nextNodes2);
    currentNodes = nextNodes2;

    expect(parent.children.length).toBe(3);
    expect(parent.children[0]!.id).toBe("item-99");
    expect(parent.children[1]!.id).toBe("item-4");
    expect(parent.children[1]).toBe(nodeMap.get(4));
    expect(parent.children[2]!.id).toBe("item-2");
    expect(parent.children[2]).toBe(nodeMap.get(2));
  });
});
