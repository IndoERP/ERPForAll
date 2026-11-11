//! `ir.ui.menu`: the menu tree (design, §6.4).
//! The tree is a
//! `_parent_store` (`parent_path`), so a cycle is the parent store's error. Which menus a user
//! sees (`_visible_menu_ids`, `load_menus`) and `complete_name` are.

use std::collections::{BTreeMap, BTreeSet};

use erp_sdk::ErrorKind;
use erp_sdk::prelude::*;
use erp_sdk::{Load, Operation, RawVals};

pub const MENUS_CACHE: &str = "ir.ui.menu";

/// `ir.ui.menu`.
#[model(
    name = "ir.ui.menu",
    description = "Menu",
    order = "sequence, id",
    parent_store = true,
    parent_name = "parent_id"
)]
pub struct IrUiMenu {
    /// Menu label.
    #[field(char, required = true, string = "Menu")]
    pub name: Option<Arc<str>>,
    /// Active flag.
    #[field(boolean, default = true)]
    pub active: bool,
    /// Order among its siblings (lower first).
    #[field(integer, default = 10)]
    pub sequence: i32,
    /// The enclosing menu.
    #[field(
        many2one,
        comodel = "ir.ui.menu",
        ondelete = "restrict",
        index = true,
        string = "Parent Menu"
    )]
    pub parent_id: Option<RecId>,
    /// The materialized path of the tree.
    #[field(char, index = true)]
    pub parent_path: Option<Arc<str>>,
    /// The menus inside it.
    #[field(
        one2many,
        comodel = "ir.ui.menu",
        inverse_name = "parent_id",
        string = "Child IDs"
    )]
    pub child_id: Vec<RecId>,
    /// Groups that see the menu; none: everybody.
    #[field(
        many2many,
        comodel = "res.groups",
        relation = "ir_ui_menu_group_rel",
        column1 = "menu_id",
        column2 = "gid",
        string = "Groups"
    )]
    pub group_ids: Vec<RecId>,
    /// Icon of a top-level menu (`module,path` or a data URL).
    #[field(char, string = "Web Icon File")]
    pub web_icon: Option<Arc<str>>,
    /// The action it opens, as `model,id`; the report slice adds its model with
    /// `selection_add`.
    #[field(
        reference,
        selection = [
            ("ir.actions.act_window", "ir.actions.act_window"),
            ("ir.actions.act_url", "ir.actions.act_url"),
            ("ir.actions.server", "ir.actions.server"),
            ("ir.actions.client", "ir.actions.client")
        ]
    )]
    pub action: Option<Arc<str>>,
    /// Hierarchical path from root to this menu, joined by `" / "`.
    #[field(char, compute = "_compute_complete_name", string = "Full Path")]
    pub complete_name: Option<Arc<str>>,
}

fn ids_domain(ids: &[i64]) -> Result<Domain> {
    Domain::condition("id", Operator::In, ids.to_vec()).map_err(Error::from)
}

fn menu_group_ids(row: &BTreeMap<String, Value>) -> Result<Vec<erp_sdk::access::GroupId>> {
    let Some(raw) = row.get("group_ids") else {
        return Err(Error::validation("Menu metadata is missing 'group_ids'."));
    };
    let items = match raw {
        Value::List(items) => items,
        _ => {
            return Err(Error::validation(
                "Menu metadata 'group_ids' must be a list of positive group IDs.",
            ));
        }
    };
    items
        .iter()
        .map(|val| match val {
            Value::Int(id) if *id > 0 => erp_sdk::access::GroupId::new(*id).map_err(Error::from),
            _ => Err(Error::validation(
                "Menu metadata 'group_ids' must contain only positive group IDs.",
            )),
        })
        .collect()
}

#[cfg(test)]
mod admission_tests {
    use super::*;

    #[test]
    fn malformed_menu_group_rows_never_become_unrestricted_menus() {
        for (value, message) in [
            (None, "Menu metadata is missing 'group_ids'."),
            (
                Some(Value::Bool(false)),
                "Menu metadata 'group_ids' must be a list of positive group IDs.",
            ),
            (
                Some(Value::List(vec![Value::Int(0)].into())),
                "Menu metadata 'group_ids' must contain only positive group IDs.",
            ),
            (
                Some(Value::List(vec![Value::Bool(true)].into())),
                "Menu metadata 'group_ids' must contain only positive group IDs.",
            ),
        ] {
            let mut row = BTreeMap::new();
            if let Some(value) = value {
                row.insert("group_ids".to_owned(), value);
            }
            let error = menu_group_ids(&row).expect_err("malformed groups must refuse");
            assert_eq!(error.kind(), ErrorKind::Validation);
            assert_eq!(error.message(), message);
        }
        let empty = BTreeMap::from([("group_ids".to_owned(), Value::List(vec![].into()))]);
        assert!(menu_group_ids(&empty).unwrap().is_empty());
        let valid = BTreeMap::from([(
            "group_ids".to_owned(),
            Value::List(vec![Value::Int(7)].into()),
        )]);
        assert_eq!(menu_group_ids(&valid).unwrap()[0].get(), 7);
    }
}

