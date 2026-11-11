//! Set-based helpers shared by `res.groups` and `res.users`: the
//! implication graph of the groups, the named groups the rules refer to, display names, and the
//! bulk recompute of the `share` flag.
//!
//! A group is named by its external id (`base.group_user`, ...), read from `ir_model_data` with
//! one statement per call.
//!
//! - **Definition lookups** ([`groups_by_xmlid`]): the user types, the light groups, the regular
//!   marker and the internal groups.
//!
//! A registry without the `ir.model.data` model is a `Programming` error for both kinds: the
//! rules cannot know the groups. Everything that reads the graph loads the whole
//! `res_groups_implied_rel` table once and closes it in memory; it holds a few hundred edges in
//! a real database, and one statement replaces a query per level or per record.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use erp_sdk::prelude::*;
use erp_sdk::{BaseInstallGrant, Command, RawVals, RealId, RecId};

/// External id of the internal-user type group.
pub const GROUP_USER: &str = "base.group_user";
/// External id of the portal user type group.
pub const GROUP_PORTAL: &str = "base.group_portal";
/// External id of the public user type group.
pub const GROUP_PUBLIC: &str = "base.group_public";
pub const GROUP_SYSTEM: &str = "base.group_system";
/// External id of the Access Rights group. Its members are not exempt from the `res.users` field-write
/// mask: only [`GROUP_SYSTEM`] edits other users' fields that are not user-writeable.
pub const GROUP_ERP_MANAGER: &str = "base.group_erp_manager";
/// External id of the marker implied by every regular (non-light) group.
pub const GROUP_USER_REGULAR: &str = "base.group_user_regular";
/// External id of the container of the groups new users get.
pub const DEFAULT_USER_GROUP: &str = "base.default_user_group";

/// The (disjoint) user type groups: internal, portal, public.
pub const USER_TYPE_XMLIDS: [&str; 3] = [GROUP_USER, GROUP_PORTAL, GROUP_PUBLIC];

/// Context key that stops `res.groups.write` from re-applying the regular marker while the
/// marker's own implication is being written.
pub const APPLY_REGULAR_RUNNING: &str = "_apply_regular_group_running";

/// A column of a row, or a `Database` error naming the statement.
macro_rules! column {
    ($row:expr, $index:expr, $statement:expr) => {
        $row.try_get($index).map_err(|e| {
            Error::db(format!(
                "unexpected value in column {} of the {} statement: {e}",
                $index, $statement
            ))
        })?
    };
}

/// The database id a record id stands for: itself, or the origin of a new record. A new record
/// without an origin stands for no row.
pub fn row_id(id: RecId) -> Option<i64> {
    id.as_real()
        .map(RealId::get)
        .or_else(|| id.origin().map(RealId::get))
}

/// Whether Python's `str.isprintable()` is false for `c`: control (Cc), format (Cf), line and
/// paragraph separators (Zl, Zp), space separators other than the ASCII space (Zs) and private
/// use (Co). The standard library has no general-category tables, so unassigned code points
/// (Cn) print as themselves where Python escapes them.
fn python_unprintable(c: char) -> bool {
    let n = u32::from(c);
    c.is_control()
        || matches!(
            n,
            0x00A0
                | 0x00AD
                | 0x0600..=0x0605
                | 0x061C
                | 0x06DD
                | 0x070F
                | 0x08E2
                | 0x1680
                | 0x180E
                | 0x2000..=0x200F
                | 0x2028..=0x202F
                | 0x205F..=0x2064
                | 0x2066..=0x206F
                | 0x3000
                | 0xE000..=0xF8FF
                | 0xFEFF
                | 0xFFF9..=0xFFFB
                | 0x110BD
                | 0x1D173..=0x1D17A
                | 0xE0001
                | 0xE0020..=0xE007F
                | 0xF0000..=0xFFFFD
                | 0x100000..=0x10FFFD
        )
}

