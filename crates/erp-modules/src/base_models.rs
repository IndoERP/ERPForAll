//! The module-metadata models of addon `base` (MOD §4.2): `ir.module.category`,
//! `ir.module.module`, `ir.module.module.dependency`, `ir.module.module.exclusion`, and
//! `ir.module.post_commit` (the post-commit phase).
//!
//! They are not linked through `MODEL_DECLS`: the module system passes [`DECLS`] to every registry
//! it composes (`RegistryBuilder::with_decls`), so registries built elsewhere keep their table set.
//! `addons/base` takes them over unchanged when it exists.
//!
//! `country_codes` (JSON array of ISO codes) replaces the many2many `country_ids` to
//!   `res.country` (M3).
//! - `license` is a char: the manifest does not restrict its values yet.
//! - Translated names, `description_html`, icons, `menus_by_module` and the other computed display
//!   fields are not declared (no translation layer or web client before M3/M4).
//! - `installed_version` (the on-disk version) is not stored: the addon index provides it.

use std::any::Any;

use erp_fields::{Error, IndexKind, OnDelete, Result};
use erp_orm::{
    BoxFuture, ConstraintImpl, ImplKind, MethodFn, MethodImpl, RawVals, Records, Super, Transaction,
};
use erp_registry::{FieldDecl, ModelDecl, StaticValue, TableObjectDecl, TableObjectKind};

/// `ir.module.category`.
pub const CATEGORY: &str = "ir.module.category";
/// `ir.module.module`.
pub const MODULE: &str = "ir.module.module";
/// `ir.module.module.dependency`.
pub const DEPENDENCY: &str = "ir.module.module.dependency";
/// `ir.module.module.exclusion`.
pub const EXCLUSION: &str = "ir.module.module.exclusion";
/// `ir.module.post_commit`.
pub const POST_COMMIT: &str = "ir.module.post_commit";

/// The stored module states (`ir_module.py` `STATES`, without `unavailable`).
pub const STATES: &[(&str, &str)] = &[
    ("uninstallable", "Uninstallable"),
    ("uninstalled", "Not Installed"),
    ("installed", "Installed"),
    ("to upgrade", "To be upgraded"),
    ("to remove", "To be removed"),
    ("to install", "To be installed"),
];

/// States of a post-commit row.
pub const POST_COMMIT_STATES: &[(&str, &str)] = &[
    ("pending", "Pending"),
    ("done", "Done"),
    ("failed", "Failed"),
];

static CATEGORY_DECL: ModelDecl = ModelDecl::new("base", CATEGORY)
    .description("Application")
    .order("sequence, name, id")
    .parent_name("parent_id")
    .fields(
        const {
            &[
                FieldDecl::char("name").string("Name").required(true),
                FieldDecl::many2one("parent_id", CATEGORY)
                    .string("Parent Application")
                    .index(Some(IndexKind::Btree)),
                FieldDecl::one2many("child_ids", CATEGORY, "parent_id")
                    .string("Child Applications"),
                FieldDecl::one2many("module_ids", MODULE, "category_id").string("Modules"),
                FieldDecl::text("description").string("Description"),
                FieldDecl::integer("sequence").string("Sequence"),
                FieldDecl::boolean("visible")
                    .string("Visible")
                    .default(StaticValue::Bool(true)),
                FieldDecl::boolean("exclusive").string("Exclusive"),
            ]
        },
    )
    .at(file!(), line!());

