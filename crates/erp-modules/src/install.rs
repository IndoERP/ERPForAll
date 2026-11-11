//! Native composition boundary for authenticated human lifecycle operations (0083).
//!
//! The HTTP route authenticates before admission; this trusted adapter must recheck the
//! credential and current administrator authority in the supplied primary transaction.
//! It is not an addon RPC extension point. Only the module engine holds OperationControl.
use erp_orm::{BoxFuture, Principal, PrincipalKind, Transaction};

/// A trusted primary-session lifecycle checker supplied by native server composition.
/// The historical name is retained for native install adapter compatibility.
pub trait InstallAuthorizer: Send + Sync {
    /// Check the current session, active user and system group after the lifecycle lock.
    /// Do not open another transaction, commit, or perform application writes here.
    ///
    /// The confirming uninstall runs this check in a READ COMMITTED transaction (sound
    /// because it holds ACCESS EXCLUSIVE on every ordinary table of the schema first), so
    /// each statement sees current data after the lock wait. Use raw `query`, `execute` and
    /// `batch` only: the record API refuses READ COMMITTED with a Programming error. Install
    /// and upgrade keep REPEATABLE READ.
    fn authorize<'a>(
        &'a self,
        tx: &'a mut Transaction,
    ) -> BoxFuture<'a, erp_fields::Result<InstallAuthorization>>;
}

/// Current human identity returned by the trusted authorizer, never an HTTP DTO.
#[derive(Debug)]
pub struct InstallAuthorization {
    pub(crate) principal: Principal,
    pub(crate) session_id: String,
}

impl InstallAuthorization {
    /// Bind a freshly checked primary session to its resolved, unelevated human principal.
    /// This validates shape, not credentials: only the trusted native authorizer calls it
    /// after checking the session and system group in the supplied lifecycle transaction.
    /// # Errors
    /// Access for an impersonated/elevated/scoped/non-user identity; Validation for malformed UUID.
    pub fn new(principal: Principal, session_id: String) -> erp_fields::Result<Self> {
        if principal.kind() != PrincipalKind::User
            || principal.actor().is_some()
            || principal.scope().is_some()
            || !matches!(principal.elevation(), erp_orm::Elevation::None)
            || !principal.resolution().is_resolved()
        {
            return Err(erp_fields::Error::access(
                "Module installation requires a resolved primary-session user",
            ));
        }
        let valid = session_id.len() == 36
            && session_id.bytes().enumerate().all(|(i, b)| {
                if [8, 13, 18, 23].contains(&i) {
                    b == b'-'
                } else {
                    b.is_ascii_digit() || (b'a'..=b'f').contains(&b)
                }
            });
        if !valid {
            return Err(erp_fields::Error::validation(
                "Invalid installation session identity",
            ));
        }
        Ok(Self {
            principal,
            session_id,
        })
    }
}

const AUDIT_DDL: &str = "CREATE TABLE IF NOT EXISTS erp_module_install_audit (
    id bigserial PRIMARY KEY,
    actor_uid bigint NOT NULL,
    session_id uuid NOT NULL,
    requested text[] NOT NULL CHECK (cardinality(requested) BETWEEN 1 AND 64),
    installed text[] NOT NULL,
    created_at timestamptz NOT NULL DEFAULT clock_timestamp()
); REVOKE ALL ON erp_module_install_audit FROM PUBLIC";
const AUDIT_AFTER: &str = "DO $$ BEGIN
    IF NOT EXISTS (SELECT 1 FROM pg_constraint WHERE conrelid='erp_module_install_audit'::regclass
                   AND conname='erp_module_install_audit_actor_fkey') THEN
        ALTER TABLE erp_module_install_audit ADD CONSTRAINT erp_module_install_audit_actor_fkey
        FOREIGN KEY (actor_uid) REFERENCES res_users(id) ON DELETE RESTRICT;
    END IF;
