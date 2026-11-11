//! The web client's model methods (GRP §4.8–4.12): `web_read`, `web_search_read`,
//! `web_name_search`, `web_save`, `web_save_multi`, `web_resequence`, `web_unlink`,
//! `formatted_read_group`, `formatted_read_grouping_sets`, `web_read_group`, `read_progress_bar`
//! and the `onchange` protocol.
//!
//! The methods are the web/JSON-2 boundary: their dynamic arguments go through the one input
//! converter of the `Value` layer and malformed client input is a `Validation`
//! error (422), never a silent default.
//!
//! # Per-call shape bounds
//!
//! Here each list argument whose length drives the call's work —
//! [`MAX_SAVE_MULTI_RECORDS`], [`MAX_RESEQUENCE_RECORDS`],
//! [`MAX_ONCHANGE_BATCH_ENTRIES`](crate::MAX_ONCHANGE_BATCH_ENTRIES), the JSON-2 binders'
//! group-list bound in `rpc/core.rs`, and the specification's
//! [`MAX_SPEC_DEPTH`] and [`MAX_SPEC_FIELDS`] — has a fixed bound, refused with a `Validation`
//! error naming the bound and the length received (standards §4: untrusted input is bounded;
//! caller learns it from the error and splits the work; the call as a whole stays bounded by the
//! request body (`limits.rpc_body_bytes`) and the deadline (`limits.rpc_deadline_secs`,
//! §3). Each constant documents why its own number was chosen.

mod groups;
pub(crate) mod panel;
pub(crate) mod read;
mod save;
pub(crate) mod spec;

use std::any::Any;
use std::collections::BTreeMap;
use std::sync::Arc;

use erp_domain::{Domain, Operator};
use erp_fields::{Error, Result, Value, ValueMap};

pub use self::panel::{
    SEARCH_PANEL_ERROR_MESSAGE, SearchPanelSelectMultiRange, SearchPanelSelectRange,
};
pub use self::save::{MAX_RESEQUENCE_RECORDS, MAX_SAVE_MULTI_RECORDS};
use self::spec::Kwargs;
pub use self::spec::{MAX_SPEC_DEPTH, MAX_SPEC_FIELDS, Spec, SpecItem, domain_from_value};

/// `web_name_search(operator: str = 'ilike')`.
pub(crate) const WEB_NAME_SEARCH_OPERATOR: Operator = Operator::ILike;
/// `web_resequence(field_name: str = 'sequence')`.
pub(crate) const WEB_RESEQUENCE_FIELD: &str = "sequence";
/// `web_resequence(offset: int = 0)`.
pub(crate) const WEB_RESEQUENCE_OFFSET: i64 = 0;
/// `web_read_group(unfold_read_default_limit: int | None = 80)`
/// .
pub(crate) const UNFOLD_READ_DEFAULT_LIMIT: u64 = 80;
use crate::Transaction;
use crate::dispatch::{BoxFuture, ImplKind, MethodFn, MethodImpl, Super, call_method};
use crate::records::Records;

/// Arguments of `web_search_read`.
#[derive(Debug, Clone)]
pub struct WebSearchRead {
    /// Records to find.
    pub domain: Domain,
    /// What to read of them.
    pub specification: Spec,
    /// Records to skip.
    pub offset: u64,
    /// At most this many records.
    pub limit: Option<u64>,
    /// Order (`None`: the model's).
    pub order: Option<String>,
    /// Stop counting at this number when the page is full.
    pub count_limit: Option<u64>,
}

impl WebSearchRead {
    /// Search `domain`, read `specification`.
    pub fn new(domain: Domain, specification: Spec) -> Self {
        Self {
            domain,
            specification,
            offset: 0,
            limit: None,
            order: None,
            count_limit: None,
        }
    }
}

