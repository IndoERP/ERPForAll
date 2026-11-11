-- Durable feature admission. No feature is enabled by schema synchronization.
-- ADD COLUMN IF NOT EXISTS still takes AccessExclusiveLock when the column already exists.
-- Converged schema sync must not lock live membership tables.
DO $$ BEGIN
    IF NOT EXISTS (SELECT FROM pg_catalog.pg_attribute
                    WHERE attrelid = 'public.erp_node'::regclass AND attname = 'capabilities'
                      AND attnum > 0 AND NOT attisdropped) THEN
        ALTER TABLE public.erp_node ADD COLUMN IF NOT EXISTS capabilities jsonb NOT NULL DEFAULT '{}';
    END IF;
END $$;
DO $$ BEGIN
    IF NOT EXISTS (SELECT FROM pg_constraint
                    WHERE conrelid = 'public.erp_node'::regclass
                      AND conname = 'erp_node_capabilities_bound') THEN
        ALTER TABLE public.erp_node ADD CONSTRAINT erp_node_capabilities_bound
            CHECK (jsonb_typeof(capabilities) = 'object' AND octet_length(capabilities::text) <= 1024);
    END IF;
END $$;
CREATE TABLE IF NOT EXISTS public.erp_node_feature (
    key text PRIMARY KEY CHECK (key ~ '^[a-z][a-z0-9_.-]{0,62}$'),
    version int4 NOT NULL CHECK (version > 0),
    enabled boolean NOT NULL DEFAULT false,
    generation int8 NOT NULL DEFAULT 0 CHECK (generation >= 0),
    enrollment_confirmed boolean NOT NULL DEFAULT false,
    enrollment_actor name,
    enrollment_at timestamptz,
    CHECK (NOT enabled OR enrollment_confirmed),
    CHECK (enrollment_confirmed = (enrollment_actor IS NOT NULL AND enrollment_at IS NOT NULL))
);
CREATE TABLE IF NOT EXISTS public.erp_node_feature_digest (
    feature text NOT NULL REFERENCES public.erp_node_feature(key),
    digest bytea NOT NULL CHECK (octet_length(digest) = 32),
    version int4 NOT NULL CHECK (version > 0),
    PRIMARY KEY (feature, digest)
);
DO $$ BEGIN
    IF EXISTS (SELECT FROM pg_class c,
        aclexplode(coalesce(c.relacl, acldefault('r', c.relowner))) a
        WHERE c.oid IN ('public.erp_node_feature'::regclass, 'public.erp_node_feature_digest'::regclass)
          AND a.grantee = 0) THEN
        REVOKE ALL ON public.erp_node_feature, public.erp_node_feature_digest FROM PUBLIC;
    END IF;
END $$;
-- Bootstrap is SECURITY INVOKER with no PUBLIC execution. Operator publication must assign
-- the NOLOGIN maintenance owner before granting runtime execution/activating the feature.
DO $install$
BEGIN
    IF pg_catalog.to_regprocedure('public.erp_node_feature_snapshot(text)') IS NULL THEN
        EXECUTE $definition$
CREATE FUNCTION public.erp_node_feature_snapshot(p_key text)
RETURNS TABLE(required_version int4, enabled boolean, generation int8)
LANGUAGE plpgsql STABLE SECURITY INVOKER SET search_path = pg_catalog, public
AS $erp_fn$
DECLARE f public.erp_node_feature%ROWTYPE;
BEGIN
    IF p_key IS NULL OR pg_catalog.octet_length(p_key) NOT BETWEEN 1 AND 63
       OR p_key !~ '^[a-z][a-z0-9_.-]{0,62}$' THEN
        RAISE EXCEPTION USING ERRCODE = 'ERF01', MESSAGE = 'invalid feature key';
    END IF;
    SELECT s.* INTO f FROM public.erp_node_feature AS s WHERE s.key = p_key;
    IF NOT FOUND THEN
        RAISE EXCEPTION USING ERRCODE = 'ERF01', MESSAGE = 'feature ' || p_key || ' state is missing';
    END IF;
    IF f.version <= 0 OR f.generation < 0
       OR (f.enabled AND NOT f.enrollment_confirmed)
       OR f.enrollment_confirmed <> (f.enrollment_actor IS NOT NULL AND f.enrollment_at IS NOT NULL) THEN
        RAISE EXCEPTION USING ERRCODE = 'ERF01', MESSAGE = 'feature ' || p_key || ' state is malformed';
    END IF;
    RETURN QUERY SELECT f.version, f.enabled, f.generation;