static MODULE_DECL: ModelDecl = ModelDecl::new("base", MODULE)
    .description("Module")
    .rec_name("shortdesc")
    .rec_names_search(&["name", "shortdesc", "summary"])
    .order("application desc, sequence, name")
    .fields(
        const {
            &[
                FieldDecl::char("name")
                    .string("Technical Name")
                    .required(true)
                    .readonly(true),
                FieldDecl::many2one("category_id", CATEGORY)
                    .string("Category")
                    .readonly(true)
                    .index(Some(IndexKind::Btree)),
                FieldDecl::char("shortdesc")
                    .string("Module Name")
                    .readonly(true),
                FieldDecl::char("summary").string("Summary").readonly(true),
                FieldDecl::text("description")
                    .string("Description")
                    .readonly(true),
                FieldDecl::char("author").string("Author").readonly(true),
                FieldDecl::char("maintainer")
                    .string("Maintainer")
                    .readonly(true),
                FieldDecl::text("contributors")
                    .string("Contributors")
                    .readonly(true),
                FieldDecl::char("website").string("Website").readonly(true),
                FieldDecl::char("latest_version")
                    .string("Installed Version")
                    .readonly(true),
                FieldDecl::char("published_version")
                    .string("Published Version")
                    .readonly(true),
                FieldDecl::char("url").string("URL").readonly(true),
                FieldDecl::integer("sequence")
                    .string("Sequence")
                    .default(StaticValue::Int(100)),
                FieldDecl::one2many("dependencies_id", DEPENDENCY, "module_id")
                    .string("Dependencies")
                    .readonly(true),
                FieldDecl::one2many("exclusion_ids", EXCLUSION, "module_id")
                    .string("Exclusions")
                    .readonly(true),
                FieldDecl::json("country_codes").string("Countries"),
                FieldDecl::boolean("auto_install").string("Automatic Installation"),
                FieldDecl::selection("state", STATES)
                    .string("Status")
                    .default(StaticValue::Str("uninstallable"))
                    .readonly(true)
                    .index(Some(IndexKind::Btree)),
                FieldDecl::boolean("demo")
                    .string("Demo Data")
                    .default(StaticValue::Bool(false))
                    .readonly(true),
                FieldDecl::char("license").string("License").readonly(true),
                FieldDecl::boolean("application")
                    .string("Application")
                    .readonly(true),
                FieldDecl::char("icon").string("Icon URL"),
                FieldDecl::boolean("to_buy")
                    .string("the reference ERP Enterprise Module")
                    .default(StaticValue::Bool(false)),
            ]
        },
    )
    .table_objects(
        const {
            &[
                TableObjectDecl {
                    name: "_name_uniq",
                    kind: TableObjectKind::Constraint {
                        definition: "UNIQUE (name)",
                        message: "The name of the module must be unique!",
                    },
                },
                TableObjectDecl {
                    name: "_state_check",
                    kind: TableObjectKind::Constraint {
                        definition: "CHECK (state IN ('uninstallable', 'uninstalled', \
                                     'installed', 'to upgrade', 'to remove', 'to install'))",
                        message: "A module state must be one of the stored states.",
                    },
                },
            ]
        },
    )
    .at(file!(), line!());

static DEPENDENCY_DECL: ModelDecl = ModelDecl::new("base", DEPENDENCY)
    .description("Module dependency")
    .log_access(false)
    .fields(
        const {
            &[
                FieldDecl::char("name").index(Some(IndexKind::Btree)),
                FieldDecl::many2one("module_id", MODULE)
                    .string("Module")
                    .ondelete(OnDelete::Cascade)
                    .index(Some(IndexKind::Btree)),
                FieldDecl::boolean("auto_install_required").default(StaticValue::Bool(true)),
            ]
        },
    )
    .at(file!(), line!());

static EXCLUSION_DECL: ModelDecl = ModelDecl::new("base", EXCLUSION)
    .description("Module exclusion")
    .log_access(false)
    .fields(
        const {
            &[
                FieldDecl::char("name").index(Some(IndexKind::Btree)),
                FieldDecl::many2one("module_id", MODULE)
                    .string("Module")
                    .ondelete(OnDelete::Cascade)
                    .index(Some(IndexKind::Btree)),
            ]
        },
    )
    .at(file!(), line!());

