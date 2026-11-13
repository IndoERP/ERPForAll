// SPDX-License-Identifier: MIT OR Apache-2.0
//! Trusted composition for the private audit reader; no credentials enter addon-visible state.
use crate::{CliError, Result};
use erp_orm::audit::{AuditReaderConnection, AuditReaderRoles};
use erp_server::config::ServerConfig;

pub(crate) fn roles(config: &ServerConfig) -> Result<AuditReaderRoles> {
    let runtime = config.runtime_connection();
    let owner = config.owner_connection();
    let node = config.configured_node_connection();
    AuditReaderRoles::new(
        runtime.get_user().ok_or_else(invalid)?,
        owner.get_user().ok_or_else(invalid)?,
        node.as_ref().and_then(tokio_postgres::Config::get_user),
    )
    .map_err(Into::into)
}
fn invalid() -> CliError {
    erp_fields::Error::programming("a later release: audit reader connection admission failed")
        .into()
}

pub(crate) async fn open(
    config: &ServerConfig,
    primary: deadpool_postgres::Pool,
) -> Result<Option<AuditReaderConnection>> {
    match config.configured_audit_reader_connection() {
        Some(reader) => AuditReaderConnection::open(reader, primary, roles(config)?)
            .await
            .map(Some)
            .map_err(Into::into),
        None => Ok(None),
    }
}

/// Verify a configured login after owner publication committed. The bounded pools are dropped
/// before the command returns; ordinary serving retains its own single reader facility.
pub(crate) async fn verify_configured(config: &ServerConfig) -> Result<()> {
    if config.configured_audit_reader_connection().is_some() {
        let primary = crate::access::pool_with_size(config.runtime_connection(), 1)?;
        open(config, primary).await?;
    }
    Ok(())
}
