//! The tables of authentication (§3.4, §14.10), core tables
//! installed by `erp_orm::Orm::sync_schema`. Every statement is idempotent: a database made by
//! an earlier revision converges on the next sync.
//!
//! - `erp_session.user_id` references `res_users(id) ON DELETE CASCADE` as a safety net for
//!   deletions outside the ORM (the ORM ends the sessions first, through the session-end hooks,
//!   so the cascade finds nothing). `res_users` is a model's table, created after the core
//!   tables: the key is added by [`AUTH_AFTER_MODELS_DDL`], once, after the models.
//! - Collection reads expired rows through `erp_session_expires` (`expires_at` is never updated,
//!   so touches stay HOT updates, `scale-out.md` §2); a `pending` row carries `expires_at` at
//!   most [`PENDING_LIFETIME_SECS`](super::store::PENDING_LIFETIME_SECS) after `created_at`
//!   (`erp_session_pending_expiry`), so the expiry index collects it too. Idle rows are found by
//!   a bounded sweep of the heap in page ranges (`super::gc`), not through an index on
//!   `last_seen_at`, which would make every touch a non-HOT update of six indexes.
//! - Storage parameters as the database review set them (round 1, M2): room on every page for the
//!   touch updates, autovacuum sized for tables of short-lived rows. The DDL owns only the
//!   parameters a table lacks: it adds a missing one and never changes a value an operator set
//!   (database review round 2, N4). The `EXECUTE` that adds them builds its text only from the
//!   constant option arrays of the DDL, filtered against the catalog: no input reaches it
//!   (security review round 2 delta N-g).
//! - Old age of `erp_login_throttle` rows is read through two partial indexes, one for the
//!   counted kinds and one for trusted pairs, so a collection of one kind never walks the other
//!   (round 2, S1); the single index of the earlier revision is dropped.
//! - A check added to a table of an earlier revision first makes its rows comply (round 2, N3):
//!   the count of rows changed is raised as a `WARNING`.
//! - No privilege for `PUBLIC`: the application role owns the tables; reporting and read-only
//!   roles get no grant (`scale-out.md` "Database roles"). The `REVOKE` runs only when `PUBLIC`
//!   holds a privilege, so a sync of a converged database rewrites no catalog row (round 2, N2).
//! - `erp_login_throttle` stays `LOGGED`: its counters must survive a failover.

use erp_orm::CoreTableRequirement;

/// `erp_session` and `erp_login_throttle`, before the models' tables.
pub const AUTH_DDL: &str = "\
CREATE TABLE IF NOT EXISTS erp_session (
    id                  uuid        PRIMARY KEY,
    user_id             int8        NOT NULL,
    sid_digest          bytea       NOT NULL UNIQUE CHECK (octet_length(sid_digest) = 32),
    prev_digest         bytea       UNIQUE CHECK (octet_length(prev_digest) = 32),
    rotated_at          timestamptz,
    token               bytea       NOT NULL CHECK (octet_length(token) = 32),
    token_kid           int2        NOT NULL,
    state               text        NOT NULL CHECK (state IN ('pending', 'active')),
    created_at          timestamptz NOT NULL DEFAULT now(),
    last_seen_at        timestamptz NOT NULL DEFAULT now(),
    expires_at          timestamptz NOT NULL,
    identity_checked_at timestamptz,
    first_addr          inet,
    first_agent         text        CHECK (char_length(first_agent) <= 1024),
    last_addr           inet,
    last_agent          text        CHECK (char_length(last_agent) <= 1024),
    device_trusted      boolean     NOT NULL DEFAULT true,
    context             jsonb       NOT NULL DEFAULT '{}',
    data                jsonb       NOT NULL DEFAULT '{}'
);
CREATE INDEX IF NOT EXISTS erp_session_user ON erp_session (user_id);
CREATE INDEX IF NOT EXISTS erp_session_expires ON erp_session (expires_at);
DO $$
DECLARE
    fixed   bigint;
    missing text;
