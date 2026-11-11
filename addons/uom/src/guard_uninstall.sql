-- Owner lifecycle only, after the fence and permanent-history refusal. No CASCADE.
DROP TRIGGER erp_uom_before_insert ON public.uom_uom;
DROP TRIGGER erp_uom_before_delete ON public.uom_uom;
DROP TRIGGER erp_uom_before_update ON public.uom_uom;
DROP TRIGGER erp_uom_after_insert ON public.uom_uom;
DROP TRIGGER erp_uom_after_update ON public.uom_uom;
DROP TRIGGER erp_uom_after_delete ON public.uom_uom;
DROP TRIGGER erp_uom_marker_immutable ON public.uom_reference_use;
DROP TRIGGER erp_uom_fence_permanent ON public.uom_reference_fence;
DROP FUNCTION public.erp_uom_register(bigint[]);
DROP FUNCTION public.erp_uom_after_mutation();
DROP FUNCTION public.erp_uom_before_mutation();
DROP FUNCTION public.erp_uom_resolve(bigint[]);
DROP FUNCTION public.erp_uom_descendants(bigint[]);
DROP FUNCTION public.erp_uom_fence();
DROP FUNCTION public.erp_uom_private_state();
