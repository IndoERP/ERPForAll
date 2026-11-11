//! Probe for the WASM typed-failure test: reads HTML on stdin and parses it under a
//! node budget of exactly the empty document's element nodes, so a text node is the first refusal. A refusal prints the typed error's message on stdout and exits with status 3;
//! a successful parse prints `ok` and exits 0. Built for `wasm32-wasip1` and run by
//! `tests/wasm_identity.rs`.

use erp_ui_compiler::html_budget::{ParseBudget, ParseLimits, parse_document_bounded};
use markup5ever_rcdom::RcDom;
use std::io::Read;

/// Node limit the probe parses under: the three element nodes of the empty document (html, head,
/// body; `empty_document_has_three_element_nodes` in `erp-ui-compiler` pins the count). A
/// document that adds only text is refused by the sink's text-node admission.
const NODES: u64 = 3;

fn main() {
    let mut html = String::new();
    if let Err(e) = std::io::stdin().read_to_string(&mut html) {
        eprintln!("cannot read stdin: {e}");
        std::process::exit(2);
    }
    let limits = match ParseLimits::new(1 << 20, 1 << 20, 256, NODES) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("{e}");
            std::process::exit(2);
        }
    };
    let budget = ParseBudget::new(limits);
    match parse_document_bounded(RcDom::default(), &[&html], 1 << 20, &budget) {
        Ok(_) => println!("ok"),
        Err(e) => {
            println!("{e}");
            std::process::exit(3);
        }
    }
}
