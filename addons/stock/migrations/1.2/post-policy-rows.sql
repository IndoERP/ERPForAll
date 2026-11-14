-- SPDX-License-Identifier: MIT OR Apache-2.0
-- The lifecycle transaction installs the trigger and fills legacy gaps atomically.
-- Preserve existing policy identities, settings and audit data; only absent rows are new.
-- Stamped in UTC like an ORM write (create_date and write_date have no time zone).
LOCK TABLE public.stock_location, public.stock_location_policy IN SHARE ROW EXCLUSIVE MODE;
INSERT INTO public.stock_location_policy (location_id, forbid_negative, create_date, write_date)
SELECT l.id, false, (now() AT TIME ZONE 'UTC'), (now() AT TIME ZONE 'UTC')
FROM public.stock_location l
WHERE NOT EXISTS (SELECT 1 FROM public.stock_location_policy p WHERE p.location_id = l.id)
ORDER BY l.id;
