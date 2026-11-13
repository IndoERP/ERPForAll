-- SPDX-License-Identifier: MIT OR Apache-2.0
-- Pre-quota schema captured from the task base, without quota columns or capture.
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