/// Python's `repr()` of a string, as the exclusive-groups message prints names: single quotes
/// unless the text holds a single and no double quote, backslash and non-printable characters
/// escaped (`\xNN`, `\uNNNN`, `\UNNNNNNNN`).
///
/// ```
/// use erp_addon_base::group_graph::py_repr;
/// assert_eq!(py_repr("it's"), "\"it's\"");
/// assert_eq!(py_repr("a\u{200b}b"), "'a\\u200bb'");
/// assert_eq!(py_repr("\u{1d173}"), "'\\U0001d173'");
/// ```
pub fn py_repr(text: &str) -> String {
    let quote = if text.contains('\'') && !text.contains('"') {
        '"'
    } else {
        '\''
    };
    let mut out = String::with_capacity(text.len() + 2);
    out.push(quote);
    for c in text.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if c == quote => {
                out.push('\\');
                out.push(c);
            }
            c if python_unprintable(c) => {
                let n = u32::from(c);
                if n <= 0xff {
                    out.push_str(&format!("\\x{n:02x}"));
                } else if n <= 0xffff {
                    out.push_str(&format!("\\u{n:04x}"));
                } else {
                    out.push_str(&format!("\\U{n:08x}"));
                }
            }
            c => out.push(c),
        }
    }
    out.push(quote);
    out
}

/// The ids of the groups named by `xmlids` (`module.name`), `None` for an id that names no
/// group (a definition lookup: the caller skips it). One statement after a flush of pending
/// `ir.model.data` writes.
///
/// # Errors
///
/// `Programming` for an id that is not `module.name`, or when the registry has no `ir.model.data`
/// model (compose `erp_data::model_data::ADDON` and its declaration with `base`); `Database` for
/// a failed statement.
pub async fn groups_by_xmlid(tx: &mut Transaction, xmlids: &[&str]) -> Result<Vec<Option<i64>>> {
    let Some(data) = tx.env().registry().model("ir.model.data").map(|m| m.id) else {
        return Err(Error::programming(
            "the security rules of res.groups and res.users look groups up by external id, but \
             this registry has no ir.model.data model: compose the data addon \
             (erp_data::model_data::ADDON with erp_data::ir_model_data_decl()) with base",
        ));
    };
    if xmlids.is_empty() {
        return Ok(Vec::new());
    }
    let mut modules = Vec::with_capacity(xmlids.len());
    let mut names = Vec::with_capacity(xmlids.len());
    for xmlid in xmlids {
        let (module, name) = xmlid.split_once('.').ok_or_else(|| {
            Error::programming(format!("'{xmlid}' is not a module.name external id"))
        })?;
        modules.push(module.to_owned());
        names.push(name.to_owned());
    }
    // an external id created earlier in this transaction may still be a pending ORM write
    tx.flush_model(data).await?;
    let rows = tx
        .query(
            "SELECT module, name, res_id FROM ir_model_data \
             WHERE model = 'res.groups' \
               AND (module, name) IN (SELECT * FROM unnest($1::text[], $2::text[]))",
            &[&modules, &names],
        )
        .await?;
    let mut found: HashMap<(String, String), i64> = HashMap::with_capacity(rows.len());
    for row in &rows {
        let module: String = column!(row, 0, "ir_model_data");
        let name: String = column!(row, 1, "ir_model_data");
        let res_id: Option<i64> = column!(row, 2, "ir_model_data");
        if let Some(res_id) = res_id {
            found.insert((module, name), res_id);
        }
    }
    Ok(modules
        .into_iter()
        .zip(names)
        .map(|key| found.get(&key).copied())
        .collect())
}

/// The id of the group `xmlid` names, if any (a definition lookup).
///
/// # Errors
///
/// As [`groups_by_xmlid`].
pub async fn group_by_xmlid(tx: &mut Transaction, xmlid: &str) -> Result<Option<i64>> {
    Ok(groups_by_xmlid(tx, &[xmlid]).await?.pop().flatten())
}

/// The state lives in the transaction object
/// ([`Transaction::base_installing`]), not in the database: no `SET` can turn it on.
#[must_use]
pub fn base_installing(tx: &Transaction) -> bool {
    tx.base_installing()
}

/// Mark this transaction as loading `base`'s data, for the rest of the transaction. Only the data
/// loader calls it, with the [`BaseInstallGrant`] it received from the ORM build
/// (`OrmBuilder::build_with_base_install_grant`), and only a transaction opened as the `System` principal
/// may be marked; while it is on, the required lookups tolerate missing groups and the
/// last-administrator rule is skipped. Logged.
///
/// # Errors
///
/// `Access` when the grant is another Orm's or the transaction was not opened as `System`.
pub fn begin_base_install(tx: &mut Transaction, grant: &BaseInstallGrant) -> Result<()> {
    tx.begin_base_install(grant)
}

/// Clear the "base is installing" state (always allowed).
pub fn end_base_install(tx: &mut Transaction) {
    tx.end_base_install();
}

