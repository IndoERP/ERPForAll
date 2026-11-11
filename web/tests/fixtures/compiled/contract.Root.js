/*! SPDX-License-Identifier: MIT OR Apache-2.0 */
import * as $ from "@erpforall/elang/abi";
const $t0 = $.template("\u003cdiv class=\"o_root\"\u003e\u003ch1\u003e\u003c\/h1\u003e\u003c!\u003e\u003cbutton class=\"inc\"\u003e+\u003c\/button\u003e\u003cbutton class=\"named\"\u003enamed\u003c\/button\u003e\u003cinput class=\"field\"\u003e\u003cspan class=\"count\"\u003e\u003c\/span\u003e\u003cul\u003e\u003c\/ul\u003e\u003c!\u003e\u003c!\u003e\u003c!\u003e\u003cspan class=\"raw\"\u003e\u003c\/span\u003e\u003c\/div\u003e", 0);
const $t1 = $.template("\u003cp class=\"mode\"\u003emode a\u003c\/p\u003e", 0);
const $t2 = $.template("\u003cp class=\"mode\"\u003emode b\u003c\/p\u003e", 0);
const $t3 = $.template("\u003cp class=\"mode\"\u003eother\u003c\/p\u003e", 0);
const $t4 = $.template("\u003cli\u003e\u003c!\u003e:\u003c\/li\u003e", 0);
const $t5 = $.template("\u003cb\u003eHeader \u003c\/b\u003e", 0);
const $t6 = $.template("\u003cem\u003ebody\u003c\/em\u003e", 0);
const $t7 = $.template("\u003ci\u003e\u003c\/i\u003e", 0);
const $t8 = $.template("empty", 1);
export const name = "contract.Root";
export function render($self) {
  const $n0 = $t0();
  $.effect(() => $.attr($n0, "title", $self.title()));
  $.effect(() => $.cls($n0, ({"o_active": $self.active()})));
  const $n1 = $n0.firstChild;
  $.insert($n1, () => $self.title(), null);
  const $n2 = $n1.nextSibling;
  $.cond($n0, $n2, () => { if (($self.mode() === "a")) return 0; if (($self.mode() === "b")) return 1; return 2; }, [() => {
    const $n3 = $t1();
    return $n3;
  }, () => {
    const $n4 = $t2();
    $.effect(() => $.attr($n4, "data-mode", $.interp("m-", $self.mode())));
    return $n4;
  }, () => {
    const $n5 = $t3();
    return $n5;
  }]);
  const $n6 = $n2.nextSibling;
  $.on($n6, "click", (() => $self.inc()), 0);
  const $n7 = $n6.nextSibling;
  $.on($n7, "click", ($e) => $.callHandler($self.onNamed, $self, $e), 1);
  const $n8 = $n7.nextSibling;
  $.effect(() => $.prop($n8, "value", $self.title()));
  const $n9 = $n8.nextSibling;
  $.insert($n9, () => $self.count(), null);
  const $n10 = $n9.nextSibling;
  $.list($n10, null, () => $self.rows(), ($ki0, $kv0, $kx0, $kc0) => $ki0.id, ($r0) => {
    const $n11 = $t4();
    $.effect(() => $.attr($n11, "data-id", $r0.item().id));
    const $n12 = $n11.firstChild;
    $.insert($n11, () => $r0.index(), $n12);
    $.insert($n11, () => $r0.item().label, null);
    return $n11;
  });
  const $n13 = $n10.nextSibling;
  $.component($n0, $n13, $self, "Child", {"label": () => $self.title(), "onPing": () => ($self.ping).bind($self)}, null, null);
  const $n14 = $n13.nextSibling;
  $.component($n0, $n14, $self, "Card", {}, null, {"header": {render: () => {
    const $n15 = $t5();
    $.insert($n15, () => $self.title(), null);
    return $n15;
  }}, "default": {render: () => {
    const $n16 = $t6();
    return $n16;
  }}});
  const $n17 = $n14.nextSibling;
  $.component($n0, $n17, $self, "Rows", {"items": () => $self.rows()}, null, {"row": {render: ($s0) => {
    const $n18 = $t7();
    $.insert($n18, () => $s0.label, null);
    return $n18;
  }}});
  const $n19 = $n17.nextSibling;
  $.out($n19, () => $self.html(), null, () => {
    const $n20 = $t8();
    return $n20;
  });
  return $n0;
}
