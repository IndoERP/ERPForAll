SELECT COUNT(*), AVG("qast_order"."amount"), MAX("qast_order"."amount") FROM "qast_order" WHERE "qast_order"."active" IS TRUE AND (("qast_order"."company_id" IS NULL OR "qast_order"."company_id" = ANY($1::int8[])) AND "qast_order"."user_id" = $2::int8) LIMIT $3::int8
-- parameters: 3
