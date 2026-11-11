//! Framework-derived domain provenance stays distinct from external user input.
#![allow(clippy::unwrap_used, clippy::panic)]

mod common;

use std::hash::{DefaultHasher, Hash, Hasher};
use std::sync::Arc;

use common::{c, list, model, opt, opt_err};
use erp_domain::{Domain, DomainKind, ErrorKind, Operator, Value};

const COUNTRY: &str = "test_orm.country";
const PARTNER: &str = "test_orm.partner";
const CONTEXT: &str = "search_method '_search_flag' for field 'test_orm.country.flag'";
const INNER_CONTEXT: &str = "search_method '_search_nested' for field 'test_orm.country.nested'";
const INVALID_MESSAGE: &str =
    "Invalid field test_orm.country.abcdefg in condition ('abcdefg', 'in', ['foo'])";

fn invalid() -> Domain {
    c("abcdefg", "in", list(["foo"]))
}

fn provider(domain: Domain) -> Domain {
    domain.with_programming_context(Arc::from(CONTEXT))
}

fn assert_provider_error(domain: &Domain, context: &str) {
    let error = opt_err(COUNTRY, domain);
    assert_eq!(error.kind(), ErrorKind::Programming);
    assert_eq!(error.to_string(), format!("{context}: {INVALID_MESSAGE}"));
    assert!(matches!(
        error.reason(),
        erp_domain::ErrorReason::Condition { .. }
    ));
    assert!(error.condition_reason().is_some());
    assert_eq!(error.python_class(), "ValueError");
}

#[test]
fn external_original_sibling_does_not_reclassify_a_provider_failure() {
    let original = c("name", "=", "user").into_external();
    let domain = original & provider(invalid());
    assert_provider_error(&domain, CONTEXT);
}

#[test]
fn original_external_error_remains_validation_beside_provider_output() {
    let original = invalid().into_external();
    let domain = original & provider(c("name", "=", "framework"));
    let error = opt_err(COUNTRY, &domain);
    assert_eq!(error.kind(), ErrorKind::Validation);
    assert_eq!(error.to_string(), INVALID_MESSAGE);
}

#[test]
fn explicit_provider_context_wins_when_a_combined_root_is_external() {
    let domain = (provider(invalid()) | c("name", "=", "user")).into_external();
    assert_provider_error(&domain, CONTEXT);
}

#[test]
fn inversion_preserves_provider_failure_context() {
    let domain = !provider(invalid());
    let error = opt_err(COUNTRY, &domain);
    assert_eq!(error.kind(), ErrorKind::Programming);
    assert_eq!(
        error.to_string(),
        format!(
            "{CONTEXT}: Invalid field test_orm.country.abcdefg in condition ('abcdefg', 'not in', ['foo'])"
        )
    );
}

#[test]
fn condition_mapping_preserves_context_on_a_replacement() {
    let domain = provider(c("name", "=", "framework")).map_conditions(|_| invalid());
    assert_provider_error(&domain, CONTEXT);
}

#[test]
fn basic_rewrites_preserve_context_for_later_condition_mapping() {
    let domain = provider(c("phone_code", "=", 32));
    let optimized = opt(COUNTRY, &domain);
    assert_eq!(optimized.as_condition().unwrap().operator(), Operator::In);
    assert_eq!(
        optimized.programming_context().map(Arc::as_ref),
        Some(CONTEXT)
    );
    let rewritten = optimized.map_conditions(|_| invalid());
    assert_provider_error(&rewritten, CONTEXT);
}

#[test]
fn provider_context_reaches_an_any_subdomain_error() {
    let sub = invalid();
    let provider_domain = provider(Domain::condition("country_id", Operator::Any, sub).unwrap());
    let domain = c("name", "=", "user").into_external() & provider_domain;
    let error = domain.optimize(&model(PARTNER)).unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Programming);
    assert_eq!(error.to_string(), format!("{CONTEXT}: {INVALID_MESSAGE}"));
}

#[test]
fn external_any_keeps_a_nested_provider_pattern_error() {
    let sub = provider(c("name", "=like", 1));
    let domain = Domain::condition("country_id", Operator::Any, sub)
        .unwrap()
        .into_external();
    assert!(domain.is_external());
    let error = domain.optimize(&model(PARTNER)).unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Programming);
    assert_eq!(
        error.to_string(),
        format!(
            "{CONTEXT}: The pattern to match must be a string in condition ('name', '=like', 1)"
        )
    );
    assert_eq!(error.python_class(), "TypeError");
    assert!(matches!(
        error.condition_reason(),
        Some(erp_domain::ConditionError::PatternNotString)
    ));
}