END $$";
erp_orm::core_ddl!(
    INSTALL_AUDIT,
    owner = "erp-modules",
    name = "erp_module_install_audit",
    purpose = "authenticated native addon installation audit",
    tables = ["erp_module_install_audit"],
    sql = AUDIT_DDL,
    after_models = { needs_table = "res_users", sql = AUDIT_AFTER }
);
const LIFECYCLE_AUDIT_DDL: &str = "CREATE TABLE IF NOT EXISTS erp_module_lifecycle_audit (
    id bigserial PRIMARY KEY,
    actor_uid bigint NOT NULL,
    session_id uuid NOT NULL,
    operation text NOT NULL CHECK (operation IN ('install','upgrade','uninstall','force_demo')),
    requested text[] NOT NULL CHECK (cardinality(requested) BETWEEN 1 AND 64),
    resolved text[] NOT NULL,
    installed text[] NOT NULL,
    upgraded text[] NOT NULL,
    uninstalled text[] NOT NULL DEFAULT '{}',
    fingerprint text,
    outcome text NOT NULL CHECK (outcome = 'committed'),
    created_at timestamptz NOT NULL DEFAULT clock_timestamp()
 );
ALTER TABLE erp_module_lifecycle_audit ADD COLUMN IF NOT EXISTS uninstalled text[] NOT NULL DEFAULT '{}';
ALTER TABLE erp_module_lifecycle_audit ADD COLUMN IF NOT EXISTS fingerprint text;
ALTER TABLE erp_module_lifecycle_audit DROP CONSTRAINT IF EXISTS erp_module_lifecycle_audit_operation_check;
ALTER TABLE erp_module_lifecycle_audit ADD CONSTRAINT erp_module_lifecycle_audit_operation_check CHECK (operation IN ('install','upgrade','uninstall','force_demo'));
ALTER TABLE erp_module_lifecycle_audit DROP CONSTRAINT IF EXISTS erp_module_lifecycle_audit_fingerprint_check;
ALTER TABLE erp_module_lifecycle_audit ADD CONSTRAINT erp_module_lifecycle_audit_fingerprint_check CHECK (
    (operation='uninstall' AND fingerprint IS NOT NULL AND fingerprint ~ '^uninstall-v1:[0-9a-f]{64}$') OR
    (operation IN ('install','upgrade','force_demo') AND fingerprint IS NULL));
CREATE OR REPLACE FUNCTION erp_module_lifecycle_audit_insert_only() RETURNS trigger LANGUAGE plpgsql AS $audit$
BEGIN RAISE EXCEPTION 'Lifecycle audit is insert-only'; END; $audit$;
DROP TRIGGER IF EXISTS erp_module_lifecycle_audit_insert_only ON erp_module_lifecycle_audit;
CREATE TRIGGER erp_module_lifecycle_audit_insert_only BEFORE UPDATE OR DELETE OR TRUNCATE ON erp_module_lifecycle_audit
FOR EACH STATEMENT EXECUTE FUNCTION erp_module_lifecycle_audit_insert_only();
REVOKE ALL ON erp_module_lifecycle_audit FROM PUBLIC;
REVOKE ALL ON SEQUENCE erp_module_lifecycle_audit_id_seq FROM PUBLIC";
const LIFECYCLE_AUDIT_AFTER: &str = "DO $$ BEGIN
    IF NOT EXISTS (SELECT 1 FROM pg_constraint WHERE conrelid='erp_module_lifecycle_audit'::regclass
                   AND conname='erp_module_lifecycle_audit_actor_fkey') THEN
        ALTER TABLE erp_module_lifecycle_audit ADD CONSTRAINT erp_module_lifecycle_audit_actor_fkey
        FOREIGN KEY (actor_uid) REFERENCES res_users(id) ON DELETE RESTRICT;
    END IF;
END $$";
erp_orm::core_ddl!(
    LIFECYCLE_AUDIT,
    owner = "erp-modules",
    name = "erp_module_lifecycle_audit",
    purpose = "authenticated native lifecycle operation audit",
    tables = ["erp_module_lifecycle_audit"],
    sql = LIFECYCLE_AUDIT_DDL,
    after_models = { needs_table = "res_users", sql = LIFECYCLE_AUDIT_AFTER }
);
static REQUIRED: [erp_orm::CoreTableRequirement; 6] = [
    erp_orm::CoreTableRequirement::new("installed_addon_identity", "erp-modules"),
    erp_orm::CoreTableRequirement::new("installed_addon_identity_audit", "erp-modules"),
    erp_orm::CoreTableRequirement::new("erp_module_install_audit", "erp-modules"),
    erp_orm::CoreTableRequirement::new("erp_module_lifecycle_audit", "erp-modules"),
    erp_orm::CoreTableRequirement::new("ir_module_package", "erp-modules"),
    erp_orm::CoreTableRequirement::new("ir_module_wasm_audit", "erp-modules"),
];
pub(crate) fn core_tables() -> &'static [erp_orm::CoreTableRequirement] {
    std::hint::black_box(&INSTALL_AUDIT);
    std::hint::black_box(&LIFECYCLE_AUDIT);
    crate::wasm_lifecycle::keep_core_tables_linked();
    crate::system::keep_identity_tables_linked();
    &REQUIRED
}

