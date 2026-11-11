/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
import * as $ from "@erpforall/elang/abi";
const $t0 = $.template("\u003c!\u003e\u003ci\u003emid\u003c\/i\u003e\u003c!\u003e", 1);
export const name = "contract.Multi";
export function render($self) {
  const $n0 = $t0();
  const $n1 = $n0.firstChild;
  $.insert($n0, () => $self.a(), $n1);
  const $n2 = $n1.nextSibling.nextSibling;
  $.insert($n0, () => $self.b(), $n2);
  return $n0;
}
