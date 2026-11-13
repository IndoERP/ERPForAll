//! Filter records provide named search filters and favourites, either global to a model or scoped
//! to a specific action and optional embedded action context. Favourites are stored unpartitioned
//! across companies and respect subject privacy: filters restricted to specific users (`user_ids`)
//! are never returned to callers outside that subject set.

use std::sync::Arc;

use erp_sdk::domain::{Operator, Value as DomainValue};
use erp_sdk::prelude::*;
use erp_sdk::{Operation, RecId, SearchOptions, ValueMap};

/// `ir.filters`: saved filters and favourites for search views.
#[model(
    name = "ir.filters",
    description = "Filter",
    table = "ir_filters",
    order = "model_id, name, id desc",
    indexes = [(
        "_get_filters_index",
        "(model_id, action_id, embedded_action_id, embedded_parent_res_id)"
    )],
    constraints = [
        (
            "_check_res_id_only_when_embedded_action",
            "CHECK (NOT (embedded_parent_res_id IS NOT NULL AND embedded_action_id IS NULL))",
            "Constraint to ensure that the embedded_parent_res_id is only defined when an embedded action is defined."
        ),
        (
            "_check_sort_json",
            "CHECK (sort IS NULL OR jsonb_typeof(sort::jsonb) = 'array')",
            "Invalid sort definition"
        ),
    ]
)]
pub struct IrFilters {
    /// Filter name.
    #[field(char, required = true, string = "Filter Name")]
    pub name: Option<Arc<str>>,
    /// Users the filter is shared with. If empty, the filter is public to all users.
    #[field(
        many2many,
        comodel = "res.users",
        relation = "ir_filters_res_users_rel",
        column1 = "filter_id",
        column2 = "user_id",
        string = "Users"
    )]
    pub user_ids: Vec<RecId>,
    /// Domain specification JSON string.
    #[field(text, required = true, default = "[]")]
    pub domain: Option<Arc<str>>,
    /// Context specification JSON string.
    #[field(text, required = true, default = "{}")]
    pub context: Option<Arc<str>>,
    /// Sort specification JSON string.
    #[field(char, required = true, default = "[]")]
    pub sort: Option<Arc<str>>,
    /// Model name string.
    #[field(char, required = true, string = "Model")]
    pub model_id: Option<Arc<str>>,
    /// Whether this filter is the default filter for the view.
    #[field(boolean, string = "Default Filter")]
    pub is_default: bool,
    /// The action this filter applies to (empty: applies to all menus/actions for this model).
    #[field(
        many2one,
        comodel = "ir.actions.actions",
        ondelete = "cascade",
        string = "Action"
    )]
    pub action_id: Option<RecId>,
    /// The embedded action this filter is applied to.
    #[field(integer, string = "Embedded Action")]
    pub embedded_action_id: Option<i32>,
    /// The record id the filter should be applied to for embedded actions.
    #[field(integer, string = "Embedded Parent Res Id")]
    pub embedded_parent_res_id: Option<i32>,
    /// Whether this filter is active.
    #[field(boolean, default = true)]
    pub active: bool,
}

#[methods(IrFilters)]
impl IrFilters {
    /// Return the list of filters available for the user on the given model.
    ///
    /// Validates parameter constraints and target model access, then filters matching public
    /// and user-private filters according to action context.
    #[method]
    #[rpc(effect = read, model)]
    pub async fn get_filters(
        this: &RecordSet<IrFilters>,
        tx: &mut Transaction,
        model: String,
        action_id: Option<i64>,
        embedded_action_id: Option<i64>,
        embedded_parent_res_id: Option<i64>,
    ) -> Result<Value> {
        Self::read_filters(
            this,
            tx,
            model,
            action_id,
            embedded_action_id,
            embedded_parent_res_id,
        )
        .await
    }

    /// Alias for `get_filters`.
    #[method]
    #[rpc(effect = read, model)]
    pub async fn erp_get_filters(
        this: &RecordSet<IrFilters>,
        tx: &mut Transaction,
        model: String,
        action_id: Option<i64>,
        embedded_action_id: Option<i64>,
        embedded_parent_res_id: Option<i64>,
    ) -> Result<Value> {
        Self::read_filters(
            this,
            tx,
            model,
            action_id,
            embedded_action_id,
            embedded_parent_res_id,
        )
        .await
    }
}

impl IrFilters {
    async fn read_filters(
        this: &RecordSet<IrFilters>,
        tx: &mut Transaction,
        model: String,
        action_id: Option<i64>,
        embedded_action_id: Option<i64>,
        embedded_parent_res_id: Option<i64>,
    ) -> Result<Value> {
        // Parameter constraint: embedded_parent_res_id requires embedded_action_id
        if embedded_parent_res_id.is_some() && embedded_action_id.is_none() {
            return Err(Error::validation(
                "Constraint to ensure that the embedded_parent_res_id is only defined when an embedded action is defined.",
            ));
        }

        // Validate target model exists in registry and caller has Read access to it
        let env = this.as_records().env();
        if env.registry().model(&model).is_none() {
            return Err(Error::validation(format!(
                "get_filters: unknown model '{model}'"
            )));
        }
        let target_records = env.records(&model)?;
        target_records.check_access(tx, Operation::Read).await?;

        let action_cond = match action_id {
            Some(aid) => Domain::condition(
                "action_id",
                Operator::In,
                DomainValue::List(vec![DomainValue::Int(aid), DomainValue::Bool(false)]),
            )
            .map_err(Error::from)?,
            None => Domain::condition("action_id", Operator::Eq, false).map_err(Error::from)?,
        };

        let embedded_cond =
            match embedded_action_id {
                Some(eaid) => Domain::condition("embedded_action_id", Operator::Eq, eaid)
                    .map_err(Error::from)?,
                None => Domain::condition("embedded_action_id", Operator::Eq, false)
                    .map_err(Error::from)?,
            };

        let parent_cond = match (embedded_action_id, embedded_parent_res_id) {
            (Some(_), Some(pid)) => Domain::condition("embedded_parent_res_id", Operator::Eq, pid)
                .map_err(Error::from)?,
            _ => Domain::condition("embedded_parent_res_id", Operator::Eq, false)
                .map_err(Error::from)?,
        };

        let model_cond =
            Domain::condition("model_id", Operator::Eq, model.as_str()).map_err(Error::from)?;

        let uid = env.uid();
        let user_cond = Domain::condition(
            "user_ids",
            Operator::In,
            DomainValue::List(vec![DomainValue::Int(uid), DomainValue::Bool(false)]),
        )
        .map_err(Error::from)?;

        let active_cond = Domain::condition("active", Operator::Eq, true).map_err(Error::from)?;

        let full_domain = Domain::and([
            action_cond,
            embedded_cond,
            parent_cond,
            model_cond,
            user_cond,
            active_cond,
        ]);

        let filter_records = env.records("ir.filters")?;
        let fields = [
            "name",
            "is_default",
            "domain",
            "context",
            "user_ids",
            "sort",
            "embedded_action_id",
            "embedded_parent_res_id",
        ];

        let results = filter_records
            .search_read(tx, &full_domain, &fields, &SearchOptions::default())
            .await?;

        let mut out = Vec::with_capacity(results.len());
        for row in results {
            let mut map = ValueMap::new();
            for (k, v) in row {
                map.insert(k, v);
            }
            out.push(Value::Map(Arc::new(map)));
        }

        Ok(Value::List(out.into()))
    }
}
