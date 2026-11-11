-- exact effective rights, including NOINHERIT roles reachable with SET ROLE.
WITH reachable AS (
 SELECT r.oid FROM pg_catalog.pg_roles r WHERE r.rolname=$1::name OR pg_catalog.pg_has_role($1::name,r.oid,'MEMBER')
), tables AS (
 SELECT c.* FROM pg_catalog.pg_class c JOIN pg_catalog.pg_namespace n ON n.oid=c.relnamespace
 WHERE n.nspname='public' AND c.relname IN ('uom_reference_fence','uom_reference_use') AND c.relkind='r'
), routines AS (
 SELECT p.* FROM pg_catalog.pg_proc p JOIN pg_catalog.pg_namespace n ON n.oid=p.pronamespace
 WHERE n.nspname='public' AND p.proname IN ('erp_uom_fence','erp_uom_resolve','erp_uom_descendants','erp_uom_register','erp_uom_before_mutation','erp_uom_after_mutation','erp_uom_private_state')
)
SELECT
 (SELECT count(*) FROM tables)<>2
 OR (SELECT count(*) FROM public.uom_reference_fence WHERE singleton=1)<>1
 OR (SELECT count(*) FROM public.uom_reference_fence)<>1
 OR NOT pg_catalog.has_table_privilege($1::name,'public.uom_reference_fence','SELECT')
 OR NOT pg_catalog.has_table_privilege($1::name,'public.uom_reference_use','SELECT')
 OR NOT pg_catalog.has_column_privilege($1::name,'public.uom_reference_fence','singleton','UPDATE')
 OR NOT pg_catalog.has_function_privilege($1::name,'public.erp_uom_register(bigint[])','EXECUTE')
 OR EXISTS(SELECT 1 FROM reachable r JOIN pg_catalog.pg_roles p ON p.oid=r.oid
   WHERE p.rolsuper OR p.rolcreaterole OR p.rolbypassrls OR pg_catalog.has_schema_privilege(r.oid,'public','CREATE'))
 OR EXISTS(SELECT 1 FROM reachable r CROSS JOIN tables c
   WHERE pg_catalog.pg_has_role(r.oid,c.relowner,'MEMBER')
    OR pg_catalog.has_table_privilege(r.oid,c.oid,'INSERT') OR pg_catalog.has_table_privilege(r.oid,c.oid,'UPDATE')
    OR pg_catalog.has_table_privilege(r.oid,c.oid,'DELETE') OR pg_catalog.has_table_privilege(r.oid,c.oid,'TRUNCATE')
    OR pg_catalog.has_table_privilege(r.oid,c.oid,'REFERENCES') OR pg_catalog.has_table_privilege(r.oid,c.oid,'TRIGGER')
    OR pg_catalog.has_table_privilege(r.oid,c.oid,'MAINTAIN')
    OR pg_catalog.has_any_column_privilege(r.oid,c.oid,'INSERT') OR pg_catalog.has_any_column_privilege(r.oid,c.oid,'REFERENCES')
    OR EXISTS(SELECT 1 FROM pg_catalog.pg_attribute a WHERE a.attrelid=c.oid AND a.attnum>0 AND NOT a.attisdropped
      AND (c.relname='uom_reference_use' OR a.attname<>'singleton') AND pg_catalog.has_column_privilege(r.oid,c.oid,a.attnum,'UPDATE')))
 OR EXISTS(SELECT 1 FROM reachable r CROSS JOIN pg_catalog.pg_class c JOIN pg_catalog.pg_namespace n ON n.oid=c.relnamespace
   WHERE n.nspname='public' AND c.relname IN ('uom_reference_fence_id_seq','uom_reference_use_id_seq')
    AND (c.relkind<>'S' OR pg_catalog.has_sequence_privilege(r.oid,c.oid,'USAGE')
     OR pg_catalog.has_sequence_privilege(r.oid,c.oid,'SELECT') OR pg_catalog.has_sequence_privilege(r.oid,c.oid,'UPDATE')))
 OR EXISTS(SELECT 1 FROM routines p WHERE NOT p.prosecdef
   OR NOT COALESCE(p.proconfig @> ARRAY['search_path=pg_catalog, public'],false)
   OR p.proowner<>(SELECT c.relowner FROM tables c WHERE c.relname='uom_reference_fence')
   OR EXISTS(SELECT 1 FROM pg_catalog.aclexplode(COALESCE(p.proacl,pg_catalog.acldefault('f',p.proowner))) a
      WHERE a.grantee=0 AND a.privilege_type='EXECUTE'))
 OR EXISTS(SELECT 1 FROM reachable r CROSS JOIN routines p WHERE p.proname<>'erp_uom_register'
     AND pg_catalog.has_function_privilege(r.oid,p.oid,'EXECUTE'))
 OR pg_catalog.to_regprocedure('public.erp_uom_fence()') IS NULL
 OR pg_catalog.to_regprocedure('public.erp_uom_resolve(bigint[])') IS NULL
 OR pg_catalog.to_regprocedure('public.erp_uom_descendants(bigint[])') IS NULL
 OR pg_catalog.to_regprocedure('public.erp_uom_register(bigint[])') IS NULL
 OR pg_catalog.to_regprocedure('public.erp_uom_before_mutation()') IS NULL
 OR pg_catalog.to_regprocedure('public.erp_uom_after_mutation()') IS NULL
 OR pg_catalog.to_regprocedure('public.erp_uom_private_state()') IS NULL
 OR (SELECT count(*) FROM pg_catalog.pg_trigger t WHERE NOT t.tgisinternal AND t.tgenabled='O'
    AND ((t.tgrelid='public.uom_uom'::regclass AND t.tgname IN ('erp_uom_before_insert','erp_uom_before_delete','erp_uom_before_update','erp_uom_after_insert','erp_uom_after_update','erp_uom_after_delete'))
      OR (t.tgrelid='public.uom_reference_use'::regclass AND t.tgname='erp_uom_marker_immutable')
      OR (t.tgrelid='public.uom_reference_fence'::regclass AND t.tgname='erp_uom_fence_permanent')))<>8
 OR EXISTS(SELECT 1 FROM pg_catalog.pg_trigger t WHERE NOT t.tgisinternal AND t.tgrelid='public.uom_uom'::regclass
    AND t.tgname NOT IN ('erp_uom_before_insert','erp_uom_before_delete','erp_uom_before_update','erp_uom_after_insert','erp_uom_after_update','erp_uom_after_delete'))
 OR (SELECT count(*) FROM pg_catalog.pg_constraint c WHERE c.conrelid='public.uom_reference_use'::regclass
    AND c.contype='f' AND c.confrelid='public.uom_uom'::regclass AND c.confdeltype='r')<>2