pub(crate) async fn record_audit(
    tx: &mut Transaction,
    authorization: &InstallAuthorization,
    requested: &[crate::ModuleName],
    installed: &[crate::ModuleName],
) -> erp_fields::Result<()> {
    let requested: Vec<&str> = requested.iter().map(crate::ModuleName::as_str).collect();
    let installed: Vec<&str> = installed.iter().map(crate::ModuleName::as_str).collect();
    tx.execute(
        "INSERT INTO erp_module_install_audit(actor_uid,session_id,requested,installed) VALUES ($1,$2::text::uuid,$3,$4)",
        &[&authorization.principal.subject().get(), &authorization.session_id, &requested, &installed],
    ).await?;
    Ok(())
}

pub(crate) async fn record_lifecycle_audit(
    tx: &mut Transaction,
    authorization: &InstallAuthorization,
    operation: &str,
    requested: &[crate::ModuleName],
    installed: &[crate::ModuleName],
    upgraded: &[crate::ModuleName],
) -> erp_fields::Result<()> {
    let requested: Vec<&str> = requested.iter().map(crate::ModuleName::as_str).collect();
    let installed: Vec<&str> = installed.iter().map(crate::ModuleName::as_str).collect();
    let upgraded: Vec<&str> = upgraded.iter().map(crate::ModuleName::as_str).collect();
    let resolved: Vec<&str> = installed
        .iter()
        .chain(&upgraded)
        .copied()
        .collect::<std::collections::BTreeSet<_>>()
        .into_iter()
        .collect();
    tx.execute(
        "INSERT INTO erp_module_lifecycle_audit(actor_uid,session_id,operation,requested,resolved,installed,upgraded,outcome) VALUES ($1,$2::text::uuid,$3,$4,$5,$6,$7,'committed')",
        &[&authorization.principal.subject().get(), &authorization.session_id, &operation, &requested, &resolved, &installed, &upgraded],
    ).await?;
    Ok(())
}

pub(crate) async fn record_uninstall_audit(
    tx: &mut Transaction,
    authorization: &InstallAuthorization,
    requested: &[crate::ModuleName],
    resolved: &[crate::ModuleName],
    fingerprint: &str,
) -> erp_fields::Result<()> {
    let requested: Vec<_> = requested.iter().map(crate::ModuleName::as_str).collect();
    let resolved: Vec<_> = resolved.iter().map(crate::ModuleName::as_str).collect();
    tx.execute(
        "INSERT INTO erp_module_lifecycle_audit(actor_uid,session_id,operation,requested,resolved,installed,upgraded,uninstalled,fingerprint,outcome) VALUES ($1,$2::text::uuid,'uninstall',$3,$4,'{}','{}',$4,$5,'committed')",
        &[&authorization.principal.subject().get(), &authorization.session_id, &requested, &resolved, &fingerprint],
    ).await?;
    Ok(())
}

#[cfg(test)]
mod primary_session_tests {
    #![allow(clippy::unwrap_used, reason = "test assertions")]
    use super::InstallAuthorization;
    use erp_orm::{
        ApiKeyId, GroupSet, MethodAllow, ModelAllow, PrincipalBuilder, Resolution, Scope, UserId,
    };
    use std::num::NonZeroU64;

    #[test]
    fn scoped_api_key_is_not_a_primary_install_authorization() {
        let scope = Scope::builder(ApiKeyId::new(1).unwrap(), "restricted key")
            .models(ModelAllow::All)
            .methods(MethodAllow::All)
            .read_only(true)
            .build()
            .unwrap();
        let principal = PrincipalBuilder::user(UserId::new(2).unwrap())
            .resolution(Resolution::in_id_order(GroupSet::default(), [], NonZeroU64::MIN).unwrap())
            .scope(scope)
            .build()
            .unwrap();
        let error =
            InstallAuthorization::new(principal, "019a0000-0000-7000-8000-000000000001".into())
                .unwrap_err();
        assert_eq!(error.kind(), erp_fields::ErrorKind::Access);
        assert_eq!(
            error.message(),
            "Module installation requires a resolved primary-session user"
        );
    }
}
