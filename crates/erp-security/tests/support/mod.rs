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
use erp_registry::{ModelSpec, Registry};
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

/// Insert the generated company restriction of each of `models` into the policy tables of the
/// database behind `client`, as the installation emission does (`erp_modules`): the `ir_access`
/// row (`crud`, no group, active, generated) and the `ir_model.company_restriction` ledger text.
/// Module installation writes these rows; a test that compiles the policy from rows it inserted
/// itself has none, and a restricted model without its generated row is denied to every principal
/// that is not in superuser mode (drift). The `ir_model` row of a model is used when it exists and
/// created otherwise, so call this after the test inserted its own `ir_model` rows. A second call
/// for the same model changes nothing.
///
/// # Panics
///
/// When a model has no generated restriction in `registry` (it is not in the registry, is
/// unrestricted or has no declaration), and on any database error: nothing is skipped silently.
pub async fn insert_generated(client: &Client, registry: &Registry, models: &[&str]) {
    let all = erp_security::company_access::generate(registry)
        .expect("generate the company restrictions of the registry");
    for model in models {
        let g = all
            .iter()
            .find(|g| g.model_name() == *model)
            .ok_or_else(|| format!("no generated company restriction for model {model}"))
            .expect("every requested model is restricted in the registry");
        let found = client
            .query_opt(
                "SELECT id::int8 FROM ir_model WHERE model = $1 ORDER BY id LIMIT 1",
                &[&g.model_name()],
            )
            .await
            .unwrap();
        let model_id: i64 = match found {
            Some(row) => row.get(0),
            None => client
                .query_one(
                    "INSERT INTO ir_model (name, model, state, transient) \
                     VALUES ($1, $1, 'base', false) RETURNING id::int8",
                    &[&g.model_name()],
                )
                .await
                .unwrap()
                .get(0),
        };
        client
            .execute(
                "INSERT INTO ir_access (name, active, model_id, operation, domain, generated) \
                 SELECT $1::text, true, $2::int8, 'crud', $3::text, true \
                  WHERE NOT EXISTS (SELECT 1 FROM ir_access WHERE model_id = $2::int8 AND generated)",
                &[&g.name(), &model_id, &g.text()],
            )
            .await
            .unwrap();
        client
            .execute(
                "UPDATE ir_model SET company_restriction = $2::text WHERE id = $1::int8",
                &[&model_id, &g.text()],
            )
            .await
            .unwrap();
    }
}
