/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
import * as $ from "@erpforall/elang/abi";
const $t0 = $.template("\u003carticle\u003e\u003cheader\u003e\u003c\/header\u003e\u003c\/article\u003e", 0);
export const name = "contract.Card";
export function render($self) {
  const $n0 = $t0();
  const $n1 = $n0.firstChild;
  $.slot($n1, null, $self, "header", null, null);
  $.slot($n0, null, $self, "default", null, null);
  return $n0;
}
