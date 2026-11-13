/*! SPDX-License-Identifier: MIT OR Apache-2.0 */

/** Unlisted items retain their relative order after the ordered items. */
export function reorderApps(apps: number[], order: number[]): number[] {
  const result = [...apps];
  result.sort((a, b) => {
    const aIndex = order.indexOf(a);
    const bIndex = order.indexOf(b);
    if (aIndex === -1 && bIndex === -1) {
      return 0;
    }
    if (aIndex === -1) {
      return 1;
    }
    if (bIndex === -1) {
      return -1;
    }
    return aIndex - bIndex;
  });
  return result;
}
