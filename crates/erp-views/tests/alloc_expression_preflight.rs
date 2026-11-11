//! Allocation proof of the expression reservation (WP1, plan §2.4 T3/T3b).
//! A safety/admission check (keeps those mandatory), not a performance pin: every
//! assertion is an inequality in the safe direction or an exact retained-size equality.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::print_stdout,
    reason = "tests"
)]
use erp_alloc_budget::{self as alloc};
use erp_expr::{Expr, Literal, parse, wire::ValidatedAst};
use erp_views::{
    DescriptionBudget, DescriptionBuilder, ExpressionKind, ReferenceScope, ReferenceVisitor,
    SchemaNode, ViewSchema, expression_decode_preflight, expression_preflight,
};

#[global_allocator]
static ALLOC: alloc::Alloc = alloc::Alloc;

alloc::harness!(
    expression_reservation_covers_measured_peak_live_bytes,
    refused_expression_allocates_nothing,
    expression_byte_refusal_allocates_nothing,
    raw_decode_reservation_covers_measured_peak_live_bytes,
    refused_raw_decode_allocates_nothing,
    builtin_wire_admission_never_allocates_beyond_remaining_budget,
);

static DOMAIN: SchemaNode = SchemaNode::Expression {
    kind: ExpressionKind::Domain,
    scope: ReferenceScope::Root,
    parent_model: None,
};

fn corpus() -> Vec<(&'static str, Expr)> {
    let list = format!(
        "[{}]",
        (0..4096)
            .map(|i| i.to_string())
            .collect::<Vec<_>>()
            .join(", ")
    );
    let nested = format!("{}1", "-".repeat(90));
    let keywords = format!(
        "f({})",
        (0..64)
            .map(|i| format!("k{i}={i}"))
            .collect::<Vec<_>>()
            .join(", ")
    );
    [
        ("single name", "name".to_owned()),
        ("4096-item list", list),
        ("90-deep unary chain", nested),
        ("call with 64 keywords", keywords),
        (
            "comprehension with 3 generators",
            "[a + b for a in x for b in y if b for c in z if c if a]".to_owned(),
        ),
        (
            "dict with nested list",
            "{'a': 1, 'b': [x, y], 'c': (z,)}".to_owned(),
        ),
        (
            "floats and wide ints",
            "[1.5, -9223372036854775807, 0.1, 'q']".to_owned(),
        ),
    ]
    .into_iter()
    .map(|(name, source)| {
        (
            name,
            parse(&source).unwrap_or_else(|e| panic!("{name}: {e}")),
        )
    })
    .chain([(
        // The parser admits ASCII identifiers only; the tree is built directly.
        "non-ASCII names",
        Expr::List(vec![
            Expr::Name("\u{e9}t\u{e9}".to_owned()),
            Expr::Name("\u{540d}\u{524d}".to_owned()),
            Expr::Literal(Literal::Str("na\u{ef}ve".to_owned())),
        ]),
    )])
    .chain([(
        "4 KiB bytes literal",
        Expr::Literal(Literal::Bytes(vec![7_u8; 4096])),
    )])
    .chain([(
        "64 KiB string literal",
        Expr::Literal(Literal::Str("x".repeat(64 * 1024))),
    )])
    .collect()
}

fn expression_reservation_covers_measured_peak_live_bytes() {
    // WP2: the builder admits an expression with the same walker the visitors use, so the corpus
    // is built as value expressions, and a tree that walker does not support (the comprehension)
    // is refused at construction, before any allocation, instead of being measured here.
    let schema = ViewSchema::new(1, &VALUE).unwrap();
    for (name, expr) in corpus() {
        let ast = ValidatedAst::from_expr(&expr).unwrap();
        let preflight = expression_preflight(ast.as_expr()).unwrap();
        let budget = DescriptionBudget::new();
        let builder = DescriptionBuilder::new(&schema, &budget);
        // Warm any lazily initialised statics outside the window.
        let Ok(warm) = builder.build(|root| root.expression(&ast)) else {
            println!("{name}: refused by the reference walker (not measured here)");
            continue;
        };
        drop(warm);
        let (stats, payload) =
            alloc::measure(|| builder.build(|root| root.expression(&ast))).expect("measure");
        let payload = payload.unwrap();
        println!(
            "{name}: reserve {} peak {} end {} total_blocks {}",
            preflight.reserve_bytes(),
            stats.peak_bytes,
            stats.end_bytes,
            stats.total_blocks
        );
        assert!(
            stats.peak_bytes <= u64::try_from(preflight.reserve_bytes()).unwrap(),
            "{name}: measured peak {} exceeds the reservation {}",
            stats.peak_bytes,
            preflight.reserve_bytes()
        );
        assert_eq!(
            stats.end_bytes,
            u64::try_from(payload.retained_bytes()).unwrap(),
            "{name}: live bytes after the build differ from the accounted retained bytes"
        );
        drop(payload);
    }
}