#[methods(IrUiMenu)]
impl IrUiMenu {
    #[compute(depends = ["name", "parent_id.complete_name"])]
    fn _compute_complete_name(batch: &mut Batch<'_, IrUiMenu>) -> Result<()> {
        for i in batch.indices() {
            let name = batch.get(i, IrUiMenu::NAME)?;
            let parent_id = batch.get(i, IrUiMenu::PARENT_ID)?;
            let full: Option<Arc<str>> = match parent_id {
                Some(p) => {
                    let parent_name = batch.value_of(IrUiMenu::COMPLETE_NAME, p)?;
                    match (parent_name, name) {
                        (Some(p_name), Some(n)) => Some(format!("{p_name} / {n}").into()),
                        (Some(p_name), None) => Some(p_name),
                        (None, Some(n)) => Some(n),
                        (None, None) => None,
                    }
                }
                None => name,
            };
            batch.set(i, IrUiMenu::COMPLETE_NAME, full)?;
        }
        Ok(())
    }

    /// Whether technical-menu group membership may count for this caller.
    ///
    /// Only a requested debug view for an actual regular/system user is eligible. This reads
    /// the caller's own role under a narrow privileged projection; it grants no model/action
    /// rights and does not change the supplied environment.
    ///
    /// # Errors
    /// Role read/compute failures, a missing current user, or malformed role metadata.
    ///
    /// ```
    /// # async fn example(env: &erp_sdk::Env, tx: &mut erp_sdk::Transaction) -> erp_sdk::Result<()> {
    /// let debug = erp_addon_base::ir_ui_menu::IrUiMenu::visibility_debug(env, tx, true).await?;
    /// let groups_env = env.with_debug(debug);
    /// assert_eq!(groups_env.principal().debug(), debug);
    /// # Ok(())
    /// # }
    /// ```
    pub async fn visibility_debug(
        env: &Env,
        tx: &mut Transaction,
        requested: bool,
    ) -> Result<bool> {
        if !requested {
            return Ok(false);
        }
        let rows = env
            .records("res.users")?
            .sudo(true)
            .browse([env.uid()])?
            .read_with(tx, &["role"], Load::Raw)
            .await?;
        if rows.len() != 1 {
            return Err(Error::invariant(
                "Current user role metadata is unavailable.",
            ));
        }
        match rows[0].get("role") {
            Some(Value::Str(role)) if matches!(role.as_ref(), "regular_user" | "group_system") => {
                Ok(true)
            }
            Some(Value::Str(role)) if role.as_ref() == "light_user" => Ok(false),
            Some(Value::False) => Ok(false), // A noninternal user has no role; technical menus stay hidden.
            _ => Err(Error::invariant(
                "Current user role metadata has an unsupported value.",
            )),
        }
    }