BEGIN
    IF NOT EXISTS (SELECT 1 FROM pg_constraint
                    WHERE conrelid = 'erp_session'::regclass
                      AND conname = 'erp_session_pending_expiry') THEN
        -- a pending row of an earlier revision may outlive the pending lifetime: cut its expiry
        -- to it first (an earlier end is safe), so the check can be added
        UPDATE erp_session SET expires_at = created_at + interval '600 seconds'
         WHERE state = 'pending' AND expires_at > created_at + interval '600 seconds';
        GET DIAGNOSTICS fixed = ROW_COUNT;
        IF fixed > 0 THEN
            RAISE WARNING 'erp_session: the expiry of % pending rows cut to the pending lifetime before adding erp_session_pending_expiry', fixed;
        END IF;
        ALTER TABLE erp_session ADD CONSTRAINT erp_session_pending_expiry
            CHECK (state <> 'pending' OR expires_at <= created_at + interval '600 seconds');
    END IF;
    -- only the parameters the table lacks: a value an operator set stays
    SELECT string_agg(o, ', ') INTO missing
      FROM unnest(ARRAY['fillfactor=85', 'autovacuum_vacuum_scale_factor=0.02',
                        'autovacuum_vacuum_threshold=500',
                        'autovacuum_vacuum_insert_scale_factor=0.05',
                        'autovacuum_vacuum_cost_delay=2',
                        'autovacuum_analyze_scale_factor=0.05']) o
     WHERE NOT EXISTS (SELECT 1 FROM pg_class c, unnest(c.reloptions) r
                        WHERE c.oid = 'erp_session'::regclass
                          AND split_part(r, '=', 1) = split_part(o, '=', 1));
    IF missing IS NOT NULL THEN
        EXECUTE 'ALTER TABLE erp_session SET (' || missing || ')';
    END IF;
END
$$;
CREATE TABLE IF NOT EXISTS erp_login_throttle (
    kind            text        NOT NULL
                    CONSTRAINT erp_login_throttle_kind_check
                    CHECK (kind IN ('address', 'login', 'trusted', 'principal', 'principal_trusted')),
    key_hash        bytea       NOT NULL CHECK (octet_length(key_hash) = 32),
    failures        int4        NOT NULL CHECK (failures > 0),
    last_failure_at timestamptz NOT NULL,
    PRIMARY KEY (kind, key_hash),
    CONSTRAINT erp_login_throttle_trusted_once CHECK (kind NOT IN ('trusted', 'principal_trusted') OR failures = 1)
);
CREATE INDEX IF NOT EXISTS erp_login_throttle_counted_age ON erp_login_throttle (last_failure_at)
    WHERE kind IN ('address', 'login', 'principal');
CREATE INDEX IF NOT EXISTS erp_login_throttle_trusted_age ON erp_login_throttle (last_failure_at)
    WHERE kind IN ('trusted', 'principal_trusted');
DO $$
DECLARE
    fixed   bigint;
    missing text;