/// Arguments of `web_name_search`.
#[derive(Debug, Clone)]
pub struct WebNameSearch {
    /// The name to look for.
    pub name: String,
    /// What to read of the records found.
    pub specification: Spec,
    /// Further restriction.
    pub domain: Domain,
    /// How names are compared (`ilike`).
    pub operator: Operator,
    /// At most this many records (`Some(100)`).
    pub limit: Option<u64>,
}

/// Arguments of `web_save`.
#[derive(Debug, Clone)]
pub struct WebSave {
    /// Values to write (or to create a record with, on an empty recordset).
    pub vals: ValueMap,
    /// What to read back.
    pub specification: Spec,
    /// Read this record instead of the saved one.
    pub next_id: Option<i64>,
}

/// Arguments of `formatted_read_group`.
#[derive(Debug, Clone)]
pub struct FormattedReadGroup {
    /// Records to group.
    pub domain: Domain,
    /// Group-by specs.
    pub groupby: Vec<String>,
    /// Aggregate specs (`:recordset` is read as `:array_agg`).
    pub aggregates: Vec<String>,
    /// `having` clause items.
    pub having: Vec<Value>,
    /// Groups to skip.
    pub offset: u64,
    /// At most this many groups.
    pub limit: Option<u64>,
    /// Group order (`None`: the group-by specs, many2one by comodel order).
    pub order: Option<String>,
    /// Also count the groups before limit and offset ([`FormattedGroups::group_count`]).
    pub count_groups: bool,
}

impl FormattedReadGroup {
    /// Group the records of `domain` by `groupby`.
    pub fn new<S: Into<String>>(domain: Domain, groupby: impl IntoIterator<Item = S>) -> Self {
        Self {
            domain,
            groupby: groupby.into_iter().map(Into::into).collect(),
            aggregates: Vec::new(),
            having: Vec::new(),
            offset: 0,
            limit: None,
            order: None,
            count_groups: false,
        }
    }

    /// Set the aggregates.
    #[must_use]
    pub fn aggregates<S: Into<String>>(mut self, specs: impl IntoIterator<Item = S>) -> Self {
        self.aggregates = specs.into_iter().map(Into::into).collect();
        self
    }
}

/// The result of `formatted_read_group`: one dictionary per group.
#[derive(Debug, Clone, PartialEq)]
pub struct FormattedGroups {
    /// The groups.
    pub groups: Vec<ValueMap>,
    /// The number of groups before limit and offset, when it was asked for.
    pub group_count: Option<u64>,
}

impl FormattedGroups {
    /// The groups as a list value (the JSON-2 result).
    pub fn to_value(&self) -> Value {
        Value::list(self.groups.iter().map(|g| Value::Map(Arc::new(g.clone()))))
    }
}

/// Arguments of `formatted_read_grouping_sets`.
#[derive(Debug, Clone)]
pub struct FormattedReadGroupingSets {
    /// Records to group.
    pub domain: Domain,
    /// One list of group-by specs per grouping.
    pub grouping_sets: Vec<Vec<String>>,
    /// Aggregate specs.
    pub aggregates: Vec<String>,
    /// Group order.
    pub order: Option<String>,
}

/// Arguments of `web_read_group`.
#[derive(Debug, Clone)]
pub struct WebReadGroup {
    /// Records to group.
    pub domain: Domain,
    /// Group-by specs, one per level.
    pub groupby: Vec<String>,
    /// Aggregate specs (`__count` is always added).
    pub aggregates: Vec<String>,
    /// At most this many first-level groups.
    pub limit: Option<u64>,
    /// First-level groups to skip.
    pub offset: u64,
    /// Search-style order (`field desc, …`), mapped to each level.
    pub order: Option<String>,
    /// Open the first groups (kanban).
    pub auto_unfold: bool,
    /// Which groups the client has open (reload).
    pub opening_info: Option<Vec<Value>>,
    /// What to read of the records of opened groups.
    pub unfold_read_specification: Option<Spec>,
    /// Records per opened group (`Some(80)`).
    pub unfold_read_default_limit: Option<u64>,
    /// What to read of the records grouped on, per relational group-by spec.
    pub groupby_read_specification: Option<BTreeMap<String, Spec>>,
}

