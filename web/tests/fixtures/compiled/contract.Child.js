/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
import * as $ from "@erpforall/elang/abi";
const $t0 = $.template("\u003csection class=\"child\"\u003e\u003c!\u003e\u003cbutton class=\"ping\"\u003eping\u003c\/button\u003e\u003c\/section\u003e", 0);
export const name = "contract.Child";
export function render($self) {
  const $n0 = $t0();
  const $n1 = $n0.firstChild;
  $.insert($n0, () => $self.props.label, $n1);
  const $n2 = $n1.nextSibling;
  $.on($n2, "click", (() => $self.props.onPing("pong")), 0);
  return $n0;
}