BEGIN
    -- the one age index of an earlier revision, replaced by the two partial ones above
    IF to_regclass('erp_login_throttle_age') IS NOT NULL THEN
        DROP INDEX erp_login_throttle_age;
    END IF;
    -- a table of an earlier revision allows only 'address', 'login', 'trusted': widen its check
    IF EXISTS (SELECT 1 FROM pg_constraint
                WHERE conrelid = 'erp_login_throttle'::regclass
                  AND conname = 'erp_login_throttle_kind_check'
                  AND pg_get_constraintdef(oid) NOT LIKE '%principal%') THEN
        ALTER TABLE erp_login_throttle
            DROP CONSTRAINT erp_login_throttle_kind_check,
            ADD CONSTRAINT erp_login_throttle_kind_check
                CHECK (kind IN ('address', 'login', 'trusted', 'principal', 'principal_trusted'));
    END IF;
    -- Converge constraints and index predicates from legacy schemas, not just new tables.
    IF NOT EXISTS (SELECT 1 FROM pg_constraint
                    WHERE conrelid = 'erp_login_throttle'::regclass
                      AND conname = 'erp_login_throttle_trusted_once'
                      AND pg_get_constraintdef(oid) LIKE '%principal_trusted%') THEN
        UPDATE erp_login_throttle SET failures = 1
         WHERE kind IN ('trusted', 'principal_trusted') AND failures <> 1;
        GET DIAGNOSTICS fixed = ROW_COUNT;
        IF fixed > 0 THEN
            RAISE WARNING 'erp_login_throttle: % legacy trusted rows normalized before constraint migration', fixed;
        END IF;
        ALTER TABLE erp_login_throttle DROP CONSTRAINT IF EXISTS erp_login_throttle_trusted_once;
        ALTER TABLE erp_login_throttle ADD CONSTRAINT erp_login_throttle_trusted_once
            CHECK (kind NOT IN ('trusted', 'principal_trusted') OR failures = 1);
    END IF;
    IF EXISTS (SELECT 1 FROM pg_index
                WHERE indexrelid = 'erp_login_throttle_counted_age'::regclass
                  AND pg_get_expr(indpred, indrelid) NOT LIKE '%principal%') THEN
        DROP INDEX erp_login_throttle_counted_age;
        CREATE INDEX erp_login_throttle_counted_age ON erp_login_throttle(last_failure_at)
            WHERE kind IN ('address', 'login', 'principal');
    END IF;
    IF EXISTS (SELECT 1 FROM pg_index
                WHERE indexrelid = 'erp_login_throttle_trusted_age'::regclass
                  AND pg_get_expr(indpred, indrelid) NOT LIKE '%principal_trusted%') THEN
        DROP INDEX erp_login_throttle_trusted_age;
        CREATE INDEX erp_login_throttle_trusted_age ON erp_login_throttle(last_failure_at)
            WHERE kind IN ('trusted', 'principal_trusted');
    END IF;
    SELECT string_agg(o, ', ') INTO missing
      FROM unnest(ARRAY['fillfactor=80', 'autovacuum_vacuum_scale_factor=0.01',
                        'autovacuum_vacuum_threshold=1000',
                        'autovacuum_vacuum_cost_delay=0',
                        'autovacuum_analyze_scale_factor=0.05']) o
     WHERE NOT EXISTS (SELECT 1 FROM pg_class c, unnest(c.reloptions) r
                        WHERE c.oid = 'erp_login_throttle'::regclass
                          AND split_part(r, '=', 1) = split_part(o, '=', 1));
    IF missing IS NOT NULL THEN
        EXECUTE 'ALTER TABLE erp_login_throttle SET (' || missing || ')';
    END IF;
    -- REVOKE rewrites the tables' catalog rows on every run: only when PUBLIC holds something
    IF EXISTS (SELECT 1 FROM pg_class c,
                      aclexplode(coalesce(c.relacl, acldefault('r', c.relowner))) a
                WHERE c.oid IN ('erp_session'::regclass, 'erp_login_throttle'::regclass)
                  AND a.grantee = 0) THEN
        REVOKE ALL ON erp_session, erp_login_throttle FROM PUBLIC;
    END IF;
END
$$";

/// The foreign key of `erp_session.user_id`, after the models' tables (a registry without
/// `res.users` has no `res_users` table and no key). Added once: rows of users that no longer
/// exist are deleted first (they fail every check already; no hook runs for them, their user is
/// gone; the count is raised as a `WARNING`, which PostgreSQL writes to its server log at the
/// default `log_min_messages = warning`; our process sees it only as a connection notice, which
/// the driver logs at `info` under `tokio_postgres` and no test asserts, so the server log is
/// where an operator finds the one-time cleanup: silent-delta N-8), then the key is added and
/// validated.
pub const AUTH_AFTER_MODELS_DDL: &str = "\
DO $$
DECLARE
    orphans bigint;
