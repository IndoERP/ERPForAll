//! The queue's core tables (design §3), created by `Orm::sync_schema` like the
//! other core tables of `erp-orm` (listed in `core_ddl::ORM_DDL`).
//!
//! No foreign keys: principals and jobs are soft references (does the same),
//! so a deleted user makes the next chunk fail `PrincipalRevoked` instead of blocking a delete.
//! `CREATE TABLE IF NOT EXISTS` never alters an existing table, so a change to an existing table
//! is an explicit idempotent step after it (the `erp_job_run` key). The admin views are recreated
//! when their version comment differs; the runtime role's grant must be published again after a
//! recreate (`grant_runtime` runs after every schema sync).

/// Tables of the queue.
pub(crate) const JOB_TABLES: crate::CoreDdl = crate::CoreDdl {
    owner: "erp-orm",
    name: "erp_job",
    purpose: "the durable job queue",
    tables: &["erp_job", "erp_job_run", "erp_job_item_failure"],
    sql: JOB_DDL,
    after_models: None,
};

/// Idempotent DDL of the three tables and their indexes. Every size limit is a `CHECK` as a
/// backstop of the Rust-side checks.
pub(crate) const JOB_DDL: &str = "\
CREATE TABLE IF NOT EXISTS erp_job (
    id                     int8 GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    job_uuid               uuid NOT NULL DEFAULT uuidv7(),
    kind                   text NOT NULL CHECK (kind ~ '^[a-z][a-z0-9_]{0,63}\\.[a-z][a-z0-9_.]{0,127}$'),
    kind_version           int2 NOT NULL CHECK (kind_version >= 1),
    state                  text NOT NULL DEFAULT 'ready' CHECK (state IN ('ready','running','held','dead')),
    failure_rank           int2 NOT NULL DEFAULT 0 CHECK (failure_rank >= 0),
    priority               int2 NOT NULL DEFAULT 5 CHECK (priority BETWEEN 0 AND 9),
    run_after              timestamptz NOT NULL DEFAULT clock_timestamp(),
    enqueued_at            timestamptz NOT NULL DEFAULT clock_timestamp(),
    started_at             timestamptz,
    attempts               int4 NOT NULL DEFAULT 0 CHECK (attempts BETWEEN 0 AND 1000),
    max_attempts           int4 NOT NULL CHECK (max_attempts BETWEEN 1 AND 100),
    lease_owner            uuid,
    lease_epoch            int8 NOT NULL DEFAULT 0 CHECK (lease_epoch >= 0),
    lease_until            timestamptz,
    singleton_key          text CHECK (octet_length(singleton_key) BETWEEN 1 AND 200),
    dedup_key              text CHECK (octet_length(dedup_key) BETWEEN 1 AND 200),
    payload                jsonb NOT NULL DEFAULT '{}'::jsonb CHECK (octet_length(payload::text) <= 65536),
    progress               jsonb CHECK (octet_length(progress::text) <= 1048576),
    items_done             int8 NOT NULL DEFAULT 0 CHECK (items_done >= 0),
    items_failed           int8 NOT NULL DEFAULT 0 CHECK (items_failed >= 0),
    items_remaining        int8 CHECK (items_remaining >= 0),
    run_as_kind            text NOT NULL CHECK (run_as_kind IN ('user','agent','service','system')),
    run_as_user            int8 NOT NULL CHECK (run_as_user > 0),
    run_as_key             int8,
    run_as_delegating_user int8,
    run_as_companies       int8[] NOT NULL DEFAULT '{}' CHECK (cardinality(run_as_companies) <= 256),
    run_as_company         int8,
    enqueued_by_kind       text NOT NULL,
    enqueued_by_user       int8 NOT NULL,
    correlation_id         uuid,
    causation_operation    uuid,
    cancel_requested       bool NOT NULL DEFAULT false,
    last_error_code        text CHECK (octet_length(last_error_code) <= 64),
    last_error             text CHECK (octet_length(last_error) <= 4096),
    last_error_at          timestamptz,
    dead_at                timestamptz,
    CONSTRAINT erp_job_lease_iff_running
        CHECK ((state = 'running') = (lease_owner IS NOT NULL AND lease_until IS NOT NULL)),
    CONSTRAINT erp_job_dead_iff_dead_at CHECK ((state = 'dead') = (dead_at IS NOT NULL)),
    CONSTRAINT erp_job_key_kinds CHECK (
        (run_as_kind IN ('agent','service')) = (run_as_key IS NOT NULL))
) WITH (fillfactor = 70);

