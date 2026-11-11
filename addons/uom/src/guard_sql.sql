-- Installed transactionally by the UoM model init hook, as schema owner.
-- Tables are ordinary addon models. Fixed object names; no caller-selected SQL.
CREATE OR REPLACE FUNCTION public.erp_uom_fence() RETURNS void
LANGUAGE plpgsql SECURITY DEFINER SET search_path = pg_catalog, public AS $body$
BEGIN
  UPDATE public.uom_reference_fence SET singleton = singleton WHERE singleton = 1;
  IF NOT FOUND THEN
    RAISE EXCEPTION USING ERRCODE='UOM03', MESSAGE='Unit reference guard state is missing or corrupt.';
  END IF;
END
$body$;

-- Fetch bounded unique graph identities once, then derive paths without per-node queries.
CREATE OR REPLACE FUNCTION public.erp_uom_resolve(requested bigint[])
RETURNS TABLE(uom_id bigint, root_id bigint, factor numeric, canonical_path text)
LANGUAGE plpgsql SECURITY DEFINER SET search_path = pg_catalog, public AS $body$
DECLARE graph jsonb; n integer; requested_id bigint; current_id bigint;
  node jsonb; parent_id bigint; ratio numeric; result_factor numeric;
  trail bigint[]; result_path text;
BEGIN
  IF requested IS NULL OR cardinality(requested) > 4096 OR array_position(requested, NULL) IS NOT NULL
     OR EXISTS (SELECT 1 FROM unnest(requested) x WHERE x <= 0) THEN
    RAISE EXCEPTION USING ERRCODE='UOM04', MESSAGE='Unit reference guard input exceeds its limits or has invalid identities.';
  END IF;
  WITH RECURSIVE ancestors(id) AS (
    SELECT DISTINCT x FROM unnest(requested) x
    UNION
    SELECT u.relative_uom_id::bigint FROM public.uom_uom u
      JOIN ancestors a ON a.id=u.id WHERE u.relative_uom_id IS NOT NULL
  ), bounded AS (SELECT id FROM ancestors LIMIT 4097)
  SELECT count(*)::integer, jsonb_object_agg(b.id::text,
    CASE WHEN u.id IS NULL THEN 'null'::jsonb
      ELSE jsonb_build_array(u.relative_uom_id, u.relative_factor) END)
    INTO n, graph FROM bounded b LEFT JOIN public.uom_uom u ON u.id=b.id;
  IF n > 4096 THEN
    RAISE EXCEPTION USING ERRCODE='UOM04', MESSAGE='Unit reference guard graph exceeds 4096 identities.';
  END IF;
  FOR requested_id IN SELECT DISTINCT x FROM unnest(requested) x ORDER BY x LOOP
    current_id := requested_id; result_factor := 1; trail := ARRAY[]::bigint[]; result_path := '';
    LOOP
      IF cardinality(trail) >= 256 THEN
        RAISE EXCEPTION USING ERRCODE='UOM04', MESSAGE='Unit of measure hierarchy exceeds 256 levels.';
      END IF;
      IF current_id = ANY(trail) THEN
        RAISE EXCEPTION USING ERRCODE='UOM02', MESSAGE='Unit of measure hierarchy is invalid.';
      END IF;
      trail := array_append(trail,current_id);
      node := graph -> current_id::text;
      IF node IS NULL OR node='null'::jsonb THEN
        RAISE EXCEPTION USING ERRCODE='UOM02', MESSAGE='Unit of measure hierarchy is invalid.';
      END IF;
      parent_id := (node->>0)::bigint; ratio := (node->>1)::numeric;
      IF ratio IS NULL OR ratio=0 THEN
        RAISE EXCEPTION USING ERRCODE='UOM02', MESSAGE='Unit of measure hierarchy is invalid.';
      END IF;
      IF parent_id IS NULL AND ratio<>1 THEN
        RAISE EXCEPTION USING ERRCODE='UOM06', MESSAGE='Reference unit of measure is missing.';
      END IF;
      result_factor := result_factor * ratio;
      result_path := current_id::text || '/' || result_path;
      EXIT WHEN parent_id IS NULL;
      current_id := parent_id;
    END LOOP;
    IF scale(trim_scale(result_factor))>28 OR
       abs(result_factor)*power(10::numeric, greatest(scale(trim_scale(result_factor)),0)) > 79228162514264337593543950335 THEN
      RAISE EXCEPTION USING ERRCODE='UOM07', MESSAGE='Unit reference factor is not representable exactly.';
    END IF;
    -- Drop insignificant SQL scale without rounding before the native Decimal boundary.
    uom_id := requested_id; root_id := current_id; factor := trim_scale(result_factor);
    canonical_path := result_path; RETURN NEXT;
  END LOOP;
END
$body$;