fn refused_expression_allocates_nothing() {
    let schema = ViewSchema::new(1, &DOMAIN).unwrap();
    let ast =
        ValidatedAst::from_expr(&parse("[('name', '=', 'x'), ('qty', 'in', [1, 2, 3])]").unwrap())
            .unwrap();
    let reserve = expression_preflight(ast.as_expr()).unwrap().reserve_bytes();
    let budget = DescriptionBudget::with_limit(reserve - 1).unwrap();
    let builder = DescriptionBuilder::new(&schema, &budget);
    let (stats, result) =
        alloc::measure(|| builder.build(|root| root.expression(&ast))).expect("measure");
    result.unwrap_err();
    assert_eq!(
        stats.total_blocks, 0,
        "a refused expression allocated: {stats:?}"
    );
    assert_eq!(
        stats.total_bytes, 0,
        "a refused expression allocated: {stats:?}"
    );
}

fn expression_byte_refusal_allocates_nothing() {
    let schema = ViewSchema::new(1, &DOMAIN).unwrap();
    let ast = ValidatedAst::from_expr(
        &parse("[(\"name\", \"=\", \"x\"), (\"qty\", \"in\", [1, 2, 3])]").unwrap(),
    )
    .unwrap();
    let encoded = ast.to_json().unwrap().len();
    let budget = DescriptionBudget::with_limits(32 * 1024 * 1024, 1_000_000, encoded - 1).unwrap();
    let builder = DescriptionBuilder::new(&schema, &budget);
    let (stats, result) =
        alloc::measure(|| builder.build(|root| root.expression(&ast))).expect("measure");
    let error = result.unwrap_err();
    assert_eq!(
        error,
        erp_views::ViewError::LimitExceeded {
            limit: erp_views::LimitKind::DescriptionExpressionBytes,
            max: encoded - 1,
            actual: Some(encoded),
        }
    );
    assert_eq!(
        error.to_string(),
        format!(
            "aggregate expression bytes per request limit exceeded: maximum {}, reached {encoded}",
            encoded - 1
        )
    );
    assert_eq!(stats.total_blocks, 0, "byte refusal allocated: {stats:?}");
    assert_eq!(stats.total_bytes, 0, "byte refusal allocated: {stats:?}");
    assert!(matches!(
        budget.check(),
        Err(erp_views::ViewError::LimitExceeded {
            limit: erp_views::LimitKind::DescriptionExpressionBytes,
            max,
            actual: None,
        }) if max == encoded - 1
    ));
}

static VALUE: SchemaNode = SchemaNode::Expression {
    kind: ExpressionKind::Value,
    scope: ReferenceScope::Root,
    parent_model: None,
};

/// WP2: the untrusted visitor decodes the embedded wire document itself. Its reservation, derived
/// from the JSON value before anything is allocated, must cover the measured peak (the reference
/// index it returns is live at the end, so it is allowed on top).
fn raw_decode_reservation_covers_measured_peak_live_bytes() {
    let visitor = ReferenceVisitor::new(ViewSchema::new(1, &VALUE).unwrap());
    for (name, expr) in corpus() {
        let ast = ValidatedAst::from_expr(&expr).unwrap();
        let value: serde_json::Value = serde_json::from_slice(&ast.to_json().unwrap()).unwrap();
        let preflight = expression_decode_preflight(&value).unwrap();
        let budget = DescriptionBudget::new();
        // Warm lazily initialised statics outside the window.
        drop(visitor.references(&value, &budget));
        let budget = DescriptionBudget::new();
        let (stats, result) =
            alloc::measure(|| visitor.references(&value, &budget)).expect("measure");
        println!(
            "{name}: decode reserve {} peak {} end {} total_blocks {} ok {}",
            preflight.reserve_bytes(),
            stats.peak_bytes,
            stats.end_bytes,
            stats.total_blocks,
            result.is_ok()
        );
        assert!(
            stats.peak_bytes <= u64::try_from(preflight.reserve_bytes()).unwrap() + stats.end_bytes,
            "{name}: measured peak {} exceeds the decode reservation {} plus the live index {}",
            stats.peak_bytes,
            preflight.reserve_bytes(),
            stats.end_bytes
        );
        drop(result);
    }
}