/// Make concurrent changes of the same users' memberships conflict (review D2).
///
/// The disjointness check
/// therefore *updates* each affected user row first (`SET id = id`, a real new row version, in
/// id order): the transaction that reaches the row second waits for the first and then fails
/// with a serialization failure (`40001`), which surfaces as a retryable `Concurrency` error. A
/// row lock alone would not do: under `REPEATABLE READ` only a row changed since the snapshot
/// conflicts. The rows are locked first in one ordered `SELECT ... FOR NO KEY UPDATE`, so two
/// transactions lock overlapping sets in the same order. The update does not change a column
/// (usually a HOT update; not guaranteed with the default fillfactor), so it costs one statement, a
/// dead tuple and some WAL per user, and fires no policy trigger.
///
/// # Errors
///
/// `Database` for a failed statement; `Concurrency` for a serialization failure or deadlock.
pub(crate) async fn lock_users_for_type_check(tx: &mut Transaction, users: &[i64]) -> Result<()> {
    if users.is_empty() {
        return Ok(());
    }
    let mut ids = users.to_vec();
    ids.sort_unstable();
    ids.dedup();
    tx.execute(
        "WITH locked AS (SELECT id FROM res_users WHERE id = ANY($1) ORDER BY id \
                          FOR NO KEY UPDATE) \
         UPDATE res_users u SET id = u.id FROM locked l WHERE u.id = l.id",
        &[&ids],
    )
    .await?;
    Ok(())
}

/// What a `res.groups` create or write does to the implication graph (the `implied_ids` and
/// `implied_by_ids` it names), for [`members_under_changed_implications`].
#[derive(Debug, Default)]
pub(crate) struct ImplicationChange {
    /// The existing groups written (empty for a create).
    groups: Vec<i64>,
    /// Groups linked into `implied_ids`.
    implied: Vec<i64>,
    /// Groups linked into `implied_by_ids`.
    implying: Vec<i64>,
    /// A value whose effect cannot be read (a plain value of another shape, a nested create or
    /// update, a record without a row): every member of every user type is locked, the widest
    /// set.
    unknown: bool,
}

/// Which side of the relation a field links.
#[derive(Clone, Copy)]
enum Side {
    Implied,
    Implying,
}

impl ImplicationChange {
    /// The graph change `vals` make on `groups` (the existing groups of a write; empty for a
    /// create). Only links and replacements can add an implication; removals add none. A field
    /// given as commands lands in `RawVals::commands`; `RawVals::contains` is true only for a
    /// plain value, which is a full replacement.
    pub(crate) fn new(groups: Vec<i64>, vals: &RawVals) -> Self {
        let mut change = Self {
            groups,
            ..Self::default()
        };
        change.read(vals, "implied_ids", Side::Implied);
        change.read(vals, "implied_by_ids", Side::Implying);
        change
    }

    fn read(&mut self, vals: &RawVals, field: &str, side: Side) {
        let mut named: Vec<RecId> = Vec::new();
        match vals.get(field) {
            None | Some(Cell::Null) => {}
            Some(Cell::Ids(ids)) => named.extend(ids.iter().copied()),
            Some(_) => self.unknown = true,
        }
        for command in vals.commands(field).unwrap_or_default() {
            match command {
                Command::Link(id) => named.push(*id),
                Command::Set(ids) => named.extend(ids.iter().copied()),
                Command::Create(_) | Command::Update(..) => self.unknown = true,
                Command::Delete(_) | Command::Unlink(_) | Command::Clear => {}
            }
        }
        for id in named {
            match row_id(id) {
                Some(row) => match side {
                    Side::Implied => self.implied.push(row),
                    Side::Implying => self.implying.push(row),
                },
                // a new record without an origin has no row to lock around
                None => self.unknown = true,
            }
        }
    }

    /// Whether the vals name any implication at all.
    fn is_empty(&self) -> bool {
        self.implied.is_empty() && self.implying.is_empty() && !self.unknown
    }
}

/// The user type groups and the implication graph, read once for a group create or write that
/// changes implications (the `internal` set is what the share recomputation compares against).
/// `Graph::load` reads the `res_groups_implied_rel` rows this transaction sees, which includes its
/// own earlier writes (relation writes go to the table directly), so no flush is needed.
pub(crate) struct GraphView {
    /// The user type groups that exist.
    pub(crate) types: Vec<i64>,
    /// `base.group_user` and every group implying it; empty when it does not exist.
    pub(crate) internal: BTreeSet<i64>,
    pub(crate) graph: Graph,
}