impl WebReadGroup {
    pub fn new<S: Into<String>>(domain: Domain, groupby: impl IntoIterator<Item = S>) -> Self {
        Self {
            domain,
            groupby: groupby.into_iter().map(Into::into).collect(),
            aggregates: Vec::new(),
            limit: None,
            offset: 0,
            order: None,
            auto_unfold: false,
            opening_info: None,
            unfold_read_specification: None,
            unfold_read_default_limit: Some(UNFOLD_READ_DEFAULT_LIMIT),
            groupby_read_specification: None,
        }
    }
}

/// Arguments of `read_progress_bar`.
#[derive(Debug, Clone)]
pub struct ProgressBar {
    /// Records counted.
    pub domain: Domain,
    /// The kanban column group-by spec.
    pub group_by: String,
    /// The progress field.
    pub field: String,
    /// The progress values counted (the `colors` keys).
    pub colors: Vec<String>,
}

impl Records {
    /// `web_read(specification)` through the method chain: one dictionary per existing record.
    pub async fn web_read(&self, tx: &mut Transaction, specification: Spec) -> Result<Vec<Value>> {
        call_method::<(Spec,), Vec<Value>>(tx, self, "web_read", (specification,)).await
    }

    /// `web_search_read(...)`: `{'length': n, 'records': [...]}`.
    pub async fn web_search_read(
        &self,
        tx: &mut Transaction,
        args: WebSearchRead,
    ) -> Result<Value> {
        call_method::<(WebSearchRead,), Value>(tx, self, "web_search_read", (args,)).await
    }

    /// `web_name_search(...)`.
    pub async fn web_name_search(
        &self,
        tx: &mut Transaction,
        args: WebNameSearch,
    ) -> Result<Vec<Value>> {
        call_method::<(WebNameSearch,), Vec<Value>>(tx, self, "web_name_search", (args,)).await
    }

    /// `web_save(vals, specification, next_id)`: write (or create on an empty recordset), then
    /// read back.
    pub async fn web_save(&self, tx: &mut Transaction, args: WebSave) -> Result<Vec<Value>> {
        call_method::<(WebSave,), Vec<Value>>(tx, self, "web_save", (args,)).await
    }

    /// `web_save_multi(vals_list, specification)`.
    pub async fn web_save_multi(
        &self,
        tx: &mut Transaction,
        vals_list: Vec<ValueMap>,
        specification: Spec,
    ) -> Result<Vec<Value>> {
        call_method::<(Vec<ValueMap>, Spec), Vec<Value>>(
            tx,
            self,
            "web_save_multi",
            (vals_list, specification),
        )
        .await
    }

    /// `web_resequence(specification, field_name, offset)`. The method chain takes the field name
    /// owned: pass a `String` (or `Cow`) to move it, a `&str` to copy it once.
    pub async fn web_resequence(
        &self,
        tx: &mut Transaction,
        specification: Spec,
        field_name: impl Into<String>,
        offset: i64,
    ) -> Result<Vec<Value>> {
        call_method::<(Spec, String, i64), Vec<Value>>(
            tx,
            self,
            "web_resequence",
            (specification, field_name.into(), offset),
        )
        .await
    }

    pub async fn web_unlink(&self, tx: &mut Transaction) -> Result<bool> {
        call_method::<(), bool>(tx, self, "web_unlink", ()).await
    }

    /// `formatted_read_group(...)`.
    pub async fn formatted_read_group(
        &self,
        tx: &mut Transaction,
        args: FormattedReadGroup,
    ) -> Result<FormattedGroups> {
        call_method::<(FormattedReadGroup,), FormattedGroups>(
            tx,
            self,
            "formatted_read_group",
            (args,),
        )
        .await
    }