BEGIN
    -- Authentication owns this key. Schema convergence never enables it or rewrites enrollment.
    IF to_regclass('public.ir_config_parameter') IS NOT NULL THEN
        PERFORM pg_advisory_xact_lock(1163022406,6);
        IF NOT EXISTS (SELECT FROM public.erp_node_feature WHERE key='auth.login_principal') THEN
            IF (SELECT count(*) FROM public.erp_node_feature) >= 8 THEN
                RAISE EXCEPTION 'authentication rollout cannot register a ninth feature key';
            END IF;
            INSERT INTO public.erp_node_feature(key,version) VALUES('auth.login_principal',1);
        ELSIF EXISTS (SELECT FROM public.erp_node_feature WHERE key='auth.login_principal' AND version<>1) THEN
            RAISE EXCEPTION 'authentication rollout capability version differs from this binary';
        END IF;
    END IF;
    IF to_regclass('res_users') IS NOT NULL
       AND NOT EXISTS (SELECT 1 FROM pg_constraint
                        WHERE conrelid = 'erp_session'::regclass
                          AND conname = 'erp_session_user_id_fkey') THEN
        DELETE FROM erp_session s
         WHERE NOT EXISTS (SELECT 1 FROM res_users u WHERE u.id = s.user_id);
        GET DIAGNOSTICS orphans = ROW_COUNT;
        IF orphans > 0 THEN
            RAISE WARNING 'erp_session: % rows of users that no longer exist deleted before adding erp_session_user_id_fkey (no session-end hook ran for them)', orphans;
        END IF;
        ALTER TABLE erp_session ADD CONSTRAINT erp_session_user_id_fkey
            FOREIGN KEY (user_id) REFERENCES res_users (id) ON DELETE CASCADE;
    END IF;
END
$$";

erp_orm::core_ddl!(
    AUTH_TABLES,
    owner = "erp-security",
    name = "erp_session",
    purpose = "sessions and the login throttle",
    tables = ["erp_session", "erp_login_throttle"],
    sql = AUTH_DDL,
    after_models = {
        needs_table = "res_users",
        sql = AUTH_AFTER_MODELS_DDL
    }
);

/// Durable progress and expiring run token of the interim authentication scheduler.
/// A runtime scheduler needs SELECT and UPDATE; it never inserts/deletes state.
pub(super) const GC_STATE_DDL: &str = "\
CREATE TABLE IF NOT EXISTS erp_auth_gc_state (
    singleton boolean PRIMARY KEY CHECK (singleton),
    page bigint NOT NULL DEFAULT 0 CHECK (page BETWEEN 0 AND 4294967295),
    lease_token bigint NOT NULL DEFAULT 0 CHECK (lease_token >= 0),
    lease_until timestamptz NOT NULL DEFAULT '-infinity',
    next_run_at timestamptz NOT NULL DEFAULT '-infinity'
);
INSERT INTO erp_auth_gc_state(singleton) VALUES (true) ON CONFLICT DO NOTHING;
DO $$ BEGIN
    IF EXISTS (SELECT 1 FROM pg_class c,
        aclexplode(coalesce(c.relacl, acldefault('r', c.relowner))) a
        WHERE c.oid='erp_auth_gc_state'::regclass AND a.grantee=0) THEN
        REVOKE ALL ON erp_auth_gc_state FROM PUBLIC;
    END IF;
END $$";

erp_orm::core_ddl!(
    GC_STATE,
    owner = "erp-security",
    name = "erp_auth_gc_state",
    purpose = "durable authentication collection cursor and run lease",
    tables = ["erp_auth_gc_state"],
    sql = GC_STATE_DDL
);

/// The core tables of authentication, as a binary requires them.
static REQUIRED: [CoreTableRequirement; 3] = [
    CoreTableRequirement::new("erp_session", "erp-security"),
    CoreTableRequirement::new("erp_login_throttle", "erp-security"),
    CoreTableRequirement::new("erp_auth_gc_state", "erp-security"),
];