impl GraphView {
    /// One lookup of the user type groups and one read of the graph.
    ///
    /// # Errors
    ///
    /// As [`groups_by_xmlid`].
    pub(crate) async fn load(tx: &mut Transaction) -> Result<Self> {
        let found = groups_by_xmlid(tx, &USER_TYPE_XMLIDS).await?;
        let graph = Graph::load(tx).await?;
        let internal = match found.first().copied().flatten() {
            Some(user) => graph.down([user]),
            None => BTreeSet::new(),
        };
        Ok(Self {
            types: found.into_iter().flatten().collect(),
            internal,
            graph,
        })
    }
}

/// Serialize graph changes: update the user type group rows in id order, the
/// same probe as [`lock_users_for_type_check`] but on `res_groups`. Two graph changes can each
/// be valid against a snapshot without the other and still form a chain together (G implies
/// portal, one adds G -> H, the other H -> internal), and no user row is shared between them, so
/// they must meet on a row every graph change touches. Taken before the user locks of the same
/// call (groups, then users). That is not one global order: a group create with `user_ids`, or a
/// write of `user_ids` together with `privilege_id` or `sequence`, locks its users first and the
/// nested `_apply_group_regular` write then runs this probe, so against a concurrent graph change
/// two transactions can lock in opposite orders and deadlock (`40P01`, retried as `Concurrency`:
/// a liveness cost, not a correctness one). Nothing is locked with fewer than two user types: there is
/// nothing to be disjoint.
///
/// # Errors
///
/// `Database` for a failed statement; `Concurrency` for a serialization failure or deadlock.
pub(crate) async fn lock_type_groups(tx: &mut Transaction, types: &[i64]) -> Result<()> {
    if types.len() < 2 {
        return Ok(());
    }
    let mut ids = types.to_vec();
    ids.sort_unstable();
    ids.dedup();
    tx.execute(
        "WITH l AS (SELECT id FROM res_groups WHERE id = ANY($1) ORDER BY id \
                     FOR NO KEY UPDATE) \
         UPDATE res_groups g SET id = g.id FROM l WHERE g.id = l.id",
        &[&ids],
    )
    .await?;
    Ok(())
}

/// Call [`lock_type_groups`] first.
///
/// A change can put a user into a second user type in two ways, and each needs a lock set:
/// - users who are members of the groups that gain the implication (the written groups and every
///   group implying them, transitively; for `implied_by_ids` the newly implying groups and their
///   own implying closure): their effective groups change, so a concurrent change of *their*
///   memberships (which locks the user) conflicts;
/// - users who hold one of the *other* user types: a concurrent transaction may add such a user
///   to a group that is gaining a user type here. That user is not a member yet, so no
///   membership row names it; the closure of the other user types is the superset of who it can
///   be. Only taken when a user type is newly reached, so a data reload that re-links existing
///   implications locks nothing.
///
/// Pairs (T1 changes the graph, T2 changes memberships): T2 changing a locked user's memberships
/// from the user side or the group side takes the same row lock in `lock_users_for_type_check`,
/// so the second to arrive waits and then fails with a serialization failure. Two graph changes
/// meet on the type group rows ([`lock_type_groups`]). A user created at the same time has no
/// row to lock (known gap).
///
/// One read for the members; the types and the graph come from the caller's [`GraphView`].
///
/// # Errors
///
/// `Database` for a failed statement.
pub(crate) async fn members_under_changed_implications(
    tx: &mut Transaction,
    view: &GraphView,
    changes: &[ImplicationChange],
) -> Result<Vec<i64>> {
    let types = &view.types;
    let graph = &view.graph;
    if changes.iter().all(ImplicationChange::is_empty) || types.len() < 2 {
        return Ok(Vec::new());
    }
    let mut affected: BTreeSet<i64> = BTreeSet::new();
    for change in changes {
        if change.unknown {
            affected.extend(graph.down(types.iter().copied()));
            affected.extend(graph.down(change.groups.iter().copied()));
            affected.extend(graph.down(change.implying.iter().copied()));
            continue;
        }
        // the user types each side reaches after the change
        let own_up = graph.up(change.groups.iter().copied());
        let gained: BTreeSet<i64> = graph
            .up(change.implied.iter().copied())
            .into_iter()
            .filter(|g| types.contains(g) && !own_up.contains(g))
            .collect();
        let reached: BTreeSet<i64> = types
            .iter()
            .copied()
            .filter(|t| own_up.contains(t) || gained.contains(t))
            .collect();
        let new_implying: Vec<i64> = {
            let down = graph.down(change.groups.iter().copied());
            change
                .implying
                .iter()
                .copied()
                .filter(|h| !down.contains(h))
                .collect()
        };
        // the types the users of the locked groups gain; the others are what they may clash with
        let mut gained_by_members: Option<BTreeSet<i64>> = None;
        if !gained.is_empty() {
            affected.extend(graph.down(change.groups.iter().copied()));
            gained_by_members = Some(gained.clone());
        }
        if !new_implying.is_empty() && !reached.is_empty() {
            affected.extend(graph.down(new_implying));
            gained_by_members = Some(match gained_by_members {
                // `gained` is a subset of `reached`, so it is what both sides gain together
                Some(g) => g,
                None => reached.clone(),
            });
        }
        if let Some(held) = gained_by_members {
            affected.extend(graph.down(types.iter().copied().filter(|t| !held.contains(t))));
        }
    }
    if affected.is_empty() {
        return Ok(Vec::new());
    }
    let groups: Vec<i64> = affected.into_iter().collect();
    let rows = tx
        .query(
            "SELECT DISTINCT uid FROM res_groups_users_rel WHERE gid = ANY($1)",
            &[&groups],
        )
        .await?;
    let mut users = Vec::with_capacity(rows.len());
    for row in &rows {
        users.push(column!(row, 0, "res_groups_users_rel"));
    }
    Ok(users)
}