    /// Return the set of menu IDs visible to the current caller.
    pub async fn visible_menu_ids(
        env: &Env,
        tx: &mut Transaction,
        debug: bool,
    ) -> Result<BTreeSet<i64>> {
        const MAX_MENU_ROWS: usize = 4096;
        const MAX_TREE_DEPTH: usize = 32;

        env.companies()?;
        let active_domain = Domain::condition("active", Operator::Eq, true).map_err(Error::from)?;
        let count = env
            .records("ir.ui.menu")?
            .search_count(tx, &active_domain, None)
            .await?;
        if count == 0 {
            return Ok(BTreeSet::new());
        }
        if count > MAX_MENU_ROWS as u64 {
            return Err(Error::new(
                ErrorKind::Validation,
                format!("Menu candidate count {count} exceeds the {MAX_MENU_ROWS}-row limit."),
            ));
        }

        let caller_menus = env
            .records("ir.ui.menu")?
            .search(tx, &active_domain)
            .await?;
        let ids = caller_menus.ids();
        if ids.is_empty() {
            return Ok(BTreeSet::new());
        }

        let selected = env
            .records("ir.ui.menu")?
            .sudo(true)
            .search(tx, &(active_domain & ids_domain(&ids)?))
            .await?;

        let group_rows = selected.read_with(tx, &["group_ids"], Load::Raw).await?;
        let debug = Self::visibility_debug(env, tx, debug).await?;
        let groups_env = env.with_debug(debug);
        let memberships = tx.group_membership(&groups_env).await?;
        let mut visible_menu_candidates = BTreeSet::new();

        // Check group_no_one (debug mode requirement)
        let no_one_id: Option<i64> = if !debug {
            let imd = env
                .records("ir.model.data")?
                .sudo(true)
                .search(
                    tx,
                    &(Domain::condition("module", Operator::Eq, "base")?
                        & Domain::condition("name", Operator::Eq, "group_no_one")?
                        & Domain::condition("model", Operator::Eq, "res.groups")?),
                )
                .await?;
            if let Some(&first_id) = imd.ids().first() {
                let rows = imd
                    .browse([first_id])?
                    .read_with(tx, &["res_id"], Load::Raw)
                    .await?;
                rows.first().and_then(|r| match r.get("res_id") {
                    Some(Value::Int(id)) if *id > 0 => Some(*id),
                    _ => None,
                })
            } else {
                None
            }
        } else {
            None
        };

        for row in &group_rows {
            let id = match row.get("id") {
                Some(Value::Int(id)) if *id > 0 => *id,
                _ => continue,
            };
            let gids = menu_group_ids(row)?;
            if gids.is_empty() {
                visible_menu_candidates.insert(id);
            } else {
                let effective_gids: Vec<_> = if let Some(no_one) = no_one_id {
                    gids.into_iter().filter(|g| g.get() != no_one).collect()
                } else {
                    gids
                };
                if !effective_gids.is_empty() && memberships.has_any(&effective_gids)? {
                    visible_menu_candidates.insert(id);
                }
            }
        }

        if visible_menu_candidates.is_empty() {
            return Ok(BTreeSet::new());
        }

        let candidate_ids: Vec<i64> = visible_menu_candidates.iter().copied().collect();
        let candidate_records = env
            .records("ir.ui.menu")?
            .sudo(true)
            .search(tx, &ids_domain(&candidate_ids)?)
            .await?;
        let candidate_rows = candidate_records
            .read_with(tx, &["parent_id", "action"], Load::Raw)
            .await?;

        let mut parents = BTreeMap::new();
        let mut action_menus = Vec::new();
        for row in &candidate_rows {
            let id = match row.get("id") {
                Some(Value::Int(id)) if *id > 0 => *id,
                _ => continue,
            };
            let parent_id = match row.get("parent_id") {
                Some(Value::Int(pid)) if *pid > 0 => Some(*pid),
                _ => None,
            };
            parents.insert(id, parent_id);
            if let Some(Value::Str(action_str)) = row.get("action").filter(|v| match v {
                Value::Str(s) => !s.is_empty(),
                _ => false,
            }) {
                action_menus.push((id, action_str.to_string()));
            }
        }

        let mut visible_ids = BTreeSet::new();
        let mut authorized_action_menus = Vec::new();
        for (menu_id, action_ref) in action_menus {
            let (model_name, res_id_str) = action_ref.split_once(',').ok_or_else(|| {
                Error::new(
                    ErrorKind::Validation,
                    format!("Malformed menu action reference '{action_ref}' on menu {menu_id}."),
                )
            })?;
            let res_id: i64 = res_id_str.parse().map_err(|_| {
                Error::new(
                    ErrorKind::Validation,
                    format!("Malformed menu action reference id '{res_id_str}' on menu {menu_id}."),
                )
            })?;
            if res_id <= 0 {
                return Err(Error::new(
                    ErrorKind::Validation,
                    format!(
                        "Menu action reference id must be positive, got {res_id} on menu {menu_id}."
                    ),
                ));
            }

            // 1. Verify action model is supported
            if !matches!(
                model_name,
                "ir.actions.act_window"
                    | "ir.actions.act_url"
                    | "ir.actions.client"
                    | "ir.actions.server"
                    | "ir.actions.report"
            ) {
                visible_menu_candidates.remove(&menu_id);
                continue;
            }

            // 2. Caller record-level access check via ORM
            let act_model = env.records(model_name)?;
            let act_records = match act_model.search(tx, &ids_domain(&[res_id])?).await {
                Ok(records) => records,
                Err(e) if e.kind() == ErrorKind::Access => {
                    visible_menu_candidates.remove(&menu_id);
                    continue;
                }
                Err(e) => return Err(e),
            };
            if act_records.ids().is_empty() {
                visible_menu_candidates.remove(&menu_id);
                continue;
            }

            // 3. Action-specific authorization and target model check
            if model_name == "ir.actions.act_window" {
                // The caller has already passed this action's record Read. Group restrictions
                // are authorization metadata: reading their comodel through the caller can hide
                // the very groups that must deny it. Inspect only these fields on the same IDs.
                let action_rows = act_records
                    .sudo(true)
                    .read_with(tx, &["group_ids", "res_model"], Load::Raw)
                    .await?;
                let Some(action_row) = action_rows.first() else {
                    visible_menu_candidates.remove(&menu_id);
                    continue;
                };
                let action_gids = menu_group_ids(action_row)?;
                if !action_gids.is_empty() {
                    let effective_gids: Vec<_> = if let Some(no_one) = no_one_id {
                        action_gids
                            .into_iter()
                            .filter(|g| g.get() != no_one)
                            .collect()
                    } else {
                        action_gids
                    };
                    if effective_gids.is_empty() || !memberships.has_any(&effective_gids)? {
                        visible_menu_candidates.remove(&menu_id);
                        continue;
                    }
                }

                let target_model = match action_row.get("res_model") {
                    Some(Value::Str(m)) if !m.is_empty() => m.to_string(),
                    _ => {
                        visible_menu_candidates.remove(&menu_id);
                        continue;
                    }
                };
                let records = env.records(&target_model)?;
                match records.check_access(tx, Operation::Read).await {
                    Ok(()) => {}
                    Err(e) if e.kind() == ErrorKind::Access => {
                        visible_menu_candidates.remove(&menu_id);
                        continue;
                    }
                    Err(e) => return Err(e),
                }
            } else if model_name == "ir.actions.server" {
                // The caller has already passed this action's record Read. Group restrictions
                // are authorization metadata: reading their comodel through the caller can hide
                // the very groups that must deny it. Inspect only these fields on the same IDs.
                let action_rows = act_records
                    .sudo(true)
                    .read_with(tx, &["group_ids", "model_id"], Load::Raw)
                    .await?;
                if let Some(action_row) = action_rows.first() {
                    let action_gids = menu_group_ids(action_row)?;
                    if !action_gids.is_empty() {
                        let effective_gids: Vec<_> = if let Some(no_one) = no_one_id {
                            action_gids
                                .into_iter()
                                .filter(|g| g.get() != no_one)
                                .collect()
                        } else {
                            action_gids
                        };
                        if effective_gids.is_empty() || !memberships.has_any(&effective_gids)? {
                            visible_menu_candidates.remove(&menu_id);
                            continue;
                        }
                    }
                }
            }

            authorized_action_menus.push(menu_id);
        }

        // Complete every action decision before walking any chain: a child can
        // precede its denied parent in sequence/id order.
        for menu_id in authorized_action_menus {
            // Climb the ancestor chain to root.
            // All ancestors must be in visible_menu_candidates.
            // Protect against cycles and depth > MAX_TREE_DEPTH.
            let mut chain = Vec::new();
            let mut curr = Some(menu_id);
            let mut valid_chain = true;

            while let Some(node_id) = curr {
                if chain.len() >= MAX_TREE_DEPTH {
                    return Err(Error::new(
                        ErrorKind::Validation,
                        format!("Menu tree exceeds the {MAX_TREE_DEPTH}-level limit."),
                    ));
                }
                if chain.contains(&node_id) {
                    return Err(Error::new(
                        ErrorKind::Validation,
                        "Menu metadata contains a parent cycle.",
                    ));
                }
                if !visible_menu_candidates.contains(&node_id) {
                    valid_chain = false; // missing/inaccessible ancestor
                    break;
                }
                chain.push(node_id);
                curr = parents.get(&node_id).copied().flatten();
            }

            if valid_chain {
                visible_ids.extend(chain);
            }
        }

        Ok(visible_ids)
    }