static POST_COMMIT_DECL: ModelDecl = ModelDecl::new("base", POST_COMMIT)
    .description("Post-commit step of a module operation")
    .order("id")
    .fields(
        const {
            &[
                FieldDecl::char("module").string("Module").required(true),
                FieldDecl::char("key").string("Key").required(true),
                // What the step does: a structured action, never SQL text (the rows run with the
                // module system's database rights). `kind` + `table_name` + `index_name` + `spec`
                // are `post_commit::PostCommitAction`; a row that does not parse back is listed
                // as invalid and never run.
                FieldDecl::selection("kind", crate::post_commit::KINDS)
                    .string("Action")
                    .required(true),
                FieldDecl::char("table_name").string("Table").required(true),
                FieldDecl::char("index_name").string("Index built"),
                FieldDecl::text("spec").string("Action details (strict JSON)"),
                // Bumped each time the step is scheduled again: a phase records the result of a
                // run only when the revision it ran is still the step's (a rescheduled step runs
                // again, even with the same statement). An `int4` column (the field kinds have no
                // `int8` integer): a bump past 2^31 - 1 raises `22003` instead of wrapping.
                FieldDecl::integer("revision")
                    .string("Revision")
                    .required(true)
                    .default(StaticValue::Int(1)),
                // Runs started since the step was last scheduled: a step that fails runs again in
                // every phase, and this shows how often.
                FieldDecl::integer("attempts")
                    .string("Attempts")
                    .required(true)
                    .default(StaticValue::Int(0)),
                FieldDecl::selection("state", POST_COMMIT_STATES)
                    .string("Status")
                    .required(true)
                    .default(StaticValue::Str("pending")),
                // The last record id of the last committed batch of a batched schema step (a
                // not-null fill or a compute backfill): the step starts after it when it runs
                // again. NULL until a first batch ended and after every (re)scheduling. An `int8`
                // column: the ORM has no int8 integer kind, so it is declared as a reference
                // whose "model" is the row's `table_name`; only the module system, in SQL, ever
                // reads or writes it.
                FieldDecl::many2one_reference("cursor", "table_name").string("Cursor"),
                // The work the cursor belongs to (`erp_orm::SchemaStep::fingerprint`): a cursor is
                // resumed only by a step with the same fingerprint.
                FieldDecl::char("cursor_fingerprint").string("Cursor belongs to"),
                FieldDecl::text("error").string("Error"),
                FieldDecl::char("sqlstate").string("SQLSTATE of the error"),
            ]
        },
    )
    .table_objects(
        const {
            &[
                TableObjectDecl {
                    name: "_module_key_uniq",
                    kind: TableObjectKind::UniqueIndex {
                        definition: "(module, key)",
                        message: "A post-commit step key is unique per module",
                    },
                },
                TableObjectDecl {
                    name: "_kind_check",
                    kind: TableObjectKind::Constraint {
                        definition: "CHECK (kind IN ('create_index', 'analyze', 'schema_step'))",
                        message: "A post-commit step kind must be create_index, analyze or \
                                  schema_step.",
                    },
                },
                TableObjectDecl {
                    name: "_spec_size_check",
                    kind: TableObjectKind::Constraint {
                        definition: "CHECK (spec IS NULL OR octet_length(spec) <= 4096)",
                        message: "The details of a post-commit step are at most 4096 bytes.",
                    },
                },
                TableObjectDecl {
                    name: "_kind_columns_check",
                    kind: TableObjectKind::Constraint {
                        definition: "CHECK ((kind = 'create_index' AND index_name IS NOT NULL \
                                     AND spec IS NOT NULL) \
                                     OR (kind = 'analyze' AND index_name IS NULL \
                                     AND spec IS NULL) \
                                     OR (kind = 'schema_step' AND index_name IS NULL \
                                     AND spec IS NOT NULL))",
                        message: "An index step has an index name and details; an analyze step \
                                  has neither; a schema step has details only.",
                    },
                },
                TableObjectDecl {
                    name: "_table_name_check",
                    kind: TableObjectKind::Constraint {
                        definition: "CHECK (table_name ~ '^[a-z_][a-z0-9_]{0,62}$' \
                                     AND table_name !~ '^pg_')",
                        message: "The table of a post-commit step is a lower-case name of at \
                                  most 63 bytes that does not start with pg_.",
                    },
                },
                TableObjectDecl {
                    name: "_cursor_fingerprint_check",
                    kind: TableObjectKind::Constraint {
                        definition: "CHECK (cursor_fingerprint IS NULL \
                                     OR octet_length(cursor_fingerprint) <= 128)",
                        message: "The fingerprint of a post-commit step cursor is at most 128 \
                                  characters.",
                    },
                },
                TableObjectDecl {
                    name: "_cursor_check",
                    kind: TableObjectKind::Constraint {
                        definition: "CHECK (\"cursor\" IS NULL OR \"cursor\" > 0)",
                        message: "The cursor of a post-commit step is a record id.",
                    },
                },
                TableObjectDecl {
                    name: "_state_check",
                    kind: TableObjectKind::Constraint {
                        definition: "CHECK (state IN ('pending', 'done', 'failed'))",
                        message: "A post-commit step state must be pending, done or failed.",
                    },
                },
            ]
        },
    )
    .at(file!(), line!());

