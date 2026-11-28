-- SPDX-License-Identifier: MIT OR Apache-2.0
-- Populate the stored order date on lines that predate the reporting field.
UPDATE public.sale_order_line AS line
   SET date_order = orders.date_order
  FROM public.sale_order AS orders
 WHERE orders.id = line.order_id
   AND line.date_order IS DISTINCT FROM orders.date_order;