fn refused_raw_decode_allocates_nothing() {
    let visitor = ReferenceVisitor::new(ViewSchema::new(1, &VALUE).unwrap());
    let ast = ValidatedAst::from_expr(&parse("a + b").unwrap()).unwrap();
    let value: serde_json::Value = serde_json::from_slice(&ast.to_json().unwrap()).unwrap();
    let encoded = ast.to_json().unwrap().len();
    let budget = DescriptionBudget::with_limits(32 * 1024 * 1024, 1_000_000, encoded - 1).unwrap();
    let (stats, result) = alloc::measure(|| visitor.references(&value, &budget)).expect("measure");
    result.unwrap_err();
    assert_eq!(
        stats.total_blocks, 0,
        "a refused raw expression allocated: {stats:?}"
    );
    assert_eq!(
        stats.total_bytes, 0,
        "a refused raw expression allocated: {stats:?}"
    );
}

fn builtin_wire_admission_never_allocates_beyond_remaining_budget() {
    let schema = erp_views::form_schema().unwrap();
    let mut successes = 0;
    let mut refusals = 0;
    for available in 0..=1024 {
        let limit = 16 * 1024;
        let budget = DescriptionBudget::with_limit(limit).unwrap();
        let payload = DescriptionBuilder::new(&schema, &budget)
            .build(|root| {
                root.object(|o| {
                    o.set("root", |v| v.integer(0))?;
                    o.set("nodes", |v| {
                        v.array(1, |_, v| {
                            v.variant("form", |v| {
                                v.object(|o| {
                                    o.set("record_mode", |v| v.text("record"))?;
                                    o.set("children", |v| v.array(0, |_, v| v.integer(0)))
                                })
                            })
                        })
                    })
                })
            })
            .unwrap();
        let _held = budget
            .reserve(limit - payload.retained_bytes() - available)
            .unwrap();
        let (stats, result) = alloc::measure(|| {
            payload.builtin_view(
                erp_views::wire::ViewIdentity::stored(
                    erp_views::wire::WireId::try_from(7).unwrap(),
                ),
                erp_views::wire::ViewType::Form,
                "web.form",
                &budget,
            )
        })
        .unwrap();
        // measure() establishes the global allocator with an 8-byte probe before the
        // operation. Its max_bytes includes that earlier probe even when the operation makes
        // no allocations, so use the operation's allocation delta for that exact boundary.
        if stats.total_blocks == 0 {
            assert_eq!(stats.total_bytes, 0);
        } else {
            assert!(
                stats.peak_bytes <= available as u64,
                "available {available}: unadmitted adapter allocation {stats:?}"
            );
        }
        match result {
            Ok(view) => {
                successes += 1;
                drop(view);
            }
            Err(error) => {
                refusals += 1;
                match error {
                    erp_views::ViewError::LimitExceeded {
                        limit: erp_views::LimitKind::DescriptionBuildBytes,
                        max,
                        actual: Some(actual),
                    } => {
                        assert_eq!(max, limit);
                        assert!(actual > max);
                    }
                    other => panic!("unexpected refusal: {other}"),
                }
                let sticky = budget.check().unwrap_err();
                assert_eq!(
                    sticky,
                    erp_views::ViewError::LimitExceeded {
                        limit: erp_views::LimitKind::DescriptionBuildBytes,
                        max: limit,
                        actual: None,
                    }
                );
                assert_eq!(
                    sticky.to_string(),
                    format!(
                        "simultaneously retained description bytes limit exceeded: maximum {limit}"
                    )
                );
            }
        }
    }
    assert!(successes > 0);
    assert!(refusals > 0);
}
