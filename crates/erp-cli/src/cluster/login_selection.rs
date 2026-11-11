//! Operator-only rollout controls. No command silently enrolls legacy processes or creates roles.
use crate::args::{AccessArgs, LoginSelectionAction};
use crate::error::Result;
use erp_orm::node::{FeatureKey, FeatureRollout, FeatureVersion, LegacyEnrollment};

pub(super) fn database(action: &LoginSelectionAction) -> &AccessArgs {
    match action {
        LoginSelectionAction::Status(db) | LoginSelectionAction::Disable(db) => db,
        LoginSelectionAction::Approve(a) => &a.db,
        LoginSelectionAction::Enable(a) => &a.db,
        LoginSelectionAction::Publish(a) => &a.db,
    }
}

pub(super) async fn run(
    name: &str,
    pool: &deadpool_postgres::Pool,
    action: &LoginSelectionAction,
) -> Result<String> {
    let feature = FeatureRollout::new(
        FeatureKey::new("auth.login_principal")?,
        FeatureVersion::new(1)?,
    );
    // Every operation identifies the actual security schema. Missing state never means disabled.
    let orm = erp_orm::Orm::builder(crate::access::registry()?, pool.clone())
        .require_core_tables(erp_security::auth::core_tables())
        .build()?;
    let mut tx = orm
        .begin_opts(
            erp_orm::BeginOptions::new(erp_orm::SUPERUSER_ID)
                .isolation(erp_orm::Isolation::ReadCommitted)
                .on_primary(),
        )
        .await?;
    erp_security::auth::schema::check_login_rollout(&mut tx).await?;
    if let LoginSelectionAction::Publish(a) = action {
        tx.grant_node_feature_roles(&a.maintenance_role, &a.request_role)
            .await?;
        tx.commit().await?;
        return Ok(format!(
            "login selection helpers published in database {name}: maintenance={} request={}; feature state unchanged",
            a.maintenance_role, a.request_role
        ));
    }
    tx.rollback().await?;
    // An idle Transaction still owns its checked-out connection. The operator pool has one slot.
    drop(tx);
    let status = match action {
        LoginSelectionAction::Status(_) => feature.status(pool).await?,
        LoginSelectionAction::Approve(a) => feature.approve(pool, a.digest).await?,
        LoginSelectionAction::Enable(a) => {
            if !a.confirm_legacy_drained_and_fenced {
                return Err(erp_fields::Error::programming(
                    "legacy drain and fencing must be explicitly confirmed",
                )
                .into());
            }
            feature
                .enable(pool, LegacyEnrollment::confirmed_drained_and_fenced())
                .await?
        }
        LoginSelectionAction::Disable(_) => feature.disable(pool).await?,
        LoginSelectionAction::Publish(_) => {
            return Err(erp_fields::Error::programming(
                "unreachable login selection publication state",
            )
            .into());
        }
    };
    let digests = status
        .approved_digests()
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>();
    Ok(format!(
        "login selection in database {name}: enabled={} generation={} version={} approved=[{}] incompatible={}",
        status.enabled(),
        status.generation(),
        status.required_version().get(),
        digests.join(","),
        status.incompatible_memberships()
    ))
}
