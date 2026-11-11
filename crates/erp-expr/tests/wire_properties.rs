//! Bounded malformed-wire generators. Fixed seed and shrinking preserve reproducible failures.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
use erp_expr::wire::{ValidatedAst, WireErrorKind};
use proptest::{prelude::*, test_runner::RngSeed};

proptest! {
    #![proptest_config(ProptestConfig {
        cases: 1_000,
        rng_seed: RngSeed::Fixed(0x0041_5354_2026_1023),
        .. ProptestConfig::default()
    })]

    #[test]
    fn generated_self_and_forward_edges_are_rejected(nodes in 1_u32..100, offset in 0_u32..10_000) {
        let mut arena = vec![serde_json::json!({"kind":"name","name":"root"})];
        for index in 1..nodes {
            arena.push(serde_json::json!({"kind":"starred","value": index - 1}));
        }
        let root = nodes - 1;
        let invalid = root + offset;
        arena[root as usize] = serde_json::json!({"kind":"starred","value":invalid});
        let bytes = serde_json::to_vec(&serde_json::json!({"schema":1,"nodes":arena})).unwrap();
        let error = ValidatedAst::from_json(&bytes).unwrap_err();
        prop_assert_eq!(error.kind(), WireErrorKind::InvalidAst);
        prop_assert_eq!(error.to_string(), format!("InvalidAst at $.nodes[{root}]: child index {invalid} must point strictly backwards"));
    }

    #[test]
    fn generated_noncanonical_integer_spellings_are_rejected(digits in "[0-9]{1,32}", negative in any::<bool>()) {
        let value = format!("{}0{digits}", if negative { "-" } else { "" });
        let bytes = serde_json::to_vec(&serde_json::json!({"schema":1,"nodes":[{"kind":"literal","value":{"kind":"int","value":value}}]})).unwrap();
        let error = ValidatedAst::from_json(&bytes).unwrap_err();
        prop_assert_eq!(error.kind(), WireErrorKind::InvalidAst);
        prop_assert_eq!(error.to_string(), "InvalidAst at $.nodes[0].value.value: expected canonical i64 decimal string");
    }

    #[test]
    fn arbitrary_float_bits_survive_transport(bits in any::<u64>()) {
        let expression = erp_expr::Expr::Literal(erp_expr::Literal::Float(f64::from_bits(bits)));
        let bytes = erp_expr::wire::encode(&expression).unwrap();
        let decoded = ValidatedAst::from_json(&bytes).unwrap();
        match decoded.as_expr() {
            erp_expr::Expr::Literal(erp_expr::Literal::Float(value)) => prop_assert_eq!(value.to_bits(), bits),
            other => prop_assert!(false, "float decoded as {:?}", other),
        }
    }

    #[test]
    fn generated_duplicate_edges_are_rejected(names in prop::collection::vec("[a-z]{1,8}", 1..50)) {
        let mut arena: Vec<_> = names.iter().map(|name| serde_json::json!({"kind":"name","name":name})).collect();
        let root = arena.len();
        let mut items: Vec<_> = (0..root).collect();
        items.push(root - 1);
        arena.push(serde_json::json!({"kind":"list","items":items}));
        let bytes = serde_json::to_vec(&serde_json::json!({"schema":1,"nodes":arena})).unwrap();
        let error = ValidatedAst::from_json(&bytes).unwrap_err();
        prop_assert_eq!(error.kind(), WireErrorKind::InvalidAst);
        prop_assert_eq!(error.to_string(), format!("InvalidAst at $.nodes[{root}]: child index {} has more than one incoming edge", root - 1));
    }
}
