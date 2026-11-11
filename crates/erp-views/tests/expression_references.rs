//! Pure semantic reference collection is not a caller-authorization witness.
use erp_views::{ExpressionKind, expression_references};
use std::collections::BTreeSet;

#[test]
fn contexts_do_not_guess_literal_domains_and_values_keep_terminal_paths() {
    let fields = expression_references(
        "{'choices': ['name', '=', 'literal'], 'group_by': ['priority', 'create_date:month']}",
        ExpressionKind::Context,
    )
    .unwrap();
    assert_eq!(
        fields,
        BTreeSet::from(["priority".to_owned(), "create_date".to_owned()])
    );
    let fields =
        expression_references("company_id.partner_id.email", ExpressionKind::Value).unwrap();
    assert_eq!(
        fields,
        BTreeSet::from(["company_id.partner_id.email".to_owned()])
    );
    let fields = expression_references("context[internal_memo]", ExpressionKind::Value).unwrap();
    assert_eq!(fields, BTreeSet::from(["internal_memo".to_owned()]));
}

#[test]
fn unsupported_and_oversized_expression_sources_refuse_explicitly() {
    let error = expression_references(
        "[('internal_memo','unknown_operator',1)]",
        ExpressionKind::Domain,
    )
    .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("Unsupported search domain operator 'unknown_operator'")
    );
    let error = expression_references("context.get('record').internal_memo", ExpressionKind::Value)
        .unwrap_err();
    assert!(error.to_string().contains("a later release"));
    let error =
        expression_references(&"x".repeat(64 * 1024 + 1), ExpressionKind::Value).unwrap_err();
    assert!(error.to_string().contains("source"));
    assert!(error.to_string().contains("limit"));
}

// ---- WP2: one walker, explicit scope, granularity ------------------------------------------

fn invalid(message: &str) -> erp_views::ViewError {
    erp_views::ViewError::InvalidDescription(message.to_owned())
}

#[test]
fn source_helper_keeps_domain_literals_as_data_and_walks_value_positions() {
    let fields = expression_references(
        "[('partner_id.name', '=', 'name'), ('state', 'in', ['draft', other_field])]",
        ExpressionKind::Domain,
    )
    .unwrap();
    assert_eq!(
        fields,
        BTreeSet::from([
            "other_field".to_owned(),
            "partner_id.name".to_owned(),
            "state".to_owned()
        ])
    );
    let fields = expression_references("items[low:high]", ExpressionKind::Value).unwrap();
    assert_eq!(
        fields,
        BTreeSet::from(["high".to_owned(), "items".to_owned(), "low".to_owned()])
    );
}

#[test]
fn source_helper_refuses_unsupported_group_by_granularity_by_name() {
    let message = "Description group_by granularity 'decade' is unsupported; expected day, week, month, quarter or year";
    assert_eq!(
        expression_references("{'group_by': 'stage_id:decade'}", ExpressionKind::Context)
            .unwrap_err(),
        invalid(message)
    );
    assert_eq!(
        expression_references("{'group_by': 'a:month:day'}", ExpressionKind::Context).unwrap_err(),
        invalid("Description group_by has more than one granularity suffix")
    );
    let fields = expression_references(
        "{'group_by': ['a:day', 'b:week', 'c:month', 'd:quarter', 'e:year', 'f']}",
        ExpressionKind::Context,
    )
    .unwrap();
    assert_eq!(fields.len(), 6);
}

#[test]
fn source_helper_refuses_parent_paths_it_cannot_scope() {
    assert_eq!(
        expression_references("parent.partner_id.name", ExpressionKind::Value).unwrap_err(),
        invalid("Description expression uses parent.<path> without a declared parent model")
    );
    assert_eq!(
        expression_references("parent", ExpressionKind::Value).unwrap(),
        BTreeSet::new()
    );
}