    /// `formatted_read_grouping_sets(...)`.
    pub async fn formatted_read_grouping_sets(
        &self,
        tx: &mut Transaction,
        args: FormattedReadGroupingSets,
    ) -> Result<Vec<Vec<Value>>> {
        call_method::<(FormattedReadGroupingSets,), Vec<Vec<Value>>>(
            tx,
            self,
            "formatted_read_grouping_sets",
            (args,),
        )
        .await
    }

    /// `web_read_group(...)`: `{'groups': [...], 'length': n}`.
    pub async fn web_read_group(&self, tx: &mut Transaction, args: WebReadGroup) -> Result<Value> {
        call_method::<(WebReadGroup,), Value>(tx, self, "web_read_group", (args,)).await
    }

    /// `read_progress_bar(domain, group_by, progress_bar)`.
    pub async fn read_progress_bar(
        &self,
        tx: &mut Transaction,
        args: ProgressBar,
    ) -> Result<Value> {
        call_method::<(ProgressBar,), Value>(tx, self, "read_progress_bar", (args,)).await
    }

    /// `search_panel_select_range(...)`: category values, hierarchy, and counters.
    ///
    /// # Errors
    ///
    /// `Access` for denied model/field/comodel reads, `Validation` for invalid field names,
    /// `UserError` for unsupported category types, and `Programming` for invalid grouped rows
    /// or hierarchy invariants. A reached positive display limit is returned as `error_msg`.
    pub async fn search_panel_select_range(
        &self,
        tx: &mut Transaction,
        args: SearchPanelSelectRange,
    ) -> Result<Value> {
        call_method::<(SearchPanelSelectRange,), Value>(
            tx,
            self,
            "search_panel_select_range",
            (args,),
        )
        .await
    }

    /// `search_panel_select_multi_range(...)`: filter values, grouping, and counters.
    ///
    /// # Errors
    ///
    /// `Access` for denied reads, `Validation` for malformed domains/group maps or field names,
    /// `UserError` for unsupported filter/group types, and `Programming` for invalid grouped rows.
    /// A reached positive display limit is returned as `error_msg`.
    pub async fn search_panel_select_multi_range(
        &self,
        tx: &mut Transaction,
        args: SearchPanelSelectMultiRange,
    ) -> Result<Value> {
        call_method::<(SearchPanelSelectMultiRange,), Value>(
            tx,
            self,
            "search_panel_select_multi_range",
            (args,),
        )
        .await
    }

