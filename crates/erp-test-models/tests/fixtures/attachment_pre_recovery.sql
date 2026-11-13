-- SPDX-License-Identifier: MIT OR Apache-2.0
-- Exact infrastructure DDL at 9a5ce2ef224fad8da786ec8bc23e10a60e2ac362; capture script in recovery evidence.
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
REVOKE ALL ON ir_blob_store, ir_blob, ir_blob_inline, ir_blob_upload, ir_blob_gc_candidate, ir_blob_gc_hold FROM PUBLIC;
-- SPDX-License-Identifier: MIT OR Apache-2.0
-- The authorized lifecycle must drain legacy writers before opening its upgrade snapshot.
-- Locks then exclude new writes while one-time backfill and capture installation commit together.
LOCK TABLE ir_blob, ir_blob_upload IN SHARE ROW EXCLUSIVE MODE;
ALTER TABLE ir_blob_upload ADD COLUMN IF NOT EXISTS reserved_bytes int8;
UPDATE ir_blob_upload SET reserved_bytes = COALESCE(size, 2147483647) WHERE reserved_bytes IS NULL;
ALTER TABLE ir_blob_upload ALTER COLUMN reserved_bytes SET DEFAULT 0;
ALTER TABLE ir_blob_upload ALTER COLUMN reserved_bytes SET NOT NULL;
ALTER TABLE ir_blob_upload DROP CONSTRAINT IF EXISTS ir_blob_upload_reserved_bytes_check;
ALTER TABLE ir_blob_upload ADD CONSTRAINT ir_blob_upload_reserved_bytes_check CHECK (reserved_bytes >= 0);
ALTER TABLE ir_blob_upload DROP CONSTRAINT IF EXISTS ir_blob_upload_state_check;
ALTER TABLE ir_blob_upload ADD CONSTRAINT ir_blob_upload_state_check CHECK (state IN (0,1,2,3));
ALTER TABLE ir_blob_upload ADD COLUMN IF NOT EXISTS cleanup_error text;
ALTER TABLE ir_blob_upload DROP CONSTRAINT IF EXISTS ir_blob_upload_cleanup_error_check;
ALTER TABLE ir_blob_upload ADD CONSTRAINT ir_blob_upload_cleanup_error_check CHECK (octet_length(cleanup_error) <= 1000);

CREATE TABLE IF NOT EXISTS ir_blob_usage_base (
    id int2 PRIMARY KEY CHECK (id = 1),
    bytes numeric(30,0) NOT NULL CHECK (bytes >= 0)
);
CREATE TABLE IF NOT EXISTS ir_blob_usage_delta (
    id int8 GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    bytes int8 NOT NULL,
    created_at timestamptz NOT NULL DEFAULT now()
);
-- This primary key is also the bounded compaction cursor. Allocation order is never a visibility watermark.
INSERT INTO ir_blob_usage_base(id, bytes)
SELECT 1, COALESCE((SELECT sum(size) FROM ir_blob),0) + COALESCE((SELECT sum(reserved_bytes) FROM ir_blob_upload),0)
ON CONFLICT (id) DO NOTHING;
REVOKE ALL ON ir_blob_usage_base, ir_blob_usage_delta FROM PUBLIC;

CREATE OR REPLACE FUNCTION ir_blob_usage_capture() RETURNS trigger LANGUAGE plpgsql AS $fn$
BEGIN
    IF TG_OP = 'INSERT' THEN
        INSERT INTO ir_blob_usage_delta(bytes) SELECT size FROM new_blobs WHERE size <> 0;
    ELSE
        INSERT INTO ir_blob_usage_delta(bytes) SELECT -size FROM old_blobs WHERE size <> 0;
    END IF;
    RETURN NULL;
END $fn$;
DROP TRIGGER IF EXISTS ir_blob_usage_insert ON ir_blob;
CREATE TRIGGER ir_blob_usage_insert AFTER INSERT ON ir_blob REFERENCING NEW TABLE AS new_blobs
FOR EACH STATEMENT EXECUTE FUNCTION ir_blob_usage_capture();
DROP TRIGGER IF EXISTS ir_blob_usage_delete ON ir_blob;
CREATE TRIGGER ir_blob_usage_delete AFTER DELETE ON ir_blob REFERENCING OLD TABLE AS old_blobs
FOR EACH STATEMENT EXECUTE FUNCTION ir_blob_usage_capture();

CREATE OR REPLACE FUNCTION ir_blob_upload_usage_capture() RETURNS trigger LANGUAGE plpgsql AS $fn$
BEGIN
    IF TG_OP = 'INSERT' THEN
        INSERT INTO ir_blob_usage_delta(bytes) SELECT reserved_bytes FROM new_uploads WHERE reserved_bytes <> 0;
    ELSIF TG_OP = 'DELETE' THEN
        INSERT INTO ir_blob_usage_delta(bytes) SELECT -reserved_bytes FROM old_uploads WHERE reserved_bytes <> 0;
    ELSE
        INSERT INTO ir_blob_usage_delta(bytes)
        SELECT n.reserved_bytes-o.reserved_bytes FROM new_uploads n JOIN old_uploads o USING(id)
        WHERE n.reserved_bytes <> o.reserved_bytes;
    END IF;
    RETURN NULL;
