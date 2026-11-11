//! Manual stack-use probe (ignored): `PROBE_KIND=<n> PROBE_STACK_KB=<kb> cargo test --test stack_probe -- --ignored`.

// Test helpers outside `#[test]` functions fail loudly too (clippy.toml only covers the tests).
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use erp_expr::{EvalContext, ExprValue, eval};

fn input(kind: &str) -> String {
    let n = 99;
    match kind {
        "paren" => format!("{}1{}", "(".repeat(n), ")".repeat(n)),
        "paren_rej" => format!("{}1{}", "(".repeat(5000), ")".repeat(5000)),
        "neg" => format!("{}1", "-".repeat(n - 1)),
        "sum" => vec!["1"; n].join("+"),
        "pow" => vec!["1"; n].join("**"),
        "list" => format!("{}1{}", "[".repeat(n - 1), "]".repeat(n - 1)),
        "fstr" => format!("{}1{}", "f'{".repeat(45), "}'".repeat(45)),
        "fstr_rej" => format!("{}1{}", "f'{".repeat(5000), "}'".repeat(5000)),
        "walrus" => "(a:=[], [a:=[a] for _ in [0]*97], str(a), a == a, a)".to_string(),
        "index" => format!("x{}", "[0]".repeat(97)),
        _ => String::from("1"),
    }
}

#[test]
#[ignore]
fn stack_probe() {
    let kind = std::env::var("PROBE_KIND").unwrap_or_default();
    let kb: usize = std::env::var("PROBE_STACK_KB")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(2048);
    let src = input(&kind);
    let h = std::thread::Builder::new()
        .stack_size(kb * 1024)
        .spawn(move || {
            let mut x = ExprValue::Int(0);
            for _ in 0..98 {
                x = ExprValue::list(vec![x]);
            }
            let ctx = EvalContext::standard().with_var("x", x);
            let r = eval(&src, &ctx);
            println!(
                "RESULT {}",
                r.map(|v| v.repr().len())
                    .map_err(|e| e.to_string())
                    .unwrap_or(0)
            );
        })
        .unwrap();
    h.join().unwrap();
}