    /// Call a web method with keyword arguments given as dynamic values (the JSON-2 body without
    /// `ids` and `context`): arguments are checked and converted, unknown or malformed ones are
    /// `Validation` errors, then the method runs through its chain. Recordsets in results are
    /// already ids.
    pub async fn web_call(
        &self,
        tx: &mut Transaction,
        method: &str,
        kwargs: &Value,
    ) -> Result<Value> {
        match method {
            "web_read" => {
                let mut k = Kwargs::new("web_read", kwargs)?;
                let spec = k.spec("specification")?;
                k.finish()?;
                Ok(Value::list(self.web_read(tx, spec).await?))
            }
            "web_search_read" => {
                let mut k = Kwargs::new("web_search_read", kwargs)?;
                let args = WebSearchRead {
                    domain: k.domain("domain")?,
                    specification: k.spec("specification")?,
                    // `null` refused, as the JSON-2 binder does (`Defaulted<u64>`)
                    offset: k.u64_present_or("offset", 0)?,
                    limit: k.u64_opt("limit")?,
                    order: k.str_opt("order")?,
                    count_limit: k.u64_opt("count_limit")?,
                };
                k.finish()?;
                self.web_search_read(tx, args).await
            }
            "web_name_search" => {
                let mut k = Kwargs::new("web_name_search", kwargs)?;
                // the same argument rules as the JSON-2 binder (`rpc/core.rs`): `name` required,
                // `domain: null` is no domain, `operator`
                // absent is `ilike` and `null` is refused, `limit: null` is no limit
                let name = k.str_req("name")?;
                let specification = k.spec("specification")?;
                let domain = match k.get("domain") {
                    None | Some(Value::False) => Domain::TRUE,
                    Some(v) => domain_from_value(v)?,
                };
                let operator = k.operator_or("operator", WEB_NAME_SEARCH_OPERATOR)?;
                let limit = k.u64_nullable_or("limit", crate::NAME_SEARCH_LIMIT)?;
                k.finish()?;
                let args = WebNameSearch {
                    name,
                    specification,
                    domain,
                    operator,
                    limit,
                };
                Ok(Value::list(self.web_name_search(tx, args).await?))
            }
            "web_save" => {
                let mut k = Kwargs::new("web_save", kwargs)?;
                let vals = k.map("vals")?;
                let specification = k.spec("specification")?;
                let next_id = match k.get("next_id") {
                    None | Some(Value::False) => None,
                    Some(Value::Int(n)) => Some(*n),
                    Some(other) => {
                        return Err(Error::validation(format!(
                            "web_save(): invalid next_id {other}"
                        )));
                    }
                };
                k.finish()?;
                let args = WebSave {
                    vals,
                    specification,
                    next_id,
                };
                Ok(Value::list(self.web_save(tx, args).await?))
            }
            "web_save_multi" => {
                let mut k = Kwargs::new("web_save_multi", kwargs)?;
                let vals_list = k
                    .list("vals_list")?
                    .into_iter()
                    .map(|v| match v {
                        Value::Map(m) => Ok(m.as_ref().clone()),
                        other => Err(Error::validation(format!(
                            "web_save_multi(): {other} is not a dictionary of values"
                        ))),
                    })
                    .collect::<Result<Vec<_>>>()?;
                let specification = k.spec("specification")?;
                k.finish()?;
                Ok(Value::list(
                    self.web_save_multi(tx, vals_list, specification).await?,
                ))
            }
            "web_resequence" => {
                let mut k = Kwargs::new("web_resequence", kwargs)?;
                // the same argument rules as the JSON-2 binder: `field_name: null` gives `[]`
                // , `offset: null` is refused; every argument is checked first
                let specification = k.spec("specification")?;
                let field_name = k.str_nullable_or("field_name", WEB_RESEQUENCE_FIELD)?;
                let offset = k.i64_present_or("offset", WEB_RESEQUENCE_OFFSET)?;
                k.finish()?;
                let Some(field_name) = field_name else {
                    return Ok(Value::list([]));
                };
                Ok(Value::list(
                    self.web_resequence(tx, specification, field_name, offset)
                        .await?,
                ))
            }
            "web_unlink" => {
                Kwargs::new("web_unlink", kwargs)?.finish()?;
                Ok(Value::Bool(self.web_unlink(tx).await?))
            }
            "formatted_read_group" => {
                let mut k = Kwargs::new("formatted_read_group", kwargs)?;
                let args = FormattedReadGroup {
                    domain: k.domain("domain")?,
                    groupby: k.strings("groupby")?,
                    aggregates: k.strings("aggregates")?,
                    having: k.list("having")?,
                    offset: k.u64_or("offset", 0)?,
                    limit: k.u64_opt("limit")?,
                    order: k.str_opt("order")?,
                    count_groups: false,
                };
                k.finish()?;
                Ok(self.formatted_read_group(tx, args).await?.to_value())
            }
            "formatted_read_grouping_sets" => {
                let mut k = Kwargs::new("formatted_read_grouping_sets", kwargs)?;
                let domain = k.domain("domain")?;
                let grouping_sets = k
                    .list("grouping_sets")?
                    .into_iter()
                    .map(|set| match set {
                        Value::List(items) => items
                            .iter()
                            .map(|v| match v {
                                Value::Str(s) => Ok(s.to_string()),
                                other => Err(Error::validation(format!(
                                    "formatted_read_grouping_sets(): {other} is not a groupby spec"
                                ))),
                            })
                            .collect::<Result<Vec<_>>>(),
                        other => Err(Error::validation(format!(
                            "formatted_read_grouping_sets(): {other} is not a list of groupby specs"
                        ))),
                    })
                    .collect::<Result<Vec<_>>>()?;
                let args = FormattedReadGroupingSets {
                    domain,
                    grouping_sets,
                    aggregates: k.strings("aggregates")?,
                    order: k.str_opt("order")?,
                };
                k.finish()?;
                let sets = self.formatted_read_grouping_sets(tx, args).await?;
                Ok(Value::list(sets.into_iter().map(Value::list)))
            }
            "web_read_group" => {
                let mut k = Kwargs::new("web_read_group", kwargs)?;
                let mut args = WebReadGroup::new(k.domain("domain")?, k.strings("groupby")?);
                args.aggregates = k.strings("aggregates")?;
                args.limit = k.u64_opt("limit")?;
                args.offset = k.u64_or("offset", 0)?;
                args.order = k.str_opt("order")?;
                args.auto_unfold = k.bool_or("auto_unfold", false)?;
                args.opening_info = match k.get("opening_info") {
                    None | Some(Value::False) => None,
                    Some(Value::List(items)) => Some(items.to_vec()),
                    Some(other) => {
                        return Err(Error::validation(format!(
                            "web_read_group(): invalid opening_info {other}"
                        )));
                    }
                };
                args.unfold_read_specification = k.spec_opt("unfold_read_specification")?;
                if k.get("unfold_read_default_limit").is_some() {
                    args.unfold_read_default_limit = k.u64_opt("unfold_read_default_limit")?;
                }
                args.groupby_read_specification = match k.get("groupby_read_specification") {
                    None | Some(Value::False) => None,
                    Some(Value::Map(map)) => Some(
                        map.iter()
                            .map(|(g, s)| Ok((g.clone(), Spec::from_value(s)?)))
                            .collect::<Result<BTreeMap<_, _>>>()?,
                    ),
                    Some(other) => {
                        return Err(Error::validation(format!(
                            "web_read_group(): invalid groupby_read_specification {other}"
                        )));
                    }
                };
                k.finish()?;
                self.web_read_group(tx, args).await
            }
            "read_progress_bar" => {
                let mut k = Kwargs::new("read_progress_bar", kwargs)?;
                let domain = k.domain("domain")?;
                let group_by = k.str_or("group_by", "")?;
                let bar = k.map("progress_bar")?;
                k.finish()?;
                let field = match bar.get("field") {
                    Some(Value::Str(s)) => s.to_string(),
                    other => {
                        return Err(Error::validation(format!(
                            "read_progress_bar(): invalid progress bar field {}",
                            other.map_or_else(|| "None".to_owned(), ToString::to_string)
                        )));
                    }
                };
                let colors = match bar.get("colors") {
                    Some(Value::Map(colors)) => colors.keys().cloned().collect(),
                    other => {
                        return Err(Error::validation(format!(
                            "read_progress_bar(): invalid progress bar colors {}",
                            other.map_or_else(|| "None".to_owned(), ToString::to_string)
                        )));
                    }
                };
                self.read_progress_bar(
                    tx,
                    ProgressBar {
                        domain,
                        group_by,
                        field,
                        colors,
                    },
                )
                .await
            }
            "onchange" => crate::onchange::onchange_call(tx, self, kwargs).await,
            "onchange_batch" => crate::onchange::onchange_batch_call(tx, self, kwargs).await,
            "search_panel_select_range" => {
                let mut k = Kwargs::new("search_panel_select_range", kwargs)?;
                let args = SearchPanelSelectRange {
                    field_name: k.str("field_name")?,
                    category_domain: k.domain_opt("category_domain")?,
                    filter_domain: k.domain_opt("filter_domain")?,
                    search_domain: k.domain_opt("search_domain")?,
                    comodel_domain: k.domain_opt("comodel_domain")?,
                    enable_counters: k.bool_or("enable_counters", false)?,
                    expand: k.bool_or("expand", false)?,
                    hierarchize: k.bool_or("hierarchize", true)?,
                    limit: k.search_panel_limit("limit")?,
                };
                k.finish()?;
                self.search_panel_select_range(tx, args).await
            }
            "search_panel_select_multi_range" => {
                let mut k = Kwargs::new("search_panel_select_multi_range", kwargs)?;
                let args = SearchPanelSelectMultiRange {
                    field_name: k.str("field_name")?,
                    search_domain: k.domain_opt("search_domain")?,
                    category_domain: k.domain_opt("category_domain")?,
                    filter_domain: k.domain_opt("filter_domain")?,
                    comodel_domain: k.domain_opt("comodel_domain")?,
                    enable_counters: k.bool_or("enable_counters", false)?,
                    expand: k.bool_or("expand", false)?,
                    group_by: k.str_opt("group_by")?,
                    group_domain: k.value_opt("group_domain"),
                    limit: k.search_panel_limit("limit")?,
                };
                k.finish()?;
                self.search_panel_select_multi_range(tx, args).await
            }
            other => Err(Error::validation(format!(
                "'{}' object has no web method '{other}'",
                self.model_name()
            ))),
        }
    }
}