CREATE UNIQUE INDEX IF NOT EXISTS erp_job_uuid ON erp_job (job_uuid);
CREATE INDEX IF NOT EXISTS erp_job_claim ON erp_job (failure_rank, priority, run_after) INCLUDE (id, kind)
    WHERE state = 'ready';
CREATE INDEX IF NOT EXISTS erp_job_running ON erp_job (id) WHERE state = 'running';
CREATE UNIQUE INDEX IF NOT EXISTS erp_job_dedup ON erp_job (kind, dedup_key)
    WHERE dedup_key IS NOT NULL AND state <> 'dead';
CREATE INDEX IF NOT EXISTS erp_job_singleton ON erp_job (singleton_key)
    WHERE singleton_key IS NOT NULL AND state = 'running';
CREATE INDEX IF NOT EXISTS erp_job_dead ON erp_job (kind, dead_at) WHERE state = 'dead';

CREATE TABLE IF NOT EXISTS erp_job_run (
    id               int8 GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    job_uuid         uuid NOT NULL,
    kind             text NOT NULL,
    kind_version     int2 NOT NULL,
    outcome          text NOT NULL CHECK (outcome IN ('done','cancelled','dead_discarded','dead_expired')),
    attempts         int4 NOT NULL,
    enqueued_at      timestamptz NOT NULL,
    started_at       timestamptz,
    finished_at      timestamptz NOT NULL DEFAULT clock_timestamp(),
    items_done       int8 NOT NULL,
    items_failed     int8 NOT NULL,
    run_as_kind      text NOT NULL,
    run_as_user      int8 NOT NULL,
    enqueued_by_user int8 NOT NULL,
    correlation_id   uuid,
    last_error_code  text
);
CREATE INDEX IF NOT EXISTS erp_job_run_job_uuid ON erp_job_run (job_uuid, finished_at);
CREATE INDEX IF NOT EXISTS erp_job_run_finished ON erp_job_run USING brin (finished_at);
ALTER TABLE erp_job_run ADD COLUMN IF NOT EXISTS id int8 GENERATED ALWAYS AS IDENTITY;
-- an older database keeps PRIMARY KEY (job_uuid, finished_at): move the key to id, so both kinds of
-- database end with the same keys (the (job_uuid, finished_at) index above replaces the old key)
DO $erp$
BEGIN
    IF EXISTS (
        SELECT 1 FROM pg_constraint c
         WHERE c.conrelid = 'erp_job_run'::regclass AND c.conname = 'erp_job_run_pkey'
           AND (SELECT array_agg(a.attname::text ORDER BY k.ord)
                  FROM unnest(c.conkey) WITH ORDINALITY k(attnum, ord)
                  JOIN pg_attribute a ON a.attrelid = c.conrelid AND a.attnum = k.attnum)
               = ARRAY['job_uuid', 'finished_at']) THEN
        ALTER TABLE erp_job_run DROP CONSTRAINT erp_job_run_pkey;
        ALTER TABLE erp_job_run ADD PRIMARY KEY (id);
    END IF;
END
$erp$;

CREATE TABLE IF NOT EXISTS erp_job_item_failure (
    job_uuid    uuid NOT NULL,
    item_key    text NOT NULL CHECK (octet_length(item_key) BETWEEN 1 AND 200),
    error_code  text NOT NULL CHECK (octet_length(error_code) <= 64),
    error       text NOT NULL CHECK (octet_length(error) <= 4096),
    failed_at   timestamptz NOT NULL DEFAULT clock_timestamp(),
    PRIMARY KEY (job_uuid, item_key)
);

-- No payload, progress or lease owner: job arguments may carry user data, and the screen has no
-- renderer for json or uuid. security_invoker: the runtime role's blanket table grant also covers these views, and an
-- owner-rights view would let it write columns that erp_job grants only by column (jobs/grant.rs).
DO $erp$
BEGIN
    IF obj_description(to_regclass('ir_job'), 'pg_class') IS DISTINCT FROM 'erp-job-view 2' THEN
        DROP VIEW IF EXISTS ir_job;
        EXECUTE $view$CREATE VIEW ir_job WITH (security_invoker = true) AS