/// One lookup of `required` and `optional` groups (definition lookups for the optional ones,
/// required lookups for the others): the ids in the order given, `None` for a missing group.
/// A missing required group is a `Programming` error unless the base module is installing
/// ([`base_installing`]); callers then treat `None` as "the base module is installing: skip".
///
/// # Errors
///
/// `Programming` naming the missing required external ids outside the base installation;
/// otherwise as [`groups_by_xmlid`].
pub async fn lookup_groups(
    tx: &mut Transaction,
    required: &[&str],
    optional: &[&str],
) -> Result<(Vec<Option<i64>>, Vec<Option<i64>>)> {
    let all: Vec<&str> = required.iter().chain(optional).copied().collect();
    let mut found = groups_by_xmlid(tx, &all).await?;
    let optional_found = found.split_off(required.len());
    if found.iter().any(Option::is_none) && !base_installing(tx) {
        let missing: Vec<String> = required
            .iter()
            .zip(&found)
            .filter(|(_, id)| id.is_none())
            .map(|(xmlid, _)| format!("'{xmlid}'"))
            .collect();
        return Err(Error::programming(format!(
            "the group {} does not exist in this database; the rules of res.users and \
             res.groups need it unless the base module is installing (the data loader sets \
             group_graph::begin_base_install)",
            missing.join(", ")
        )));
    }
    Ok((found, optional_found))
}

/// # Errors
///
/// As [`lookup_groups`].
pub async fn required_groups(tx: &mut Transaction, xmlids: &[&str]) -> Result<Vec<Option<i64>>> {
    Ok(lookup_groups(tx, xmlids, &[]).await?.0)
}

/// The id of the group `xmlid` names, which must exist unless the base module is installing.
///
/// # Errors
///
/// As [`lookup_groups`].
pub async fn required_group(tx: &mut Transaction, xmlid: &str) -> Result<Option<i64>> {
    Ok(required_groups(tx, &[xmlid]).await?.pop().flatten())
}

/// The implication graph: the `implied_ids` relation of `res.groups`, closed on demand.
#[derive(Debug, Default)]
pub struct Graph {
    implied: HashMap<i64, Vec<i64>>,
    implying: HashMap<i64, Vec<i64>>,
}

impl Graph {
    /// Read the whole relation (one statement).
    ///
    /// # Errors
    ///
    /// `Database` for a failed statement.
    pub async fn load(tx: &mut Transaction) -> Result<Self> {
        let rows = tx
            .query("SELECT gid, hid FROM res_groups_implied_rel", &[])
            .await?;
        let mut graph = Self::default();
        for row in &rows {
            let gid: i64 = column!(row, 0, "res_groups_implied_rel");
            let hid: i64 = column!(row, 1, "res_groups_implied_rel");
            graph.implied.entry(gid).or_default().push(hid);
            graph.implying.entry(hid).or_default().push(gid);
        }
        Ok(graph)
    }

