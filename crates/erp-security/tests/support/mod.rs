//! Helpers shared by the `erp-security` database tests.
#![allow(
    dead_code,
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "test helpers"
)]

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use erp_fields::{Error, Result};
use erp_orm::{
    AccessDomain, AccessPolicy, Capability, ExplainRequest, Explanation, FieldMask, GroupId,
    GroupSet, GroupSetInterner, InternedGroups, Operation, PolicyGeneration, PrincipalBinding,
    SnapshotId, SnapshotOrigin, SnapshotRef,
};
use erp_registry::ModelSpec;
use erp_testkit::Harness;
use erp_testkit::tokio_postgres::{Client, NoTls};

/// `base` (policy tables) and `ir.model.data` on a fresh database.
pub async fn harness() -> Harness {
    Harness::builder([&erp_addon_base::ADDON, &erp_data::model_data::ADDON])
        .with_decls([erp_data::ir_model_data_decl()])
        // key issuance writes an audited event: fixtures that open plain
        // transactions get an operation identity, as the RPC layer would attach one
        .identify_transactions()
        .build()
        .await
        .expect("harness")
}

/// As [`harness`] but transactions opened without an operation identity stay untagged, for tests
/// of the identity requirement of audited writes.
pub async fn harness_untagged() -> Harness {
    Harness::builder([&erp_addon_base::ADDON, &erp_data::model_data::ADDON])
        .with_decls([erp_data::ir_model_data_decl()])
        .build()
        .await
        .expect("harness")
}

/// A pool of its own on the harness database.
pub fn pool(h: &Harness, size: usize) -> erp_testkit::deadpool_postgres::Pool {
    erp_orm::pool::create_pool(h.db().config(), size).unwrap()
}

/// A connection of its own to the harness database, outside every pool.
pub async fn connect(h: &Harness) -> Client {
    let (client, connection) = h.db().config().connect(NoTls).await.unwrap();
    tokio::spawn(async move {
        if let Err(error) = connection.await {
            eprintln!("erp-security test: connection closed with an error: {error}");
        }
    });
    client
}

/// Reparse the catalog definitions, as a dump/restore does, without copying any
/// handwritten constraint text into the regression fixture.
pub async fn restore_api_key_constraints(c: &Client) {
    c.batch_execute(r#"DO $$
DECLARE c record;
BEGIN
    FOR c IN SELECT conname, pg_get_constraintdef(oid) AS definition
             FROM pg_constraint WHERE conrelid = 'res_users_apikeys'::regclass
               AND conname LIKE 'apikey_%' ORDER BY conname LOOP
        EXECUTE format('ALTER TABLE res_users_apikeys DROP CONSTRAINT %I', c.conname);
        EXECUTE format('ALTER TABLE res_users_apikeys ADD CONSTRAINT %I %s', c.conname, c.definition);
    END LOOP;
END $$;"#).await.unwrap();
}

fn undecided() -> Error {
    Error::programming("the test snapshot decides nothing")
}

/// A snapshot that decides nothing but closes group sets over a fixed implication map (and
/// refuses groups it does not know, like a compiled snapshot).
pub struct TestSnapshot {
    id: SnapshotId,
    origin: SnapshotOrigin,
    generation: PolicyGeneration,
    interner: GroupSetInterner,
    /// group → the groups it implies directly; every known group is a key
    implied: BTreeMap<GroupId, Vec<GroupId>>,
}

impl TestSnapshot {
    /// A snapshot knowing `implied` (group → directly implied groups).
    pub fn new(
        origin: SnapshotOrigin,
        generation: PolicyGeneration,
        implied: BTreeMap<GroupId, Vec<GroupId>>,
    ) -> Result<Self> {
        Ok(Self {
            id: SnapshotId::next()?,
            origin,
            generation,
            interner: GroupSetInterner::new(64)?,
            implied,
        })
    }
}

impl AccessPolicy for TestSnapshot {
    /// An immutable snapshot: what it resolves never changes.
    fn group_refs_are_stable(&self) -> bool {
        true
    }

    fn generation(&self) -> PolicyGeneration {
        self.generation
    }

    fn snapshot(&self) -> SnapshotRef {
        SnapshotRef::new(self.id, self.origin)
    }

    fn needs_resolved_principals(&self) -> bool {
        true
    }

    fn intern_groups(&self, groups: &GroupSet) -> Result<InternedGroups> {
        Ok(InternedGroups::new(self.interner.intern(groups)?, false))
    }

    fn check_model(&self, _: &PrincipalBinding<'_>, _: &ModelSpec, _: Operation) -> Result<()> {
        Err(undecided())
    }

    fn access_domain(
        &self,
        _: &PrincipalBinding<'_>,
        _: &ModelSpec,
        _: Operation,
    ) -> Result<AccessDomain> {
        Err(undecided())
    }

    fn field_access(&self, _: &PrincipalBinding<'_>, _: &ModelSpec) -> Result<Arc<FieldMask>> {
        Err(undecided())
    }

    fn groups(&self, direct: &[GroupId]) -> Result<GroupSet> {
        let mut seen: BTreeSet<GroupId> = BTreeSet::new();
        let mut todo: Vec<GroupId> = direct.to_vec();
        while let Some(group) = todo.pop() {
            let Some(next) = self.implied.get(&group) else {
                return Err(Error::programming(format!(
                    "the test snapshot has no group {}",
                    group.get()
                )));
            };
            if seen.insert(group) {
                todo.extend(next.iter().copied());
            }
        }
        Ok(GroupSet::new(seen))
    }

    fn allowing_groups(&self, _: &ModelSpec, _: Operation) -> Result<Vec<GroupId>> {
        Err(undecided())
    }

    fn has_capability(&self, _: &PrincipalBinding<'_>, _: &Capability) -> Result<bool> {
        Err(undecided())
    }

    fn explain(&self, _: &ExplainRequest<'_>) -> Result<Explanation> {
        Err(undecided())
    }
}

/// A shared test snapshot over `implied`.
pub fn snapshot(implied: BTreeMap<GroupId, Vec<GroupId>>) -> Arc<TestSnapshot> {
    Arc::new(
        TestSnapshot::new(SnapshotOrigin::Shared, PolicyGeneration::default(), implied).unwrap(),
    )
}
