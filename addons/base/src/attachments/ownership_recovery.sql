-- SPDX-License-Identifier: MIT OR Apache-2.0
-- No ownership is created or admitted by installation.
CREATE TABLE IF NOT EXISTS ir_blob_owner_claim (
    id int8 GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    operation uuid NOT NULL UNIQUE,
    store_id int2 NOT NULL REFERENCES ir_blob_store(id) ON DELETE RESTRICT,
    namespace uuid NOT NULL,
    fingerprint text NOT NULL CHECK (octet_length(fingerprint) BETWEEN 1 AND 128),
    chosen bytea NOT NULL CHECK (octet_length(chosen)=108),
    previous_coordinator jsonb NOT NULL CHECK (octet_length(previous_coordinator::text)<=16384),
    prior_owner bytea CHECK (octet_length(prior_owner)=108),
    prior_etag text CHECK (octet_length(prior_etag) BETWEEN 1 AND 1024),
    prior_version text CHECK (octet_length(prior_version) BETWEEN 1 AND 1024),
    contest bytea CHECK (octet_length(contest) IN (108,116)),
    contest_etag text CHECK (octet_length(contest_etag) BETWEEN 1 AND 1024),
    contest_version text CHECK (octet_length(contest_version) BETWEEN 1 AND 1024),
    reason text NOT NULL CHECK (reason IN ('restore_primary','retire_clone')),
    operator_role name NOT NULL DEFAULT current_user,
    decided_at timestamptz NOT NULL DEFAULT clock_timestamp(),
    CHECK ((prior_owner IS NULL)=(prior_etag IS NULL)),
    CHECK (prior_version IS NULL OR prior_etag IS NOT NULL),
    CHECK ((contest IS NULL)=(contest_etag IS NULL)),
    CHECK (contest_version IS NULL OR contest_etag IS NOT NULL)
);
CREATE TABLE IF NOT EXISTS ir_blob_owner_recovery (
    claim_id int8 PRIMARY KEY REFERENCES ir_blob_owner_claim(id) ON DELETE RESTRICT,
    store_id int2 NOT NULL REFERENCES ir_blob_store(id) ON DELETE RESTRICT,
    phase text NOT NULL DEFAULT 'decision' CHECK (phase IN ('decision','owner','resolution','completed')),
    owner_etag text CHECK (octet_length(owner_etag) BETWEEN 1 AND 1024),
    owner_version text CHECK (octet_length(owner_version) BETWEEN 1 AND 1024),
    resolution_etag text CHECK (octet_length(resolution_etag) BETWEEN 1 AND 1024),
    resolution_version text CHECK (octet_length(resolution_version) BETWEEN 1 AND 1024),
    updated_at timestamptz NOT NULL DEFAULT clock_timestamp(),
    CHECK ((phase='decision')=(owner_etag IS NULL)),
    CHECK (owner_version IS NULL OR owner_etag IS NOT NULL),
    CHECK ((phase='completed')=(resolution_etag IS NOT NULL)),
    CHECK (resolution_version IS NULL OR resolution_etag IS NOT NULL)
);
CREATE UNIQUE INDEX IF NOT EXISTS ir_blob_owner_recovery_active ON ir_blob_owner_recovery(store_id) WHERE phase<>'completed';
CREATE OR REPLACE FUNCTION ir_blob_owner_claim_immutable() RETURNS trigger LANGUAGE plpgsql AS $fn$
BEGIN
    RAISE EXCEPTION 'Attachment ownership decisions are immutable' USING ERRCODE='42501';
END $fn$;
DROP TRIGGER IF EXISTS ir_blob_owner_claim_immutable ON ir_blob_owner_claim;
CREATE TRIGGER ir_blob_owner_claim_immutable BEFORE UPDATE OR DELETE OR TRUNCATE ON ir_blob_owner_claim
FOR EACH STATEMENT EXECUTE FUNCTION ir_blob_owner_claim_immutable();
REVOKE ALL ON ir_blob_owner_claim,ir_blob_owner_recovery FROM PUBLIC;
REVOKE ALL ON FUNCTION ir_blob_owner_claim_immutable() FROM PUBLIC;
