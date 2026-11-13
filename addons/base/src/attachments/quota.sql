-- SPDX-License-Identifier: MIT OR Apache-2.0
-- Version 3 is installed only under the lifecycle writer drain. Repeated addon installs
-- validate the catalog without rewriting quota constraints, triggers or accounting.
DO $quota$
DECLARE
    blob_capture constant text := $blob$
BEGIN
    IF TG_OP = 'INSERT' THEN
        INSERT INTO ir_blob_usage_delta(bytes) SELECT size FROM new_blobs WHERE size <> 0;
    ELSE
        INSERT INTO ir_blob_usage_delta(bytes) SELECT -size FROM old_blobs WHERE size <> 0;
    END IF;
    RETURN NULL;
END $blob$;
    upload_capture constant text := $upload$
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
END $upload$;
    installed boolean := false;
    has_version boolean;
BEGIN
    SELECT EXISTS (SELECT 1 FROM pg_attribute WHERE attrelid=to_regclass('public.ir_blob_usage_base')
        AND attname='schema_version' AND NOT attisdropped) INTO has_version;
    IF has_version THEN
        SELECT schema_version=3 INTO installed FROM ir_blob_usage_base WHERE id=1;
        IF EXISTS (SELECT 1 FROM ir_blob_usage_base WHERE schema_version NOT IN (2,3)) THEN
            RAISE EXCEPTION 'Attachment quota schema drift: unsupported version';
        END IF;
    END IF;
    IF NOT installed THEN
        LOCK TABLE ir_blob, ir_blob_upload IN SHARE ROW EXCLUSIVE MODE;
        -- Another installer may have committed while this one waited.
        SELECT EXISTS (SELECT 1 FROM pg_attribute WHERE attrelid=to_regclass('public.ir_blob_usage_base')
            AND attname='schema_version' AND NOT attisdropped) INTO has_version;
        IF has_version THEN
            SELECT schema_version=3 INTO installed FROM ir_blob_usage_base WHERE id=1;
            IF EXISTS (SELECT 1 FROM ir_blob_usage_base WHERE schema_version NOT IN (2,3)) THEN
                RAISE EXCEPTION 'Attachment quota schema drift: unsupported version';
            END IF;
        END IF;
        IF NOT installed THEN
            CREATE TABLE IF NOT EXISTS ir_blob_usage_base (
                id int2 PRIMARY KEY CHECK (id=1),
                bytes numeric(30,0) NOT NULL CHECK (bytes>=0)
            );
            CREATE TABLE IF NOT EXISTS ir_blob_usage_delta (
                id int8 GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
                bytes int8 NOT NULL,
                created_at timestamptz NOT NULL DEFAULT now()
            );
            LOCK TABLE ir_blob_usage_base, ir_blob_usage_delta IN SHARE ROW EXCLUSIVE MODE;
            -- Disable old capture before the one-time correction, then rebuild the base
            -- from authoritative rows. Otherwise a correction could be applied twice.
            DROP TRIGGER IF EXISTS ir_blob_usage_insert ON ir_blob;
            DROP TRIGGER IF EXISTS ir_blob_usage_delete ON ir_blob;
            DROP TRIGGER IF EXISTS ir_blob_upload_usage_insert ON ir_blob_upload;
            DROP TRIGGER IF EXISTS ir_blob_upload_usage_update ON ir_blob_upload;
            DROP TRIGGER IF EXISTS ir_blob_upload_usage_delete ON ir_blob_upload;
            ALTER TABLE ir_blob_upload ADD COLUMN IF NOT EXISTS reserved_bytes int8;
            ALTER TABLE ir_blob_upload ADD COLUMN IF NOT EXISTS cleanup_error text;
            ALTER TABLE ir_blob_upload ADD COLUMN IF NOT EXISTS reservation_unknown boolean NOT NULL DEFAULT false;
            ALTER TABLE ir_blob_upload DROP CONSTRAINT IF EXISTS ir_blob_upload_state_check;
            ALTER TABLE ir_blob_upload ADD CONSTRAINT ir_blob_upload_state_check CHECK (state IN (0,1,2,3));
            -- Legacy receiving rows without a known size are quarantined, never guessed
            -- to occupy the maximum file size or silently admitted as known empty objects.
            UPDATE ir_blob_upload SET reservation_unknown=true, state=3, reserved_bytes=0,
                cleanup_error='Legacy upload size is unknown; fenced cleanup required.'
                WHERE size IS NULL AND inline_data IS NULL;
            UPDATE ir_blob_upload SET reserved_bytes=CASE WHEN state=2 AND inline_data IS NOT NULL THEN 0 ELSE COALESCE(size,0) END
                WHERE NOT reservation_unknown;
            ALTER TABLE ir_blob_upload ALTER COLUMN reserved_bytes SET DEFAULT 0;
            ALTER TABLE ir_blob_upload ALTER COLUMN reserved_bytes SET NOT NULL;
            ALTER TABLE ir_blob_upload DROP CONSTRAINT IF EXISTS ir_blob_upload_reserved_bytes_check;
            ALTER TABLE ir_blob_upload ADD CONSTRAINT ir_blob_upload_reserved_bytes_check CHECK (reserved_bytes>=0);
            ALTER TABLE ir_blob_upload DROP CONSTRAINT IF EXISTS ir_blob_upload_cleanup_error_check;
            ALTER TABLE ir_blob_upload ADD CONSTRAINT ir_blob_upload_cleanup_error_check CHECK (octet_length(cleanup_error)<=1000);
            ALTER TABLE ir_blob_upload DROP CONSTRAINT IF EXISTS ir_blob_upload_reservation_unknown_check;
            ALTER TABLE ir_blob_upload ADD CONSTRAINT ir_blob_upload_reservation_unknown_check
                CHECK (NOT reservation_unknown OR (state=3 AND reserved_bytes=0));
            CREATE INDEX IF NOT EXISTS ir_blob_upload_unknown_reservation ON ir_blob_upload(id) WHERE reservation_unknown;
            TRUNCATE ir_blob_usage_delta;
            INSERT INTO ir_blob_usage_base(id,bytes)
                SELECT 1, COALESCE((SELECT sum(size) FROM ir_blob),0)
                    + COALESCE((SELECT sum(reserved_bytes) FROM ir_blob_upload),0)
                ON CONFLICT(id) DO UPDATE SET bytes=EXCLUDED.bytes;
            EXECUTE format('CREATE OR REPLACE FUNCTION ir_blob_usage_capture() RETURNS trigger LANGUAGE plpgsql AS %L', blob_capture);
            EXECUTE format('CREATE OR REPLACE FUNCTION ir_blob_upload_usage_capture() RETURNS trigger LANGUAGE plpgsql AS %L', upload_capture);
            CREATE TRIGGER ir_blob_usage_insert AFTER INSERT ON ir_blob REFERENCING NEW TABLE AS new_blobs
            FOR EACH STATEMENT EXECUTE FUNCTION ir_blob_usage_capture();
            CREATE TRIGGER ir_blob_usage_delete AFTER DELETE ON ir_blob REFERENCING OLD TABLE AS old_blobs
            FOR EACH STATEMENT EXECUTE FUNCTION ir_blob_usage_capture();

            CREATE TRIGGER ir_blob_upload_usage_insert AFTER INSERT ON ir_blob_upload REFERENCING NEW TABLE AS new_uploads
            FOR EACH STATEMENT EXECUTE FUNCTION ir_blob_upload_usage_capture();
            CREATE TRIGGER ir_blob_upload_usage_update AFTER UPDATE ON ir_blob_upload REFERENCING OLD TABLE AS old_uploads NEW TABLE AS new_uploads
            FOR EACH STATEMENT EXECUTE FUNCTION ir_blob_upload_usage_capture();
            CREATE TRIGGER ir_blob_upload_usage_delete AFTER DELETE ON ir_blob_upload REFERENCING OLD TABLE AS old_uploads
            FOR EACH STATEMENT EXECUTE FUNCTION ir_blob_upload_usage_capture();
            REVOKE ALL ON ir_blob_usage_base, ir_blob_usage_delta FROM PUBLIC;
            ALTER TABLE ir_blob_usage_base ADD COLUMN IF NOT EXISTS schema_version int2 NOT NULL DEFAULT 3;
            ALTER TABLE ir_blob_usage_base DROP CONSTRAINT IF EXISTS ir_blob_usage_base_schema_version_check;
            ALTER TABLE ir_blob_usage_base ALTER COLUMN schema_version SET DEFAULT 3;
            UPDATE ir_blob_usage_base SET schema_version=3;
            ALTER TABLE ir_blob_usage_base ADD CONSTRAINT ir_blob_usage_base_schema_version_check CHECK (schema_version=3);
        END IF;
    END IF;
    IF (SELECT count(*) FROM ir_blob_usage_base WHERE id=1 AND schema_version=3) <> 1 THEN
        RAISE EXCEPTION 'Attachment quota schema drift: version or base row';
    END IF;
    IF (SELECT count(*) FROM pg_attribute WHERE attrelid='public.ir_blob_upload'::regclass
        AND ((attname='reserved_bytes' AND atttypid='int8'::regtype AND attnotnull)
          OR (attname='reservation_unknown' AND atttypid='bool'::regtype AND attnotnull)
          OR (attname='cleanup_error' AND atttypid='text'::regtype)) AND NOT attisdropped) <> 3 THEN
        RAISE EXCEPTION 'Attachment quota schema drift: upload columns';
    END IF;
    IF (SELECT count(*) FROM (VALUES
        ('ir_blob_upload_state_check', '(state = ANY (ARRAY[0, 1, 2, 3]))'),
        ('ir_blob_upload_reserved_bytes_check', '(reserved_bytes >= 0)'),
        ('ir_blob_upload_cleanup_error_check', '(octet_length(cleanup_error) <= 1000)'),
        ('ir_blob_upload_reservation_unknown_check', '((NOT reservation_unknown) OR ((state = 3) AND (reserved_bytes = 0)))')
    ) AS expected(name,expression)
    JOIN pg_constraint c ON c.conrelid='public.ir_blob_upload'::regclass
        AND c.conname=expected.name AND c.contype='c' AND c.convalidated AND NOT c.connoinherit
        AND pg_get_expr(c.conbin,c.conrelid)=expected.expression) <> 4 THEN
        RAISE EXCEPTION 'Attachment quota schema drift: upload constraints';
    END IF;
    IF NOT EXISTS (SELECT 1 FROM pg_proc WHERE oid=to_regprocedure('public.ir_blob_usage_capture()')
        AND prosrc=blob_capture AND prorettype='trigger'::regtype AND prolang=(SELECT oid FROM pg_language WHERE lanname='plpgsql') AND NOT prosecdef AND proconfig IS NULL)
       OR NOT EXISTS (SELECT 1 FROM pg_proc WHERE oid=to_regprocedure('public.ir_blob_upload_usage_capture()')
        AND prosrc=upload_capture AND prorettype='trigger'::regtype AND prolang=(SELECT oid FROM pg_language WHERE lanname='plpgsql') AND NOT prosecdef AND proconfig IS NULL) THEN
        RAISE EXCEPTION 'Attachment quota schema drift: capture functions';
    END IF;
    IF (SELECT count(*) FROM (VALUES
        ('ir_blob_usage_insert','ir_blob','ir_blob_usage_capture()',4,NULL::text,'new_blobs'),
        ('ir_blob_usage_delete','ir_blob','ir_blob_usage_capture()',8,'old_blobs',NULL),
        ('ir_blob_upload_usage_insert','ir_blob_upload','ir_blob_upload_usage_capture()',4,NULL,'new_uploads'),
        ('ir_blob_upload_usage_update','ir_blob_upload','ir_blob_upload_usage_capture()',16,'old_uploads','new_uploads'),
        ('ir_blob_upload_usage_delete','ir_blob_upload','ir_blob_upload_usage_capture()',8,'old_uploads',NULL)
    ) AS expected(name,rel,func,kind,old_table,new_table)
    JOIN pg_trigger t ON t.tgname=expected.name AND t.tgrelid=to_regclass('public.'||expected.rel)
        AND t.tgfoid=to_regprocedure('public.'||expected.func) AND t.tgtype=expected.kind
        AND t.tgoldtable IS NOT DISTINCT FROM expected.old_table
        AND t.tgnewtable IS NOT DISTINCT FROM expected.new_table
        AND t.tgenabled='O' AND NOT t.tgisinternal AND t.tgqual IS NULL AND t.tgnargs=0) <> 5 THEN
        RAISE EXCEPTION 'Attachment quota schema drift: capture triggers';
    END IF;
END
$quota$;
