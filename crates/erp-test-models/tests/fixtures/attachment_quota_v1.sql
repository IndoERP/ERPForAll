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