/// The declarations, in the order the module system passes them.
pub static DECLS: [&ModelDecl; 5] = [
    &CATEGORY_DECL,
    &MODULE_DECL,
    &DEPENDENCY_DECL,
    &EXCLUSION_DECL,
    &POST_COMMIT_DECL,
];

// ---- ir.module.module.unlink: installed rows cannot be removed ----------------

type UnlinkFn = MethodFn<(), ()>;

pub const UNLINK_INSTALLED_MESSAGE: &str =
    "You are trying to remove a module that is installed or will be installed.";

fn module_unlink<'a>(
    tx: &'a mut Transaction,
    records: &'a Records,
    _args: (),
    sup: Super<(), ()>,
) -> BoxFuture<'a, Result<()>> {
    Box::pin(async move {
        if !records.is_empty() {
            let ids = records.ids();
            let rows = tx
                .query(
                    "SELECT 1 FROM ir_module_module WHERE id = ANY($1) AND state IN \
                     ('installed', 'to upgrade', 'to remove', 'to install') LIMIT 1",
                    &[&ids],
                )
                .await?;
            if !rows.is_empty() {
                return Err(Error::user(UNLINK_INSTALLED_MESSAGE));
            }
        }
        sup.call(tx, records, ()).await
    })
}

fn module_unlink_entry() -> &'static (dyn Any + Send + Sync) {
    static F: UnlinkFn = module_unlink;
    &F
}

static MODULE_UNLINK: MethodImpl = MethodImpl {
    addon: "base",
    model: MODULE,
    method: "unlink",
    kind: ImplKind::Override,
    sig_name: std::any::type_name::<UnlinkFn>,
    entry: module_unlink_entry,
    file: file!(),
    line: line!(),
};

// ---- ir.module.module: state, version and demo flag are written by the module system only ----

/// The columns of `ir.module.module` only the module system writes (`store.rs`, with its own SQL):
/// an ORM write of `state` would schedule or fake an install without the operation's checks.
pub const MODULE_GUARDED_FIELDS: [&str; 3] = ["state", "latest_version", "demo"];

/// The message of an ORM `create` or `write` on `ir.module.module` that names a guarded field.
pub fn module_guard_message(fields: &[&str]) -> String {
    format!(
        "ir.module.module: {} written only by the module system (erp_modules::ModuleSystem operations)",
        if fields.len() == 1 {
            format!("field {} is", fields.join(", "))
        } else {
            format!("fields {} are", fields.join(", "))
        }
    )
}