END $fn$;
DROP TRIGGER IF EXISTS ir_blob_upload_usage_insert ON ir_blob_upload;
CREATE TRIGGER ir_blob_upload_usage_insert AFTER INSERT ON ir_blob_upload REFERENCING NEW TABLE AS new_uploads
FOR EACH STATEMENT EXECUTE FUNCTION ir_blob_upload_usage_capture();
DROP TRIGGER IF EXISTS ir_blob_upload_usage_update ON ir_blob_upload;
CREATE TRIGGER ir_blob_upload_usage_update AFTER UPDATE ON ir_blob_upload REFERENCING OLD TABLE AS old_uploads NEW TABLE AS new_uploads
FOR EACH STATEMENT EXECUTE FUNCTION ir_blob_upload_usage_capture();
DROP TRIGGER IF EXISTS ir_blob_upload_usage_delete ON ir_blob_upload;
CREATE TRIGGER ir_blob_upload_usage_delete AFTER DELETE ON ir_blob_upload REFERENCING OLD TABLE AS old_uploads
FOR EACH STATEMENT EXECUTE FUNCTION ir_blob_upload_usage_capture();
-- SPDX-License-Identifier: MIT OR Apache-2.0
-- Additive infrastructure only. Installation never invents ownership or successful heartbeats.
CREATE TABLE IF NOT EXISTS ir_blob_owner_identity (
    singleton boolean PRIMARY KEY DEFAULT true CHECK (singleton),
    system_identifier numeric(20,0) NOT NULL CHECK (system_identifier BETWEEN 1 AND 18446744073709551615),
    database_oid int8 NOT NULL CHECK (database_oid BETWEEN 1 AND 4294967295),
    incarnation uuid NOT NULL CHECK (incarnation <> '00000000-0000-0000-0000-000000000000')
);
CREATE TABLE IF NOT EXISTS ir_blob_owner_state (
    store_id int2 PRIMARY KEY REFERENCES ir_blob_store(id) ON DELETE RESTRICT,
    namespace uuid NOT NULL,
    fingerprint text NOT NULL CHECK (octet_length(fingerprint) BETWEEN 1 AND 128),
    mode text NOT NULL DEFAULT 'uninitialized' CHECK (mode IN ('uninitialized','recovering','healthy','unsupported','contest_pending','contested')),
    confirmed_marker bytea CHECK (octet_length(confirmed_marker) = 108),
    confirmed_etag text CHECK (octet_length(confirmed_etag) BETWEEN 1 AND 1024 AND confirmed_etag !~ '[^!-~]' AND confirmed_etag <> '*' AND confirmed_etag NOT LIKE 'W/%' AND position(',' IN confirmed_etag) = 0),
    confirmed_version text CHECK (octet_length(confirmed_version) BETWEEN 1 AND 1024 AND confirmed_version !~ '[^!-~]'),
    pending_marker bytea CHECK (octet_length(pending_marker) = 108),
    pending_etag text CHECK (octet_length(pending_etag) BETWEEN 1 AND 1024 AND pending_etag !~ '[^!-~]' AND pending_etag <> '*' AND pending_etag NOT LIKE 'W/%' AND position(',' IN pending_etag) = 0),
    pending_version text CHECK (octet_length(pending_version) BETWEEN 1 AND 1024 AND pending_version !~ '[^!-~]'),
    pending_purpose text CHECK (pending_purpose IN ('heartbeat','observe')),
    last_success_at timestamptz,
    failure_code text CHECK (octet_length(failure_code) BETWEEN 1 AND 64),
    probe_protocol int2 CHECK (probe_protocol = 1),
    observation_id uuid,
    observation_started_at timestamptz,
    observation_not_before timestamptz,
    CHECK ((confirmed_marker IS NULL) = (confirmed_etag IS NULL)),
    CHECK (confirmed_version IS NULL OR confirmed_etag IS NOT NULL),
    CHECK ((confirmed_marker IS NULL) = (last_success_at IS NULL)),
    CHECK ((pending_marker IS NULL) = (pending_purpose IS NULL)),
    CHECK (pending_etag IS NOT DISTINCT FROM CASE WHEN pending_marker IS NOT NULL THEN confirmed_etag END),
    CHECK (pending_version IS NOT DISTINCT FROM CASE WHEN pending_marker IS NOT NULL THEN confirmed_version END),
    CONSTRAINT ir_blob_owner_pending_check CHECK (mode IN ('uninitialized','healthy') OR pending_marker IS NOT NULL),
    CONSTRAINT ir_blob_owner_uninitialized_check CHECK (mode <> 'uninitialized' OR (confirmed_marker IS NULL AND pending_marker IS NULL)),
    CONSTRAINT ir_blob_owner_probe_check CHECK ((confirmed_marker IS NULL) = (probe_protocol IS NULL)),
    CHECK (mode <> 'unsupported' OR confirmed_marker IS NULL),
    CHECK (mode <> 'healthy' OR (pending_marker IS NULL AND confirmed_marker IS NOT NULL AND probe_protocol = 1)),
    CHECK ((observation_id IS NULL) = (observation_started_at IS NULL)),
    CHECK ((observation_id IS NULL) = (observation_not_before IS NULL)),
    CHECK (observation_not_before >= observation_started_at + interval '660 seconds')
);
REVOKE ALL ON ir_blob_owner_identity, ir_blob_owner_state FROM PUBLIC;
