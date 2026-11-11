SELECT "qast_order"."id", "qast_order"."name", "qast_order"."amount" FROM "qast_order" WHERE ("qast_order"."active" IS TRUE AND "qast_order"."state" = ANY($1::text[])) AND (("qast_order"."company_id" IS NULL OR "qast_order"."company_id" = ANY($2::int8[])) AND "qast_order"."user_id" = $3::int8) ORDER BY "qast_order"."amount" DESC, "qast_order"."id" LIMIT $4::int8
-- parameters: 4