    fn close(
        edges: &HashMap<i64, Vec<i64>>,
        start: impl IntoIterator<Item = i64>,
    ) -> BTreeSet<i64> {
        let mut seen: BTreeSet<i64> = BTreeSet::new();
        let mut todo: Vec<i64> = start.into_iter().collect();
        while let Some(id) = todo.pop() {
            if !seen.insert(id) {
                continue;
            }
            if let Some(next) = edges.get(&id) {
                todo.extend(next.iter().copied().filter(|n| !seen.contains(n)));
            }
        }
        seen
    }

    /// The groups implied by `ids`, `ids` included (reflexive transitive closure; a cycle just
    /// puts its members together).
    pub fn up(&self, ids: impl IntoIterator<Item = i64>) -> BTreeSet<i64> {
        Self::close(&self.implied, ids)
    }

    /// The groups implying one of `ids`, `ids` included.
    pub fn down(&self, ids: impl IntoIterator<Item = i64>) -> BTreeSet<i64> {
        Self::close(&self.implying, ids)
    }

    /// The groups directly implied by `id`.
    pub fn direct(&self, id: i64) -> &[i64] {
        self.implied.get(&id).map_or(&[], Vec::as_slice)
    }

    /// The groups that directly imply `id`.
    pub fn direct_by(&self, id: i64) -> &[i64] {
        self.implying.get(&id).map_or(&[], Vec::as_slice)
    }
}

/// The user type groups that exist, in the order of [`USER_TYPE_XMLIDS`].
///
/// # Errors
///
/// As [`groups_by_xmlid`].
pub async fn user_type_groups(tx: &mut Transaction) -> Result<Vec<i64>> {
    Ok(groups_by_xmlid(tx, &USER_TYPE_XMLIDS)
        .await?
        .into_iter()
        .flatten()
        .collect())
}

/// The ids of the groups whose members are internal users: `base.group_user` and every group
/// implying it; empty when that group does not exist.
///
/// # Errors
///
/// As [`groups_by_xmlid`].
pub async fn internal_group_ids(tx: &mut Transaction) -> Result<BTreeSet<i64>> {
    let Some(user) = group_by_xmlid(tx, GROUP_USER).await? else {
        return Ok(BTreeSet::new());
    };
    Ok(Graph::load(tx).await?.down([user]))
}

/// Groups of `ids` by the users' direct memberships (one statement).
///
/// # Errors
///
/// `Database` for a failed statement.
pub async fn direct_groups(
    tx: &mut Transaction,
    user_ids: &[i64],
) -> Result<BTreeMap<i64, Vec<i64>>> {
    let rows = tx
        .query(
            "SELECT uid, gid FROM res_groups_users_rel WHERE uid = ANY($1) ORDER BY uid, gid",
            &[&user_ids],
        )
        .await?;
    let mut groups: BTreeMap<i64, Vec<i64>> = BTreeMap::new();
    for row in &rows {
        let uid: i64 = column!(row, 0, "res_groups_users_rel");
        let gid: i64 = column!(row, 1, "res_groups_users_rel");
        groups.entry(uid).or_default().push(gid);
    }
    Ok(groups)
}

/// `(id, display name)` of the groups among `ids`, in the order of `res.groups` (the privilege in
/// the order of `res.groups.privilege`: sequence, name, id; groups without privilege last; then
/// sequence, name, id). The display name is `Privilege / Group`, or the group's name alone.
///
/// # Errors
///
/// `Database` for a failed statement.
pub async fn display_names(tx: &mut Transaction, ids: &[i64]) -> Result<Vec<(i64, String)>> {
    let rows = tx
        .query(
            "SELECT g.id, CASE WHEN p.id IS NULL THEN g.name ELSE p.name || ' / ' || g.name END \
               FROM res_groups g LEFT JOIN res_groups_privilege p ON p.id = g.privilege_id \
              WHERE g.id = ANY($1) \
              ORDER BY p.sequence, p.name, p.id, g.sequence, g.name, g.id",
            &[&ids],
        )
        .await?;
    let mut out = Vec::with_capacity(rows.len());
    for row in &rows {
        let id: i64 = column!(row, 0, "res_groups");
        let name: String = column!(row, 1, "res_groups");
        out.push((id, name));
    }
    Ok(out)
}

