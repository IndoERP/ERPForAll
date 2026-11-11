//! Compiles the example (kept in sync by hand: the guide
//! says so).
#![allow(dead_code, clippy::expect_used, reason = "the guide's example")]

use erp_addon_mail::store_fetch::{
    Audience, ParamDecl, SchemaFragment, SchemaPart, StoreTopic, TopicFuture, TopicInput,
    TopicOutput,
};
use erp_sdk::Transaction;
use erp_sdk::store_wire::{FieldKind, FieldSpec, FieldValue};

const PARAMS: &[ParamDecl] = &[ParamDecl::int("limit", 1, 50)];

fn run<'a>(
    tx: &'a mut Transaction,
    input: &'a TopicInput<'a>,
    out: &'a mut TopicOutput,
) -> TopicFuture<'a> {
    Box::pin(async move {
        let limit = input.params().int("limit")?;
        let rows = tx
            .query(
                "SELECT id::int8 FROM res_partner ORDER BY id LIMIT $1::int8",
                &[&limit],
            )
            .await?;
        for row in &rows {
            let id: i64 = row.get(0);
            let id = u64::try_from(id).expect("ids are positive");
            out.set("demo.partner", id, "known", FieldValue::Bool(true));
        }
        Ok(())
    })
}

erp_addon_mail::store_topic!(
    DEMO_TOPIC = StoreTopic::new("my_addon", "demo.partners", run)
        .audience(Audience::Internal)
        .params(PARAMS)
        .statements(1)
);

fn schema(fragment: &mut SchemaFragment) {
    fragment.field("demo.partner", "known", FieldSpec::new(FieldKind::Bool));
}

erp_addon_mail::store_schema_part!(DEMO_SCHEMA = SchemaPart::new("my_addon", schema));

#[test]
fn the_guide_example_registers() {
    assert_eq!(DEMO_TOPIC.topic_name(), "demo.partners");
    assert_eq!(DEMO_TOPIC.budget(), 1);
}
