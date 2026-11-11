//! `<delete>`.
//!
//! The records to delete are the union of what the `search` domain finds and the record the `id`
//! external id names, then one `unlink`. It runs in every mode, also under `noupdate`. An `id`
//! that does not exist (or whose record is already gone) is a [`NoticeKind::DeleteTargetMissing`]
//! notice, not an error.

use std::collections::HashMap;

use erp_domain::Domain;
use erp_orm::Transaction;

use super::error::OpLabel;
use super::expr::{Known, qualify, to_value};
use super::{DataNotice, FileContext, LoadError, LoadErrorKind, LoadSession, NoticeKind, env};
use crate::ast::{DeleteOp, DeleteTarget, XmlId};

/// Delete the records a `<delete>` names.
pub(crate) async fn load(
    session: &mut LoadSession,
    tx: &mut Transaction,
    file: &FileContext,
    op: &DeleteOp,
) -> Result<(), LoadError> {
    let label = OpLabel::new(format!("<delete model=\"{}\">", op.model()));
    let result = run(session, tx, file, op).await;
    // the unlink may have removed `ir.model.data` rows behind the cache
    session.clear_cache();
    result.map_err(|kind| LoadError::at(op.source_file(), op.line(), label, kind))
}

async fn run(
    session: &mut LoadSession,
    tx: &mut Transaction,
    file: &FileContext,
    op: &DeleteOp,
) -> Result<(), LoadErrorKind> {
    let mut known: HashMap<String, Known> = HashMap::new();
    let env = env::operation_env(session, tx, file, op.env(), &mut known).await?;
    if env.registry().model(op.model()).is_none() {
        return Err(LoadErrorKind::ModelMissing {
            what: "<delete>".to_owned(),
            model: op.model().to_owned(),
        });
    }
    let records = env.records(op.model()).map_err(LoadErrorKind::Orm)?;
    let target = match op.target() {
        DeleteTarget::Search(expr) => {
            let result = session.eval(tx, file.module(), expr, &mut known).await?;
            let value = to_value(&result).map_err(|got| LoadErrorKind::NotAFieldValue {
                model: op.model().to_owned(),
                field: "search".to_owned(),
                got,
            })?;
            let json = erp_fields::convert::value_to_json(&value).map_err(LoadErrorKind::Orm)?;
            let domain = Domain::from_json(&json).map_err(|e| LoadErrorKind::Orm(e.into()))?;
            records
                .search(tx, &domain)
                .await
                .map_err(LoadErrorKind::Orm)?
        }
        DeleteTarget::Id(id) => {
            let name = qualify(&id.to_string(), file.module());
            session
                .resolve(tx, std::slice::from_ref(&name), &mut known)
                .await?;
            let xid = XmlId::parse(&name).map_err(LoadErrorKind::InvalidExternalId)?;
            let found = known.get(&name).and_then(|k| k.id());
            let exists = match found {
                Some(res_id) => {
                    // `resolve` caches every row it finds: a miss is a defect of the loader,
                    // and the wrong-model check must not be skipped by it
                    let row_model = session
                        .cache
                        .get(&xid)
                        .map(|row| row.model.clone())
                        .ok_or_else(|| {
                            LoadErrorKind::Internal(format!(
                                "external id {name} was resolved but is not in the session cache"
                            ))
                        })?;
                    if row_model != op.model() {
                        return Err(LoadErrorKind::DeleteWrongModel {
                            xid: name,
                            xid_model: row_model,
                            model: op.model().to_owned(),
                        });
                    }
                    let candidate = records.browse([res_id]).map_err(LoadErrorKind::Orm)?;
                    candidate.exists(tx).await.map_err(LoadErrorKind::Orm)?
                }
                None => records
                    .browse(std::iter::empty())
                    .map_err(LoadErrorKind::Orm)?,
            };
            if exists.is_empty() {
                // the target is missing, which does not matter here
                session.notices.push(DataNotice::new(
                    file.module(),
                    file.path(),
                    NoticeKind::DeleteTargetMissing { xid: name },
                ));
            }
            exists
        }
    };

    if !target.is_empty() {
        target.unlink(tx).await.map_err(LoadErrorKind::Orm)?;
    }
    Ok(())
}
