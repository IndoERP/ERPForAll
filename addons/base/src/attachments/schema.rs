//! The tables of the attachment store, registered as core DDL of the base
//! addon: `ir_blob_store`, `ir_blob`, `ir_blob_inline`, `ir_blob_upload`, `ir_blob_gc_candidate`,
//! `ir_blob_gc_hold`, and, after the models, `ir_attachment.blob_id` with its statement triggers.
//!
//! `blob_id` is deliberately not an ORM field: it would show which records share content. The
//! reference is the only one, guarded by a foreign key with `ON DELETE RESTRICT`, so no row
//! removal can orphan a referenced blob whatever role runs it. `CREATE TABLE IF NOT EXISTS` never
//! alters an existing table (no deployed database had these tables when they were added).

use erp_sdk::__private::{CoreTableRequirement, core_ddl};

/// The tables created before the models.
pub const BLOB_DDL: &str = concat!("\
CREATE TABLE IF NOT EXISTS ir_blob_store (
    id int2 GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    store_key text NOT NULL CHECK (octet_length(store_key) BETWEEN 1 AND 64),
    namespace uuid NOT NULL DEFAULT gen_random_uuid(),
    role text NOT NULL DEFAULT 'own' CHECK (role IN ('own', 'borrowed')),
    fingerprint text NOT NULL CHECK (octet_length(fingerprint) BETWEEN 1 AND 128),
    created_at timestamptz NOT NULL DEFAULT now(),
    CONSTRAINT ir_blob_store_namespace_key UNIQUE (store_key, namespace)
);
CREATE UNIQUE INDEX IF NOT EXISTS ir_blob_store_own ON ir_blob_store (store_key) WHERE role = 'own';

CREATE TABLE IF NOT EXISTS ir_blob (
    id int8 GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    sha256 bytea NOT NULL CHECK (octet_length(sha256) = 32),
    size int8 NOT NULL CHECK (size >= 0),
    store_id int2 REFERENCES ir_blob_store (id),
    state int2 NOT NULL DEFAULT 0 CHECK (state IN (0, 1)),
    deleting_since timestamptz,
    sweep_attempted_at timestamptz,
    detected_mimetype text CHECK (octet_length(detected_mimetype) <= 127),
    enc_kid int2,
    scan_state int2 NOT NULL DEFAULT 0,
    created_at timestamptz NOT NULL DEFAULT now(),
    CONSTRAINT ir_blob_sha256_key UNIQUE (sha256),
    CONSTRAINT ir_blob_deleting_since CHECK ((state = 1) = (deleting_since IS NOT NULL))
);
CREATE INDEX IF NOT EXISTS ir_blob_deleting ON ir_blob (deleting_since) WHERE state = 1;

CREATE TABLE IF NOT EXISTS ir_blob_inline (
    blob_id int8 PRIMARY KEY REFERENCES ir_blob (id) ON DELETE CASCADE,
    data bytea NOT NULL
);
ALTER TABLE ir_blob_inline ALTER COLUMN data SET STORAGE MAIN;
ALTER TABLE ir_blob_inline SET (toast_tuple_target = 8160);

CREATE TABLE IF NOT EXISTS ir_blob_upload (
    id uuid PRIMARY KEY DEFAULT uuidv7(),
    state int2 NOT NULL DEFAULT 0 CHECK (state IN (0, 1, 2)),
    store_id int2 REFERENCES ir_blob_store (id),
    subject_user int8 NOT NULL,
    actor text NOT NULL DEFAULT '' CHECK (octet_length(actor) <= 64),
    expires_at timestamptz NOT NULL,
    sha256 bytea CHECK (sha256 IS NULL OR octet_length(sha256) = 32),
    sha1 bytea CHECK (sha1 IS NULL OR octet_length(sha1) = 20),
    size int8 CHECK (size IS NULL OR size >= 0),
    detected_mimetype text CHECK (octet_length(detected_mimetype) <= 127),
    declared_name text CHECK (octet_length(declared_name) <= 1024),
    declared_mimetype text CHECK (octet_length(declared_mimetype) <= 127),
    inline_data bytea,
    created_at timestamptz NOT NULL DEFAULT now()
) WITH (fillfactor = 70);
CREATE INDEX IF NOT EXISTS ir_blob_upload_expires ON ir_blob_upload (expires_at);
CREATE INDEX IF NOT EXISTS ir_blob_upload_subject ON ir_blob_upload (subject_user, state);

CREATE TABLE IF NOT EXISTS ir_blob_gc_candidate (
    id int8 GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    blob_id int8 NOT NULL,
    enqueued_at timestamptz NOT NULL DEFAULT now()
) WITH (autovacuum_vacuum_scale_factor = 0, autovacuum_vacuum_threshold = 1000);