/// The guarded fields `vals` names, in declaration order.
fn guarded_in(vals: &RawVals) -> Vec<&'static str> {
    MODULE_GUARDED_FIELDS
        .into_iter()
        .filter(|field| vals.contains(field))
        .collect()
}

fn module_create<'a>(
    tx: &'a mut Transaction,
    records: &'a Records,
    args: (Vec<RawVals>,),
    sup: Super<(Vec<RawVals>,), Records>,
) -> BoxFuture<'a, Result<Records>> {
    Box::pin(async move {
        for vals in &args.0 {
            let guarded = guarded_in(vals);
            if !guarded.is_empty() {
                return Err(Error::access(module_guard_message(&guarded)));
            }
        }
        sup.call(tx, records, args).await
    })
}

fn module_write<'a>(
    tx: &'a mut Transaction,
    records: &'a Records,
    args: (RawVals,),
    sup: Super<(RawVals,), ()>,
) -> BoxFuture<'a, Result<()>> {
    Box::pin(async move {
        let guarded = guarded_in(&args.0);
        if !guarded.is_empty() {
            return Err(Error::access(module_guard_message(&guarded)));
        }
        sup.call(tx, records, args).await
    })
}

fn module_create_entry() -> &'static (dyn Any + Send + Sync) {
    static F: CreateFn = module_create;
    &F
}

fn module_write_entry() -> &'static (dyn Any + Send + Sync) {
    static F: WriteFn = module_write;
    &F
}

static MODULE_CREATE: MethodImpl = MethodImpl {
    addon: "base",
    model: MODULE,
    method: "create",
    kind: ImplKind::Override,
    sig_name: std::any::type_name::<CreateFn>,
    entry: module_create_entry,
    file: file!(),
    line: line!(),
};

static MODULE_WRITE: MethodImpl = MethodImpl {
    addon: "base",
    model: MODULE,
    method: "write",
    kind: ImplKind::Override,
    sig_name: std::any::type_name::<WriteFn>,
    entry: module_write_entry,
    file: file!(),
    line: line!(),
};

// ---- ir.module.category parent cycles ---------------------------------------

pub const RECURSIVE_CATEGORY_MESSAGE: &str = "Error ! You cannot create recursive categories.";

/// Most ancestors walked by the cycle check; a deeper chain is refused as a cycle would be
/// (bounded work on data read from the database).
pub const MAX_CATEGORY_ANCESTORS: i32 = 1_000;

fn check_parent_not_circular<'a>(
    tx: &'a mut Transaction,
    records: &'a Records,
    _args: (),
    _sup: Super<(), ()>,
) -> BoxFuture<'a, Result<()>> {
    Box::pin(async move {
        if records.is_empty() {
            return Ok(());
        }
        tx.flush().await?;
        let ids = records.ids();
        // Walk up from each record; a path that meets its start again is a cycle. The depth
        // bound stops a walk on a cycle that does not contain the start (already refused when it
        // was created) and on absurdly deep chains.
        let rows = tx
            .query(
                "WITH RECURSIVE up(start, id, depth) AS ( \
                   SELECT c.id, c.parent_id, 1 FROM ir_module_category c WHERE c.id = ANY($1) \
                   UNION ALL \
                   SELECT up.start, c.parent_id, up.depth + 1 FROM up \
                   JOIN ir_module_category c ON c.id = up.id \
                   WHERE up.id <> up.start AND up.depth < $2 \
                 ) \
                 SELECT 1 FROM up WHERE id = start OR depth >= $2 LIMIT 1",
                &[&ids, &MAX_CATEGORY_ANCESTORS],
            )
            .await?;
        if rows.is_empty() {
            Ok(())
        } else {
            Err(Error::validation(RECURSIVE_CATEGORY_MESSAGE))
        }
    })
}

fn check_parent_entry() -> &'static (dyn Any + Send + Sync) {
    static F: MethodFn<(), ()> = check_parent_not_circular;
    &F
}