/// The core tables of authentication, for `OrmBuilder::require_core_tables` at the build site of
/// every binary that serves authentication or ends sessions (the server, the module system,
/// tests):. Calling it also keeps the registration in the binary (it references it).
pub fn core_tables() -> &'static [CoreTableRequirement] {
    // the reference pulls the registration's object file into the binary
    std::hint::black_box(&AUTH_TABLES);
    std::hint::black_box(&GC_STATE);
    std::hint::black_box(&API_KEY_TABLE);
    &REQUIRED
}

/// Check that `orm` was built requiring the core tables of authentication
/// ([`core_tables`], `OrmBuilder::require_core_tables`), so its build checked that their
/// registration is linked. The server calls it where it makes the authentication
/// service; a build site that forgot the requirement then fails at startup, not at the first login.
///
/// # Errors
///
/// `Programming` naming the first table of [`core_tables`] the ORM does not require.
pub fn check_orm(orm: &erp_orm::Orm) -> erp_fields::Result<()> {
    for need in core_tables() {
        if !orm.required_core_tables().contains(&need) {
            return Err(erp_fields::Error::programming(format!(
                "the ORM was built without requiring the core table '{}' of '{}': build it with \
                 OrmBuilder::require_core_tables(erp_security::auth::core_tables())",
                need.table(),
                need.owner()
            )));
        }
    }
    Ok(())
}

/// Whether the registration above is in the binary. Calling it also keeps it there: the linker
/// pulls an object file of a library only for a symbol something references. Still needed next to
/// [`core_tables`]: `base` ends sessions on archive and delete in binaries that build their ORM
/// without naming authentication, and [`super::store`] refuses there instead of deleting from a
/// table that was never created.
pub(crate) fn registered() -> bool {
    erp_orm::CORE_DDL
        .iter()
        .any(|ddl| std::ptr::eq(ddl, &AUTH_TABLES))
}

#[cfg(test)]
mod tests {
    use super::{AUTH_DDL, core_tables};

    /// The pending CHECK and the collection agree on the pending lifetime.
    #[test]
    fn pending_check_matches_the_pending_lifetime() {
        let secs = super::super::store::PENDING_LIFETIME_SECS;
        assert_eq!(secs.fract(), 0.0);
        let interval = format!("created_at + interval '{secs:.0} seconds'");
        assert!(AUTH_DDL.contains(&interval), "{interval}");
    }

    /// Requiring the tables keeps the registration.
    #[test]
    fn core_tables_are_registered() {
        for need in core_tables() {
            assert!(
                erp_orm::CORE_DDL
                    .iter()
                    .any(|ddl| ddl.tables.contains(&need.table())),
                "{}",
                need.table()
            );
        }
    }
}

// This registration owns no table: base owns the model and ordinary columns.
// Separate needs_table prevents a users-only registry from running key DDL.
#[linkme::distributed_slice(erp_orm::CORE_DDL)]
static API_KEY_TABLE: erp_orm::CoreDdl = erp_orm::CoreDdl {
    owner: "erp-security",
    name: "res_users_apikeys_private",
    purpose: "a later release private digest and invariants",
    tables: &[],
    sql: "",
    after_models: Some(erp_orm::AfterModels {
        needs_table: "res_users_apikeys",
        sql: API_KEY_AFTER_MODELS_DDL,
    }),
};