CREATE OR REPLACE FUNCTION public.erp_uom_descendants(requested bigint[]) RETURNS bigint[]
LANGUAGE plpgsql SECURITY DEFINER SET search_path = pg_catalog, public AS $body$
DECLARE affected bigint[];
BEGIN
  IF requested IS NULL OR cardinality(requested)>4096 OR array_position(requested,NULL) IS NOT NULL
    OR EXISTS(SELECT 1 FROM unnest(requested) x WHERE x<=0) THEN
    RAISE EXCEPTION USING ERRCODE='UOM04', MESSAGE='Unit reference guard input exceeds its limits or has invalid identities.';
  END IF;
  WITH RECURSIVE closure(id) AS (
    SELECT DISTINCT x FROM unnest(requested) x
    UNION SELECT u.id::bigint FROM public.uom_uom u JOIN closure c ON u.relative_uom_id=c.id
  ), bounded AS (SELECT id FROM closure LIMIT 4097)
  SELECT array_agg(id ORDER BY id) INTO affected FROM bounded;
  IF cardinality(affected)>4096 THEN
    RAISE EXCEPTION USING ERRCODE='UOM04', MESSAGE='Unit reference guard graph exceeds 4096 identities.';
  END IF;
  RETURN COALESCE(affected,ARRAY[]::bigint[]); -- Empty closure is explicitly an empty batch.
END
$body$;

CREATE OR REPLACE FUNCTION public.erp_uom_register(requested bigint[])
RETURNS TABLE(uom_id bigint, root_id bigint, factor numeric)
LANGUAGE plpgsql SECURITY DEFINER SET search_path = pg_catalog, public AS $body$
DECLARE identities bigint[]; missing bigint[];
BEGIN
  IF requested IS NULL OR cardinality(requested)>4096 OR array_position(requested,NULL) IS NOT NULL
    OR EXISTS(SELECT 1 FROM unnest(requested) x WHERE x<=0) THEN
    RAISE EXCEPTION USING ERRCODE='UOM04', MESSAGE='Unit reference guard input exceeds its limits or has invalid identities.';
  END IF;
  SELECT array_agg(DISTINCT x ORDER BY x) INTO identities FROM unnest(requested) x;
  IF identities IS NULL THEN RETURN; END IF; -- An empty input is a no-op.
  -- C1: visible permanent markers need compatible locks, never a singleton write.
  PERFORM 1 FROM public.uom_reference_use m WHERE m.uom_id=ANY(identities)
    ORDER BY m.uom_id FOR KEY SHARE;
  SELECT array_agg(x ORDER BY x) INTO missing FROM unnest(identities) x
    WHERE NOT EXISTS(SELECT 1 FROM public.uom_reference_use m WHERE m.uom_id=x);
  IF missing IS NOT NULL THEN
    PERFORM public.erp_uom_fence();
    INSERT INTO public.uom_reference_use(uom_id,root_id,factor)
      SELECT r.uom_id,r.root_id,r.factor FROM public.erp_uom_resolve(missing) r
      ORDER BY r.uom_id ON CONFLICT DO NOTHING;
  END IF;
  IF (SELECT count(*) FROM public.uom_reference_use m WHERE m.uom_id=ANY(identities))<>cardinality(identities) THEN
    RAISE EXCEPTION USING ERRCODE='UOM03', MESSAGE='Unit reference guard state is missing or corrupt.';
  END IF;
  RETURN QUERY SELECT m.uom_id::bigint,m.root_id::bigint,trim_scale(m.factor)
    FROM public.uom_reference_use m WHERE m.uom_id=ANY(identities) ORDER BY m.uom_id;
END
$body$;

CREATE OR REPLACE FUNCTION public.erp_uom_before_mutation() RETURNS trigger
LANGUAGE plpgsql SECURITY DEFINER SET search_path = pg_catalog, public AS $body$
BEGIN PERFORM public.erp_uom_fence(); RETURN NULL; END
$body$;