SELECT
    id,
    job_uuid::text AS job_uuid,
    kind::varchar AS kind,
    kind_version::int4 AS kind_version,
    state::varchar AS state,
    failure_rank::int4 AS failure_rank,
    priority::int4 AS priority,
    (run_after AT TIME ZONE 'UTC') AS run_after,
    (enqueued_at AT TIME ZONE 'UTC') AS enqueued_at,
    (started_at AT TIME ZONE 'UTC') AS started_at,
    attempts,
    max_attempts,
    (lease_until AT TIME ZONE 'UTC') AS lease_until,
    singleton_key::varchar AS singleton_key,
    dedup_key::varchar AS dedup_key,
    LEAST(items_done, 2147483647)::int4 AS items_done,
    LEAST(items_failed, 2147483647)::int4 AS items_failed,
    (CASE WHEN items_remaining > 2147483647 THEN 2147483647 ELSE items_remaining END)::int4 AS items_remaining,
    run_as_kind::varchar AS run_as_kind,
    run_as_user,
    run_as_company,
    enqueued_by_user,
    cancel_requested,
    last_error_code::varchar AS last_error_code,
    last_error,
    (last_error_at AT TIME ZONE 'UTC') AS last_error_at,
    (dead_at AT TIME ZONE 'UTC') AS dead_at
FROM erp_job$view$;
        COMMENT ON VIEW ir_job IS 'erp-job-view 2';
    END IF;
END
$erp$;

DO $erp$
BEGIN
    IF obj_description(to_regclass('ir_job_run'), 'pg_class') IS DISTINCT FROM 'erp-job-view 2' THEN
        DROP VIEW IF EXISTS ir_job_run;
        EXECUTE $view$CREATE VIEW ir_job_run WITH (security_invoker = true) AS
SELECT
    id,
    job_uuid::text AS job_uuid,
    kind::varchar AS kind,
    kind_version::int4 AS kind_version,
    outcome::varchar AS outcome,
    attempts,
    (enqueued_at AT TIME ZONE 'UTC') AS enqueued_at,
    (started_at AT TIME ZONE 'UTC') AS started_at,
    (finished_at AT TIME ZONE 'UTC') AS finished_at,
    LEAST(items_done, 2147483647)::int4 AS items_done,
    LEAST(items_failed, 2147483647)::int4 AS items_failed,
    run_as_kind::varchar AS run_as_kind,
    run_as_user,
    enqueued_by_user,
    last_error_code::varchar AS last_error_code
FROM erp_job_run$view$;
        COMMENT ON VIEW ir_job_run IS 'erp-job-view 2';
    END IF;
END
$erp$;

REVOKE ALL ON erp_job, erp_job_run, erp_job_item_failure FROM PUBLIC";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_registration_lists_every_created_table() {
        for table in ["erp_job", "erp_job_run", "erp_job_item_failure"] {
            assert!(JOB_TABLES.tables.contains(&table));
            assert!(JOB_DDL.contains(&format!("CREATE TABLE IF NOT EXISTS {table} (")));
        }
        assert_eq!(JOB_TABLES.tables.len(), 3);
    }

    #[test]
    fn the_admin_views_run_with_the_callers_rights_and_hide_job_arguments() {
        // the runtime role's blanket grant covers views; owner-rights views would bypass erp_job's column grants
        for view in ["ir_job", "ir_job_run"] {
            assert!(JOB_DDL.contains(&format!(
                "CREATE VIEW {view} WITH (security_invoker = true) AS"
            )));
        }
        let views = &JOB_DDL[JOB_DDL.find("CREATE VIEW").unwrap()..];
        for hidden in ["payload", "progress", "lease_owner"] {
            assert!(!views.contains(hidden), "{hidden} must not be exposed");
        }
    }

    #[test]
    fn the_size_limits_match_the_default_settings() {
        use crate::jobs::Setting;
        assert!(JOB_DDL.contains(&format!(
            "octet_length(payload::text) <= {}",
            Setting::PayloadMaxBytes.bounds().1
        )));
        assert!(JOB_DDL.contains(&format!(
            "octet_length(progress::text) <= {}",
            Setting::ProgressMaxBytes.bounds().1
        )));
    }
}