// ----- the `web` addon's implementations on `base` -------------------------------------------

macro_rules! web_impl {
    (
        $stat:ident, $entry:ident, $imp:ident, $name:literal, $args:ty, $ret:ty,
        |$tx:ident, $records:ident, $a:ident| $body:expr
    ) => {
        fn $imp<'a>(
            $tx: &'a mut Transaction,
            $records: &'a Records,
            $a: $args,
            _sup: Super<$args, $ret>,
        ) -> BoxFuture<'a, Result<$ret>> {
            Box::pin(async move { $body })
        }

        fn $entry() -> &'static (dyn Any + Send + Sync) {
            static F: MethodFn<$args, $ret> = $imp;
            &F
        }

        static $stat: MethodImpl = MethodImpl {
            addon: "web",
            model: "base",
            method: $name,
            kind: ImplKind::Define,
            sig_name: std::any::type_name::<MethodFn<$args, $ret>>,
            entry: $entry,
            file: file!(),
            line: line!(),
        };
    };
}

web_impl!(
    WEB_READ,
    web_read_entry,
    web_read_impl,
    "web_read",
    (Spec,),
    Vec<Value>,
    |tx, records, args| read::web_read(tx, records, &args.0).await
);

web_impl!(
    WEB_SEARCH_READ,
    web_search_read_entry,
    web_search_read_impl,
    "web_search_read",
    (WebSearchRead,),
    Value,
    |tx, records, args| read::web_search_read(tx, records, args.0).await
);

