//! The topics and schema part `mail` registers itself: `schema` and `messaging.self`.

use erp_sdk::store_wire::{FieldKind, FieldSpec, FieldValue};
use erp_sdk::{Error, Transaction};

use super::registry::TopicRegistry;
use super::topic::{
    Audience, SchemaFragment, SchemaPart, StoreTopic, TopicFuture, TopicInput, TopicOutput,
};

/// The model of the caller's identity record (`messaging.self` topic).
pub const SELF_MODEL: &str = "messaging.self";

crate::store_topic!(
    SCHEMA_TOPIC = StoreTopic::new("mail", "schema", run_schema)
        .audience(Audience::LoggedIn)
        .statements(0)
);

crate::store_topic!(
    SELF_TOPIC = StoreTopic::new("mail", "messaging.self", run_self)
        .audience(Audience::LoggedIn)
        .statements(1)
);

crate::store_schema_part!(MAIL_SCHEMA = SchemaPart::new("mail", mail_schema));

fn mail_schema(fragment: &mut SchemaFragment) {
    // the partner's own fields (`name`) are declared by the thread schema part
    fragment.model("res.partner");
    fragment.field(
        SELF_MODEL,
        "partner_id",
        FieldSpec::new(FieldKind::Many2One).target("res.partner"),
    );
    fragment.field(SELF_MODEL, "share", FieldSpec::new(FieldKind::Bool));
}

fn run_schema<'a>(
    _tx: &'a mut Transaction,
    _input: &'a TopicInput<'a>,
    out: &'a mut TopicOutput,
) -> TopicFuture<'a> {
    Box::pin(async move {
        out.data(TopicRegistry::linked()?.schema_document().clone());
        Ok(())
    })
}

fn run_self<'a>(
    tx: &'a mut Transaction,
    input: &'a TopicInput<'a>,
    out: &'a mut TopicOutput,
) -> TopicFuture<'a> {
    Box::pin(async move {
        let uid = input.env().uid();
        let rows = tx
            .query(
                "SELECT u.partner_id::int8, u.share FROM res_users u WHERE u.id = $1::int8",
                &[&uid],
            )
            .await?;
        let [row] = rows.as_slice() else {
            return Err(Error::invariant(format!(
                "messaging.self: the caller's user {uid} has no res.users row"
            )));
        };
        let partner: Option<i64> = row.try_get(0).map_err(|e| {
            Error::internal("messaging.self: partner_id is not an id").with_source(e)
        })?;
        let share: bool = row.try_get(1).map_err(|e| {
            Error::internal("messaging.self: share is not a boolean").with_source(e)
        })?;
        let partner = partner
            .and_then(|id| u64::try_from(id).ok())
            .filter(|id| *id > 0)
            .ok_or_else(|| {
                Error::invariant(format!("messaging.self: user {uid} has no partner"))
            })?;
        let id = u64::try_from(uid).map_err(|_| {
            Error::invariant(format!("messaging.self: user id {uid} is not positive"))
        })?;
        out.set(
            SELF_MODEL,
            id,
            "partner_id",
            FieldValue::Many2One(Some(partner)),
        );
        out.set(SELF_MODEL, id, "share", FieldValue::Bool(share));
        Ok(())
    })
}