static CATEGORY_CHECK_PARENT: MethodImpl = MethodImpl {
    addon: "base",
    model: CATEGORY,
    method: "_check_parent_not_circular",
    kind: ImplKind::Define,
    sig_name: std::any::type_name::<MethodFn<(), ()>>,
    entry: check_parent_entry,
    file: file!(),
    line: line!(),
};

static CATEGORY_CHECK_PARENT_CONSTRAINT: ConstraintImpl = ConstraintImpl {
    addon: "base",
    model: CATEGORY,
    method: "_check_parent_not_circular",
    fields: &["parent_id"],
    file: file!(),
    line: line!(),
};

// ---- ir.module.post_commit: written by the module system only ----------------

/// The message of every ORM write on `ir.module.post_commit`: its rows hold SQL that runs with
/// the module system's database rights, so only `schedule_post_commit` adds them.
pub const POST_COMMIT_WRITE_MESSAGE: &str = "ir.module.post_commit is written only by the module \
                                             system (erp_modules::schedule_post_commit)";

type CreateFn = MethodFn<(Vec<RawVals>,), Records>;
type WriteFn = MethodFn<(RawVals,), ()>;

fn post_commit_create<'a>(
    _tx: &'a mut Transaction,
    _records: &'a Records,
    _args: (Vec<RawVals>,),
    _sup: Super<(Vec<RawVals>,), Records>,
) -> BoxFuture<'a, Result<Records>> {
    Box::pin(async move { Err(Error::access(POST_COMMIT_WRITE_MESSAGE)) })
}

fn post_commit_write<'a>(
    _tx: &'a mut Transaction,
    _records: &'a Records,
    _args: (RawVals,),
    _sup: Super<(RawVals,), ()>,
) -> BoxFuture<'a, Result<()>> {
    Box::pin(async move { Err(Error::access(POST_COMMIT_WRITE_MESSAGE)) })
}

fn post_commit_unlink<'a>(
    _tx: &'a mut Transaction,
    _records: &'a Records,
    _args: (),
    _sup: Super<(), ()>,
) -> BoxFuture<'a, Result<()>> {
    Box::pin(async move { Err(Error::access(POST_COMMIT_WRITE_MESSAGE)) })
}

fn post_commit_create_entry() -> &'static (dyn Any + Send + Sync) {
    static F: CreateFn = post_commit_create;
    &F
}

fn post_commit_write_entry() -> &'static (dyn Any + Send + Sync) {
    static F: WriteFn = post_commit_write;
    &F
}

fn post_commit_unlink_entry() -> &'static (dyn Any + Send + Sync) {
    static F: UnlinkFn = post_commit_unlink;
    &F
}

static POST_COMMIT_CREATE: MethodImpl = MethodImpl {
    addon: "base",
    model: POST_COMMIT,
    method: "create",
    kind: ImplKind::Override,
    sig_name: std::any::type_name::<CreateFn>,
    entry: post_commit_create_entry,
    file: file!(),
    line: line!(),
};

static POST_COMMIT_WRITE: MethodImpl = MethodImpl {
    addon: "base",
    model: POST_COMMIT,
    method: "write",
    kind: ImplKind::Override,
    sig_name: std::any::type_name::<WriteFn>,
    entry: post_commit_write_entry,
    file: file!(),
    line: line!(),
};

static POST_COMMIT_UNLINK: MethodImpl = MethodImpl {
    addon: "base",
    model: POST_COMMIT,
    method: "unlink",
    kind: ImplKind::Override,
    sig_name: std::any::type_name::<UnlinkFn>,
    entry: post_commit_unlink_entry,
    file: file!(),
    line: line!(),
};

/// Method implementations of the metadata models.
pub static IMPLS: [&MethodImpl; 7] = [
    &MODULE_UNLINK,
    &MODULE_CREATE,
    &MODULE_WRITE,
    &CATEGORY_CHECK_PARENT,
    &POST_COMMIT_CREATE,
    &POST_COMMIT_WRITE,
    &POST_COMMIT_UNLINK,
];