web_impl!(
    WEB_NAME_SEARCH,
    web_name_search_entry,
    web_name_search_impl,
    "web_name_search",
    (WebNameSearch,),
    Vec<Value>,
    |tx, records, args| read::web_name_search(tx, records, args.0).await
);

web_impl!(
    WEB_SAVE,
    web_save_entry,
    web_save_impl,
    "web_save",
    (WebSave,),
    Vec<Value>,
    |tx, records, args| save::web_save(tx, records, args.0).await
);

web_impl!(
    WEB_SAVE_MULTI,
    web_save_multi_entry,
    web_save_multi_impl,
    "web_save_multi",
    (Vec<ValueMap>, Spec),
    Vec<Value>,
    |tx, records, args| save::web_save_multi(tx, records, args.0, args.1).await
);

web_impl!(
    WEB_RESEQUENCE,
    web_resequence_entry,
    web_resequence_impl,
    "web_resequence",
    (Spec, String, i64),
    Vec<Value>,
    |tx, records, args| save::web_resequence(tx, records, args.0, args.1, args.2).await
);

web_impl!(
    WEB_UNLINK,
    web_unlink_entry,
    web_unlink_impl,
    "web_unlink",
    (),
    bool,
    |tx, records, _args| save::web_unlink(tx, records).await
);

