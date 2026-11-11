//! The audit columns
//! `create_uid` and `create_date` are the record; the login statement inserts the row itself
//! . Old rows are collected by the authentication collection
//! (`erp_security::auth::gc`, `Auth::gc_from`), which keeps the newest row of each user.
//!
//! The one index, `(create_uid, create_date DESC NULLS LAST, id DESC)`, serves both the lookups
//! by user (its prefix) and the collection, which finds each user's newest row by one top-1 read
//! of it (database review round 2, S2). A database of an earlier revision has `(create_uid)`
//! under the same name: the sync rebuilds it concurrently when the declaration changes.

use erp_sdk::prelude::*;

/// `res.users.log`.
#[model(
    name = "res.users.log",
    description = "Users Log",
    order = "id desc",
    indexes = [(
        "_create_uid_index",
        "(create_uid, create_date DESC NULLS LAST, id DESC)"
    )]
)]
pub struct ResUsersLog {}