CREATE TABLE IF NOT EXISTS ir_blob_gc_hold (
    id int8 GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    until timestamptz NOT NULL,
    reason text NOT NULL CHECK (octet_length(reason) BETWEEN 1 AND 500),
    created_at timestamptz NOT NULL DEFAULT now()
);
REVOKE ALL ON ir_blob_store, ir_blob, ir_blob_inline, ir_blob_upload, ir_blob_gc_candidate, ir_blob_gc_hold FROM PUBLIC;", include_str!("quota.sql"), include_str!("ownership.sql"), include_str!("ownership_recovery.sql"), include_str!("peer_probe.sql"));

/// The part that refers to `ir_attachment`, run after the models' tables.
pub const ATTACHMENT_LINK_DDL: &str = "\
CREATE TABLE IF NOT EXISTS ir_attachment_pending_guard (
    subject_user int8 NOT NULL REFERENCES res_users(id) ON DELETE CASCADE,
    placeholder_model text NOT NULL CHECK (octet_length(placeholder_model) BETWEEN 1 AND 128),
    generation boolean NOT NULL DEFAULT false,
    PRIMARY KEY(subject_user, placeholder_model)
);
REVOKE ALL ON ir_attachment_pending_guard FROM PUBLIC;
ALTER TABLE ir_attachment ADD COLUMN IF NOT EXISTS blob_id int8 REFERENCES ir_blob (id) ON DELETE RESTRICT;
CREATE INDEX IF NOT EXISTS ir_attachment_blob_id ON ir_attachment (blob_id) WHERE blob_id IS NOT NULL;

CREATE OR REPLACE FUNCTION ir_attachment_enqueue_gc_delete() RETURNS trigger LANGUAGE plpgsql AS $fn$
BEGIN
    INSERT INTO ir_blob_gc_candidate (blob_id)
    SELECT DISTINCT blob_id FROM old_rows WHERE blob_id IS NOT NULL;
    RETURN NULL;
END $fn$;

CREATE OR REPLACE FUNCTION ir_attachment_enqueue_gc_update() RETURNS trigger LANGUAGE plpgsql AS $fn$
BEGIN
    INSERT INTO ir_blob_gc_candidate (blob_id)
    SELECT DISTINCT o.blob_id FROM old_rows o JOIN new_rows n ON n.id = o.id
    WHERE o.blob_id IS NOT NULL AND o.blob_id IS DISTINCT FROM n.blob_id;
    RETURN NULL;
END $fn$;

CREATE OR REPLACE FUNCTION ir_attachment_enqueue_gc_truncate() RETURNS trigger LANGUAGE plpgsql AS $fn$
BEGIN
    INSERT INTO ir_blob_gc_candidate (blob_id) SELECT id FROM ir_blob WHERE state = 0;
    RETURN NULL;
END $fn$;

DROP TRIGGER IF EXISTS ir_attachment_gc_delete ON ir_attachment;
CREATE TRIGGER ir_attachment_gc_delete AFTER DELETE ON ir_attachment
    REFERENCING OLD TABLE AS old_rows FOR EACH STATEMENT
    EXECUTE FUNCTION ir_attachment_enqueue_gc_delete();
DROP TRIGGER IF EXISTS ir_attachment_gc_update ON ir_attachment;
CREATE TRIGGER ir_attachment_gc_update AFTER UPDATE ON ir_attachment
    REFERENCING OLD TABLE AS old_rows NEW TABLE AS new_rows FOR EACH STATEMENT
    EXECUTE FUNCTION ir_attachment_enqueue_gc_update();
DROP TRIGGER IF EXISTS ir_attachment_gc_truncate ON ir_attachment;
CREATE TRIGGER ir_attachment_gc_truncate AFTER TRUNCATE ON ir_attachment
    FOR EACH STATEMENT EXECUTE FUNCTION ir_attachment_enqueue_gc_truncate();";

core_ddl!(
    ATTACHMENT_TABLES,
    owner = "base",
    name = "ir_blob",
    purpose = "the attachment blob store: blobs, uploads and the garbage-collection queue",
    tables = [
        "ir_blob_store",
        "ir_blob",
        "ir_blob_inline",
        "ir_blob_upload",
        "ir_blob_gc_candidate",
        "ir_blob_gc_hold",
        "ir_blob_usage_base",
        "ir_blob_usage_delta",
        "ir_blob_owner_identity",
        "ir_blob_owner_state",
        "ir_blob_peer_probe",
        "ir_blob_owner_claim",
        "ir_blob_owner_recovery",
        "ir_attachment_pending_guard"
    ],
    sql = BLOB_DDL,
    after_models = { needs_table = "ir_attachment", sql = ATTACHMENT_LINK_DDL }
);

pub(crate) const REQUIREMENT: CoreTableRequirement = CoreTableRequirement::new("ir_blob", "base");

pub(crate) fn keep_registration() {
    std::hint::black_box(&ATTACHMENT_TABLES);
}
