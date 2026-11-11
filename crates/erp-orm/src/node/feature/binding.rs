//! Feature checks derived from actual membership and scoped to a registry/database lineage.

use std::sync::Arc;

use erp_fields::{Error, Result};
use erp_registry::Registry;
use tokio_postgres::Row;

use super::super::{Digest, NodeAdmission};
use super::{FeatureKey, FeatureVersion};
use crate::{Orm, SUPERUSER_ID, Transaction};

/// An authoritative feature snapshot with private binding provenance.
///
/// A snapshot alone grants no commit authority. Revalidate it with [`FeatureBinding::protect_in`]
/// in the transaction that commits the protected work. Snapshots cannot move between bindings.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct FeatureSnapshot {
    enabled: bool,
    generation: i64,
    serving: bool,
    provenance: Arc<()>,
}

impl FeatureSnapshot {
    /// Whether the database feature was enabled when read.
    pub fn enabled(&self) -> bool {
        self.enabled
    }

    /// The nonnegative activation generation when read.
    pub fn generation(&self) -> i64 {
        self.generation
    }

    /// Whether the authoritative row and local admission both permitted serving when read.
    ///
    /// This is a snapshot, not a lease extension or a substitute for protection before commit.
    pub fn is_serving(&self) -> bool {
        self.serving
    }
}

/// Membership-derived authority to observe and protect one feature in one ORM registry.
///
/// Construction accepts starting membership so application composition can precede readiness.
/// Protected work still requires actual serving admission. A detached benchmark admission never
/// grants this authority, even if it carries an identity also registered by a real membership.
#[derive(Clone)]
#[non_exhaustive]
pub struct FeatureBinding {
    admission: NodeAdmission,
    registry: Arc<Registry>,
    database_id: u64,
    key: FeatureKey,
    version: FeatureVersion,
    node_text: String,
    digest: Digest,
    provenance: Arc<()>,
}

impl std::fmt::Debug for FeatureBinding {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FeatureBinding")
            .field("database", &self.admission.database())
            .field("node", &self.node_text)
            .field("key", &self.key)
            .field("version", &self.version)
            .finish_non_exhaustive()
    }
}

impl FeatureBinding {
    /// Validate a real membership against this database, binary declaration and registry.
    ///
    /// Every installed registry addon must occur in the immutable identity catalogue; ordinary
    /// registry rebuilds and installed subsets of that catalogue remain valid. The trusted native
    /// composer must also recheck source/version content against its actual AddonIndex: registry
    /// descriptors expose names, not manifest versions or file content.
    ///
    /// The read uses the ORM's pool and published narrow member helper. A successful constructor
    /// rolls its read transaction back; a failed or cancelled constructor discards the transaction
    /// connection through [`Transaction`]'s guard. No rollback failure is ignored.
    ///
    /// ```no_run
    /// # async fn example(orm: &erp_orm::Orm, member: &erp_orm::node::NodeMembership) -> erp_fields::Result<()> {
    /// use erp_orm::node::{FeatureBinding, FeatureKey, FeatureVersion};
    /// let binding = FeatureBinding::new(member.admission(), orm,
    ///     FeatureKey::new("auth.login_principal")?, FeatureVersion::new(1)?).await?;
    /// // Later, snapshot and protect using transactions of this same ORM registry.
    /// # let _ = binding;
    /// # Ok(()) }
    /// ```
    ///
    /// # Errors
    ///
    /// `Programming` for detached admission, wrong database, declaration mismatch or a database
    /// feature-policy refusal. Other store faults keep their original kind, message and source.
    /// A policy SQLSTATE `ERF01` retains its full diagnostic, SQLSTATE and source as `Programming`.
    pub async fn new(
        admission: NodeAdmission,
        orm: &Orm,
        key: FeatureKey,
        version: FeatureVersion,
    ) -> Result<Self> {
        if !admission.is_registered() {
            return Err(Error::programming(
                "feature binding requires registered node admission",
            ));
        }
        if admission.identity().features().get(&key) != Some(version) {
            return Err(Error::programming(
                "feature binding declaration does not match required version",
            ));
        }
        // Identity covers the whole locally held catalogue, whereas a registry may use any
        // installed subset. Native composition separately re-hashes package/source versions;
        // an embedded descriptor has no manifest version and is covered by the native build.
        let catalogue = admission.identity().addons().entries();
        for addon in orm.registry().addons() {
            if catalogue
                .binary_search_by(|entry| entry.name().cmp(addon.name))
                .is_err()
            {
                return Err(Error::programming(
                    "feature binding registry addon is absent from node catalogue",
                ));
            }
        }
        let node_text = admission.identity().id().to_string();
        let digest = admission.identity().digest();
        let binding = Self {
            admission,
            registry: Arc::clone(orm.registry()),
            database_id: orm.id(),
            key,
            version,
            node_text,
            digest,
            provenance: Arc::new(()),
        };
        let mut tx = orm.begin(SUPERUSER_ID).await?;
        tx.batch("SET LOCAL search_path = pg_catalog, public")
            .await?;
        super::grant::validate_helpers(&mut tx).await?;
        binding.snapshot_in(&mut tx).await?;
        tx.rollback().await?;
        Ok(binding)
    }

