SELECT "qast_order"."user_id", SUM("qast_order"."amount") FROM "qast_order" WHERE (("qast_order"."company_id" IS NULL OR "qast_order"."company_id" = ANY($1::int8[])) AND "qast_order"."user_id" = $2::int8) GROUP BY "qast_order"."user_id" HAVING SUM("qast_order"."amount") >= $3::int8 ORDER BY "qast_order"."user_id" DESC LIMIT $4::int8
-- parameters: 4