/// Constraints of the metadata models.
pub static CONSTRAINTS: [&ConstraintImpl; 1] = [&CATEGORY_CHECK_PARENT_CONSTRAINT];

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, reason = "tests")]

    use erp_registry::TableObjectKind;

    use super::*;
    use crate::graph::ModuleState;

    /// Every state a module row can be stored in, written without a wildcard: a new
    /// [`ModuleState`] variant does not compile until it is placed here, and then the test below
    /// compares it with [`STATES`].
    fn stored(state: ModuleState) -> bool {
        match state {
            ModuleState::Uninstalled
            | ModuleState::Installed
            | ModuleState::ToInstall
            | ModuleState::ToUpgrade
            | ModuleState::ToRemove
            | ModuleState::Uninstallable => true,
            ModuleState::Unavailable => false,
        }
    }

    const ALL_STATES: [ModuleState; 7] = [
        ModuleState::Uninstalled,
        ModuleState::Installed,
        ModuleState::ToInstall,
        ModuleState::ToUpgrade,
        ModuleState::ToRemove,
        ModuleState::Uninstallable,
        ModuleState::Unavailable,
    ];

    /// The selection of `ir.module.module.state` is exactly the stored [`ModuleState`]s, with
    /// their labels.
    #[test]
    fn module_states_match_module_state() {
        let mut from_selection: Vec<ModuleState> = Vec::new();
        for (value, label) in STATES {
            let state = ModuleState::from_db(value)
                .unwrap_or_else(|| panic!("'{value}' is not a stored module state"));
            assert_eq!(state.as_str(), *value);
            assert_eq!(state.label(), *label, "label of '{value}'");
            from_selection.push(state);
        }
        let mut expected: Vec<ModuleState> =
            ALL_STATES.into_iter().filter(|s| stored(*s)).collect();
        expected.sort();
        from_selection.sort();
        assert_eq!(from_selection, expected);
    }

    /// The `CHECK` constraints on a post-commit row repeat the limits of the action types.
    #[test]
    fn post_commit_checks_match_the_action_limits() {
        let definition = |name: &str| {
            let object = POST_COMMIT_DECL
                .table_objects
                .iter()
                .find(|o| o.name == name)
                .unwrap();
            let TableObjectKind::Constraint { definition, .. } = object.kind else {
                panic!("{name} is not a constraint");
            };
            definition
        };
        assert_eq!(
            definition("_spec_size_check"),
            format!(
                "CHECK (spec IS NULL OR octet_length(spec) <= {})",
                crate::post_commit::MAX_ACTION_SPEC_BYTES
            )
        );
        let kinds: Vec<String> = crate::post_commit::KINDS
            .iter()
            .map(|(v, _)| format!("'{v}'"))
            .collect();
        assert_eq!(
            definition("_kind_check"),
            format!("CHECK (kind IN ({}))", kinds.join(", "))
        );
    }

    /// The `CHECK` constraint on the state column lists the same values as the selection.
    #[test]
    fn state_check_constraints_list_the_selection() {
        for (decl, selection) in [
            (&MODULE_DECL, STATES),
            (&POST_COMMIT_DECL, POST_COMMIT_STATES),
        ] {
            let object = decl
                .table_objects
                .iter()
                .find(|o| o.name == "_state_check")
                .unwrap();
            let TableObjectKind::Constraint { definition, .. } = object.kind else {
                panic!("_state_check of {} is not a constraint", decl.name);
            };
            let values: Vec<String> = selection.iter().map(|(v, _)| format!("'{v}'")).collect();
            assert_eq!(
                definition,
                format!("CHECK (state IN ({}))", values.join(", ")),
                "{}",
                decl.name
            );
        }
    }
}
