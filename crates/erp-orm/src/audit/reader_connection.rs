// SPDX-License-Identifier: MIT OR Apache-2.0
//! Private reader pool: every new connection proves its role, privileges and primary database.
use super::reader_grants::{AuditReaderRoles, CHECK_SQL};
use deadpool_postgres::{Hook, HookError, Pool, Runtime, Timeouts};
use erp_fields::{Error, Result};
use std::{fmt, sync::Arc, time::Duration};
use tokio_postgres::Config;

// Failed or cancelled admission must discard a client with an unfinished protocol response.
// Successful probes explicitly release it back to its bounded pool.
struct AdmissionLease(Option<deadpool_postgres::Object>);
impl AdmissionLease {
    fn release(mut self) {
        drop(self.0.take());
    }
}
impl Drop for AdmissionLease {
    fn drop(&mut self) {
        if let Some(client) = self.0.take() {
            drop(deadpool_postgres::Object::take(client));
        }
    }
}

const REFUSED: &str = "a later release: audit reader connection admission failed";
fn refused() -> Error {
    Error::programming(REFUSED)
}

/// An admitted, single-slot audit reader connection facility. The pool and raw storage remain
/// private to the ORM; this handle exposes no SQL or unprojected-row API to addons.
#[derive(Clone)]
#[non_exhaustive]
pub struct AuditReaderConnection {
    pool: Pool,
    role: String,
    isolated: Arc<AuditReaderRoles>,
}
impl fmt::Debug for AuditReaderConnection {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AuditReaderConnection")
            .finish_non_exhaustive()
    }
}
impl AuditReaderConnection {
    /// Open a reader from its own credential and prove it belongs to the supplied trusted primary.
    /// The primary pool is used only for a live-session identity challenge, never for audit reads.
    /// Admission runs on every newly created connection. Credentials and driver errors are not
    /// retained as error sources because they can contain secret connection material.
    /// # Errors
    /// `Programming` on any connection, identity, isolation or privilege admission failure.
    pub async fn open(config: Config, primary: Pool, isolated: AuditReaderRoles) -> Result<Self> {
        let role = config.get_user().ok_or_else(refused)?.to_owned();
        if !isolated.accepts(&role) {
            return Err(refused());
        }
        let isolated = Arc::new(isolated);
        let connection_role = role.clone();
        let connection_roles = Arc::clone(&isolated);
        let hook = Hook::async_fn(move |client, _| {
            let role = role.clone();
            let primary = primary.clone();
            let isolated = Arc::clone(&isolated);
            Box::pin(async move {
                let denied = || HookError::message(REFUSED);
                let mut bytes = [0u8; 24];
                getrandom::fill(&mut bytes).map_err(|_| denied())?;
                let challenge = bytes.iter().map(|b| format!("{b:02x}")).collect::<String>();
                client
                    .simple_query("SET search_path TO pg_catalog")
                    .await
                    .map_err(|_| denied())?;
                let row = client.query_one("SELECT session_user::text,current_user::text,pg_catalog.current_database()::text,pg_catalog.pg_backend_pid(),pg_catalog.set_config('application_name',$1,false)", &[&challenge]).await.map_err(|_| denied())?;
                let session: String = row.try_get(0).map_err(|_| denied())?;
                let current: String = row.try_get(1).map_err(|_| denied())?;
                let database: String = row.try_get(2).map_err(|_| denied())?;
                let pid: i32 = row.try_get(3).map_err(|_| denied())?;
                if session != role || current != role {
                    return Err(denied());
                }
                let probe = AdmissionLease(Some(primary.get().await.map_err(|_| denied())?));
                let primary = probe.0.as_ref().ok_or_else(denied)?;
                let same: bool = primary.query_one("SELECT pg_catalog.current_database()=$1 AND EXISTS (SELECT FROM pg_catalog.pg_stat_activity WHERE pid=$2 AND datname=$1 AND usename=$3 AND application_name=$4)", &[&database,&pid,&role,&challenge]).await.map_err(|_| denied())?.try_get(0).map_err(|_| denied())?;
                if !same {
                    return Err(denied());
                }
                probe.release();
                let unsafe_rights: bool = client
                    .query_one(
                        CHECK_SQL,
                        &[&role, &isolated.names, &false, &isolated.serving],
                    )
                    .await
                    .map_err(|_| denied())?
                    .try_get(0)
                    .map_err(|_| denied())?;
                if unsafe_rights {
                    return Err(denied());
                }
                client
                    .query_one(
                        "SELECT pg_catalog.set_config('application_name','erp-audit-reader',false)",
                        &[],
                    )
                    .await
                    .map_err(|_| denied())?;
                Ok(())
            })
        });
        let pool = crate::pool::pool_builder(config)
            .post_create(hook)
            .max_size(1)
            .runtime(Runtime::Tokio1)
            .timeouts(Timeouts {
                wait: Some(Duration::from_secs(10)),
                create: Some(Duration::from_secs(10)),
                recycle: Some(Duration::from_secs(10)),
            })
            .build()
            .map_err(|_| refused())?;
        let admitted = Self {
            pool,
            role: connection_role,
            isolated: connection_roles,
        };
        admitted.verify().await?;
        Ok(admitted)
    }
    /// Verify current privileges again before publishing a serving generation.
    /// # Errors
    /// A sanitized admission failure. This never substitutes another pool.
    pub async fn verify(&self) -> Result<()> {
        let lease = AdmissionLease(Some(self.pool.get().await.map_err(|_| refused())?));
        let result = tokio::time::timeout(Duration::from_secs(10), async {
            let client = lease.0.as_ref().ok_or_else(refused)?;
            let row = client
                .query_one("SELECT session_user::text, current_user::text", &[])
                .await
                .map_err(|_| refused())?;
            if row.try_get::<_, String>(0).map_err(|_| refused())? != self.role
                || row.try_get::<_, String>(1).map_err(|_| refused())? != self.role
            {
                return Err(refused());
            }
            let unsafe_rights: bool = client
                .query_one(
                    CHECK_SQL,
                    &[
                        &self.role,
                        &self.isolated.names,
                        &false,
                        &self.isolated.serving,
                    ],
                )
                .await
                .map_err(|_| refused())?
                .try_get(0)
                .map_err(|_| refused())?;
            if unsafe_rights {
                return Err(refused());
            }
            Ok(())
        })
        .await
        .map_err(|_| refused())?;
        result?;
        lease.release();
        Ok(())
    }
}