web_impl!(
    FORMATTED_READ_GROUP,
    formatted_read_group_entry,
    formatted_read_group_impl,
    "formatted_read_group",
    (FormattedReadGroup,),
    FormattedGroups,
    |tx, records, args| groups::formatted_read_group(tx, records, args.0).await
);

web_impl!(
    FORMATTED_READ_GROUPING_SETS,
    formatted_read_grouping_sets_entry,
    formatted_read_grouping_sets_impl,
    "formatted_read_grouping_sets",
    (FormattedReadGroupingSets,),
    Vec<Vec<Value>>,
    |tx, records, args| groups::formatted_read_grouping_sets(tx, records, args.0).await
);

web_impl!(
    WEB_READ_GROUP,
    web_read_group_entry,
    web_read_group_impl,
    "web_read_group",
    (WebReadGroup,),
    Value,
    |tx, records, args| groups::web_read_group(tx, records, args.0).await
);

web_impl!(
    READ_PROGRESS_BAR,
    read_progress_bar_entry,
    read_progress_bar_impl,
    "read_progress_bar",
    (ProgressBar,),
    Value,
    |tx, records, args| groups::read_progress_bar(tx, records, args.0).await
);

web_impl!(
    ONCHANGE,
    onchange_entry,
    onchange_impl,
    "onchange",
    (ValueMap, Vec<String>, Spec),
    Value,
    |tx, records, args| crate::onchange::onchange(tx, records, args.0, args.1, args.2).await
);

web_impl!(
    ONCHANGE_BATCH,
    onchange_batch_entry,
    onchange_batch_impl,
    "onchange_batch",
    (Vec<ValueMap>, Vec<String>, Spec),
    Vec<Value>,
    |tx, records, args| crate::onchange::onchange_batch(tx, records, args.0, args.1, args.2).await
);

web_impl!(
    SEARCH_PANEL_SELECT_RANGE,
    search_panel_select_range_entry,
    search_panel_select_range_impl,
    "search_panel_select_range",
    (SearchPanelSelectRange,),
    Value,
    |tx, records, args| panel::search_panel_select_range(tx, records, args.0).await
);

web_impl!(
    SEARCH_PANEL_SELECT_MULTI_RANGE,
    search_panel_select_multi_range_entry,
    search_panel_select_multi_range_impl,
    "search_panel_select_multi_range",
    (SearchPanelSelectMultiRange,),
    Value,
    |tx, records, args| panel::search_panel_select_multi_range(tx, records, args.0).await
);

/// The `web` addon's methods on `base`, registered by [`crate::Dispatch::build`].
pub(crate) static BUILTIN_IMPLS: [&MethodImpl; 15] = [
    &WEB_READ,
    &WEB_SEARCH_READ,
    &WEB_NAME_SEARCH,
    &WEB_SAVE,
    &WEB_SAVE_MULTI,
    &WEB_RESEQUENCE,
    &WEB_UNLINK,
    &FORMATTED_READ_GROUP,
    &FORMATTED_READ_GROUPING_SETS,
    &WEB_READ_GROUP,
    &READ_PROGRESS_BAR,
    &ONCHANGE,
    &ONCHANGE_BATCH,
    &SEARCH_PANEL_SELECT_RANGE,
    &SEARCH_PANEL_SELECT_MULTI_RANGE,
];