/// One `UPDATE` that touches only the rows whose value changes, then the ORM cache of
/// the column is dropped. `users` restricts it to some users.
///
/// `res.partner.partner_share` follows the users when the model exists;
/// `base` has no `res.partner` yet (the partner model arrives with), so nothing else
/// is written.
///
/// # Errors
///
/// `Database` for a failed statement.
pub async fn recompute_user_share(tx: &mut Transaction, users: Option<&[i64]>) -> Result<()> {
    // a pending `share`, `active` or membership write must reach the table before the set-based
    // statement reads and rewrites it
    tx.flush().await?;
    let internal: Vec<i64> = internal_group_ids(tx).await?.into_iter().collect();
    let sql = "\
        WITH _share AS (\
            SELECT u.id, NOT EXISTS (SELECT 1 FROM res_groups_users_rel r \
                                      WHERE r.uid = u.id AND r.gid = ANY($1)) AS share \
              FROM res_users u \
             WHERE ($2::int8[] IS NULL OR u.id = ANY($2))) \
        UPDATE res_users u SET share = s.share \
          FROM _share s \
         WHERE s.id = u.id AND s.share IS DISTINCT FROM u.share";
    let only: Option<Vec<i64>> = users.map(<[i64]>::to_vec);
    let changed = tx.execute(sql, &[&internal, &only]).await?;
    if changed > 0 {
        tx.invalidate_model("res.users", Some(&["share"])).await?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, reason = "tests")]

    use std::sync::Arc;

    use super::*;

    fn real(id: i64) -> RecId {
        RecId::from_real(id).unwrap()
    }

    fn virtual_id() -> RecId {
        RecId::new_unique(1).unwrap()
    }

    fn change(vals: &RawVals) -> ImplicationChange {
        ImplicationChange::new(vec![7], vals)
    }

    /// An absent field names nothing.
    #[test]
    fn absent_fields_name_no_implication() {
        let c = change(&RawVals::new());
        assert!(c.is_empty());
        assert_eq!(c.groups, vec![7]);
    }

    /// Link and Set add implications on the field's side; the five other commands add none (the
    /// create and update ones are unreadable, so they set `unknown`).
    #[test]
    fn each_command_is_read_for_the_side_it_links() {
        let cases: [(Command, Vec<i64>, bool); 7] = [
            (Command::create(RawVals::new()), vec![], true),
            (Command::update(real(1), RawVals::new()), vec![], true),
            (Command::delete(real(1)), vec![], false),
            (Command::unlink(real(1)), vec![], false),
            (Command::link(real(1)), vec![1], false),
            (Command::clear(), vec![], false),
            (Command::set([real(2), real(3)]), vec![2, 3], false),
        ];
        for (command, linked, unknown) in cases {
            let vals = RawVals::new().set_commands("implied_ids", [command.clone()]);
            let c = change(&vals);
            assert_eq!(
                (c.implied, c.implying, c.unknown),
                (linked.clone(), vec![], unknown)
            );
            let vals = RawVals::new().set_commands("implied_by_ids", [command]);
            let c = change(&vals);
            assert_eq!(
                (c.implied, c.implying, c.unknown),
                (vec![], linked, unknown)
            );
        }
    }

    /// A plain list is a replacement: its ids count as links; another shape is unreadable.
    #[test]
    fn plain_values_are_read_or_widened() {
        let ids: Arc<[RecId]> = Arc::from(vec![real(4), real(5)]);
        let c = change(&RawVals::new().set_cell("implied_by_ids", Cell::Ids(ids)));
        assert_eq!((c.implying, c.unknown), (vec![4, 5], false));
        let c = change(&RawVals::new().set_cell("implied_ids", Cell::Str("x".into())));
        assert!(c.unknown);
    }

    /// A record without a row cannot be locked around: the change widens, in commands and in a
    /// plain list.
    #[test]
    fn a_virtual_id_widens_the_change() {
        let c = change(&RawVals::new().set_commands("implied_ids", [Command::link(virtual_id())]));
        assert!(c.unknown);
        let ids: Arc<[RecId]> = Arc::from(vec![real(4), virtual_id()]);
        let c = change(&RawVals::new().set_cell("implied_by_ids", Cell::Ids(ids)));
        assert!(c.unknown);
        assert_eq!(c.implying, vec![4]);
    }
}
