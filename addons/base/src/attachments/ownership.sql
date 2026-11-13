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
