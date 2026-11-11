//! The synthetic set and the measuring code shared by the allocation pins of `analyze`
//! (`tests/alloc_analyze.rs`) and `compile` (`tests/alloc_compile.rs`), one binary each (rust
//! review M8: one test per process, `erp_alloc_budget::harness`).

use erp_alloc_budget as alloc;
use erp_ui_compiler::{AddonName, TemplatePath, TemplateSet, TemplateSource};

/// One synthetic template using the codegen subset: attributes, bindings, a branch chain, a keyed
/// loop with an event handler, outputs and a component with a named slot.
fn template(i: usize) -> String {
    format!(
        "<t t-name=\"bench.T{i}\">\
           <div class=\"o_row o_{i}\" t-att-title=\"this.props.title\" t-att-class=\"{{ o_active: this.state.active }}\">\
             <span t-if=\"this.props.count === 0\">none</span>\
             <span t-elif=\"this.props.count gt 1 and this.state.open\">many <t t-out=\"this.props.count\"/></span>\
             <span t-else=\"\">one</span>\
             <ul><li t-foreach=\"this.props.rows\" t-as=\"row\" t-key=\"row.id\" t-on-click=\"() => this.select(row.id)\">\
               <t t-out=\"row.label\"/> <small t-out=\"row_index + 1\"/>\
             </li></ul>\
             <Card title.translate=\"Details\" record=\"this.props.record\">\
               <t t-set-slot=\"footer\"><b t-out=\"this.props.footer\"/></t>\
               <p>body {i}</p>\
             </Card>\
           </div>\
         </t>"
    )
}

pub fn set(n: usize) -> TemplateSet {
    let mut xml = String::from("<templates>");
    for i in 0..n {
        xml.push_str(&template(i));
    }
    xml.push_str("</templates>");
    let mut set = TemplateSet::new();
    set.push(TemplateSource::addon_file(
        AddonName::new("bench").unwrap(),
        TemplatePath::new("static/src/bench.xml").unwrap(),
        xml,
    ));
    set
}

/// What one operation costs with `N` and with `2N` templates: `(blocks, bytes, peak bytes)`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Cost {
    pub n: (u64, u64, u64),
    pub n2: (u64, u64, u64),
}

pub const N: usize = 50;

pub fn cost(op: impl Fn(&TemplateSet)) -> Cost {
    let small = set(N);
    let large = set(2 * N);
    let (a, ()) = alloc::measure(|| op(&small)).unwrap();
    let (b, ()) = alloc::measure(|| op(&large)).unwrap();
    Cost {
        n: (a.total_blocks, a.total_bytes, a.peak_bytes),
        n2: (b.total_blocks, b.total_bytes, b.peak_bytes),
    }
}

/// Relative margin a measured figure may move before the pin must be updated.
const MARGIN: f64 = 0.03;

pub fn check(what: &str, measured: Cost, pin: Cost) {
    let per = |x: u64, y: u64| y.saturating_sub(x) / N as u64;
    let report = format!(
        "{what}: measured {measured:?}; per added template {} blocks, {} bytes, {} peak bytes",
        per(measured.n.0, measured.n2.0),
        per(measured.n.1, measured.n2.1),
        per(measured.n.2, measured.n2.2),
    );
    let near = |m: u64, p: u64| (m as f64 - p as f64).abs() <= (p as f64 * MARGIN).max(2.0);
    let all =
        |m: (u64, u64, u64), p: (u64, u64, u64)| near(m.0, p.0) && near(m.1, p.1) && near(m.2, p.2);
    assert!(
        all(measured.n, pin.n) && all(measured.n2, pin.n2),
        "{report}; pinned {pin:?} (margin {MARGIN})"
    );
}