// Issuance is infrequent: validate the same canonical definitions as sync on
// an empty temporary table. This avoids a name-only guard certifying CHECK(TRUE).
// The performance cost is unmeasured and deferred.
macro_rules! api_key_validation_ddl {
    ($prefix:literal, $missing:literal) => { concat!($prefix, r#"DO $$
DECLARE c record; expected_definition text; next_definition text; matches boolean; attempt integer;
BEGIN
    IF to_regclass('res_users_apikeys') IS NULL THEN
        RAISE EXCEPTION 'a later release requires res_users_apikeys table';
    END IF;
    IF NOT EXISTS (SELECT FROM pg_attribute WHERE attrelid = 'res_users_apikeys'::regclass
                   AND attname = 'digest' AND atttypid = 'bytea'::regtype AND NOT attisdropped) THEN
        RAISE EXCEPTION 'a later release requires res_users_apikeys.digest bytea';
    END IF;
    CREATE TEMP TABLE erp_apikey_expected (LIKE res_users_apikeys) ON COMMIT DROP;
    FOR c IN SELECT * FROM (VALUES
      ('apikey_digest_unique', 'UNIQUE (digest)'),
      ('apikey_digest_length', 'CHECK (digest IS NULL OR octet_length(digest) = 32)'),
      ('apikey_representation', 'CHECK ((digest IS NOT NULL) <> (legacy_hash IS NOT NULL))'),
      ('apikey_kid', 'CHECK (digest_kid BETWEEN 1 AND 32767)'),
      ('apikey_kind', 'CHECK (kind IN (''user'', ''agent'', ''service''))'),
      ('apikey_scope', 'CHECK (scope IN (''rpc'', ''mcp''))'),
      ('apikey_capabilities', 'CHECK (capabilities IN (''none'', ''subject''))'),
      ('apikey_user', 'CHECK (kind <> ''user'' OR (models IS NULL AND methods IS NULL AND NOT read_only AND capabilities = ''subject''))'),
      ('apikey_mcp', 'CHECK (scope <> ''mcp'' OR (kind = ''agent'' AND capabilities = ''none''))'),
      ('apikey_prefix', 'CHECK (char_length(prefix) = 8)'),
      ('apikey_name', 'CHECK (octet_length(name) BETWEEN 1 AND 128 AND length(btrim(name)) > 0)'),
      ('apikey_models', 'CHECK (models IS NULL OR jsonb_typeof(models) = ''array'')'),
      ('apikey_methods', 'CHECK (methods IS NULL OR jsonb_typeof(methods) = ''array'')')
    ) AS constraints(name, definition) LOOP
        IF NOT EXISTS (SELECT FROM pg_constraint WHERE conrelid = 'res_users_apikeys'::regclass
                       AND conname = c.name) THEN
"#, $missing, r#"
        END IF;
        -- Let PostgreSQL parse/deparse the trusted definition on an empty throwaway
        -- table. Compare canonical definitions, not only names: CHECK(TRUE) under
        -- a known name must not certify a weakened production constraint.
        -- Deparse/reparse is not initially idempotent (varchar IN casts and
        -- BETWEEN/AND grouping change on restore). Accept only exact forms
        -- reached from our trusted definition. Never execute catalog text.
        expected_definition := c.definition;
        matches := false;
        FOR attempt IN 1..8 LOOP
            EXECUTE format('ALTER TABLE pg_temp.erp_apikey_expected ADD CONSTRAINT %I %s', c.name, expected_definition);
            SELECT pg_get_constraintdef(oid) INTO STRICT next_definition
              FROM pg_constraint WHERE conrelid = 'pg_temp.erp_apikey_expected'::regclass
                AND conname = c.name;
            SELECT EXISTS (
                SELECT FROM pg_constraint actual
                WHERE actual.conrelid = 'res_users_apikeys'::regclass
                  AND actual.conname = c.name AND actual.convalidated
                  AND NOT actual.condeferrable
                  AND pg_get_constraintdef(actual.oid) = next_definition
            ) INTO matches;
            EXIT WHEN matches OR next_definition = expected_definition;
            EXECUTE format('ALTER TABLE pg_temp.erp_apikey_expected DROP CONSTRAINT %I', c.name);
            expected_definition := next_definition;
        END LOOP;
        IF NOT matches THEN
            RAISE EXCEPTION 'a later release constraint % differs', c.name;
        END IF;
    END LOOP;
    DROP TABLE erp_apikey_expected;
END $$;
"#) };
}
const API_KEY_AFTER_MODELS_DDL: &str = api_key_validation_ddl!(
    "ALTER TABLE res_users_apikeys ADD COLUMN IF NOT EXISTS digest bytea;",
    "EXECUTE format('ALTER TABLE res_users_apikeys ADD CONSTRAINT %I %s', c.name, c.definition);"
);
pub(crate) const API_KEY_REQUIRE_SCHEMA_DDL: &str = api_key_validation_ddl!(
    "",
    "RAISE EXCEPTION 'a later release constraint % missing; run schema sync', c.name;"
);

/// Validate the canonical principal-accounting constraints and retention indexes before rollout.
///
/// This only reads catalogs; it does not repair schema or normalize counters. Equivalent custom
/// definitions must be migrated by the operator to the accepted canonical schema before admission.
///
/// # Errors
/// `Programming` for absent, invalid or differently defined objects. Catalog faults retain their
/// `Database` kind, full diagnostic and source. No object name alone certifies schema convergence.
pub async fn check_login_rollout(tx: &mut erp_orm::Transaction) -> erp_fields::Result<()> {
    let rows=tx.query("SELECT \
        (SELECT pg_get_expr(conbin,conrelid)=$1 AND convalidated FROM pg_constraint WHERE conrelid=to_regclass('public.erp_login_throttle') AND conname='erp_login_throttle_kind_check') IS TRUE AND \
        (SELECT pg_get_expr(conbin,conrelid)=$2 AND convalidated FROM pg_constraint WHERE conrelid=to_regclass('public.erp_login_throttle') AND conname='erp_login_throttle_trusted_once') IS TRUE AND \
        (SELECT pg_get_expr(i.indpred,i.indrelid)=$3 AND i.indisvalid AND i.indisready AND NOT i.indisunique AND am.amname='btree' AND i.indnkeyatts=1 AND i.indnatts=1 AND i.indkey[0]=a.attnum FROM pg_index i JOIN pg_class c ON c.oid=i.indexrelid JOIN pg_am am ON am.oid=c.relam JOIN pg_attribute a ON a.attrelid=i.indrelid AND a.attname='last_failure_at' AND NOT a.attisdropped WHERE i.indexrelid=to_regclass('public.erp_login_throttle_counted_age') AND i.indrelid=to_regclass('public.erp_login_throttle')) IS TRUE AND \
        (SELECT pg_get_expr(i.indpred,i.indrelid)=$4 AND i.indisvalid AND i.indisready AND NOT i.indisunique AND am.amname='btree' AND i.indnkeyatts=1 AND i.indnatts=1 AND i.indkey[0]=a.attnum FROM pg_index i JOIN pg_class c ON c.oid=i.indexrelid JOIN pg_am am ON am.oid=c.relam JOIN pg_attribute a ON a.attrelid=i.indrelid AND a.attname='last_failure_at' AND NOT a.attisdropped WHERE i.indexrelid=to_regclass('public.erp_login_throttle_trusted_age') AND i.indrelid=to_regclass('public.erp_login_throttle')) IS TRUE",
        &[&"(kind = ANY (ARRAY['address'::text, 'login'::text, 'trusted'::text, 'principal'::text, 'principal_trusted'::text]))",&"((kind <> ALL (ARRAY['trusted'::text, 'principal_trusted'::text])) OR (failures = 1))",&"(kind = ANY (ARRAY['address'::text, 'login'::text, 'principal'::text]))",&"(kind = ANY (ARRAY['trusted'::text, 'principal_trusted'::text]))"]).await?;
    let [row] = rows.as_slice() else {
        return Err(erp_fields::Error::programming(
            "login selection rollout schema validation returned no unique state",
        ));
    };
    let valid: bool = row.try_get(0).map_err(|error| {
        erp_fields::Error::db("cannot decode login rollout schema state").with_source(error)
    })?;
    if !valid {
        return Err(erp_fields::Error::programming(
            "login selection rollout requires canonical principal throttle schema",
        ));
    }
    Ok(())
}