END
$erp_fn$;
$definition$;
    END IF;
    IF EXISTS (SELECT FROM pg_proc p,
        aclexplode(coalesce(p.proacl, acldefault('f', p.proowner))) a
        WHERE p.oid = 'public.erp_node_feature_snapshot(text)'::regprocedure AND a.grantee = 0) THEN
        REVOKE ALL ON FUNCTION public.erp_node_feature_snapshot(text) FROM PUBLIC;
    END IF;
END
$install$;
DO $install$
BEGIN
    IF pg_catalog.to_regprocedure('public.erp_node_feature_protect(text,integer,uuid,bytea,bigint,boolean)') IS NULL THEN
        EXECUTE $definition$
CREATE FUNCTION public.erp_node_feature_protect(
    p_key text, p_version int4, p_node uuid, p_digest bytea,
    p_generation int8, p_enabled boolean)
RETURNS TABLE(required_version int4, enabled boolean, generation int8)
LANGUAGE plpgsql VOLATILE SECURITY INVOKER SET search_path = pg_catalog, public
AS $erp_fn$
DECLARE f public.erp_node_feature%ROWTYPE;
BEGIN
    IF p_key IS NULL OR pg_catalog.octet_length(p_key) NOT BETWEEN 1 AND 63
       OR p_key !~ '^[a-z][a-z0-9_.-]{0,62}$'
       OR p_version IS NULL OR p_version <= 0 OR p_generation IS NULL
       OR p_generation < 0 OR p_enabled IS NULL THEN
        RAISE EXCEPTION USING ERRCODE = 'ERF01', MESSAGE = 'invalid feature protection arguments';
    END IF;
    PERFORM 1 FROM public.erp_node_feature s WHERE s.key = p_key FOR SHARE;
    IF NOT FOUND THEN
        RAISE EXCEPTION USING ERRCODE = 'ERF01', MESSAGE = 'feature ' || p_key || ' state is missing';
    END IF;
    -- Separate command after waiting: READ COMMITTED gets current state. A stale REPEATABLE
    -- READ row-lock attempt must serialize/refuse rather than accepting an obsolete row.
    SELECT s.* INTO f FROM public.erp_node_feature s WHERE s.key = p_key;
    IF f.version <> p_version THEN
        RAISE EXCEPTION USING ERRCODE = 'ERF01', MESSAGE = 'feature capability version does not match';
    END IF;
    IF f.generation <> p_generation OR f.enabled <> p_enabled THEN
        RAISE EXCEPTION USING ERRCODE = 'ERF01', MESSAGE = 'login selection rollout generation changed';
    END IF;
    IF p_node IS NULL AND p_digest IS NULL THEN
        IF f.enabled THEN
            RAISE EXCEPTION USING ERRCODE = 'ERF01', MESSAGE = 'custom login selection requires an active membership-bound rollout';
        END IF;
    ELSE
        IF p_node IS NULL OR p_digest IS NULL OR pg_catalog.octet_length(p_digest) <> 32 THEN
            RAISE EXCEPTION USING ERRCODE = 'ERF01', MESSAGE = 'login selection membership does not match this authentication service';
        END IF;
        IF NOT EXISTS (SELECT FROM public.erp_node n
                        WHERE n.id = p_node AND n.digest = p_digest AND n.state = 'ready'
                          AND n.expires_at > pg_catalog.clock_timestamp()) THEN
            RAISE EXCEPTION USING ERRCODE = 'ERF01', MESSAGE = 'login selection membership is not serving';
        END IF;
        IF f.enabled AND NOT EXISTS (
            SELECT FROM public.erp_node_feature_digest d JOIN public.erp_node n ON n.id = p_node
             WHERE d.feature = p_key AND d.digest = p_digest AND d.version = p_version
               AND n.capabilities -> p_key = pg_catalog.to_jsonb(p_version)) THEN
            RAISE EXCEPTION USING ERRCODE = 'ERF01', MESSAGE = 'login selection membership does not match this authentication service';
        END IF;
    END IF;
    RETURN QUERY SELECT f.version, f.enabled, f.generation;
