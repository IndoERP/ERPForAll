/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
import * as $ from "@erpforall/elang/abi";
const $t0 = $.template("\u003col\u003e\u003c\/ol\u003e", 0);
const $t1 = $.template("\u003cli\u003e\u003c\/li\u003e", 0);
export const name = "contract.Rows";
export function render($self) {
  const $n0 = $t0();
  $.list($n0, null, () => $self.props.items, ($ki0, $kv0, $kx0, $kc0) => $ki0.id, ($r0) => {
    const $n1 = $t1();
    $.slot($n1, null, $self, "row", () => ({"label": $r0.item().label}), null);
    return $n1;
  });
  return $n0;
}
