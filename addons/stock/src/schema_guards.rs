// SPDX-License-Identifier: MIT OR Apache-2.0
//! Addon-owned integrity backstops (1,.10/D6.1).
//! Static errors deliberately contain no row identities; typed writers supply authorized labels.
use erp_sdk::sql_objects::{
    SqlConditionDecl, SqlConditionKind, SqlDep, SqlEvent, SqlFunctionDecl, SqlLanguage, SqlLevel,
    SqlObjectDecl, SqlObjectManifest, SqlResult, SqlTiming, SqlTriggerDecl,
};

/// Independently required by every native build site that can compose stock.
/// The manifest is separate from the linked declarations so a dropped registration fails loudly.
pub static MANIFEST: SqlObjectManifest = SqlObjectManifest::new(
    "stock",
    &[
        "done_at_immutable",
        "correction_row_immutable",
        "location_policy_required",
        "fn_done_at",
        "fn_correction",
        "fn_policy_insert",
        "fn_policy_keep",
        "move_done_at",
        "line_done_at",
        "correction_update",
        "correction_delete",
        "correction_truncate",
        "location_insert",
        "policy_update",
        "policy_delete",
        "policy_truncate",
    ],
);

macro_rules! object {
    ($name:ident, $decl:expr) => {
        #[erp_sdk::sql_objects::linkme::distributed_slice(erp_sdk::sql_objects::SQL_OBJECT_DECLS)]
        #[linkme(crate = erp_sdk::sql_objects::linkme)]
        static $name: SqlObjectDecl = $decl;
    };
}

object!(
    DONE_CONDITION,
    SqlObjectDecl::condition(
        "stock",
        "done_at_immutable",
        SqlConditionDecl::new(
            "ERP01",
            "stock_done_at_immutable",
            "The first completion time cannot be changed or cleared."
        )
        .kind(SqlConditionKind::Validation)
    )
);
object!(
    CORRECTION_CONDITION,
    SqlObjectDecl::condition(
        "stock",
        "correction_row_immutable",
        SqlConditionDecl::new(
            "ERP02",
            "stock_correction_row_immutable",
            "A stock correction history row was modified."
        )
        .kind(SqlConditionKind::Programming)
    )
);
object!(
    POLICY_CONDITION,
    SqlObjectDecl::condition(
        "stock",
        "location_policy_required",
        SqlConditionDecl::new(
            "ERP03",
            "stock_location_policy_required",
            "Each inventory location must retain its stock policy."
        )
        .kind(SqlConditionKind::Validation)
    )
);

object!(
    DONE_FUNCTION,
    SqlObjectDecl::function(
        "stock",
        "fn_done_at",
        SqlFunctionDecl::new(
            "stock_guard_done_at",
            SqlLanguage::PlPgSql,
            SqlResult::Trigger,
            r#"
BEGIN
    IF EXISTS (
        SELECT 1 FROM old_rows o LEFT JOIN new_rows n ON n.id = o.id
        WHERE o.done_at IS NOT NULL AND (n.id IS NULL OR n.done_at IS DISTINCT FROM o.done_at)
    ) THEN
        RAISE EXCEPTION 'immutable completion time'
            USING ERRCODE = 'ERP01', CONSTRAINT = 'stock_done_at_immutable';
    END IF;
    RETURN NULL;
END
"#
        )
        .deps(&[
            SqlDep::Object {
                addon: "stock",
                key: "done_at_immutable"
            },
            SqlDep::Model("stock.move"),
            SqlDep::Model("stock.move.line"),
        ])
    )
);

object!(
    CORRECTION_FUNCTION,
    SqlObjectDecl::function(
        "stock",
        "fn_correction",
        SqlFunctionDecl::new(
            "stock_guard_correction",
            SqlLanguage::PlPgSql,
            SqlResult::Trigger,
            r#"
BEGIN
    IF TG_OP = 'TRUNCATE' THEN
        RAISE EXCEPTION 'append-only correction history'
            USING ERRCODE = 'ERP02', CONSTRAINT = 'stock_correction_row_immutable';
    ELSIF EXISTS (SELECT 1 FROM old_rows) THEN
        RAISE EXCEPTION 'append-only correction history'
            USING ERRCODE = 'ERP02', CONSTRAINT = 'stock_correction_row_immutable';
    END IF;
    RETURN NULL;
END
"#
        )
        .deps(&[
            SqlDep::Object {
                addon: "stock",
                key: "correction_row_immutable"
            },
            SqlDep::Model("stock.move.line.correction"),
        ])
    )
);

