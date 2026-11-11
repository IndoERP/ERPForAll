//! Singleton direct one2many counts, with both sides authorized.
use erp_domain::{Domain, Operator};
use erp_fields::{Error, FieldType, Result};

use crate::{Records, SearchOptions, Transaction};

pub(super) async fn count(tx: &mut Transaction, records: &Records, name: &str) -> Result<u64> {
    let [id] = records.rec_ids() else {
        return Err(Error::validation(
            "relation_count requires one persisted record",
        ));
    };
    let id = id
        .as_real()
        .ok_or_else(|| Error::validation("relation_count requires one persisted record"))?;
    let field = records.spec().field(name).ok_or_else(|| {
        Error::validation(format!(
            "Unknown relation_count field '{}.{name}'",
            records.model_name()
        ))
    })?;
    // The normal field read gate validates model, field and owner record/company rights.
    // It does not fetch/materialize an O2M's child ids, including for a cold field.
    tx.field_read_checks(records, field.id).await?;
    let FieldType::One2many { comodel, inverse } = &field.ty else {
        return Err(Error::validation(
            "relation_count supports only direct one2many fields",
        ));
    };
    if !field.store
        || field.compute.is_some()
        || field.related.is_some()
        || field.domain.is_some()
        || field.context.is_some()
        || field.inherited.is_some()
    {
        return Err(Error::validation(
            "relation_count does not support nonstored, computed, related, inherited, domain or context one2many fields",
        ));
    }
    // A normal field gate permits a missing id's empty value. A count must distinguish
    // that from a real owner's empty relation, using an authorized existence predicate.
    // Directly opened archived owners remain valid; active_test belongs to the children.
    let owner_domain = Domain::condition("id", Operator::Eq, id.get())?;
    let owner = records
        .search_query(tx, &owner_domain, &SearchOptions::new().active_test(false))
        .await?;
    if owner.count(tx).await? == 0 {
        return Err(Error::missing(format!(
            "relation_count owner '{}({})' does not exist or is not readable",
            records.model_name(),
            id.get()
        )));
    }
    let target = records.env().records(comodel)?;
    let inverse_field = target.spec().field(inverse).ok_or_else(|| {
        Error::programming(format!(
            "Missing relation_count inverse '{comodel}.{inverse}'"
        ))
    })?;
    if !inverse_field.has_column()
        || !matches!(&inverse_field.ty, FieldType::Many2one { comodel, .. } if comodel == records.model_name())
    {
        return Err(Error::validation(
            "relation_count requires a stored many2one inverse",
        ));
    }
    let domain = Domain::condition(inverse, Operator::Eq, id.get())?;
    // search_count prepares target model and inverse-field access even when zero children
    // exist. Record/company rules and active_test are applied in SQL before COUNT; pending
    // inverse writes flush normally. No sudo, relation-value cache or application id vector.
    target.search_count(tx, &domain, None).await
}
