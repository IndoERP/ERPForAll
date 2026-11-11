-- One visible guard statement, including the version fence. No privileged multi-query wrapper.
WITH RECURSIVE fence AS (
 UPDATE public.uom_reference_fence SET singleton=singleton WHERE singleton=1 RETURNING singleton
), closure(id) AS (
 SELECT u.id::bigint FROM public.uom_uom u CROSS JOIN fence WHERE u.id=ANY($1::bigint[])
 UNION
 SELECT u.id::bigint FROM public.uom_uom u JOIN closure c ON u.relative_uom_id=c.id
), bounded AS MATERIALIZED (SELECT id FROM closure LIMIT 4097), marked AS (
 SELECT m.uom_id::bigint,m.root_id::bigint,m.factor FROM public.uom_reference_use m JOIN bounded b ON b.id=m.uom_id
), edges AS NOT MATERIALIZED (
 SELECT u.id::bigint,
   CASE WHEN u.id=ANY($1::bigint[]) AND $2::boolean THEN $3::bigint ELSE u.relative_uom_id::bigint END parent_id,
   CASE WHEN u.id=ANY($1::bigint[]) AND $4::boolean THEN $5::text::numeric ELSE u.relative_factor END ratio
 FROM public.uom_uom u
), walk(unit_id,id,parent_id,product,trail,depth) AS (
 SELECT m.uom_id,e.id,e.parent_id,e.ratio,ARRAY[e.id],1 FROM marked m JOIN edges e ON e.id=m.uom_id
 UNION ALL
 SELECT w.unit_id,e.id,e.parent_id,w.product*e.ratio,w.trail||e.id,w.depth+1
 FROM walk w JOIN edges e ON e.id=w.parent_id WHERE w.depth<=256 AND NOT e.id=ANY(w.trail)
)
SELECT (SELECT count(*) FROM fence)::bigint,
 (SELECT count(*) FROM bounded)::bigint,
 EXISTS(SELECT 1 FROM marked m JOIN public.uom_uom u ON u.id=m.uom_id
   WHERE u.id=ANY($1::bigint[]) AND
    (($2::boolean AND u.relative_uom_id::bigint IS DISTINCT FROM $3::bigint)
      OR ($4::boolean AND u.relative_factor IS DISTINCT FROM $5::text::numeric)))
 OR EXISTS(SELECT 1 FROM marked m WHERE NOT EXISTS(
   SELECT 1 FROM walk w WHERE w.unit_id=m.uom_id AND w.parent_id IS NULL
     AND w.id=m.root_id AND w.product=m.factor AND w.depth<=256)) AS denied,
 EXISTS(SELECT 1 FROM walk w WHERE w.depth>256 OR w.parent_id=ANY(w.trail)) AS invalid