#[test]
fn basic_path_decomposition_keeps_context_on_the_generated_subdomain() {
    let domain = provider(c("country_id.phone_code", "=", 32));
    let optimized = opt(PARTNER, &domain);
    let condition = optimized.as_condition().unwrap();
    assert_eq!(condition.operator(), Operator::Any);
    let Value::Domain(subdomain) = condition.value() else {
        panic!("path optimization must produce an any subdomain");
    };
    assert_eq!(
        subdomain.programming_context().map(Arc::as_ref),
        Some(CONTEXT)
    );
    let invalid_subdomain = subdomain.map_conditions(|_| invalid());
    assert_provider_error(&invalid_subdomain, CONTEXT);
}

#[test]
fn existing_nested_provider_context_takes_precedence() {
    let inner = invalid().with_programming_context(Arc::from(INNER_CONTEXT));
    let domain = provider(c("name", "=", "outer") & inner);
    assert_provider_error(&domain, INNER_CONTEXT);
}

#[test]
fn annotating_a_boolean_tree_tags_its_leaves() {
    let domain = provider(c("name", "=", "framework") | c("code", "=", "BE"));
    let DomainKind::Or(children) = domain.kind() else {
        panic!("fixture must retain both branches");
    };
    assert_eq!(children.len(), 2);
    for child in children {
        assert_eq!(child.programming_context().map(Arc::as_ref), Some(CONTEXT));
    }
}

#[test]
fn provenance_does_not_change_semantic_equality_or_hashing() {
    let domain = c("name", "=", "framework");
    let annotated = provider(domain.clone());
    assert_eq!(annotated, domain);
    let mut original_hash = DefaultHasher::new();
    domain.hash(&mut original_hash);
    let mut annotated_hash = DefaultHasher::new();
    annotated.hash(&mut annotated_hash);
    assert_eq!(original_hash.finish(), annotated_hash.finish());
}

#[test]
fn merging_same_provider_conditions_preserves_context() {
    let domain = provider(c("phone_code", "=", 32) | c("phone_code", "=", 33));
    let optimized = opt(COUNTRY, &domain);
    let rewritten = optimized.map_conditions(|_| invalid());
    assert_provider_error(&rewritten, CONTEXT);
}

#[test]
fn equal_conditions_do_not_erase_provider_failure_provenance() {
    let original = c("name", "=", "same").into_external();
    let domain = provider(c("name", "=", "same")) & original;
    let optimized = opt(COUNTRY, &domain);
    let rewritten = optimized.map_conditions(|_| invalid());
    assert_provider_error(&rewritten, CONTEXT);
}

#[test]
fn original_external_subdomain_keeps_validation_inside_a_typed_any() {
    let domain = Domain::condition("country_id", Operator::Any, invalid().into_external()).unwrap();
    let error = domain.optimize(&model(PARTNER)).unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Validation);
    assert_eq!(error.to_string(), INVALID_MESSAGE);
}

#[test]
fn equal_any_predicates_keep_distinct_nested_provider_origins() {
    const LEFT_CONTEXT: &str = "search_method '_search_left' for field 'test_orm.partner.left'";
    const RIGHT_CONTEXT: &str = "search_method '_search_right' for field 'test_orm.partner.right'";

    fn nested_predicate(context: &str) -> Domain {
        let subdomain = c("name", "=", "same").with_programming_context(Arc::from(context));
        provider(Domain::condition("child_ids", Operator::Any, subdomain).unwrap())
    }

    fn collect_contexts(domain: &Domain, contexts: &mut std::collections::BTreeSet<String>) {
        if let Some(context) = domain.programming_context() {
            contexts.insert(context.to_string());
        }
        match domain.kind() {
            DomainKind::Not(child) => collect_contexts(child, contexts),
            DomainKind::And(children) | DomainKind::Or(children) => {
                for child in children {
                    collect_contexts(child, contexts);
                }
            }
            DomainKind::Condition(condition) => {
                if let Value::Domain(subdomain) = condition.value() {
                    collect_contexts(subdomain, contexts);
                }
            }
            DomainKind::True | DomainKind::False | DomainKind::Custom(_) => {}
        }
    }

    let left = nested_predicate(LEFT_CONTEXT);
    let right = nested_predicate(RIGHT_CONTEXT);
    assert_eq!(left, right, "provenance remains outside predicate equality");
    let optimized = opt(PARTNER, &(left & right));
    let mut contexts = std::collections::BTreeSet::new();
    collect_contexts(&optimized, &mut contexts);
    assert_eq!(
        contexts,
        [CONTEXT, LEFT_CONTEXT, RIGHT_CONTEXT]
            .into_iter()
            .map(str::to_owned)
            .collect()
    );
}

#[test]
fn native_list_parser_preserves_nested_provider_context() {
    let nested = invalid().with_programming_context(Arc::from(INNER_CONTEXT));
    let items = vec![Value::Domain(nested)];
    let parsed = Domain::from_list_native(&items).unwrap();
    assert_provider_error(&provider(parsed), INNER_CONTEXT);
}