END
$erp_fn$;
$definition$;
    END IF;
    IF EXISTS (SELECT FROM pg_proc p,
        aclexplode(coalesce(p.proacl, acldefault('f', p.proowner))) a
        WHERE p.oid = 'public.erp_node_feature_protect(text,integer,uuid,bytea,bigint,boolean)'::regprocedure AND a.grantee = 0) THEN
        REVOKE ALL ON FUNCTION public.erp_node_feature_protect(text,integer,uuid,bytea,bigint,boolean) FROM PUBLIC;
    END IF;
END
$install$;
DO $install$
BEGIN
    IF pg_catalog.to_regprocedure('public.erp_node_feature_guard()') IS NULL THEN
        EXECUTE $definition$
CREATE FUNCTION public.erp_node_feature_guard()
RETURNS trigger LANGUAGE plpgsql VOLATILE SECURITY INVOKER SET search_path = pg_catalog, public
AS $erp_fn$
DECLARE f record; feature_key text; declaration record; declarations int4 := 0;
BEGIN
    -- A new feature key is a phantom for an older RR snapshot. Refuse that isolation rather
    -- than mistaking its invisible registry entry for disabled. Compatible memberships use RC.
    IF pg_catalog.current_setting('transaction_isolation') <> 'read committed' THEN
        RAISE EXCEPTION USING ERRCODE = 'ERF01', MESSAGE = 'node feature membership writes require READ COMMITTED';
    END IF;
    IF NEW.capabilities IS NULL OR pg_catalog.jsonb_typeof(NEW.capabilities) <> 'object'
       OR pg_catalog.pg_column_size(NEW.capabilities) > 1024 THEN
        RAISE EXCEPTION USING ERRCODE = 'ERF01', MESSAGE = 'invalid bounded node capability map';
    END IF;
    FOR declaration IN SELECT * FROM pg_catalog.jsonb_each(NEW.capabilities) LOOP
        declarations := declarations + 1;
        IF declarations > 8 OR declaration.key !~ '^[a-z][a-z0-9_.-]{0,62}$'
           OR pg_catalog.jsonb_typeof(declaration.value) <> 'number' THEN
            RAISE EXCEPTION USING ERRCODE = 'ERF01', MESSAGE = 'invalid bounded node capability map';
        END IF;
        IF declaration.value::text !~ '^[1-9][0-9]{0,9}$'
           OR (declaration.value::text)::numeric > 2147483647 THEN
            RAISE EXCEPTION USING ERRCODE = 'ERF01', MESSAGE = 'invalid node capability version';
        END IF;
    END LOOP;
    IF TG_OP = 'UPDATE' THEN
        IF NEW.id <> OLD.id OR NEW.digest <> OLD.digest OR NEW.capabilities <> OLD.capabilities THEN
            RAISE EXCEPTION USING ERRCODE = 'ERF01', MESSAGE = 'node feature identity is immutable within a membership';
        END IF;
        -- Safe departure never extends an unsupported node's lease or restores its readiness.
        IF NEW.state IN ('stopping', 'stopped') AND NEW.expires_at <= OLD.expires_at THEN
            RETURN NEW;
        END IF;
    END IF;
    PERFORM pg_catalog.pg_advisory_xact_lock_shared(1163022406, 6);
    IF (SELECT count(*) FROM public.erp_node_feature) > 8 THEN
        RAISE EXCEPTION USING ERRCODE = 'ERF01', MESSAGE = 'node feature registry exceeds 8 keys';
    END IF;
    FOR feature_key IN SELECT s.key FROM public.erp_node_feature s ORDER BY s.key LOOP
        PERFORM 1 FROM public.erp_node_feature s WHERE s.key = feature_key FOR SHARE;
        IF NOT FOUND THEN
            RAISE EXCEPTION USING ERRCODE = 'ERF01', MESSAGE = 'node feature state disappeared';
        END IF;
        -- A distinct post-lock command gives RC a fresh view after a waiting activation.
        SELECT s.* INTO f FROM public.erp_node_feature s WHERE s.key = feature_key;
        IF f.version IS NULL OR f.version <= 0 OR f.enabled IS NULL OR f.generation IS NULL
           OR f.generation < 0 OR f.enrollment_confirmed IS NULL
           OR (f.enabled AND NOT f.enrollment_confirmed) THEN
            RAISE EXCEPTION USING ERRCODE = 'ERF01', MESSAGE = 'node feature registry state is malformed';
        END IF;
        IF f.enabled AND NOT EXISTS (
            SELECT FROM public.erp_node_feature_digest d
             WHERE d.feature = feature_key AND d.digest = NEW.digest AND d.version = f.version
               AND NEW.capabilities -> feature_key = pg_catalog.to_jsonb(f.version)) THEN
            RAISE EXCEPTION USING ERRCODE = 'ERF01',
                MESSAGE = 'node feature ' || feature_key || ' refuses unapproved digest';
        END IF;
    END LOOP;
    RETURN NEW;
