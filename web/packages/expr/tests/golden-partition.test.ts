/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
import { readFileSync } from "node:fs";
import { resolve } from "node:path";
import { describe, expect, it } from "vitest";
import { parseBoundedJson } from "../src/index";

interface GoldenRow {
  readonly ordinal: number;
  readonly source: string;
  readonly evaluates: boolean;
  readonly expected: string;
  readonly ast: unknown;
}

interface ManifestEntry {
  readonly ordinal: number;
  readonly source: string;
  readonly owner: "CORE" | "FORMAT" | "DATES";
}

describe("Golden subset manifest and partition tests", () => {
  const goldensPath = resolve(process.cwd(), "packages/expr/tests/generated-goldens.json");
  const manifestPath = resolve(process.cwd(), "packages/expr/tests/golden-manifest.json");

  const goldensRaw = parseBoundedJson(readFileSync(goldensPath, "utf8")) as unknown as readonly GoldenRow[];
  const manifestRaw = JSON.parse(readFileSync(manifestPath, "utf8")) as unknown as readonly ManifestEntry[];

  it("partitions all 482 golden rows exhaustively without hiding gaps", () => {
    expect(goldensRaw).toHaveLength(482);
    expect(manifestRaw).toHaveLength(482);

    let coreCount = 0;
    let formatCount = 0;
    let datesCount = 0;

    for (let i = 0; i < 482; i++) {
      const g = goldensRaw[i]!;
      const m = manifestRaw[i]!;
      expect(g.ordinal).toBe(i);
      expect(m.ordinal).toBe(i);
      expect(m.source).toBe(g.source);

      if (m.owner === "CORE") coreCount++;
      else if (m.owner === "FORMAT") formatCount++;
      else if (m.owner === "DATES") datesCount++;
      else throw new Error(`Unknown owner ${m.owner} at row ${i}`);
    }

    expect(coreCount).toBe(272);
    expect(formatCount).toBe(99);
    expect(datesCount).toBe(111);
    expect(coreCount + formatCount + datesCount).toBe(482);
  });
});