object!(POLICY_INSERT_FUNCTION, SqlObjectDecl::function("stock", "fn_policy_insert",
    SqlFunctionDecl::new("stock_create_location_policies", SqlLanguage::PlPgSql, SqlResult::Trigger, r#"
BEGIN
    INSERT INTO public.stock_location_policy (location_id, forbid_negative, create_uid, write_uid, create_date, write_date)
        SELECT id, false, create_uid, write_uid, (now() AT TIME ZONE 'UTC'), (now() AT TIME ZONE 'UTC') FROM new_rows ORDER BY id;
    RETURN NULL;
END
"#).deps(&[SqlDep::Model("stock.location"), SqlDep::Model("stock.location.policy")])));

object!(
    POLICY_KEEP_FUNCTION,
    SqlObjectDecl::function(
        "stock",
        "fn_policy_keep",
        SqlFunctionDecl::new(
            "stock_guard_location_policy",
            SqlLanguage::PlPgSql,
            SqlResult::Trigger,
            r#"
BEGIN
    IF TG_OP = 'TRUNCATE' THEN
        RAISE EXCEPTION 'location policy required'
            USING ERRCODE = 'ERP03', CONSTRAINT = 'stock_location_policy_required';
    ELSIF EXISTS (
        SELECT 1 FROM old_rows o
        JOIN public.stock_location l ON l.id = o.location_id
        LEFT JOIN public.stock_location_policy p ON p.location_id = l.id
        WHERE p.id IS NULL
    ) THEN
        RAISE EXCEPTION 'location policy required'
            USING ERRCODE = 'ERP03', CONSTRAINT = 'stock_location_policy_required';
    END IF;
    RETURN NULL;
END
"#
        )
        .deps(&[
            SqlDep::Object {
                addon: "stock",
                key: "location_policy_required"
            },
            SqlDep::Model("stock.location"),
            SqlDep::Model("stock.location.policy"),
        ])
    )
);

const fn update(name: &'static str, model: &'static str, function: &'static str) -> SqlTriggerDecl {
    SqlTriggerDecl::new(
        name,
        model,
        ("stock", function),
        SqlTiming::After,
        &[SqlEvent::Update],
        SqlLevel::Statement,
    )
    .old_table("old_rows")
    .new_table("new_rows")
}
const fn delete(name: &'static str, model: &'static str, function: &'static str) -> SqlTriggerDecl {
    SqlTriggerDecl::new(
        name,
        model,
        ("stock", function),
        SqlTiming::After,
        &[SqlEvent::Delete],
        SqlLevel::Statement,
    )
    .old_table("old_rows")
}
const fn truncate(
    name: &'static str,
    model: &'static str,
    function: &'static str,
) -> SqlTriggerDecl {
    SqlTriggerDecl::new(
        name,
        model,
        ("stock", function),
        SqlTiming::Before,
        &[SqlEvent::Truncate],
        SqlLevel::Statement,
    )
}
object!(
    MOVE_DONE,
    SqlObjectDecl::trigger(
        "stock",
        "move_done_at",
        update("stock_move_done_at_guard", "stock.move", "fn_done_at")
    )
);
object!(
    LINE_DONE,
    SqlObjectDecl::trigger(
        "stock",
        "line_done_at",
        update("stock_line_done_at_guard", "stock.move.line", "fn_done_at")
    )
);
object!(
    CORRECTION_UPDATE,
    SqlObjectDecl::trigger(
        "stock",
        "correction_update",
        update(
            "stock_correction_update_guard",
            "stock.move.line.correction",
            "fn_correction"
        )
    )
);
object!(
    CORRECTION_DELETE,
    SqlObjectDecl::trigger(
        "stock",
        "correction_delete",
        delete(
            "stock_correction_delete_guard",
            "stock.move.line.correction",
            "fn_correction"
        )
    )
);
object!(
    CORRECTION_TRUNCATE,
    SqlObjectDecl::trigger(
        "stock",
        "correction_truncate",
        truncate(
            "stock_correction_truncate_guard",
            "stock.move.line.correction",
            "fn_correction"
        )
    )
);
object!(
    LOCATION_INSERT,
    SqlObjectDecl::trigger(
        "stock",
        "location_insert",
        SqlTriggerDecl::new(
            "stock_location_policy_insert",
            "stock.location",
            ("stock", "fn_policy_insert"),
            SqlTiming::After,
            &[SqlEvent::Insert],
            SqlLevel::Statement
        )
        .new_table("new_rows")
    )
);
object!(
    POLICY_UPDATE,
    SqlObjectDecl::trigger(
        "stock",
        "policy_update",
        update(
            "stock_policy_update_guard",
            "stock.location.policy",
            "fn_policy_keep"
        )
    )
);
object!(
    POLICY_DELETE,
    SqlObjectDecl::trigger(
        "stock",
        "policy_delete",
        delete(
            "stock_policy_delete_guard",
            "stock.location.policy",
            "fn_policy_keep"
        )
    )
);
object!(
    POLICY_TRUNCATE,
    SqlObjectDecl::trigger(
        "stock",
        "policy_truncate",
        truncate(
            "stock_policy_truncate_guard",
            "stock.location.policy",
            "fn_policy_keep"
        )
    )
);