END
$erp_fn$;
$definition$;
    END IF;
    IF EXISTS (SELECT FROM pg_proc p,
        aclexplode(coalesce(p.proacl, acldefault('f', p.proowner))) a
        WHERE p.oid = 'public.erp_node_feature_guard()'::regprocedure AND a.grantee = 0) THEN
        REVOKE ALL ON FUNCTION public.erp_node_feature_guard() FROM PUBLIC;
    END IF;
END
$install$;

DO $install$
BEGIN
    IF pg_catalog.to_regprocedure('public.erp_node_feature_member(text,integer,uuid,bytea)') IS NULL THEN
        EXECUTE $definition$
CREATE FUNCTION public.erp_node_feature_member(p_key text, p_version int4, p_node uuid, p_digest bytea)
RETURNS TABLE(required_version int4, enabled boolean, generation int8, node_serving boolean)
LANGUAGE plpgsql VOLATILE SECURITY INVOKER SET search_path = pg_catalog, public
AS $erp_fn$
DECLARE f record; n public.erp_node%ROWTYPE;
BEGIN
    SELECT * INTO f FROM public.erp_node_feature_snapshot(p_key);
    IF p_version IS NULL OR f.required_version <> p_version THEN
        RAISE EXCEPTION USING ERRCODE = 'ERF01', MESSAGE = 'feature capability version does not match';
    END IF;
    SELECT * INTO n FROM public.erp_node WHERE id = p_node;
    IF NOT FOUND OR p_digest IS NULL OR pg_catalog.octet_length(p_digest) <> 32
       OR n.digest <> p_digest OR n.state IN ('stopping', 'stopped')
       OR n.expires_at <= pg_catalog.clock_timestamp()
       OR n.capabilities -> p_key IS DISTINCT FROM pg_catalog.to_jsonb(p_version) THEN
        RAISE EXCEPTION USING ERRCODE = 'ERF01', MESSAGE = 'login selection membership does not match this authentication service';
    END IF;
    IF f.enabled AND NOT EXISTS (SELECT FROM public.erp_node_feature_digest d
        WHERE d.feature = p_key AND d.digest = p_digest AND d.version = p_version) THEN
        RAISE EXCEPTION USING ERRCODE = 'ERF01', MESSAGE = 'login selection membership does not match this authentication service';
    END IF;
    RETURN QUERY SELECT f.required_version, f.enabled, f.generation, n.state = 'ready';
END
$erp_fn$;
$definition$;
    END IF;
    IF EXISTS (SELECT FROM pg_proc p, aclexplode(coalesce(p.proacl, acldefault('f',p.proowner))) a
        WHERE p.oid = 'public.erp_node_feature_member(text,integer,uuid,bytea)'::regprocedure AND a.grantee = 0) THEN
        REVOKE ALL ON FUNCTION public.erp_node_feature_member(text,integer,uuid,bytea) FROM PUBLIC;
    END IF;
END
$install$;
