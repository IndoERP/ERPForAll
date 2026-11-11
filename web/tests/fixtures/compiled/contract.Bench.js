/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
import * as $ from "@erpforall/elang/abi";
const $t0 = $.template("\u003ctable\u003e\u003ctbody\u003e\u003c\/tbody\u003e\u003c\/table\u003e", 0);
const $t1 = $.template("\u003ctr\u003e\u003ctd class=\"col-md-1\"\u003e\u003c\/td\u003e\u003ctd class=\"col-md-4\"\u003e\u003ca\u003e\u003c\/a\u003e\u003c\/td\u003e\u003ctd class=\"col-md-1\"\u003e\u003ca\u003e\u003cspan class=\"remove\" aria-hidden=\"true\"\u003e\u003c\/span\u003e\u003c\/a\u003e\u003c\/td\u003e\u003ctd class=\"col-md-6\"\u003e\u003c\/td\u003e\u003c\/tr\u003e", 0);
export const name = "contract.Bench";
export function render($self) {
  const $n0 = $t0();
  const $n1 = $n0.firstChild;
  $.list($n1, null, () => $self.rows(), ($ki0, $kv0, $kx0, $kc0) => $ki0.id, ($r0) => {
    const $n2 = $t1();
    $.effect(() => $.cls($n2, ({"danger": ($r0.item().id === $self.selected())})));
    const $n3 = $n2.firstChild;
    $.insert($n3, () => $r0.item().id, null);
    const $n4 = $n3.nextSibling;
    const $n5 = $n4.firstChild;
    $.on($n5, "click", (() => $self.select($r0.item().id)), 0);
    $.insert($n5, () => $r0.item().label(), null);
    const $n6 = $n4.nextSibling;
    const $n7 = $n6.firstChild;
    $.on($n7, "click", (() => $self.remove($r0.item().id)), 0);
    return $n2;
  });
  return $n0;
}