    /// Read state and membership through the caller's existing transaction.
    ///
    /// Starting or unready membership yields a non-serving snapshot; missing, stopped, expired or
    /// mismatched membership refuses. No feature protection is held after this observation.
    ///
    /// # Errors
    ///
    /// `Programming` for registry/database mismatch, malformed helper output or policy refusal;
    /// other store errors preserve their original kind, full message, SQLSTATE and source.
    pub async fn snapshot_in(&self, tx: &mut Transaction) -> Result<FeatureSnapshot> {
        self.check_scope(tx)?;
        self.check_database(tx).await?;
        let version = self.version.get();
        let digest = self.digest.as_bytes().as_slice();
        let rows = tx.query(
            "SELECT pg_catalog.current_database(), required_version, enabled, generation, node_serving \
             FROM public.erp_node_feature_member($1, $2, $3::text::uuid, $4)",
            &[&self.key.as_str(), &version, &self.node_text, &digest],
        ).await.map_err(policy_error)?;
        let row = unique_row(&rows, "feature membership helper returned no unique state")?;
        self.validate_database(row)?;
        self.validate_version(row)?;
        let enabled: bool = decode(row, 2)?;
        let generation: i64 = decode(row, 3)?;
        if generation < 0 {
            return Err(Error::programming("feature generation is negative"));
        }
        let node_serving: bool = decode(row, 4)?;
        Ok(FeatureSnapshot {
            enabled,
            generation,
            serving: node_serving && self.admission.is_serving(),
            provenance: Arc::clone(&self.provenance),
        })
    }

    /// Revalidate the original snapshot and hold shared feature protection through caller commit.
    ///
    /// The narrow helper runs on `tx` itself, not another connection. Its row protection ends only
    /// when this transaction commits, rolls back or is discarded. No lock is held across work
    /// performed before this call. Local serving admission is checked again before protection.
    ///
    /// # Errors
    ///
    /// `Programming` for another binding's snapshot, registry/database mismatch, non-serving local
    /// membership or policy refusal. Store faults retain their original kind, message and source;
    /// `ERF01` policy faults retain their SQLSTATE and full source chain as `Programming`.
    pub async fn protect_in(&self, tx: &mut Transaction, snapshot: &FeatureSnapshot) -> Result<()> {
        self.check_scope(tx)?;
        if !Arc::ptr_eq(&self.provenance, &snapshot.provenance) {
            return Err(Error::programming(
                "feature snapshot does not belong to this binding",
            ));
        }
        if !snapshot.serving || !self.admission.is_serving() {
            return Err(Error::programming("feature membership is not serving"));
        }
        self.check_database(tx).await?;
        let version = self.version.get();
        let digest = self.digest.as_bytes().as_slice();
        let rows = tx
            .query(
                "SELECT pg_catalog.current_database(), required_version, enabled, generation \
             FROM public.erp_node_feature_protect($1, $2, $3::text::uuid, $4, $5, $6)",
                &[
                    &self.key.as_str(),
                    &version,
                    &self.node_text,
                    &digest,
                    &snapshot.generation,
                    &snapshot.enabled,
                ],
            )
            .await
            .map_err(policy_error)?;
        let row = unique_row(&rows, "feature protection helper returned no unique state")?;
        self.validate_database(row)?;
        self.validate_version(row)?;
        let enabled: bool = decode(row, 2)?;
        let generation: i64 = decode(row, 3)?;
        if enabled != snapshot.enabled || generation != snapshot.generation {
            return Err(Error::programming(
                "login selection rollout generation changed",
            ));
        }
        // A drain/self-fence may occur while the helper waits for feature protection.
        if !self.admission.is_serving() {
            return Err(Error::programming("feature membership is not serving"));
        }
        Ok(())
    }

    fn check_scope(&self, tx: &Transaction) -> Result<()> {
        if !Arc::ptr_eq(&self.registry, tx.registry()) {
            return Err(Error::programming(
                "feature binding transaction does not match its registry",
            ));
        }
        if tx.database_id() != self.database_id {
            return Err(Error::programming(
                "feature binding transaction does not match its database",
            ));
        }
        Ok(())
    }

    async fn check_database(&self, tx: &mut Transaction) -> Result<()> {
        // Check before invoking a helper in another database: its missing membership/state must
        // not hide the actual database mismatch behind an unrelated policy fault.
        let rows = tx
            .query("SELECT pg_catalog.current_database()", &[])
            .await?;
        let row = unique_row(&rows, "feature database check returned no unique state")?;
        self.validate_database(row)
    }

    fn validate_database(&self, row: &Row) -> Result<()> {
        let database: &str = decode(row, 0)?;
        if database != self.admission.database() {
            return Err(Error::programming(
                "feature binding database does not match node membership",
            ));
        }
        Ok(())
    }

    fn validate_version(&self, row: &Row) -> Result<()> {
        let version: i32 = decode(row, 1)?;
        if version != self.version.get() {
            return Err(Error::programming(
                "feature capability version does not match",
            ));
        }
        Ok(())
    }
}

fn unique_row<'a>(rows: &'a [Row], message: &'static str) -> Result<&'a Row> {
    match rows {
        [row] => Ok(row),
        _ => Err(Error::programming(message)),
    }
}

fn decode<'a, T: tokio_postgres::types::FromSql<'a>>(row: &'a Row, index: usize) -> Result<T> {
    row.try_get(index)
        .map_err(|error| Error::db("cannot decode feature state").with_source(error))
}

fn policy_error(error: Error) -> Error {
    if error.sqlstate() == Some("ERF01") {
        Error::programming(error.message().to_owned())
            .with_sqlstate(Some("ERF01".to_owned()))
            .with_source(error)
    } else {
        error
    }
}
