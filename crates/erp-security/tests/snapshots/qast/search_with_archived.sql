SELECT "qast_order"."id", "qast_order"."name", "qast_order"."active" FROM "qast_order" WHERE (("qast_order"."company_id" IS NULL OR "qast_order"."company_id" = ANY($1::int8[])) AND "qast_order"."user_id" = $2::int8) ORDER BY "qast_order"."name" ASC, "qast_order"."id" LIMIT $3::int8
-- parameters: 3