CREATE OR REPLACE FUNCTION public.erp_uom_after_mutation() RETURNS trigger
LANGUAGE plpgsql SECURITY DEFINER SET search_path = pg_catalog, public AS $body$
DECLARE changed bigint[]; affected bigint[];
BEGIN
  IF TG_OP='DELETE' THEN RETURN NULL; END IF; -- RESTRICT marker FKs prevent historical deletion.
  IF TG_OP='UPDATE' THEN
    SELECT array_agg(n.id::bigint ORDER BY n.id) INTO changed FROM uom_new n JOIN uom_old o USING(id)
      WHERE (n.relative_uom_id,n.relative_factor,n.factor,n.parent_path)
        IS DISTINCT FROM (o.relative_uom_id,o.relative_factor,o.factor,o.parent_path);
    IF changed IS NULL THEN RETURN NULL; END IF;
    IF EXISTS(SELECT 1 FROM uom_new n JOIN uom_old o USING(id)
      JOIN public.uom_reference_use m ON m.uom_id=n.id
      WHERE (n.relative_uom_id,n.relative_factor) IS DISTINCT FROM (o.relative_uom_id,o.relative_factor)) THEN
      RAISE EXCEPTION USING ERRCODE='UOM01', MESSAGE='A referenced unit of measure would change. Create a new unit instead.';
    END IF;
  ELSE
    SELECT array_agg(n.id::bigint ORDER BY n.id) INTO changed FROM uom_new n;
  END IF;
  IF changed IS NULL THEN RETURN NULL; END IF;
  affected := public.erp_uom_descendants(changed);
  -- Validate every affected graph, including unmarked/archived/hidden units.
  IF EXISTS(SELECT 1 FROM public.erp_uom_resolve(affected) r
    JOIN public.uom_reference_use m ON m.uom_id=r.uom_id
    WHERE (r.root_id,r.factor) IS DISTINCT FROM (m.root_id,m.factor)) THEN
    RAISE EXCEPTION USING ERRCODE='UOM01', MESSAGE='A referenced unit of measure would change. Create a new unit instead.';
  END IF;
  IF TG_OP='UPDATE' THEN
  IF EXISTS(
    SELECT 1 FROM uom_new n JOIN uom_old o USING(id)
    JOIN public.erp_uom_resolve(changed) r ON r.uom_id=n.id
    WHERE (n.factor IS DISTINCT FROM o.factor AND n.factor IS DISTINCT FROM r.factor)
       OR (n.parent_path IS DISTINCT FROM o.parent_path AND n.parent_path IS DISTINCT FROM r.canonical_path)
  ) THEN
    RAISE EXCEPTION USING ERRCODE='UOM02', MESSAGE='Unit of measure hierarchy is invalid.';
  END IF;
  END IF;
  RETURN NULL;
END
$body$;

CREATE OR REPLACE FUNCTION public.erp_uom_private_state() RETURNS trigger
LANGUAGE plpgsql SECURITY DEFINER SET search_path = pg_catalog, public AS $body$
BEGIN
  RAISE EXCEPTION USING ERRCODE='UOM05', MESSAGE='Unit reference guard records cannot be modified through ordinary CRUD.';
END
$body$;

DROP TRIGGER IF EXISTS erp_uom_before_insert ON public.uom_uom;
CREATE TRIGGER erp_uom_before_insert BEFORE INSERT ON public.uom_uom
  FOR EACH STATEMENT EXECUTE FUNCTION public.erp_uom_before_mutation();
DROP TRIGGER IF EXISTS erp_uom_before_delete ON public.uom_uom;
CREATE TRIGGER erp_uom_before_delete BEFORE DELETE ON public.uom_uom
  FOR EACH STATEMENT EXECUTE FUNCTION public.erp_uom_before_mutation();
DROP TRIGGER IF EXISTS erp_uom_before_update ON public.uom_uom;
CREATE TRIGGER erp_uom_before_update BEFORE UPDATE OF relative_uom_id,relative_factor,factor,parent_path ON public.uom_uom
  FOR EACH STATEMENT EXECUTE FUNCTION public.erp_uom_before_mutation();
DROP TRIGGER IF EXISTS erp_uom_after_insert ON public.uom_uom;
CREATE TRIGGER erp_uom_after_insert AFTER INSERT ON public.uom_uom REFERENCING NEW TABLE AS uom_new
  FOR EACH STATEMENT EXECUTE FUNCTION public.erp_uom_after_mutation();
DROP TRIGGER IF EXISTS erp_uom_after_update ON public.uom_uom;
CREATE TRIGGER erp_uom_after_update AFTER UPDATE ON public.uom_uom REFERENCING OLD TABLE AS uom_old NEW TABLE AS uom_new
  FOR EACH STATEMENT EXECUTE FUNCTION public.erp_uom_after_mutation();
DROP TRIGGER IF EXISTS erp_uom_after_delete ON public.uom_uom;
CREATE TRIGGER erp_uom_after_delete AFTER DELETE ON public.uom_uom REFERENCING OLD TABLE AS uom_old
  FOR EACH STATEMENT EXECUTE FUNCTION public.erp_uom_after_mutation();
DROP TRIGGER IF EXISTS erp_uom_marker_immutable ON public.uom_reference_use;
CREATE TRIGGER erp_uom_marker_immutable BEFORE UPDATE OR DELETE OR TRUNCATE ON public.uom_reference_use
  FOR EACH STATEMENT EXECUTE FUNCTION public.erp_uom_private_state();
DROP TRIGGER IF EXISTS erp_uom_fence_permanent ON public.uom_reference_fence;
CREATE TRIGGER erp_uom_fence_permanent BEFORE DELETE OR TRUNCATE ON public.uom_reference_fence
  FOR EACH STATEMENT EXECUTE FUNCTION public.erp_uom_private_state();
REVOKE ALL ON public.uom_reference_fence,public.uom_reference_use FROM PUBLIC;
REVOKE ALL ON FUNCTION public.erp_uom_fence(),public.erp_uom_resolve(bigint[]),
  public.erp_uom_descendants(bigint[]),public.erp_uom_register(bigint[]),
  public.erp_uom_before_mutation(),public.erp_uom_after_mutation(),public.erp_uom_private_state() FROM PUBLIC;