    #[method(override)]
    async fn create(
        this: &RecordSet<IrUiMenu>,
        tx: &mut Transaction,
        vals_list: Vec<RawVals>,
        sup: Super,
    ) -> Result<Records> {
        let created = sup.call(tx, this.as_records(), (vals_list,)).await?;
        tx.invalidate_ormcache(MENUS_CACHE);
        Ok(created)
    }

    #[method(override)]
    async fn write(
        this: &RecordSet<IrUiMenu>,
        tx: &mut Transaction,
        vals: RawVals,
        sup: Super,
    ) -> Result<()> {
        sup.call(tx, this.as_records(), (vals,)).await?;
        tx.invalidate_ormcache(MENUS_CACHE);
        Ok(())
    }

    #[method(override)]
    async fn unlink(this: &RecordSet<IrUiMenu>, tx: &mut Transaction, sup: Super) -> Result<()> {
        sup.call(tx, this.as_records(), ()).await?;
        tx.invalidate_ormcache(MENUS_CACHE);
        Ok(())
    }

    /// Return the list of menu IDs visible to the user.
    #[method]
    async fn _visible_menu_ids(
        this: &RecordSet<IrUiMenu>,
        tx: &mut Transaction,
        debug: Option<bool>,
    ) -> Result<Vec<i64>> {
        let debug = debug.unwrap_or(false);
        let ids = Self::visible_menu_ids(this.env(), tx, debug).await?;
        Ok(ids.into_iter().collect())
    }
}
