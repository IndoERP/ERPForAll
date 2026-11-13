//! Caller-authorized native menu, action and view metadata RPCs.
//!
//! Database metadata is read through the ORM. The trusted `sudo` reads below are limited to
//! framework-owned presentation rows and run only after the caller's model/record permission
//! checks; target-model and field reads remain bound to the caller.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use erp_addon_base::ir_actions::IrActionsActWindow;
use erp_addon_base::ir_ui_menu::IrUiMenu;
use erp_addon_base::ir_ui_view::select::{ViewId, ViewRequest, ViewType};
use erp_sdk::prelude::*;
use erp_sdk::serde::Serialize;
use erp_sdk::views::{
    DescriptionBudget, DescriptionBuffer, DescriptionEncoder, DescriptionReservation,
    LimitKind as ViewLimitKind, ViewError, ViewLimits, wire,
};
#[cfg(test)]
use erp_sdk::views::{
    ViewCatalogue,
    record::{ViewMode, ViewRow},
};
use erp_sdk::{ErrorKind, FieldSpec, FieldType, Load, ModelSpec, Operation, ValueMap, extend};

#[extend(IrUiMenu, addon = "web")]
struct WebIrUiMenu {}

#[extend(IrActionsActWindow, addon = "web")]
struct WebIrActionsActWindow {}

#[extend(Base, addon = "web")]
struct WebBase {}

const MAX_MENU_ROWS: usize = 4096;
const MAX_VIEW_NODES: usize = 4096;
const MAX_TREE_DEPTH: usize = 32;
const MAX_LABEL_BYTES: usize = 4096;
const MAX_ICON_BYTES: usize = 2048;
const MAX_MENU_ACTION_REF_BYTES: usize = 256;
const MAX_MENU_PAYLOAD_BYTES: usize = 32 * 1024 * 1024;
const MAX_ACTION_MODEL_BYTES: usize = 256;
const MAX_ACTION_EXPR_BYTES: usize = 4096;
const MAX_ACTION_TARGET_BYTES: usize = 32;
const MAX_ACTION_PAYLOAD_BYTES: usize = 16 * 1024;
const MAX_ENCODED_RESPONSE_BYTES: usize = 8 * 1024 * 1024;
const MAX_FIELD_METADATA_BYTES: usize = 2 * 1024 * 1024;
const MAX_METADATA_GROUP_REFS: usize = 65_536;

/// Read only aggregate sizes and relation counts before ORM presentation reads.
/// Callers and the following ORM read run in the same default REPEATABLE READ transaction
/// snapshot, so the selected IDs and the admitted payload describe the same database state.
async fn preflight_stats(
    tx: &mut Transaction,
    sql: &str,
    ids: &[i64],
    columns: usize,
    label: &str,
    admission: &mut ResponseAdmission,
) -> Result<Vec<i64>> {
    let mut values = admission.vector::<i64>(columns)?;
    let rows = tx.query(sql, &[&ids]).await?;
    let row = rows
        .first()
        .ok_or_else(|| Error::invariant(format!("{label} preflight returned no row")))?;
    for index in 0..columns {
        let value: i64 = row
            .try_get(index)
            .map_err(|error| Error::db(format!("unexpected {label} preflight value: {error}")))?;
        if value < 0 {
            return Err(Error::invariant(format!(
                "{label} preflight value is negative"
            )));
        }
        values.push(value);
    }
    Ok(values)
}

fn ensure_below(value: i64, maximum: usize, label: &str, unit: &str) -> Result<()> {
    let maximum = i64::try_from(maximum)
        .map_err(|_| Error::invariant(format!("{label} limit exceeds i64")))?;
    if value > maximum {
        return Err(fail(format!("{label} exceeds the {maximum}-{unit} limit.")));
    }
    Ok(())
}

fn fail(message: impl Into<String>) -> Error {
    Error::validation(message)
}

/// One call's shared description admission (WM-05).
///
/// A single `DescriptionBudget` serves every view, field catalogue and encoded copy of one
/// `get_views`/`load_menus`/action call. Retained storage keeps its reservation here until the
/// call ends, so multi-view requests add up and never reset; a refusal is sticky, and a caught
/// error cannot be followed by a successful serialization.
struct ResponseAdmission {
    budget: DescriptionBudget,
    limits: ViewLimits,
    held: Vec<DescriptionReservation>,
    ledger: Option<DescriptionReservation>,
    ledger_spare: Option<DescriptionReservation>,
    #[cfg(feature = "test-support")]
    stages: Vec<&'static str>,
}

impl ResponseAdmission {
    fn new() -> Self {
        Self {
            budget: DescriptionBudget::new(),
            limits: ViewLimits::DEFAULT,
            held: Vec::new(),
            ledger: None,
            ledger_spare: None,
            #[cfg(feature = "test-support")]
            stages: Vec::new(),
        }
    }

    #[cfg(any(test, feature = "test-support"))]
    fn with_limits(build_bytes: usize, encoded_bytes: usize) -> Result<Self> {
        let limits = ViewLimits::DEFAULT
            .lowered(ViewLimitKind::DescriptionEncodedBytes, encoded_bytes)
            .map_err(|error| fail(error.to_string()))?;
        Ok(Self {
            budget: DescriptionBudget::with_limit(build_bytes)
                .map_err(|error| fail(error.to_string()))?,
            limits,
            held: Vec::new(),
            ledger: None,
            ledger_spare: None,
            #[cfg(feature = "test-support")]
            stages: Vec::new(),
        })
    }

    /// Static checkpoints exist only in the trusted test-support build. The recorder is test
    /// instrumentation, never RPC data or part of the default producer's owned storage.
    fn phase(
        &mut self,
        #[allow(unused_variables, reason = "feature-gated trace")] phase: &'static str,
    ) {
        #[cfg(feature = "test-support")]
        self.stages.push(phase);
    }

    /// Reserve `bytes` of retained storage before it is allocated; held until the call ends.
    fn charge(&mut self, bytes: usize) -> Result<()> {
        let reservation = self.budget.reserve(bytes).map_err(admission_error)?;
        self.hold(reservation)
    }

    /// Reserve the reservation ledger itself before growing it; old and new buffers overlap
    /// during reallocation and both remain charged until the old allocation is released.
    fn hold(&mut self, reservation: DescriptionReservation) -> Result<()> {
        if self.held.len() == self.held.capacity() {
            let count = self
                .held
                .len()
                .checked_add(1)
                .ok_or_else(|| fail("Description reservation storage overflow."))?;
            let bytes = count
                .checked_mul(std::mem::size_of::<DescriptionReservation>())
                .ok_or_else(|| fail("Description reservation storage overflow."))?;
            let slots = self.budget.reserve(bytes).map_err(admission_error)?;
            self.held
                .try_reserve_exact(1)
                .map_err(|_| fail("Description reservation allocation failed."))?;
            let extra = self
                .held
                .capacity()
                .checked_sub(count)
                .and_then(|count| count.checked_mul(std::mem::size_of::<DescriptionReservation>()))
                .ok_or_else(|| fail("Description reservation storage overflow."))?;
            let spare = if extra == 0 {
                None
            } else {
                Some(self.budget.reserve(extra).map_err(admission_error)?)
            };
            self.ledger = Some(slots);
            self.ledger_spare = spare;
        }
        self.held.push(reservation);
        Ok(())
    }

    /// Count formatting without an owned temporary, reserve, then format into admitted storage.
    fn format(&mut self, args: std::fmt::Arguments<'_>) -> Result<String> {
        struct Count(usize);
        impl std::fmt::Write for Count {
            fn write_str(&mut self, text: &str) -> std::fmt::Result {
                self.0 = self.0.checked_add(text.len()).ok_or(std::fmt::Error)?;
                Ok(())
            }
        }
        let mut count = Count(0);
        std::fmt::write(&mut count, args)
            .map_err(|_| fail("Description formatted text storage overflow."))?;
        self.charge(count.0)?;
        let mut text = String::new();
        text.try_reserve_exact(count.0)
            .map_err(|_| fail("Description formatted text allocation failed."))?;
        if text.capacity() > count.0 {
            self.charge(text.capacity() - count.0)?;
        }
        std::fmt::write(&mut text, args)
            .map_err(|_| Error::invariant("admitted description formatting failed"))?;
        Ok(text)
    }

    /// Charge `count` retained values of type `T`.
    fn charge_items<T>(&mut self, count: usize) -> Result<()> {
        let bytes = count
            .checked_mul(std::mem::size_of::<T>())
            .ok_or_else(|| fail("Description collection storage overflow."))?;
        self.charge(bytes)
    }

    /// Allocate exactly the known number of slots before producer-owned materialization.
    fn vector<T>(&mut self, count: usize) -> Result<Vec<T>> {
        self.charge_items::<T>(count)?;
        let mut values = Vec::new();
        values
            .try_reserve_exact(count)
            .map_err(|_| fail("Description collection allocation failed."))?;
        if values.capacity() > count {
            self.charge_items::<T>(values.capacity() - count)?;
        }
        Ok(values)
    }

    /// Growth holds the complete new allocation alongside the old reservation before realloc.
    /// Old reservations are conservatively retained until the request ends.
    fn extend<T>(&mut self, target: &mut Vec<T>, incoming: Vec<T>) -> Result<()> {
        let count = target
            .len()
            .checked_add(incoming.len())
            .ok_or_else(|| fail("Description collection storage overflow."))?;
        if count > target.capacity() {
            self.charge_items::<T>(count)?;
            target
                .try_reserve_exact(count - target.len())
                .map_err(|_| fail("Description collection allocation failed."))?;
            if target.capacity() > count {
                self.charge_items::<T>(target.capacity() - count)?;
            }
        }
        target.extend(incoming);
        Ok(())
    }

    /// Returned read_with maps, their row vector/key scratch, owned text and relation arrays.
    /// Transaction/cache and SQL driver storage remain owned by the ORM, rather than this DTO.
    fn source_rows(
        &mut self,
        rows: usize,
        fields: &[&str],
        text_bytes: usize,
        text_columns: usize,
        relation_items: usize,
        relation_columns: usize,
    ) -> Result<()> {
        let columns = fields
            .len()
            .checked_add(1)
            .ok_or_else(|| fail("Description source storage overflow."))?;
        self.charge_items::<BTreeMap<String, Value>>(rows)?;
        let map = crate::h18_storage::btree_storage_bytes::<String, Value>(columns)?;
        self.charge(
            map.checked_mul(rows)
                .ok_or_else(|| fail("Description source storage overflow."))?,
        )?;
        let names = fields.iter().try_fold("id".len(), |bytes, field| {
            bytes
                .checked_add(field.len())
                .ok_or_else(|| fail("Description source storage overflow."))
        })?;
        self.charge(
            names
                .checked_mul(rows)
                .and_then(|bytes| bytes.checked_add(text_bytes))
                .ok_or_else(|| fail("Description source storage overflow."))?,
        )?;
        self.charge_items::<&str>(columns)?; // read_with's borrowed key vector
        let headers = text_columns
            .checked_add(relation_columns)
            .and_then(|count| count.checked_mul(rows))
            .ok_or_else(|| fail("Description source storage overflow."))?;
        self.charge_items::<(
            std::sync::atomic::AtomicUsize,
            std::sync::atomic::AtomicUsize,
        )>(headers)?;
        self.charge_items::<Value>(relation_items)
    }

    fn fields(&mut self, aggregate: &mut BTreeSet<String>, names: Vec<String>) -> Result<()> {
        for name in names {
            if !aggregate.contains(&name) {
                let before =
                    crate::h18_storage::btree_storage_bytes::<String, ()>(aggregate.len())?;
                let after = crate::h18_storage::btree_storage_bytes::<String, ()>(
                    aggregate
                        .len()
                        .checked_add(1)
                        .ok_or_else(|| fail("Description field storage overflow."))?,
                )?;
                self.charge(
                    after
                        .checked_sub(before)
                        .ok_or_else(|| Error::invariant("Description field envelope shrank"))?,
                )?;
                aggregate.insert(name); // move text; its producer reservation remains held
            }
        }
        Ok(())
    }

    /// Charge one selected view's node vector before it is retained with the others.
    #[cfg(test)]
    fn charge_view_nodes(&mut self, nodes: usize) -> Result<()> {
        self.charge_items::<wire::ViewNode>(nodes)
    }

    /// Charge the field catalogue (entries plus their owned text) before cloning any label.
    fn charge_field_catalogue(&mut self, fields: usize, text_bytes: usize) -> Result<()> {
        let entries =
            crate::h18_storage::btree_storage_bytes::<String, wire::FieldDescription>(fields)?
                .checked_add(text_bytes)
                .ok_or_else(|| fail("Description collection storage overflow."))?;
        self.charge(entries)
    }

    fn encode_within<T: Serialize + ?Sized>(
        &self,
        maximum: usize,
        value: &T,
    ) -> std::result::Result<DescriptionBuffer, ViewError> {
        let limits = self
            .limits
            .lowered(
                ViewLimitKind::DescriptionEncodedBytes,
                maximum.min(self.limits.get(ViewLimitKind::DescriptionEncodedBytes)),
            )
            .map_err(|error| ViewError::InvalidDescription(error.to_string()))?;
        DescriptionEncoder::new(&self.budget, &limits).encode(value)
    }

    fn encode<T: Serialize + ?Sized>(&self, value: &T) -> Result<DescriptionBuffer> {
        self.encode_within(MAX_ENCODED_RESPONSE_BYTES, value)
            .map_err(admission_error)
    }

    /// Materialize once under admission, validate encoded bytes, then retain the JSON transport.
    fn wire<T: Serialize + ?Sized>(&mut self, value: &T) -> Result<Value> {
        self.budget.check().map_err(admission_error)?;
        let json = crate::h18_transport::materialize(value, &mut |bytes| self.charge(bytes))?;
        // Encode the admitted tree, never invoke a potentially stateful Serialize value twice.
        let _encoded = self.encode(&json)?;
        self.charge(transport_arc_bytes()?)?;
        Ok(Value::Json(Arc::new(json)))
    }
}

/// The pinned standard library's repr(C) ArcInner has two atomic counters followed by T.
/// Include tail alignment and the JSON value itself, not merely the outer Arc pointer.
fn transport_arc_bytes() -> Result<usize> {
    use std::{alloc::Layout, sync::atomic::AtomicUsize};
    Layout::new::<(AtomicUsize, AtomicUsize)>()
        .extend(Layout::new::<serde_json::Value>())
        .map(|(layout, _)| layout.pad_to_align().size())
        .map_err(|_| fail("Description transport storage overflow."))
}

/// Map an admission refusal to the RPC error; the encoded ceiling keeps its historical text.
fn admission_error(error: ViewError) -> Error {
    match error {
        ViewError::LimitExceeded {
            limit: ViewLimitKind::DescriptionEncodedBytes,
            max,
            ..
        } => fail(format!("Encoded response exceeds the {max}-byte limit.")),
        ViewError::InvariantViolation(message) => Error::invariant(message),
        other => fail(other.to_string()),
    }
}

fn wire_id(id: i64) -> Result<wire::WireId> {
    let raw = u64::try_from(id).map_err(|_| fail("Metadata identity must be positive."))?;
    wire::WireId::try_from(raw).map_err(|error| fail(error.to_string()))
}

fn value<'a>(row: &'a BTreeMap<String, Value>, field: &str) -> Result<&'a Value> {
    row.get(field)
        .ok_or_else(|| Error::invariant(format!("metadata read omitted '{field}'")))
}

fn optional_id(row: &BTreeMap<String, Value>, field: &str) -> Result<Option<i64>> {
    match value(row, field)? {
        Value::Int(id) if *id > 0 => Ok(Some(*id)),
        Value::False => Ok(None),
        other => Err(Error::invariant(format!(
            "metadata field '{field}' read as {other:?}, not a nullable raw id"
        ))),
    }
}

/// Serving uses only the immutable declarations published for this exact registry generation.
fn published_view_types(env: &Env) -> Result<Arc<erp_sdk::views::PublishedViewTypes>> {
    let published = env.generation_attachment::<erp_sdk::views::PublishedViewTypes>()?;
    published
        .require_registry(env.registry())
        .map_err(admission_error)?;
    Ok(published)
}

fn wire_type(env: &Env, name: &str) -> Result<wire::ViewType> {
    let published = published_view_types(env)?;
    if published.find(name).is_none() {
        return Err(fail(format!("View type '{name}' is not registered.")));
    }
    wire::ViewType::from_wire_str(name).ok_or_else(|| {
        fail(format!(
            "View type '{name}' is not supported by the current human UI contract."
        ))
    })
}

async fn can_read_model(env: &Env, tx: &mut Transaction, model: &str) -> Result<bool> {
    if env.registry().model(model).is_none() {
        return Err(fail(format!("Unknown action model '{model}'.")));
    }
    let records = env.records(model)?;
    match records.check_access(tx, Operation::Read).await {
        Ok(()) => Ok(true),
        Err(error) if error.kind() == ErrorKind::Access => Ok(false),
        Err(error) => Err(error),
    }
}

async fn require_model_read(env: &Env, tx: &mut Transaction, model: &str) -> Result<Records> {
    let records = env.records(model)?;
    records.check_access(tx, Operation::Read).await?;
    Ok(records)
}

fn group_ids(
    row: &BTreeMap<String, Value>,
    field: &str,
    admission: &mut ResponseAdmission,
) -> Result<Vec<erp_sdk::access::GroupId>> {
    let Value::List(values) = value(row, field)? else {
        return Err(Error::invariant(
            "metadata group field is not a raw id list",
        ));
    };
    let mut result = admission.vector(values.len())?;
    for value in values.iter() {
        let Value::Int(id) = value else {
            return Err(Error::invariant(
                "metadata group field contains a non-integer",
            ));
        };
        result.push(
            erp_sdk::access::GroupId::new(*id)
                .map_err(|error| Error::invariant(format!("invalid {field} group id: {error}")))?,
        );
    }
    Ok(result)
}

fn text_value<'a>(row: &'a BTreeMap<String, Value>, field: &str) -> Result<&'a str> {
    match value(row, field)? {
        Value::Str(text) => Ok(text),
        _ => Err(Error::invariant("metadata text field is not text")),
    }
}
fn optional_text<'a>(row: &'a BTreeMap<String, Value>, field: &str) -> Result<Option<&'a str>> {
    match value(row, field)? {
        Value::Str(text) => Ok(Some(text)),
        Value::False => Ok(None),
        _ => Err(Error::invariant(
            "metadata optional text field is not nullable text",
        )),
    }
}
// Rust 1.98.1 raw_vec::grow_amortized selects max(2*old, required, min_non_zero).
// At a growth the old capacity is below final logical count; count+max(2*count,min)
// therefore owns old and replacement together. This is a source bound, not a measured ratio.
fn vec_growth_slots<T>(count: usize) -> Result<usize> {
    if count == 0 {
        return Ok(0);
    }
    let size = std::mem::size_of::<T>();
    let minimum = if size == 1 {
        8
    } else if size <= 1024 {
        4
    } else {
        1
    };
    count
        .checked_mul(2)
        .map(|new| new.max(minimum))
        .and_then(|new| new.checked_add(count))
        .ok_or_else(|| fail("Description vector storage overflow."))
}
fn stat_size(value: i64) -> Result<usize> {
    usize::try_from(value).map_err(|_| fail("Description source storage exceeds usize."))
}
fn source_ids(admission: &mut ResponseAdmission, count: usize) -> Result<()> {
    admission.charge_items::<erp_sdk::RecId>(count)?; // returned recordset ids
    admission.charge_items::<i64>(count) // Records::ids owned projection
}
// Source: erp-domain::domain::{NodeData, NodeKind}. The sum includes every public payload
// shape (rather than guessing the private enum's padding), its tag and depth/hash/fold fields.
// Arc counts cover the node itself; the field-path Arc has an independent header below.
#[repr(C)]
#[allow(
    dead_code,
    reason = "layout-only upper envelope for domain query input"
)]
struct QueryConditionEnvelope {
    arc_counts: (
        std::sync::atomic::AtomicUsize,
        std::sync::atomic::AtomicUsize,
    ),
    tag: isize,
    unary: Domain,
    nary: Vec<Domain>,
    condition: erp_sdk::domain::Condition,
    custom: Arc<dyn erp_sdk::domain::CustomDomain>,
    depth: usize,
    hash: u64,
    fold: u64,
}
fn budget_ids_domain(admission: &mut ResponseAdmission, ids: &[i64]) -> Result<Domain> {
    admission.charge_items::<i64>(ids.len())?;
    admission.charge_items::<erp_sdk::domain::Value>(ids.len())?;
    admission.charge_items::<(
        std::sync::atomic::AtomicUsize,
        std::sync::atomic::AtomicUsize,
    )>(1)?;
    admission.charge(
        std::mem::size_of::<QueryConditionEnvelope>()
            .checked_add("id".len())
            .ok_or_else(|| fail("Description domain storage overflow."))?,
    )?;
    ids_domain(ids)
}

async fn in_groups(
    tx: &mut Transaction,
    env: &Env,
    groups: &[erp_sdk::access::GroupId],
) -> Result<bool> {
    if groups.is_empty() {
        Ok(true)
    } else {
        tx.has_any_group(env, groups).await
    }
}

fn ids_domain(ids: &[i64]) -> Result<Domain> {
    Domain::condition("id", Operator::In, ids.to_vec()).map_err(Error::from)
}

fn checked_search_options(limit: usize) -> Result<SearchOptions> {
    let limit =
        u64::try_from(limit).map_err(|_| Error::invariant("metadata row cap exceeds u64"))?;
    Ok(SearchOptions::default().limit(limit))
}

#[derive(Clone)]
struct MenuRecord {
    id: i64,
    parent: Option<i64>,
    sequence: i64,
    label: String,
    icon: Option<String>,
    groups: Vec<erp_sdk::access::GroupId>,
    action: Option<i64>,
}

/// Resolve only retained menu IDs. SQL reduces aliases and reads sizes/identities; the trusted
/// ORM read below is limited to those framework presentation rows, after caller authorization.
/// No external IDs are created, and no provenance namespace supplies an access grant.
async fn menu_modules(
    env: &Env,
    tx: &mut Transaction,
    ids: &[i64],
    menu_text_bytes: usize,
    admission: &mut ResponseAdmission,
) -> Result<Vec<(i64, Option<String>)>> {
    if ids.is_empty() {
        return Ok(Vec::new());
    }
    let stats = preflight_stats(
        tx,
        "SELECT coalesce(sum(octet_length(d.module)), 0)::bigint, \
                coalesce(max(octet_length(d.module)), 0)::bigint, count(*)::bigint \
           FROM ir_model_data d JOIN ( \
             SELECT min(id) AS id FROM ir_model_data \
              WHERE model = 'ir.ui.menu' AND res_id = ANY($1) GROUP BY res_id \
           ) oldest ON oldest.id = d.id",
        ids,
        3,
        "menu module",
        admission,
    )
    .await?;
    ensure_below(stats[1], MAX_LABEL_BYTES, "Menu module", "byte")?;
    let text_bytes = stat_size(stats[0])?;
    let total_text = menu_text_bytes
        .checked_add(text_bytes)
        .ok_or_else(|| fail("Menu metadata text payload overflow."))?;
    ensure_below(
        i64::try_from(total_text).map_err(|_| fail("Menu metadata text payload exceeds i64."))?,
        MAX_MENU_PAYLOAD_BYTES,
        "Menu metadata text payload",
        "byte",
    )?;
    let count = stat_size(stats[2])?;
    if count > ids.len() {
        return Err(Error::invariant(
            "Menu module selection exceeds retained menus",
        ));
    }
    if count == 0 {
        return Ok(Vec::new());
    }
    admission.phase("menus:modules:reserve");
    let mut selected_ids = admission.vector::<i64>(count)?;
    for row in tx
        .query(
            "SELECT min(id) FROM ir_model_data \
              WHERE model = 'ir.ui.menu' AND res_id = ANY($1) GROUP BY res_id",
            &[&ids],
        )
        .await?
    {
        let id: i64 = row
            .try_get(0)
            .map_err(|error| Error::db(format!("unexpected menu module identity: {error}")))?;
        if id <= 0 || selected_ids.len() >= count {
            return Err(Error::invariant("Invalid menu module selection"));
        }
        selected_ids.push(id);
    }
    if selected_ids.len() != count {
        return Err(Error::invariant(
            "Menu module selection changed within snapshot",
        ));
    }
    source_ids(admission, count)?;
    let selected = env
        .records("ir.model.data")?
        .sudo(true)
        .search_with(
            tx,
            &budget_ids_domain(admission, &selected_ids)?,
            &checked_search_options(count)?,
        )
        .await?;
    admission.source_rows(count, &["res_id", "module"], text_bytes, 1, 0, 0)?;
    let mut modules = admission.vector::<(i64, Option<String>)>(count)?;
    // Own both the ORM source strings and the eventual wire copies before reading either.
    admission.charge(text_bytes)?;
    admission.phase("menus:modules:read");
    let rows = selected
        .read_with(tx, &["res_id", "module"], Load::Raw)
        .await?;
    if rows.len() != count {
        return Err(Error::invariant(
            "Menu module presentation rows are missing",
        ));
    }
    for row in &rows {
        let id = optional_id(row, "res_id")?
            .ok_or_else(|| Error::invariant("Menu module target is missing"))?;
        if ids.binary_search(&id).is_err() {
            return Err(Error::invariant("Menu module target was not retained"));
        }
        let module = match optional_text(row, "module")? {
            Some(text) if !text.is_empty() => Some(text.to_owned()),
            Some(_) | None => None,
        };
        modules.push((id, module));
    }
    modules.sort_unstable_by_key(|(id, _)| *id);
    if modules.windows(2).any(|pair| pair[0].0 == pair[1].0) {
        return Err(Error::invariant("Duplicate menu module target"));
    }
    Ok(modules)
}

async fn load_menus(
    env: &Env,
    tx: &mut Transaction,
    admission: &mut ResponseAdmission,
) -> Result<wire::MenuTree> {
    env.companies()?;
    let caller_menus = require_model_read(env, tx, "ir.ui.menu").await?;
    let count = usize::try_from(
        caller_menus
            .search_count(tx, &Domain::TRUE, Some((MAX_MENU_ROWS + 1) as u64))
            .await?,
    )
    .map_err(|_| fail("Menu row count exceeds usize."))?;
    if count > MAX_MENU_ROWS {
        return Err(fail("Menu metadata exceeds the 4096-node limit."));
    }
    admission.phase("menus:ids:reserve");
    source_ids(admission, count)?;
    admission.phase("menus:ids:search");
    let caller_visible = caller_menus
        .search_with(
            tx,
            &Domain::TRUE,
            &checked_search_options(MAX_MENU_ROWS + 1)?,
        )
        .await?;
    let ids = caller_visible.ids();
    if ids.len() > MAX_MENU_ROWS {
        return Err(fail("Menu metadata exceeds the 4096-node limit."));
    }
    if ids.is_empty() {
        return Ok(wire::MenuTree::new(
            admission.format(format_args!("{}", env.registry().generation()))?,
            Vec::new(),
            Vec::new(),
        ));
    }
    source_ids(admission, ids.len())?;
    let selected = env
        .records("ir.ui.menu")?
        .sudo(true)
        .search_with(
            tx,
            &budget_ids_domain(admission, &ids)?,
            &checked_search_options(MAX_MENU_ROWS + 1)?,
        )
        .await?;
    let selected_ids = selected.ids();
    if selected_ids.len() > MAX_MENU_ROWS {
        return Err(fail("Menu metadata exceeds the 4096-node limit."));
    }
    let menu_group_stats = preflight_stats(
        tx,
        "SELECT coalesce(sum((SELECT count(*) FROM ir_ui_menu_group_rel g \
             WHERE g.menu_id = m.id)), 0)::bigint FROM ir_ui_menu m WHERE m.id = ANY($1)",
        &selected_ids,
        1,
        "menu group",
        admission,
    )
    .await?;
    ensure_below(
        menu_group_stats[0],
        MAX_METADATA_GROUP_REFS,
        "Menu group reference count",
        "item",
    )?;
    // Technical menu eligibility is presentation-only. Model/record reads below continue to use
    // the original caller; only group visibility uses this narrower debug projection.
    if env.principal().debug() {
        source_ids(admission, 1)?;
        admission.source_rows(
            1,
            &["role"],
            "regular_user"
                .len()
                .max("group_system".len())
                .max("light_user".len()),
            1,
            0,
            0,
        )?;
    }
    let debug = IrUiMenu::visibility_debug(env, tx, env.principal().debug()).await?;
    let groups_env = env.with_debug(debug);
    admission.phase("menus:groups:reserve");
    admission.source_rows(
        selected_ids.len(),
        &["group_ids"],
        0,
        0,
        stat_size(menu_group_stats[0])?,
        1,
    )?;
    admission.phase("menus:groups:read");
    let group_rows = selected.read_with(tx, &["group_ids"], Load::Raw).await?;
    let mut visible_ids = admission.vector(group_rows.len())?;
    for row in &group_rows {
        let id = match value(row, "id")? {
            Value::Int(id) if *id > 0 => *id,
            other => return Err(Error::invariant(format!("menu identity read as {other:?}"))),
        };
        if in_groups(tx, &groups_env, &group_ids(row, "group_ids", admission)?).await? {
            visible_ids.push(id);
        }
    }
    if visible_ids.is_empty() {
        return Ok(wire::MenuTree::new(
            admission.format(format_args!("{}", env.registry().generation()))?,
            Vec::new(),
            Vec::new(),
        ));
    }
    let menu_stats = preflight_stats(
        tx,
        "SELECT coalesce(sum(name_bytes + icon_bytes + action_bytes), 0)::bigint, \
                coalesce(max(name_bytes), 0)::bigint, coalesce(max(icon_bytes), 0)::bigint, \
                coalesce(max(action_bytes), 0)::bigint FROM ( \
           SELECT coalesce(octet_length(m.name)::bigint, 0) AS name_bytes, \
                  coalesce(octet_length(m.web_icon)::bigint, 0) AS icon_bytes, \
                  coalesce(octet_length(m.action)::bigint, 0) AS action_bytes \
             FROM ir_ui_menu m WHERE m.id = ANY($1) \
         ) AS menu_payload",
        &visible_ids,
        4,
        "menu metadata",
        admission,
    )
    .await?;
    ensure_below(
        menu_stats[0],
        MAX_MENU_PAYLOAD_BYTES,
        "Menu metadata text payload",
        "byte",
    )?;
    ensure_below(menu_stats[1], MAX_LABEL_BYTES, "Menu label", "byte")?;
    ensure_below(menu_stats[2], MAX_ICON_BYTES, "Menu icon reference", "byte")?;
    ensure_below(
        menu_stats[3],
        MAX_MENU_ACTION_REF_BYTES,
        "Menu action reference",
        "byte",
    )?;
    source_ids(admission, visible_ids.len())?;
    let visible_selected = env
        .records("ir.ui.menu")?
        .sudo(true)
        .search_with(
            tx,
            &budget_ids_domain(admission, &visible_ids)?,
            &checked_search_options(MAX_MENU_ROWS + 1)?,
        )
        .await?;
    admission.phase("menus:text:reserve");
    admission.source_rows(
        visible_ids.len(),
        &[
            "name",
            "parent_id",
            "sequence",
            "group_ids",
            "action",
            "web_icon",
        ],
        stat_size(menu_stats[0])?,
        3,
        stat_size(menu_group_stats[0])?,
        1,
    )?;
    admission.phase("menus:text:read");
    let rows = visible_selected
        .read_with(
            tx,
            &[
                "name",
                "parent_id",
                "sequence",
                "group_ids",
                "action",
                "web_icon",
            ],
            Load::Raw,
        )
        .await?;
    let mut entries = admission.vector::<MenuRecord>(rows.len())?;
    let mut requested_actions = admission.vector::<i64>(rows.len())?;
    for row in &rows {
        let id = value(row, "id")?.clone();
        let id = match id {
            Value::Int(id) if id > 0 => id,
            other => return Err(Error::invariant(format!("menu identity read as {other:?}"))),
        };
        let action = optional_text(row, "action")?
            .map(parse_action_ref)
            .transpose()?;
        if let Some((model, action_id)) = &action {
            if *model != "ir.actions.act_window" {
                return Err(fail(format!(
                    "Menu {id} uses unsupported action model '{model}'."
                )));
            }
            requested_actions.push(*action_id);
        }
        entries.push(MenuRecord {
            id,
            parent: optional_id(row, "parent_id")?,
            sequence: match value(row, "sequence")? {
                Value::Int(value) => *value,
                other => return Err(Error::invariant(format!("menu sequence read as {other:?}"))),
            },
            label: admission.format(format_args!("{}", text_value(row, "name")?))?,
            icon: optional_text(row, "web_icon")?
                .map(|text| admission.format(format_args!("{text}")))
                .transpose()?,
            groups: group_ids(row, "group_ids", admission)?,
            action: action.map(|(_, action_id)| action_id),
        });
    }
    requested_actions.sort_unstable();
    requested_actions.dedup();

    // Preserve caller action ACL/rules before reading framework-owned action presentation data.
    let visible_action_ids = if requested_actions.is_empty()
        || !can_read_model(env, tx, "ir.actions.act_window").await?
    {
        Vec::new()
    } else {
        source_ids(admission, requested_actions.len())?;
        let caller_actions = require_model_read(env, tx, "ir.actions.act_window").await?;
        caller_actions
            .search_with(
                tx,
                &budget_ids_domain(admission, &requested_actions)?,
                &checked_search_options(MAX_MENU_ROWS + 1)?,
            )
            .await?
            .ids()
    };
    let action_rows = if visible_action_ids.is_empty() {
        Vec::new()
    } else {
        source_ids(admission, visible_action_ids.len())?;
        let system_actions = env.records("ir.actions.act_window")?.sudo(true);
        let action_set = system_actions
            .search_with(
                tx,
                &budget_ids_domain(admission, &visible_action_ids)?,
                &checked_search_options(MAX_MENU_ROWS + 1)?,
            )
            .await?;
        let action_ids = action_set.ids();
        if action_ids.len() > MAX_MENU_ROWS {
            return Err(fail("Menu action set exceeds the 4096-action limit."));
        }
        let action_group_stats = preflight_stats(
            tx,
            "SELECT coalesce(sum((SELECT count(*) FROM ir_act_window_group_rel g \
                 WHERE g.act_id = w.id)), 0)::bigint FROM ir_act_window w WHERE w.id = ANY($1)",
            &action_ids,
            1,
            "menu action",
            admission,
        )
        .await?;
        ensure_below(
            action_group_stats[0],
            MAX_METADATA_GROUP_REFS,
            "Menu action group reference count",
            "item",
        )?;
        admission.source_rows(
            action_ids.len(),
            &["group_ids"],
            0,
            0,
            stat_size(action_group_stats[0])?,
            1,
        )?;
        let action_group_rows = action_set.read_with(tx, &["group_ids"], Load::Raw).await?;
        let mut group_allowed_ids = admission.vector(action_group_rows.len())?;
        for row in &action_group_rows {
            let id = match value(row, "id")? {
                Value::Int(id) if *id > 0 => *id,
                other => {
                    return Err(Error::invariant(format!(
                        "action identity read as {other:?}"
                    )));
                }
            };
            if in_groups(tx, &groups_env, &group_ids(row, "group_ids", admission)?).await? {
                group_allowed_ids.push(id);
            }
        }
        if group_allowed_ids.is_empty() {
            Vec::new()
        } else {
            source_ids(admission, group_allowed_ids.len())?;
            let authorized_actions = env
                .records("ir.actions.act_window")?
                .sudo(true)
                .search_with(
                    tx,
                    &budget_ids_domain(admission, &group_allowed_ids)?,
                    &checked_search_options(MAX_MENU_ROWS + 1)?,
                )
                .await?;
            let action_stats = preflight_stats(
                tx,
                "SELECT coalesce(sum(model_bytes), 0)::bigint, \
                        coalesce(max(model_bytes), 0)::bigint FROM ( \
                   SELECT coalesce(octet_length(res_model)::bigint, 0) AS model_bytes \
                     FROM ir_act_window WHERE id = ANY($1) \
                 ) AS action_payload",
                &group_allowed_ids,
                2,
                "menu action",
                admission,
            )
            .await?;
            ensure_below(
                action_stats[0],
                MAX_MENU_ROWS.saturating_mul(MAX_ACTION_MODEL_BYTES),
                "Menu action model-name payload",
                "byte",
            )?;
            ensure_below(
                action_stats[1],
                MAX_ACTION_MODEL_BYTES,
                "Menu action model name",
                "byte",
            )?;
            admission.source_rows(
                group_allowed_ids.len(),
                &["res_model"],
                stat_size(action_stats[0])?,
                1,
                0,
                0,
            )?;
            authorized_actions
                .read_with(tx, &["res_model"], Load::Raw)
                .await?
        }
    };
    let mut allowed_actions = admission.vector::<i64>(action_rows.len())?;
    for row in &action_rows {
        let id = match value(row, "id")? {
            Value::Int(id) if *id > 0 => *id,
            other => {
                return Err(Error::invariant(format!(
                    "action identity read as {other:?}"
                )));
            }
        };
        let model = text_value(row, "res_model")?;
        if can_read_model(env, tx, model).await? {
            allowed_actions.push(id);
        }
    }

    allowed_actions.sort_unstable();
    let mut visible = admission.vector::<MenuRecord>(entries.len())?;
    for entry in entries {
        if !in_groups(tx, &groups_env, &entry.groups).await? {
            continue;
        }
        if entry
            .action
            .is_some_and(|id| allowed_actions.binary_search(&id).is_err())
        {
            continue;
        }
        visible.push(entry);
    }
    visible.sort_unstable_by_key(|entry| entry.id);
    let mut keep = admission.vector::<bool>(visible.len())?;
    keep.resize(visible.len(), false);
    let mut path = admission.vector::<usize>(MAX_TREE_DEPTH)?;
    for seed in visible.iter().filter(|entry| entry.action.is_some()) {
        path.clear();
        let mut cursor = Some(seed.id);
        while let Some(id) = cursor {
            if path.len() >= MAX_TREE_DEPTH {
                return Err(fail("Menu tree exceeds the 32-level limit."));
            }
            let Ok(index) = visible.binary_search_by_key(&id, |entry| entry.id) else {
                path.clear();
                break;
            };
            if path.contains(&index) {
                return Err(fail("Menu metadata contains a parent cycle."));
            }
            path.push(index);
            cursor = visible[index].parent;
        }
        for &index in &path {
            keep[index] = true;
        }
    }
    let mut index = 0usize;
    visible.retain(|_| {
        let retained = keep[index];
        index += 1;
        retained
    });
    let mut ids = admission.vector::<i64>(visible.len())?;
    ids.extend(visible.iter().map(|entry| entry.id));
    let mut modules = menu_modules(env, tx, &ids, stat_size(menu_stats[0])?, admission).await?;
    let mut edges = admission.vector::<(i64, i64, i64)>(visible.len())?;
    let mut roots = admission.vector::<(i64, i64)>(visible.len())?;
    for entry in &visible {
        match entry
            .parent
            .filter(|parent| ids.binary_search(parent).is_ok())
        {
            Some(parent) => edges.push((parent, entry.sequence, entry.id)),
            None => roots.push((entry.sequence, entry.id)),
        }
    }
    edges.sort_unstable();
    roots.sort_unstable();
    let mut wire_roots = admission.vector::<wire::WireId>(roots.len())?;
    for (_, id) in roots {
        wire_roots.push(wire_id(id)?);
    }
    admission.phase("menus:output:reserve");
    let mut wire_nodes = admission.vector::<wire::MenuNode>(visible.len())?;
    admission.phase("menus:output:copy");
    for entry in visible {
        let start = edges.partition_point(|(parent, _, _)| *parent < entry.id);
        let end = edges.partition_point(|(parent, _, _)| *parent <= entry.id);
        let mut children = admission.vector::<wire::WireId>(end - start)?;
        for (_, _, child) in &edges[start..end] {
            children.push(wire_id(*child)?);
        }
        let module = match modules.binary_search_by_key(&entry.id, |(id, _)| *id) {
            Ok(index) => modules[index].1.take(),
            Err(_) => None,
        };
        wire_nodes.push(
            wire::MenuNode::new(
                wire_id(entry.id)?,
                entry
                    .parent
                    .filter(|parent| ids.binary_search(parent).is_ok())
                    .map(wire_id)
                    .transpose()?,
                children,
                entry.label,
                entry.icon,
                entry
                    .action
                    .filter(|id| allowed_actions.binary_search(id).is_ok())
                    .map(wire_id)
                    .transpose()?,
            )
            .with_module(module),
        );
    }
    let version = admission.format(format_args!("{}", env.registry().generation()))?;
    Ok(wire::MenuTree::new(version, wire_roots, wire_nodes))
}

fn parse_action_ref(source: &str) -> Result<(&str, i64)> {
    let (model, id) = source
        .split_once(',')
        .ok_or_else(|| fail("Malformed menu action reference."))?;
    if model.is_empty() || id.is_empty() || id.bytes().any(|byte| !byte.is_ascii_digit()) {
        return Err(fail("Malformed menu action reference."));
    }
    let id = id
        .parse::<i64>()
        .map_err(|_| fail("Menu action identity is outside the supported range."))?;
    if id <= 0 {
        return Err(fail("Menu action identity must be positive."));
    }
    Ok((model, id))
}

async fn load_action(
    this: &Records,
    tx: &mut Transaction,
    admission: &mut ResponseAdmission,
) -> Result<wire::WindowAction> {
    let env = this.env().clone();
    env.companies()?;
    if this.len() != 1 {
        return Err(fail(
            "erp_load_web_action requires exactly one window action.",
        ));
    }
    this.check_access(tx, Operation::Read).await?;
    source_ids(admission, 1)?;
    let ids = this.ids();
    let action_id = ids[0];
    let action_stats_sql = admission.format(format_args!(
        "WITH action_payload AS ( \
           SELECT w.id, coalesce(octet_length(a.name)::bigint, 0) AS name_bytes, \
                  coalesce(octet_length(w.res_model)::bigint, 0) AS model_bytes, \
                  coalesce(octet_length(w.target)::bigint, 0) AS target_bytes, \
                  coalesce(octet_length(w.domain)::bigint, 0) AS domain_bytes, \
                  coalesce(octet_length(w.context)::bigint, 0) AS context_bytes, \
                  coalesce(octet_length(w.view_mode)::bigint, 0) AS view_mode_bytes, \
                  (SELECT count(*) FROM ir_act_window_group_rel g WHERE g.act_id = w.id) AS groups, \
                  (SELECT coalesce(sum(octet_length(v.view_mode)::bigint), 0) \
                     FROM ir_act_window_view v WHERE v.act_window_id = w.id) AS explicit_mode_bytes, \
                  (SELECT coalesce(max(octet_length(v.view_mode)::bigint), 0) \
                     FROM ir_act_window_view v WHERE v.act_window_id = w.id) AS max_explicit_mode_bytes, \
                  (SELECT count(*) FROM ir_act_window_view v WHERE v.act_window_id = w.id) + \
                  (SELECT count(*) FROM unnest(CASE \
                    WHEN octet_length(coalesce(w.view_mode, '')) <= {} \
                    THEN string_to_array(coalesce(w.view_mode, ''), ',') ELSE ARRAY[]::text[] END) \
                    AS modes(mode) \
                    WHERE mode <> '' AND NOT EXISTS (SELECT 1 FROM ir_act_window_view v \
                      WHERE v.act_window_id = w.id AND v.view_mode = mode)) AS computed_views \
             FROM ir_actions a JOIN ir_act_window w ON w.id = a.id WHERE w.id = ANY($1) \
         ) SELECT coalesce(max(name_bytes), 0)::bigint, coalesce(max(model_bytes), 0)::bigint, \
                  coalesce(max(target_bytes), 0)::bigint, coalesce(max(domain_bytes), 0)::bigint, \
                  coalesce(max(context_bytes), 0)::bigint, coalesce(max(view_mode_bytes), 0)::bigint, \
                  coalesce(max(name_bytes + model_bytes + target_bytes + domain_bytes + context_bytes + view_mode_bytes), 0)::bigint, \
                  coalesce(sum(groups), 0)::bigint, coalesce(max(computed_views), 0)::bigint, \
                  coalesce(max(explicit_mode_bytes), 0)::bigint, \
                  coalesce(max(max_explicit_mode_bytes), 0)::bigint \
             FROM action_payload",
        erp_addon_base::ir_actions::window::MAX_VIEW_MODE_BYTES
    ))?;
    let action_stats =
        preflight_stats(tx, &action_stats_sql, &ids, 11, "window action", admission).await?;
    ensure_below(
        action_stats[7],
        MAX_METADATA_GROUP_REFS,
        "Window action group reference count",
        "item",
    )?;
    admission.source_rows(1, &["group_ids"], 0, 0, stat_size(action_stats[7])?, 1)?;
    let group_rows = this
        .sudo(true)
        .read_with(tx, &["group_ids"], Load::Raw)
        .await?;
    let group_row = group_rows.first().ok_or_else(|| {
        Error::invariant("authorized window action disappeared during group read")
    })?;
    if !in_groups(tx, &env, &group_ids(group_row, "group_ids", admission)?).await? {
        return Err(Error::access("You may not read this window action."));
    }
    ensure_below(
        action_stats[1],
        MAX_ACTION_MODEL_BYTES,
        "Window action model name",
        "byte",
    )?;
    admission.source_rows(1, &["res_model"], stat_size(action_stats[1])?, 1, 0, 0)?;
    let model_rows = this
        .sudo(true)
        .read_with(tx, &["res_model"], Load::Raw)
        .await?;
    let model_row = model_rows.first().ok_or_else(|| {
        Error::invariant("authorized window action disappeared during target-model read")
    })?;
    let model = admission.format(format_args!("{}", text_value(model_row, "res_model")?))?;
    require_model_read(&env, tx, &model).await?;
    ensure_below(
        action_stats[0],
        MAX_LABEL_BYTES,
        "Window action name",
        "byte",
    )?;
    ensure_below(
        action_stats[2],
        MAX_ACTION_TARGET_BYTES,
        "Window action target",
        "byte",
    )?;
    ensure_below(
        action_stats[3],
        MAX_ACTION_EXPR_BYTES,
        "Window action domain",
        "byte",
    )?;
    ensure_below(
        action_stats[4],
        MAX_ACTION_EXPR_BYTES,
        "Window action context",
        "byte",
    )?;
    ensure_below(
        action_stats[5],
        erp_addon_base::ir_actions::window::MAX_VIEW_MODE_BYTES,
        "Window action view_mode",
        "byte",
    )?;
    ensure_below(
        action_stats[6],
        MAX_ACTION_PAYLOAD_BYTES,
        "Window action text payload",
        "byte",
    )?;
    ensure_below(
        action_stats[8],
        erp_addon_base::ir_actions::window::MAX_VIEW_MODES,
        "Window action computed view count",
        "item",
    )?;
    ensure_below(
        action_stats[9],
        MAX_ACTION_PAYLOAD_BYTES,
        "Window action explicit view-mode payload",
        "byte",
    )?;
    ensure_below(
        action_stats[10],
        erp_addon_base::ir_actions::window::MAX_VIEW_MODE_BYTES,
        "Window action explicit view mode",
        "byte",
    )?;
    admission.phase("action:text:reserve");
    admission.source_rows(
        1,
        &["name", "target", "views", "domain", "context"],
        stat_size(action_stats[6])?,
        4,
        0,
        0,
    )?;
    let view_count = stat_size(action_stats[8])?;
    let modes = stat_size(action_stats[5])?
        .checked_add(stat_size(action_stats[9])?)
        .ok_or_else(|| fail("Action view storage overflow."))?;
    // Source: base window::_compute_views -> views_of -> views_json. Named allocations:
    // given Arc modes, borrowed pairs, missing/result vectors including moving growth,
    // result String modes and their independent JSON String copies, outer/pair JSON arrays.
    admission.charge_items::<(Option<i64>, Arc<str>)>(view_count)?;
    admission.charge_items::<(Option<i64>, &str)>(view_count)?;
    admission.charge_items::<&str>(vec_growth_slots::<&str>(view_count)?)?;
    admission.charge_items::<(Option<i64>, String)>(vec_growth_slots::<(Option<i64>, String)>(
        view_count,
    )?)?;
    admission.charge(modes)?;
    admission.charge(modes)?;
    admission.charge_items::<serde_json::Value>(view_count)?;
    admission.charge_items::<serde_json::Value>(
        view_count
            .checked_mul(2)
            .ok_or_else(|| fail("Action view storage overflow."))?,
    )?;
    admission.charge_items::<serde_json::Value>(1)?; // computed JSON root behind Arc
    admission.charge_items::<(
        std::sync::atomic::AtomicUsize,
        std::sync::atomic::AtomicUsize,
    )>(1)?;
    admission.phase("action:text:read");
    let rows = this
        .sudo(true)
        .read_with(
            tx,
            &["name", "target", "views", "domain", "context"],
            Load::Raw,
        )
        .await?;
    let row = rows.first().ok_or_else(|| {
        Error::invariant("authorized window action disappeared during metadata read")
    })?;
    let id = action_id;
    let target = match optional_text(row, "target")? {
        Some("current") => wire::ActionTarget::Current,
        Some("main") => wire::ActionTarget::Main,
        Some("new") => wire::ActionTarget::New,
        Some(other) => {
            return Err(fail(format!(
                "Window action target '{other}' is unsupported."
            )));
        }
        None => return Err(fail("Window action target is required.")),
    };
    let Value::Json(views) = value(row, "views")? else {
        return Err(Error::invariant("window action views is not JSON"));
    };
    let views = views
        .as_array()
        .ok_or_else(|| Error::invariant("window action views is not an array"))?;
    if views.is_empty() || views.len() > 32 {
        return Err(fail("Window action must have between 1 and 32 views."));
    }
    admission.phase("action:views:reserve");
    let mut result_views = admission.vector::<wire::ActionView>(views.len())?;
    let mut seen = admission.vector::<&str>(views.len())?;
    for item in views {
        let pair = item
            .as_array()
            .filter(|pair| pair.len() == 2)
            .ok_or_else(|| Error::invariant("window action view item is not an id/type pair"))?;
        let id = if pair[0].is_null() || pair[0] == false {
            None
        } else {
            let id = pair[0]
                .as_i64()
                .filter(|id| *id > 0)
                .ok_or_else(|| fail("Window action contains an invalid view identity."))?;
            Some(wire_id(id)?)
        };
        let view_type = pair[1]
            .as_str()
            .ok_or_else(|| Error::invariant("window action view type is not text"))?;
        if seen.contains(&view_type) {
            return Err(fail("Window action contains a duplicate view type."));
        }
        seen.push(view_type);
        result_views.push(wire::ActionView::new(id, wire_type(&env, view_type)?));
    }
    let action_domain = ensure_action_sources(
        optional_text(row, "domain")?,
        optional_text(row, "context")?,
    )?;
    Ok(wire::WindowAction::new(
        wire_id(id)?,
        admission.format(format_args!("{}", text_value(row, "name")?))?,
        model,
        target,
        result_views,
        action_domain,
        None,
    ))
}

fn ensure_action_sources(
    domain: Option<&str>,
    context: Option<&str>,
) -> Result<Option<wire::WireDocument>> {
    ensure_action_context(context)?;
    parse_action_domain(domain)
}

fn parse_action_domain(domain: Option<&str>) -> Result<Option<wire::WireDocument>> {
    let domain = domain.filter(|source| !source.is_empty() && source.trim() != "[]");
    let Some(source) = domain else {
        return Ok(None);
    };
    if source.trim().is_empty() {
        return Err(fail(
            "Non-empty window-action domain/context expressions are not supported by the current human UI client; additional action expressions require a later release.",
        ));
    }
    let parsed = erp_sdk::expr::parse(source.trim()).map_err(|error| {
        fail(format!(
            "Unsupported window action domain expression: {error}"
        ))
    })?;
    let doc = wire::WireDocument::from_expr(&parsed).map_err(|error| {
        fail(format!(
            "Unsupported window action domain expression: {error}"
        ))
    })?;
    Ok(Some(doc))
}

fn ensure_action_context(context: Option<&str>) -> Result<()> {
    // Keep exact prior emptiness: outer whitespace around {} is accepted; whitespace-only
    // and differently formatted nonempty source is refused before parsing or AST allocation.
    let context = context.filter(|source| !source.is_empty() && source.trim() != "{}");
    if context.is_some() {
        return Err(fail(
            "Non-empty window-action context expressions are not supported by the current human UI client; additional action expressions require a later release.",
        ));
    }
    Ok(())
}

fn get_i32(row: &BTreeMap<String, Value>, field: &str) -> Result<i32> {
    match value(row, field)? {
        Value::Int(value) => i32::try_from(*value)
            .map_err(|_| Error::invariant(format!("view {field} is outside i32"))),
        other => Err(Error::invariant(format!(
            "view {field} read as {other:?}, not integer"
        ))),
    }
}
type FieldWireMetadata<'a> = (
    wire::FieldKind,
    Option<&'a [(String, String)]>,
    Option<&'a str>,
    Option<&'a str>,
);

pub(crate) fn field_kind<'a>(model: &str, field: &'a FieldSpec) -> Result<FieldWireMetadata<'a>> {
    let (kind, selection, relation, relation_field) = match &field.ty {
        FieldType::Char { .. } | FieldType::Uuid => (wire::FieldKind::Char, None, None, None),
        FieldType::Text | FieldType::Json => (wire::FieldKind::Text, None, None, None),
        FieldType::Html => (wire::FieldKind::Html, None, None, None),
        FieldType::Boolean => (wire::FieldKind::Boolean, None, None, None),
        // The record identity renders as a plain integer; it stays readonly and never enters writes.
        FieldType::Id | FieldType::Integer => (wire::FieldKind::Integer, None, None, None),
        FieldType::Float { .. } => (wire::FieldKind::Float, None, None, None),
        FieldType::Monetary { .. } => (wire::FieldKind::Monetary, None, None, None),
        FieldType::Date => (wire::FieldKind::Date, None, None, None),
        FieldType::Datetime => (wire::FieldKind::Datetime, None, None, None),
        FieldType::Selection {
            values: Some(values),
            ..
        } => (
            wire::FieldKind::Selection,
            Some(values.as_slice()),
            None,
            None,
        ),
        FieldType::Selection { .. } => {
            return Err(fail(format!(
                "Dynamic selection field '{model}.{}' is unsupported by the current human UI contract.",
                field.name
            )));
        }
        FieldType::Many2one { comodel, .. } => (
            wire::FieldKind::Many2one,
            None,
            Some(comodel.as_str()),
            None,
        ),
        FieldType::One2many { comodel, inverse } => (
            wire::FieldKind::One2many,
            None,
            Some(comodel.as_str()),
            Some(inverse.as_str()),
        ),
        FieldType::Many2many { comodel, .. } => (
            wire::FieldKind::Many2many,
            None,
            Some(comodel.as_str()),
            None,
        ),
        _ => {
            return Err(fail(format!(
                "Field type '{}' has no registered human UI renderer.",
                field.ty.type_name()
            )));
        }
    };
    Ok((kind, selection, relation, relation_field))
}

#[derive(Serialize)]
struct FieldMetadataPreview<'a> {
    #[serde(rename = "type")]
    kind: wire::FieldKind,
    label: &'a str,
    required: bool,
    readonly: bool,
    selection: Option<&'a [(String, String)]>,
    relation: Option<&'a str>,
    relation_field: Option<&'a str>,
}

fn preview_field<'a>(model: &str, field: &'a FieldSpec) -> Result<FieldMetadataPreview<'a>> {
    let (kind, selection, relation, relation_field) = field_kind(model, field)?;
    Ok(FieldMetadataPreview {
        kind,
        label: &field.string,
        required: field.required,
        readonly: field.readonly,
        selection,
        relation,
        relation_field,
    })
}

struct FieldMapPreview<'a> {
    model: &'a str,
    spec: &'a ModelSpec,
    names: &'a BTreeSet<String>,
}

impl Serialize for FieldMapPreview<'_> {
    fn serialize<S: serde::Serializer>(
        &self,
        serializer: S,
    ) -> std::result::Result<S::Ok, S::Error> {
        use serde::ser::{Error as _, SerializeMap};
        let mut map = serializer.serialize_map(Some(self.names.len()))?;
        for name in self.names {
            let field = self
                .spec
                .field(name)
                .ok_or_else(|| S::Error::custom("metadata field missing from registry"))?;
            let preview = preview_field(self.model, field).map_err(S::Error::custom)?;
            map.serialize_entry(name, &preview)?;
        }
        map.end()
    }
}

struct MetadataPreview<'a>(FieldMapPreview<'a>);
impl Serialize for MetadataPreview<'_> {
    fn serialize<S: serde::Serializer>(
        &self,
        serializer: S,
    ) -> std::result::Result<S::Ok, S::Error> {
        use serde::ser::SerializeMap;
        let mut map = serializer.serialize_map(Some(1))?;
        map.serialize_entry(self.0.model, &self.0)?;
        map.end()
    }
}

fn admit_field_metadata<T: Serialize>(admission: &ResponseAdmission, metadata: &T) -> Result<()> {
    admission
        .encode_within(MAX_FIELD_METADATA_BYTES, metadata)
        .map(drop)
        .map_err(|error| match error {
            ViewError::LimitExceeded {
                limit: ViewLimitKind::DescriptionEncodedBytes,
                max,
                ..
            } => fail(format!("Field metadata exceeds the {max}-byte limit.")),
            ViewError::InvalidDescription(detail) => {
                fail(format!("Field metadata cannot be encoded: {detail}"))
            }
            other => admission_error(other),
        })
}

#[cfg(test)]
fn parse_bool_attr(node: &erp_sdk::views::NodeRef<'_>, key: &str, default: bool) -> Result<bool> {
    match node.attr(key) {
        None => Ok(default),
        Some("1" | "true" | "True") => Ok(true),
        Some("0" | "false" | "False") => Ok(false),
        Some(_) => Err(fail(format!(
            "Dynamic '{key}' modifier is unsupported by the current human UI contract."
        ))),
    }
}

/// One explicit static boolean override as a literal expression document.
#[cfg(test)]
fn reject_attrs(node: &erp_sdk::views::NodeRef<'_>, allowed: &[&str]) -> Result<()> {
    for (key, _) in node.attributes() {
        if !allowed.contains(&key) {
            return Err(fail(format!(
                "View attribute '{key}' is unsupported by the current human UI contract."
            )));
        }
    }
    if !node.text().trim().is_empty() {
        return Err(fail(format!(
            "Text content in <{}> is unsupported by the current human UI contract.",
            node.tag()
        )));
    }
    Ok(())
}

#[cfg(test)]
fn service_widget(node: &erp_sdk::views::NodeRef<'_>) -> Result<wire::ViewNode> {
    reject_attrs(node, &["name", "scope_kinds", "app"])?;
    if node.children().next().is_some() {
        return Err(fail("Service widgets cannot contain child nodes."));
    }
    let widget = node
        .attr("name")
        .ok_or_else(|| fail("Service widget requires a name."))?;
    let (widget, options) = match widget {
        "web.settings" => {
            let scope_text = node
                .attr("scope_kinds")
                .ok_or_else(|| fail("web.settings requires scope_kinds."))?;
            let mut seen = BTreeSet::new();
            let mut scopes = Vec::new();
            for kind in scope_text.split(',') {
                let scope = match kind {
                    "global" => wire::ScopeKind::Global,
                    "company" => wire::ScopeKind::Company,
                    _ => return Err(fail(format!("Unknown web.settings scope kind '{kind}'."))),
                };
                if !seen.insert(kind) {
                    return Err(fail(format!("Duplicate web.settings scope kind '{kind}'.")));
                }
                scopes.push(scope);
            }
            if scopes.is_empty() {
                return Err(fail("web.settings scope_kinds cannot be empty."));
            }
            let mut settings = wire::SettingsOptions::new(scopes);
            if let Some(app) = node.attr("app") {
                if !wire::is_settings_app_name(app) {
                    return Err(fail(format!(
                        "web.settings app '{app}' is not an addon technical name."
                    )));
                }
                settings = settings.with_app(app.to_owned());
            }
            (
                wire::ServiceWidget::Settings,
                wire::WidgetOptions::Settings(settings),
            )
        }
        "web.addons" => {
            if node.attr("scope_kinds").is_some() {
                return Err(fail("web.addons does not accept scope_kinds."));
            }
            if node.attr("app").is_some() {
                return Err(fail("web.addons does not accept app."));
            }
            (
                wire::ServiceWidget::Addons,
                wire::WidgetOptions::Empty(wire::EmptyOptions::new()),
            )
        }
        other => return Err(fail(format!("Service widget '{other}' is not registered."))),
    };
    Ok(wire::ViewNode::Widget {
        widget,
        options,
        modifiers: wire::Modifiers::empty(),
    })
}

// The raw XML walk is bounded before authorization, including denied subtrees. The
// preorder parent indexes also let lowering prune a subtree without inspecting its fields.
#[cfg(test)]
fn layout_plan<'a>(
    root: erp_sdk::views::NodeRef<'a>,
    form: bool,
    record_mode: wire::RecordMode,
) -> Result<Vec<(erp_sdk::views::NodeRef<'a>, usize)>> {
    fn visit<'a>(
        node: erp_sdk::views::NodeRef<'a>,
        parent: usize,
        depth: usize,
        layout: bool,
        entries: &mut Vec<(erp_sdk::views::NodeRef<'a>, usize)>,
    ) -> Result<()> {
        if depth > MAX_TREE_DEPTH {
            return Err(fail("View exceeds the 32-level depth limit."));
        }
        if entries.len() >= MAX_VIEW_NODES {
            return Err(fail("View exceeds the 4096-node limit."));
        }
        if !node.text().trim().is_empty() {
            return Err(layout_error(format!(
                "Text content in <{}> is unsupported.",
                node.tag()
            )));
        }
        if !node.tail().trim().is_empty() {
            return Err(layout_error("Text between view elements is unsupported."));
        }
        let index = entries.len();
        entries.push((node, parent));
        if node.tag() == "footer"
            && node
                .children()
                .filter(|child| child.attr("special").is_some())
                .count()
                > 1
        {
            return Err(layout_error(
                "A <footer> allows at most one special button.",
            ));
        }
        if node.tag() == "form"
            && node
                .children()
                .filter(|child| child.tag() == "aside")
                .count()
                > 1
        {
            return Err(layout_error("A record form allows at most one <aside>."));
        }
        for (position, child) in node.children().enumerate() {
            let allowed = match node.tag() {
                "form" if !layout => child.tag() == "widget",
                "list" => child.tag() == "field",
                "notebook" if layout => child.tag() == "page",
                "header" if layout => {
                    child.tag() == "button"
                        || (child.tag() == "field" && child.attr("widget") == Some("statusbar"))
                }
                "footer" if layout => child.tag() == "button",
                "aside" if layout => matches!(child.tag(), "field" | "group" | "separator"),
                "div" if layout && node.attr("class") == Some("oe_title") => {
                    matches!(child.tag(), "h1" | "h2" | "field")
                }
                "h1" | "h2" if layout => child.tag() == "field",
                "div" if layout && node.attr("name") == Some("button_box") => {
                    child.tag() == "button"
                }
                "button" if layout && node.attr("class") == Some("oe_stat_button") => {
                    child.tag() == "field"
                }
                "form" | "sheet" | "group" | "page" if layout => {
                    matches!(child.tag(), "field" | "group" | "notebook" | "separator")
                        || (child.tag() == "div" && child.attr("class") == Some("oe_title"))
                        || (node.tag() == "sheet"
                            && child.tag() == "div"
                            && child.attr("name") == Some("button_box"))
                        || (node.tag() == "form"
                            && matches!(child.tag(), "sheet" | "header" | "footer" | "aside"))
                }
                _ => false, // Closed parent grammar: leaves and unknown nodes have no children.
            };
            if child.tag() == "header" && allowed && position != 0 {
                return Err(layout_error(
                    "<header> must be the first child of <form>, before the sheet and content.",
                ));
            }
            if child.tag() == "footer" && allowed && position + 1 != node.children().count() {
                return Err(layout_error(
                    "<footer> must be the single last child of <form>, after the sheet and content.",
                ));
            }
            if !allowed {
                return Err(layout_error(format!(
                    "View element <{}> is not allowed under <{}>.",
                    child.tag(),
                    node.tag()
                )));
            }
            visit(child, index, depth + 1, layout, entries)?;
        }
        Ok(())
    }
    let expected = if form { "form" } else { "list" };
    if root.tag() != expected {
        return Err(layout_error(format!(
            "Expected <{expected}> view root, found <{}>.",
            root.tag()
        )));
    }
    let mut entries = Vec::new();
    visit(
        root,
        0,
        1,
        form && record_mode == wire::RecordMode::Record,
        &mut entries,
    )?;
    Ok(entries)
}

#[cfg(test)]
fn layout_error(message: impl std::fmt::Display) -> Error {
    fail(format!(
        "{message} Unsupported form layout requires a later release."
    ))
}

#[cfg(test)]
fn layout_tracks(node: &erp_sdk::views::NodeRef<'_>, key: &str, default: u32) -> Result<u32> {
    let Some(raw) = node.attr(key) else {
        return Ok(default);
    };
    let invalid = || {
        layout_error(format!(
            "<{0}> {key} must be an even integer from 2 through 12.",
            node.tag()
        ))
    };
    // XML integer tracks have no signs, fractions, expressions or surrounding whitespace.
    if raw.is_empty() || !raw.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(invalid());
    }
    let value = raw.parse::<u32>().map_err(|_| invalid())?;
    if !(2..=12).contains(&value) || value % 2 != 0 {
        return Err(invalid());
    }
    Ok(value)
}

#[cfg(test)]
fn layout_node(node: &erp_sdk::views::NodeRef<'_>) -> Result<wire::ViewNode> {
    let allowed: &[&str] = match node.tag() {
        "sheet" | "header" | "footer" | "aside" => &["groups", "invisible"],
        "div" if node.attr("class") == Some("oe_title") => {
            &["class", "name", "groups", "invisible"]
        }
        "h1" | "h2" => &["name", "groups", "invisible"],
        "div" if node.attr("name") == Some("button_box") => {
            &["name", "class", "groups", "invisible"]
        }
        "group" => &["name", "string", "col", "colspan", "groups", "invisible"],
        "notebook" => &["name", "groups", "invisible"],
        "page" => &["name", "string", "groups", "invisible"],
        "separator" => &["name", "string", "groups", "invisible"],
        tag => return Err(layout_error(format!("Unknown layout element <{tag}>."))),
    };
    reject_attrs(node, allowed).map_err(layout_error)?;
    for key in ["name", "string"] {
        if node
            .attr(key)
            .is_some_and(|value| value.len() > MAX_LABEL_BYTES)
        {
            return Err(layout_error(format!(
                "Layout '{key}' exceeds the 4096-byte limit."
            )));
        }
    }
    // Absent captions are intentionally empty; a page with no caption uses its stable name.
    let label = node.attr("string").unwrap_or("").to_owned();
    let children = Vec::new();
    Ok(match node.tag() {
        "sheet" => wire::ViewNode::Sheet { children },
        "aside" => wire::ViewNode::Aside { children },
        "div" if node.attr("class") == Some("oe_title") => wire::ViewNode::Title { children },
        "h1" => wire::ViewNode::Heading { level: 1, children },
        "h2" => wire::ViewNode::Heading { level: 2, children },
        "div" if node.attr("name") == Some("button_box") => {
            if node
                .attr("class")
                .is_some_and(|value| value != "oe_button_box")
            {
                return Err(layout_error(
                    "button_box accepts only class='oe_button_box'.",
                ));
            }
            wire::ViewNode::ButtonBox { children }
        }
        "header" => wire::ViewNode::Header { children },
        "footer" => wire::ViewNode::Footer { children },
        "group" => wire::ViewNode::Group {
            label,
            columns: layout_tracks(
                node,
                "col",
                if node.children().any(|child| child.tag() == "group") {
                    4
                } else {
                    2
                },
            )?,
            colspan: layout_tracks(node, "colspan", 2)?,
            children,
        },
        "notebook" => wire::ViewNode::Notebook { children },
        "page" => {
            let name = node
                .attr("name")
                .filter(|name| !name.trim().is_empty())
                .or_else(|| node.attr("string").filter(|label| !label.trim().is_empty()))
                .ok_or_else(|| layout_error("Page requires a nonempty name or string."))?;
            wire::ViewNode::Page {
                name: name.to_owned(),
                label: if label.trim().is_empty() {
                    name.to_owned()
                } else {
                    label
                },
                children,
            }
        }
        "separator" => wire::ViewNode::Separator { label },
        _ => return Err(Error::invariant("validated layout tag changed")),
    })
}

/// A validated header button before authorization pruning and the exposure check.
#[cfg(test)]
struct ButtonPlan {
    node: wire::ViewNode,
    /// Field names the `invisible` expression reads.
    names: BTreeSet<String>,
    /// Static `invisible="1"`: the button is pruned like any hidden node.
    hidden: bool,
}

#[cfg(test)]
const MAX_METHOD_BYTES: usize = 128;

#[cfg(test)]
fn button_names(expr: &erp_sdk::expr::Expr, names: &mut BTreeSet<String>) -> Result<()> {
    use erp_sdk::expr::Expr;
    let unsupported = || {
        layout_error(
            "Button invisible expressions support names, literals, comparisons, not/and/or, arithmetic and conditionals only.",
        )
    };
    match expr {
        Expr::Literal(_) => {}
        Expr::Name(name) => {
            names.insert(name.clone());
        }
        Expr::Unary { operand, .. } => button_names(operand, names)?,
        Expr::Binary { left, right, .. } => {
            button_names(left, names)?;
            button_names(right, names)?;
        }
        Expr::BoolOp { values, .. } | Expr::List(values) | Expr::Tuple(values) => {
            for value in values {
                button_names(value, names)?;
            }
        }
        Expr::Compare {
            left, comparators, ..
        } => {
            button_names(left, names)?;
            for value in comparators {
                button_names(value, names)?;
            }
        }
        Expr::IfExp { test, body, orelse } => {
            for part in [test, body, orelse] {
                button_names(part, names)?;
            }
        }
        _ => return Err(unsupported()),
    }
    Ok(())
}

/// Validate one `<button>` of a form `<header>`. Only `type="object"` buttons exist in this
/// slice; `context`, `special`, `type="action"`, icons and every other attribute are refused
/// with the slice that will add them.
#[cfg(test)]
fn header_button(node: &erp_sdk::views::NodeRef<'_>) -> Result<ButtonPlan> {
    button_plan(node, false)
}

/// Closed emphasis vocabulary shared by header and footer buttons.
#[cfg(test)]
fn button_variant(node: &erp_sdk::views::NodeRef<'_>) -> Result<wire::ButtonVariant> {
    Ok(match node.attr("class") {
        None | Some("btn-secondary" | "oe_stat_button") => wire::ButtonVariant::Secondary,
        Some("btn-primary" | "oe_highlight") => wire::ButtonVariant::Primary,
        Some("btn-danger") => wire::ButtonVariant::Danger,
        Some(other) => {
            return Err(layout_error(format!(
                "Button class '{other}' is unsupported; use btn-primary, btn-secondary or btn-danger."
            )));
        }
    })
}

/// The caption and its bound, shared by every button kind.
#[cfg(test)]
fn button_label<'a>(node: &erp_sdk::views::NodeRef<'a>) -> Result<&'a str> {
    let label = node
        .attr("string")
        .filter(|label| !label.trim().is_empty())
        .ok_or_else(|| layout_error("Button requires a nonempty string."))?;
    if label.len() > MAX_LABEL_BYTES {
        return Err(layout_error("Button 'string' exceeds the 4096-byte limit."));
    }
    Ok(label)
}

/// The static or expression `invisible` of a button: modifiers, the field names it reads and
/// whether it is statically hidden.
#[cfg(test)]
fn button_invisible(
    node: &erp_sdk::views::NodeRef<'_>,
) -> Result<(wire::Modifiers, BTreeSet<String>, bool)> {
    let mut names = BTreeSet::new();
    let mut hidden = false;
    let mut modifiers = wire::Modifiers::empty();
    match node.attr("invisible") {
        None | Some("0" | "false" | "False") => {}
        Some("1" | "true" | "True") => hidden = true,
        Some(source) => {
            let parsed = erp_sdk::expr::parse(source).map_err(|error| {
                layout_error(format!("Unsupported button invisible expression: {error}"))
            })?;
            button_names(&parsed, &mut names)?;
            modifiers.invisible = Some(
                erp_sdk::expr::wire::WireDocument::from_expr(&parsed).map_err(|error| {
                    layout_error(format!("Unsupported button invisible expression: {error}"))
                })?,
            );
        }
    }
    Ok((modifiers, names, hidden))
}

/// A footer button with `special="cancel"`: no method, no confirmation, no save.
#[cfg(test)]
fn cancel_button(node: &erp_sdk::views::NodeRef<'_>) -> Result<ButtonPlan> {
    match node.attr("special") {
        Some("cancel") => {}
        Some(other) => {
            return Err(layout_error(format!(
                "Button special '{other}' is unsupported; only special='cancel' is available."
            )));
        }
        None => return Err(Error::invariant("cancel_button without special")),
    }
    for conflicting in ["name", "type", "confirm"] {
        if node.attr(conflicting).is_some() {
            return Err(layout_error(format!(
                "A special='cancel' button cannot combine with '{conflicting}'."
            )));
        }
    }
    reject_attrs(node, &["special", "string", "class", "invisible", "groups"])
        .map_err(layout_error)?;
    let label = button_label(node)?.to_owned();
    let variant = button_variant(node)?;
    let (modifiers, names, hidden) = button_invisible(node)?;
    Ok(ButtonPlan {
        node: wire::ViewNode::CancelButton {
            label,
            variant,
            modifiers,
            details: None,
        },
        names,
        hidden,
    })
}

/// Validate a header or footer `<button>`; `in_footer` admits the cancel kind.
#[cfg(test)]
fn button_plan(node: &erp_sdk::views::NodeRef<'_>, in_footer: bool) -> Result<ButtonPlan> {
    if in_footer && node.attr("special").is_some() {
        return cancel_button(node);
    }
    reject_attrs(
        node,
        &[
            "name",
            "type",
            "string",
            "class",
            "invisible",
            "confirm",
            "groups",
        ],
    )
    .map_err(layout_error)?;
    match node.attr("type") {
        Some("object") => {}
        Some(other) => {
            return Err(layout_error(format!(
                "Button type '{other}' is unsupported; only type='object' is available."
            )));
        }
        None => return Err(layout_error("Button requires type='object'.")),
    }
    let method = node
        .attr("name")
        .ok_or_else(|| layout_error("Button requires a name."))?;
    let valid_name = method.len() <= MAX_METHOD_BYTES
        && method.starts_with(|c: char| c.is_ascii_lowercase())
        && method
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_');
    if !valid_name {
        return Err(layout_error(format!(
            "Button name '{method}' is not a public method name."
        )));
    }
    let statistic = node.attr("class") == Some("oe_stat_button");
    let label = if statistic && node.attr("string").is_none() {
        let field = node
            .children()
            .find(|child| matches!(child.attr("widget"), Some("statinfo" | "relation_count")))
            .ok_or_else(|| {
                layout_error("A smart button needs a statistic field or an explicit string.")
            })?;
        button_label(&field)?
    } else {
        button_label(node)?
    };
    let confirm = match node.attr("confirm") {
        None => None,
        Some(text) if !text.trim().is_empty() => Some(text.to_owned()),
        Some(_) => return Err(layout_error("Button confirm cannot be empty.")),
    };
    if confirm
        .as_deref()
        .is_some_and(|value| value.len() > MAX_LABEL_BYTES)
    {
        return Err(layout_error(
            "Button 'confirm' exceeds the 4096-byte limit.",
        ));
    }
    let variant = button_variant(node)?;
    let (modifiers, names, hidden) = button_invisible(node)?;
    Ok(ButtonPlan {
        node: if statistic {
            wire::ViewNode::StatButton {
                name: method.to_owned(),
                button_type: wire::ButtonType::Object,
                label: label.to_owned(),
                variant,
                confirm,
                modifiers,
                children: Vec::new(),
                details: None,
            }
        } else {
            wire::ViewNode::Button {
                name: method.to_owned(),
                button_type: wire::ButtonType::Object,
                label: label.to_owned(),
                variant,
                confirm,
                modifiers,
                details: None,
            }
        },
        names,
        hidden,
    })
}

/// Bound the opt-in to static, groupable metadata; never guess a workflow from a field name.
#[cfg(test)]
fn validate_list_filter_choices(stored: bool, kind: &FieldType) -> Result<()> {
    if !stored {
        return Err(fail(
            "Search panel selection category requires a stored selection field.",
        ));
    }
    let FieldType::Selection {
        values: Some(choices),
        ..
    } = kind
    else {
        return Err(fail(
            "Search panel selection category requires a static selection field.",
        ));
    };
    if choices.is_empty() || choices.len() > 32 {
        return Err(fail(
            "Search panel selection category requires 1–32 selection choices.",
        ));
    }
    let mut keys = BTreeSet::new();
    for (key, label) in choices {
        if key.trim().is_empty()
            || label.trim().is_empty()
            || key.len() > 256
            || label.len() > 256
            || !keys.insert(key)
        {
            return Err(fail(
                "Search panel selection category choices require distinct nonempty keys and nonempty labels of at most 256 bytes.",
            ));
        }
    }
    Ok(())
}

struct ActionListPresentation {
    search_view: Option<i64>,
    limit: wire::WirePageLimit,
}

fn admitted_page_limit(value: i64) -> Result<wire::WirePageLimit> {
    let value = u64::try_from(value)
        .map_err(|_| fail("List page limit must be a positive browser-safe integer."))?;
    wire::WirePageLimit::try_from(value)
        .map_err(|_| fail("List page limit must be a positive browser-safe integer."))
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum PanelScalar<'a> {
    Selection(&'a str),
    Record(u64),
}
fn panel_default_error(id: u32, kind: &str) -> Error {
    fail(format!("Search panel section {id} {kind}."))
}
fn panel_scalar<'a>(
    value: &'a Value,
    field: &erp_sdk::FieldSpec,
    id: u32,
) -> Result<PanelScalar<'a>> {
    match &field.ty {
        FieldType::Selection {
            values: Some(choices),
            ..
        } => {
            let Value::Str(key) = value else {
                return Err(panel_default_error(id, "has an invalid selection default"));
            };
            if key.len() > 256 {
                return Err(panel_default_error(id, "exceeds selection key byte limit"));
            }
            if key.is_empty() || !choices.iter().any(|(choice, _)| choice == key.as_ref()) {
                return Err(panel_default_error(id, "has an invalid selection default"));
            }
            Ok(PanelScalar::Selection(key))
        }
        FieldType::Many2one { .. } | FieldType::Many2many { .. } => {
            let Value::Int(value) = value else {
                return Err(panel_default_error(id, "has an invalid relational default"));
            };
            let value = u64::try_from(*value)
                .map_err(|_| panel_default_error(id, "has an invalid relational default"))?;
            wire::WireId::try_from(value)
                .map_err(|_| panel_default_error(id, "has an invalid relational default"))?;
            Ok(PanelScalar::Record(value))
        }
        _ => Err(panel_default_error(id, "has an unsupported default field")),
    }
}
fn section_defaults(
    env: &Env,
    model: &str,
    sections: &mut [wire::PanelSection],
    admission: &mut ResponseAdmission,
) -> Result<()> {
    let spec = env
        .registry()
        .model(model)
        .ok_or_else(|| panel_default_error(0, "references an invalid root model"))?;
    for section in sections {
        let id = section.id();
        let field = spec
            .field(&section.field().name)
            .ok_or_else(|| panel_default_error(id, "references an invalid root field"))?;
        let supplied = env.context().extra().iter().find_map(|(key, value)| {
            key.strip_prefix("searchpanel_default_")
                .filter(|name| *name == section.field().name)
                .map(|_| value)
        });
        let category = matches!(section, wire::PanelSection::Category { .. });
        let value = match supplied {
            None | Some(Value::False | Value::Bool(false)) => continue,
            Some(value) => value,
        };
        let input = if category {
            if matches!(
                value,
                Value::List(_) | Value::Map(_) | Value::Rows(_) | Value::Row(_)
            ) {
                return Err(panel_default_error(id, "has an invalid category default"));
            }
            std::slice::from_ref(value)
        } else {
            let Value::List(values) = value else {
                return Err(panel_default_error(id, "requires an array default"));
            };
            values.as_ref()
        };
        if input.len() > if category { 1 } else { 64 } {
            return Err(panel_default_error(id, "exceeds default count limit"));
        }
        for (i, value) in input.iter().enumerate() {
            let scalar = panel_scalar(value, field, id)?;
            for prior in &input[..i] {
                if panel_scalar(prior, field, id)? == scalar {
                    return Err(panel_default_error(id, "has duplicate defaults"));
                }
            }
        }
        // Caller input is borrowed. Reserve all retained output before copying any scalar.
        let mut output = admission.vector(input.len())?;
        for value in input {
            output.push(match panel_scalar(value, field, id)? {
                PanelScalar::Selection(key) => {
                    wire::PanelValue::Selection(admission.format(format_args!("{key}"))?)
                }
                PanelScalar::Record(value) => {
                    wire::PanelValue::Record(wire::WireId::try_from(value).map_err(|_| {
                        panel_default_error(id, "has an invalid relational default")
                    })?)
                }
            });
        }
        let defaults = wire::PanelCallerDefaults::checked(section, spec, output)?;
        section.set_caller_defaults(defaults)?;
    }
    Ok(())
}

/// Apply only caller-owned presentation context after the checked panel has been authorized.
fn panel_context(
    env: &Env,
    model: &str,
    panel: &mut Option<wire::ListSearchPanel>,
    admission: &mut ResponseAdmission,
) -> Result<()> {
    if let Some(wire::ListSearchPanel::Sections { sections, .. }) = panel {
        return section_defaults(env, model, sections, admission);
    }
    let Some(wire::ListSearchPanel::Selection {
        field,
        default_value,
        ..
    }) = panel
    else {
        return Ok(());
    };
    let supplied = env.context().extra().iter().find_map(|(key, value)| {
        key.strip_prefix("searchpanel_default_")
            .filter(|name| *name == field.name)
            .map(|_| value)
    });
    let value = match supplied {
        None | Some(Value::False | Value::Bool(false) | Value::Int(0)) => return Ok(()),
        Some(Value::Str(value)) if value.is_empty() => return Ok(()),
        Some(Value::Str(value)) => value,
        Some(_) => {
            return Err(fail(
                "Search panel default must be a selection key or false.",
            ));
        }
    };
    let spec = env
        .registry()
        .model(model)
        .ok_or_else(|| fail(format!("Unknown action model '{model}'.")))?;
    let field = spec
        .field(&field.name)
        .ok_or_else(|| fail("Unknown search panel field."))?;
    let FieldType::Selection {
        values: Some(choices),
        ..
    } = &field.ty
    else {
        return Err(Error::invariant(
            "validated search panel field is not a static selection",
        ));
    };
    if !choices.iter().any(|(key, _)| key == value.as_ref()) {
        return Err(fail(
            "Search panel default is not a declared selection value.",
        ));
    }
    admission.charge(value.len())?;
    *default_value = Some(value.to_string());
    Ok(())
}

struct PanelNotices<'a>(&'a [erp_addon_base::ir_ui_view::select::ViewNotice]);
impl std::fmt::Display for PanelNotices<'_> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        for (index, notice) in self.0.iter().enumerate() {
            if index != 0 {
                formatter.write_str(" ")?;
            }
            write!(formatter, "{notice}")?;
        }
        Ok(())
    }
}

/// Obtain a list's optional search panel directly from a checked search tree. No XML is emitted
/// or parsed again, and unsupported unrelated facet syntax does not block a list-only request.
async fn checked_list_panel(
    env: &Env,
    tx: &mut Transaction,
    model: &str,
    action_search_view: Option<i64>,
    admission: &mut ResponseAdmission,
) -> Result<(Option<wire::ListSearchPanel>, Vec<String>)> {
    admission.charge("search".len())?;
    admission.charge_items::<(
        std::sync::atomic::AtomicUsize,
        std::sync::atomic::AtomicUsize,
    )>(1)?;
    let ty = ViewType::new(tx, "search")?;
    let has_ref = match env.context().get("search_view_ref") {
        None | Some(Value::False | Value::Bool(false)) => false,
        Some(Value::Str(value)) if value.is_empty() => false,
        Some(_) => true,
    };
    if action_search_view.is_none()
        && !has_ref
        && erp_addon_base::ir_ui_view::select::default_view(tx, Some(model), &ty)
            .await?
            .is_none()
    {
        return Ok((None, Vec::new()));
    }
    let published = published_view_types(env)?;
    let declaration = published
        .find("search")
        .ok_or_else(|| fail("View type 'search' is not registered."))?;
    let request = match action_search_view {
        Some(id) => ViewRequest::new(model, &ty).view_id(ViewId::new(id)?),
        None => ViewRequest::new(model, &ty).context(env.context()),
    };
    let budget = admission.budget.clone();
    let result = erp_addon_base::ir_ui_view::with_checked_panel_view(
        tx,
        env,
        &request,
        &budget,
        |input, id, notices| {
            if input.root().is_none() {
                return if action_search_view.is_some() {
                    Err(Error::access("You may not read the selected search view."))
                } else {
                    Ok(None)
                };
            }
            let payload =
                crate::h18_lower::build_panel_description(input, &budget, declaration.schema())
                    .map_err(admission_error)?;
            let notice = if notices.is_empty() {
                None
            } else {
                Some(admission.format(format_args!("{}", PanelNotices(notices)))?)
            };
            Ok(Some((payload, id, notice)))
        },
    )
    .await;
    let result = match result {
        Ok(result) => result,
        Err(error) if error.kind() == ErrorKind::Access => {
            if action_search_view.is_some() {
                return Err(Error::access("You may not read the selected search view."));
            }
            return Ok((None, Vec::new()));
        }
        Err(error) => {
            if let Some(message) = erp_sdk::views::host_access::panel_syntax_notice(&error) {
                return Ok((
                    Some(wire::ListSearchPanel::Unsupported {
                        message: admission.format(format_args!("{message}"))?,
                    }),
                    Vec::new(),
                ));
            }
            return Err(error);
        }
    };
    let Some((payload, id, notice)) = result else {
        return Ok((None, Vec::new()));
    };
    let (mut view, storage) = payload
        .builtin_view(
            wire::ViewIdentity::stored(wire_id(id.get())?),
            wire::ViewType::Search,
            declaration.renderer(),
            &budget,
        )
        .map_err(admission_error)?
        .into_parts();
    admission.hold(storage)?;
    let fields = crate::h18_authorize::Request::new(&payload, declaration, model, env, &budget)
        .panel(&mut view, tx)
        .await?;
    let (names, storage) = fields.into_parts();
    admission.hold(storage)?;
    let mut panel = match view.nodes.into_iter().nth(view.root as usize) {
        Some(wire::ViewNode::Search { search_panel, .. }) => search_panel,
        _ => {
            return Err(Error::invariant(
                "Checked panel builder did not produce a Search root",
            ));
        }
    };
    match &mut panel {
        Some(wire::ListSearchPanel::Selection { notice: target, .. }) => *target = notice,
        Some(wire::ListSearchPanel::Sections { notice: target, .. }) => {
            if let Some(fallback) = notice {
                *target = Some(match target.as_deref() {
                    Some(compatibility) => {
                        admission.format(format_args!("{compatibility} {fallback}"))?
                    }
                    None => fallback,
                });
            }
        }
        _ => {}
    }
    panel_context(env, model, &mut panel, admission)?;
    Ok((panel, names))
}

/// The caller-supplied part of one `get_views` call.
struct ViewsRequest<'a> {
    model: &'a str,
    views: Vec<Value>,
    wire_major: u64,
    options: ValueMap,
    action_presentation: Option<ActionListPresentation>,
}

fn description_mobile(options: &ValueMap) -> Result<bool> {
    for (k, v) in options.iter() {
        match (k.as_str(), v) {
            ("mobile", Value::Bool(_)) => {}
            ("toolbar" | "load_filters", Value::Bool(true)) => {
                return Err(Error::validation(format!(
                    "Option '{k}' is unsupported by a later release; deferred to a later release."
                )));
            }
            ("toolbar" | "load_filters", Value::Bool(false)) => {}
            ("action_id" | "embedded_action_id" | "embedded_parent_res_id", _) => {
                return Err(Error::validation(format!(
                    "Option '{k}' is unavailable in get_views; filter and action context are deferred to a later release."
                )));
            }
            ("studio", _) => {
                return Err(Error::validation(
                    "Option 'studio' is forbidden in get_views.",
                ));
            }
            ("mobile" | "toolbar" | "load_filters", _) => {
                return Err(fail(format!("get_views option '{k}' must be boolean.")));
            }
            _ => return Err(fail(format!("Unknown get_views option '{k}'."))),
        }
    }
    match options.get("mobile") {
        None => Ok(false),
        Some(Value::Bool(mobile)) => Ok(*mobile),
        Some(_) => Err(Error::invariant("validated mobile option changed type")),
    }
}

async fn get_views(
    env: &Env,
    tx: &mut Transaction,
    request: ViewsRequest<'_>,
    admission: &mut ResponseAdmission,
) -> Result<wire::ViewDescription> {
    let ViewsRequest {
        model,
        views,
        wire_major,
        options,
        action_presentation,
    } = request;
    env.companies()?;
    if wire_major != 1 {
        return Err(fail("Unsupported human UI wire major."));
    }
    let mobile = description_mobile(&options)?;
    if views.is_empty() || views.len() > 16 {
        return Err(fail("get_views requires between 1 and 16 view requests."));
    }
    let caller_model = require_model_read(env, tx, model).await?;
    let spec = caller_model.spec();
    let mut requested = admission.vector::<(Option<i64>, &str)>(views.len())?;
    let mut types = admission.vector::<&str>(views.len())?;
    for item in &views {
        let Value::List(pair) = item else {
            return Err(fail("get_views view entries must be [id|null,type] pairs."));
        };
        if pair.len() != 2 {
            return Err(fail("get_views view entries must be [id|null,type] pairs."));
        }
        let id = match &pair[0] {
            Value::False => None,
            Value::Int(id) if *id > 0 => Some(*id),
            other => return Err(fail(format!("Invalid view identity {other:?}."))),
        };
        let view_type = match &pair[1] {
            Value::Str(value) => value.as_ref(),
            other => return Err(fail(format!("Invalid view type {other:?}."))),
        };
        if types.contains(&view_type) {
            return Err(fail(format!(
                "Duplicate requested view type '{view_type}'."
            )));
        }
        types.push(view_type);
        requested.push((id, view_type));
    }

    let published = published_view_types(env)?;
    let budget = admission.budget.clone();
    let mut serialized = admission.vector::<wire::View>(requested.len())?;
    let mut all_fields = BTreeSet::new();
    let mut total_nodes = 0usize;
    let mut notices = Vec::new();
    for (id, view_type) in requested {
        let declaration = published
            .find(view_type)
            .ok_or_else(|| fail(format!("View type '{view_type}' is not registered.")))?;
        let kind = wire_type(env, view_type)?;
        admission.charge(view_type.len())?;
        admission.charge_items::<(
            std::sync::atomic::AtomicUsize,
            std::sync::atomic::AtomicUsize,
        )>(1)?;
        let ty = ViewType::new(tx, view_type)?;
        let selected = match id {
            Some(id) => ViewRequest::new(model, &ty).view_id(ViewId::new(id)?),
            None => ViewRequest::new(model, &ty).context(env.context()),
        };
        let (payload, selected_id, selected_notices) =
            erp_addon_base::ir_ui_view::with_checked_panel_view(
                tx,
                env,
                &selected,
                &budget,
                |input, selected_id, selected_notices| {
                    let input = input.with_mobile(mobile);
                    total_nodes = total_nodes
                        .checked_add(input.allowed_node_count())
                        .ok_or_else(|| fail("Combined view node count overflow."))?;
                    if total_nodes > MAX_VIEW_NODES {
                        return Err(fail(
                            "Requested views exceed the 4096-node aggregate limit.",
                        ));
                    }
                    declaration
                        .validate(&input, &budget)
                        .map_err(admission_error)?;
                    let payload = declaration
                        .build(&input, &budget)
                        .map_err(admission_error)?;
                    let mut notices = admission.vector::<wire::Notice>(selected_notices.len())?;
                    for notice in selected_notices {
                        let code = admission.format(format_args!("{notice:?}"))?;
                        let message = admission.format(format_args!("{notice}"))?;
                        notices.push(wire::Notice::new(code, message));
                    }
                    Ok((payload, selected_id, notices))
                },
            )
            .await?;
        let (mut view, storage) = payload
            .builtin_view(
                wire::ViewIdentity::stored(wire_id(selected_id.get())?),
                kind,
                declaration.renderer(),
                &budget,
            )
            .map_err(admission_error)?
            .into_parts();
        admission.hold(storage)?;
        let authorized =
            crate::h18_authorize::Request::new(&payload, declaration, model, env, &budget)
                .authorize(&mut view, tx)
                .await?;
        let (fields, storage) = authorized.into_parts();
        admission.hold(storage)?;
        admission.phase("views:aggregate-fields:reserve");
        admission.fields(&mut all_fields, fields)?;
        admission.phase("views:aggregate-fields:copied");
        if view_type == "list" {
            let (panel, names) = checked_list_panel(
                env,
                tx,
                model,
                action_presentation
                    .as_ref()
                    .and_then(|action| action.search_view),
                admission,
            )
            .await?;
            admission.fields(&mut all_fields, names)?;
            if let Some(wire::ViewNode::List {
                page_limit,
                search_panel,
                ..
            }) = view.nodes.get_mut(view.root as usize)
            {
                *search_panel = panel;
                if page_limit.is_none() {
                    *page_limit = action_presentation.as_ref().map(|action| action.limit);
                }
            }
        } else if let Some(wire::ViewNode::Search { search_panel, .. }) =
            view.nodes.get_mut(view.root as usize)
        {
            panel_context(env, model, search_panel, admission)?;
        }
        serialized.push(view);
        admission.phase("views:notices:reserve");
        admission.extend(&mut notices, selected_notices)?;
        admission.phase("views:notices:copied");
    }
    let mut readable_fields = admission.vector::<&str>(all_fields.len())?;
    readable_fields.extend(all_fields.iter().map(String::as_str));
    if !readable_fields.is_empty() {
        admission.source_rows(0, &readable_fields, 0, 0, 0, 0)?;
        env.records(model)?.read(tx, &readable_fields).await?;
    }
    // The readonly-only fields_get path retains just the requested mask entries. Charge its
    // field-pointer scratch, map entries, owned names and the fixed key before invoking it.
    // fields_get filters into a pointer Vec, then FromIterator collects/sorts outer pairs.
    admission.charge_items::<&FieldSpec>(vec_growth_slots::<&FieldSpec>(all_fields.len())?)?;
    admission.charge_items::<(String, BTreeMap<String, Value>)>(vec_growth_slots::<(
        String,
        BTreeMap<String, Value>,
    )>(all_fields.len())?)?;
    // Pinned stable::driftsort scratch is max(len, SMALL_SORT_GENERAL_THRESHOLD + 16)
    // at most; <=20 uses insertion sort. 1.98.1 threshold=32, so the source constant is48.
    if all_fields.len() > 20 {
        admission.charge_items::<(String, BTreeMap<String, Value>)>(all_fields.len().max(48))?;
    }
    admission.charge(crate::h18_storage::btree_storage_bytes::<
        String,
        BTreeMap<String, Value>,
    >(all_fields.len())?)?;
    let mask_map = crate::h18_storage::btree_storage_bytes::<String, Value>(1)?;
    for name in &all_fields {
        admission.charge(
            mask_map
                .checked_add(name.len())
                .and_then(|bytes| bytes.checked_add("readonly".len()))
                .ok_or_else(|| fail("Field access metadata storage overflow."))?,
        )?;
    }
    let field_access = env
        .records(model)?
        .fields_get(tx, Some(&readable_fields), Some(&["readonly"]))
        .await?;
    for name in &all_fields {
        if !field_access.contains_key(name) {
            return Err(Error::access(format!(
                "You may not read field '{model}.{name}'."
            )));
        }
    }
    // A referenced field type the human client cannot render fails here with the located message;
    // inside the serializer the same error would collapse into a generic encoding failure.
    for name in &all_fields {
        if let Some(field) = spec.field(name) {
            field_kind(model, field)?;
        }
    }
    // Count the exact borrowed wire representation before cloning labels/selections.
    admit_field_metadata(
        admission,
        &MetadataPreview(FieldMapPreview {
            model,
            spec,
            names: &all_fields,
        }),
    )?;
    let mut catalogue_text = 0usize;
    for name in &all_fields {
        let field = spec
            .field(name)
            .ok_or_else(|| fail(format!("View references unknown field '{model}.{name}'.")))?;
        let (_, selection, relation, relation_field) = field_kind(model, field)?;
        let mut field_bytes = name
            .len()
            .checked_mul(2)
            .and_then(|bytes| bytes.checked_add(field.string.len()))
            .and_then(|bytes| bytes.checked_add(relation.map_or(0, str::len)))
            .and_then(|bytes| bytes.checked_add(relation_field.map_or(0, str::len)))
            .ok_or_else(|| fail("Description collection storage overflow."))?;
        if let Some(pairs) = selection {
            for (key, label) in pairs {
                field_bytes = field_bytes
                    .checked_add(std::mem::size_of::<(String, String)>())
                    .and_then(|bytes| bytes.checked_add(key.len()))
                    .and_then(|bytes| bytes.checked_add(label.len()))
                    .ok_or_else(|| fail("Description collection storage overflow."))?;
            }
        }
        catalogue_text = catalogue_text
            .checked_add(field_bytes)
            .ok_or_else(|| fail("Description collection storage overflow."))?;
    }
    admission.charge_field_catalogue(all_fields.len(), catalogue_text)?;
    let mut field_descriptions = BTreeMap::new();
    for name in all_fields {
        let field = spec
            .field(&name)
            .ok_or_else(|| fail(format!("View references unknown field '{model}.{name}'.")))?;
        let (kind, selection, relation, relation_field) = field_kind(model, field)?;
        field_descriptions.insert(
            name.clone(),
            wire::FieldDescription::new(
                kind,
                field.string.clone(),
                field.required,
                match field_access
                    .get(&name)
                    .and_then(|description| description.get("readonly"))
                {
                    Some(Value::Bool(value)) => *value,
                    _ => {
                        return Err(Error::invariant(
                            "fields_get omitted the readonly access mask",
                        ));
                    }
                },
                selection.map(<[(String, String)]>::to_vec),
                relation.map(str::to_owned),
                relation_field.map(str::to_owned),
            ),
        );
    }
    admission.charge(crate::h18_storage::btree_storage_bytes::<
        String,
        BTreeMap<String, wire::FieldDescription>,
    >(1)?)?;
    let field_model = admission.format(format_args!("{model}"))?;
    let mut fields = BTreeMap::new();
    fields.insert(field_model, field_descriptions);
    let description_version = description_hash(admission, model, &serialized, &fields)?;
    Ok(wire::ViewDescription::new(
        admission.format(format_args!("{}", env.registry().generation()))?,
        description_version,
        admission.format(format_args!("{model}"))?,
        fields,
        serialized,
        notices,
    ))
}

fn description_hash<T: Serialize>(
    admission: &mut ResponseAdmission,
    model: &str,
    views: &[T],
    fields: &BTreeMap<String, BTreeMap<String, wire::FieldDescription>>,
) -> Result<String> {
    let encoded = admission.encode(&(model, views, fields))?;
    // Same canonical FNV-1a token as before, over the bounded, budget-charged encoding.
    let hash = encoded
        .as_json_bytes()
        .iter()
        .fold(0xcbf29ce484222325_u64, |state, byte| {
            (state ^ u64::from(*byte)).wrapping_mul(0x100000001b3)
        });
    admission.format(format_args!("{hash:016x}"))
}

#[methods(IrUiMenu, addon = "web")]
impl WebIrUiMenu {
    /// Load the visible, ancestor-pruned menu forest.
    #[method]
    #[rpc(effect = read, model)]
    async fn erp_load_web_menus(this: &RecordSet<IrUiMenu>, tx: &mut Transaction) -> Result<Value> {
        let mut admission = ResponseAdmission::new();
        let menus = load_menus(this.env(), tx, &mut admission).await?;
        admission.wire(&menus)
    }
}

async fn load_action_views(
    this: &Records,
    tx: &mut Transaction,
    wire_major: u64,
    options: Option<ValueMap>,
    admission: &mut ResponseAdmission,
) -> Result<wire::WindowActionViews> {
    let action = load_action(this, tx, admission).await?;
    let mut requested = admission.vector::<Value>(action.views.len())?;
    for view in &action.views {
        let id = view
            .id
            .map(|id| {
                i64::try_from(u64::from(id))
                    .map(Value::Int)
                    .map_err(|_| Error::invariant("wire view identity exceeds i64"))
            })
            .transpose()?
            .unwrap_or(Value::False);
        let name = view.view_type.as_str();
        admission.charge(name.len())?;
        admission.charge_items::<(
            std::sync::atomic::AtomicUsize,
            std::sync::atomic::AtomicUsize,
        )>(1)?;
        let mut pair = admission.vector::<Value>(2)?;
        pair.push(id);
        pair.push(Value::Str(name.into()));
        // Arc<[Value]> construction owns replacement storage while the temporary Vec lives.
        admission.charge_items::<Value>(2)?;
        admission.charge_items::<(
            std::sync::atomic::AtomicUsize,
            std::sync::atomic::AtomicUsize,
        )>(1)?;
        requested.push(Value::List(pair.into()));
    }
    let presentation = if action
        .views
        .iter()
        .any(|view| matches!(view.view_type, wire::ViewType::List))
    {
        admission.source_rows(1, &["search_view_id", "limit"], 0, 0, 0, 0)?;
        let rows = this
            .sudo(true)
            .read_with(tx, &["search_view_id", "limit"], Load::Raw)
            .await?;
        Some(ActionListPresentation {
            search_view: optional_id(&rows[0], "search_view_id")?,
            limit: admitted_page_limit(i64::from(get_i32(&rows[0], "limit")?))?,
        })
    } else {
        None
    };
    let views = get_views(
        this.env(),
        tx,
        ViewsRequest {
            model: &action.model,
            views: requested,
            wire_major,
            options: options.unwrap_or_default(),
            action_presentation: presentation,
        },
        admission,
    )
    .await?;
    Ok(wire::WindowActionViews::new(action, views))
}

#[methods(IrActionsActWindow, addon = "web")]
impl WebIrActionsActWindow {
    /// Load an authorized action and its bounded view descriptions in this transaction.
    /// The action owns the model/view selection; a failed member rejects the whole RPC.
    #[method]
    #[rpc(effect = read)]
    async fn erp_load_web_action_views(
        this: &RecordSet<IrActionsActWindow>,
        tx: &mut Transaction,
        wire_major: u64,
        options: Option<ValueMap>,
    ) -> Result<Value> {
        let mut admission = ResponseAdmission::new();
        let result =
            load_action_views(this.as_records(), tx, wire_major, options, &mut admission).await?;
        admission.wire(&result)
    }

    /// Load one caller-authorized native window action.
    #[method]
    #[rpc(effect = read)]
    async fn erp_load_web_action(
        this: &RecordSet<IrActionsActWindow>,
        tx: &mut Transaction,
    ) -> Result<Value> {
        let mut admission = ResponseAdmission::new();
        let action = load_action(this.as_records(), tx, &mut admission).await?;
        admission.wire(&action)
    }
}

struct RegistryLookup<'a>(&'a Env);
impl crate::action_result::ModelLookup for RegistryLookup<'_> {
    fn has_model(&self, model: &str) -> bool {
        self.0.registry().model(model).is_some()
    }
    fn has_field(&self, model: &str, field: &str) -> bool {
        self.0
            .registry()
            .model(model)
            .is_some_and(|spec| spec.field(field).is_some())
    }
}

/// Validate a returned method value into a typed result (/0107). The object method
/// is not re-executed. Window-model Read is checked; current-list domain planning can run the
/// ORM's registered search-domain hooks under this caller and Read transaction, never an
/// elevated environment. Actual later searches recheck current authority.
async fn normalize_action_result(
    env: &Env,
    tx: &mut Transaction,
    wire_major: u64,
    result: Value,
) -> Result<wire::ActionResultEnvelope> {
    env.companies()?;
    if wire_major != 1 {
        return Err(fail("Unsupported human UI wire major."));
    }
    let json = crate::action_result::to_json(&result, 1)?;
    let normalized = crate::action_result::normalize(&json, &RegistryLookup(env))?;
    if let wire::ActionResult::Window { model, .. }
    | wire::ActionResult::CurrentWindow { model, .. } = &normalized
    {
        require_model_read(env, tx, model).await?;
    }
    if let wire::ActionResult::CurrentWindow { model, domain, .. } = &normalized {
        let domain =
            Domain::from_json(&serde_json::Value::Array(domain.clone())).map_err(Error::from)?;
        // Admission must reject unknown/hidden domain fields now, even for an empty result.
        // Planning follows the ordinary caller-bound search access path; it never executes
        // the method again or elevates the environment. Actual later searches recheck access.
        let _plan = env
            .records(model)?
            .search_query(tx, &domain, &SearchOptions::new())
            .await?;
    }
    Ok(wire::ActionResultEnvelope::new(normalized))
}

#[methods(Base, addon = "web")]
impl WebBase {
    /// Validate the value an object method returned into a typed, bounded action result.
    /// Never re-executes the object method; current-list domain admission uses
    /// ordinary caller-bound search planning and its read/search-domain hooks.
    #[method]
    #[rpc(effect = read, model)]
    async fn erp_normalize_action_result(
        this: &RecordSet<Base>,
        tx: &mut Transaction,
        wire_major: u64,
        result: Value,
    ) -> Result<Value> {
        let normalized = normalize_action_result(this.env(), tx, wire_major, result).await?;
        ResponseAdmission::new().wire(&normalized)
    }

    /// Select, combine and describe caller-readable list/form views.
    #[method]
    #[rpc(effect = read, model)]
    async fn get_views(
        this: &RecordSet<Base>,
        tx: &mut Transaction,
        views: Vec<Value>,
        wire_major: u64,
        options: Option<ValueMap>,
    ) -> Result<Value> {
        let mut admission = ResponseAdmission::new();
        let description = get_views(
            this.env(),
            tx,
            ViewsRequest {
                model: this.as_records().model_name(),
                views,
                wire_major,
                options: options.unwrap_or_default(),
                action_presentation: None,
            },
            &mut admission,
        )
        .await?;
        admission.wire(&description)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recovery_expression_indices_are_authorized_and_dynamic_paths_refused() {
        let fields = erp_sdk::views::expression_references(
            "context[internal_memo]",
            erp_sdk::views::ExpressionKind::Value,
        )
        .unwrap();
        assert_eq!(fields, BTreeSet::from(["internal_memo".to_owned()]));
        let error = erp_sdk::views::expression_references(
            "context.get('record').internal_memo",
            erp_sdk::views::ExpressionKind::Value,
        )
        .unwrap_err();
        assert!(matches!(
            error,
            erp_sdk::views::ViewError::InvalidDescription(_)
        ));
        assert!(error.to_string().contains("a later release"));
    }

    #[test]
    fn recovery_expression_values_track_complete_model_attribute_paths() {
        let fields = erp_sdk::views::expression_references(
            "company_id.partner_id.internal_memo",
            erp_sdk::views::ExpressionKind::Value,
        )
        .unwrap();
        assert_eq!(
            fields,
            BTreeSet::from(["company_id.partner_id.internal_memo".to_owned()])
        );
    }

    #[test]
    fn recovery_metadata_preview_matches_the_owned_wire_shape() {
        let pairs = vec![("normal".to_owned(), "Normal".to_owned())];
        let preview = FieldMetadataPreview {
            kind: wire::FieldKind::Selection,
            label: "Priority",
            required: false,
            readonly: true,
            selection: Some(&pairs),
            relation: None,
            relation_field: None,
        };
        let owned = wire::FieldDescription::new(
            wire::FieldKind::Selection,
            "Priority".to_owned(),
            false,
            true,
            Some(pairs.clone()),
            None,
            None,
        );
        assert_eq!(
            erp_sdk::serde_json::to_value(&preview).unwrap(),
            erp_sdk::serde_json::to_value(&owned).unwrap()
        );
    }

    #[test]
    fn uuid_and_json_fields_map_to_human_wire_kinds() {
        let registry = erp_sdk::RegistryBuilder::new([&erp_addon_base::ADDON, &crate::ADDON])
            .build()
            .unwrap();
        // `ir.job` no longer has a uuid or json column (its admin view shows the uuid as text), so
        // the two types are tried on copies of one of its fields.
        let job = registry.model("ir.job").unwrap();
        let mut field = job.field("job_uuid").unwrap().clone();
        field.ty = FieldType::Uuid;
        let (uuid_kind, _, _, _) = field_kind("ir.job", &field).unwrap();
        assert_eq!(uuid_kind, wire::FieldKind::Char);
        field.ty = FieldType::Json;
        let (json_kind, _, _, _) = field_kind("ir.job", &field).unwrap();
        assert_eq!(json_kind, wire::FieldKind::Text);
    }

    #[test]
    fn recovery_metadata_preview_charges_escaped_content_before_copying() {
        // Escaping consumes the encoded budget even though the borrowed text is smaller.
        let label = "\n".repeat(MAX_FIELD_METADATA_BYTES / 2);
        let preview = FieldMetadataPreview {
            kind: wire::FieldKind::Char,
            label: &label,
            required: false,
            readonly: false,
            selection: None,
            relation: None,
            relation_field: None,
        };
        let admission = ResponseAdmission::new();
        let error = admit_field_metadata(&admission, &preview).unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Validation);
        assert_eq!(
            error.message(),
            "Field metadata exceeds the 2097152-byte limit."
        );
        // A fresh request is unaffected; the refused one stays refused (sticky test below).
        assert!(
            admit_field_metadata(
                &ResponseAdmission::new(),
                &FieldMetadataPreview {
                    label: "Allowed",
                    ..preview
                }
            )
            .is_ok()
        );
    }

    #[test]
    fn recovery_streamed_description_hash_matches_canonical_encoding() {
        let fields = BTreeMap::new();
        let views = ["Unicode 概要", "quoted \" value"];
        let canonical = erp_sdk::serde_json::to_vec(&("test.note", &views[..], &fields)).unwrap();
        let expected = canonical
            .iter()
            .fold(0xcbf29ce484222325_u64, |state, byte| {
                (state ^ u64::from(*byte)).wrapping_mul(0x100000001b3)
            });
        assert_eq!(
            description_hash(&mut ResponseAdmission::new(), "test.note", &views, &fields).unwrap(),
            format!("{expected:016x}")
        );
    }

    #[test]
    fn recovery_description_hash_obeys_the_response_admission_limit() {
        let views = ["x".repeat(MAX_ENCODED_RESPONSE_BYTES + 1)];
        let error = description_hash(
            &mut ResponseAdmission::new(),
            "test.note",
            &views,
            &BTreeMap::new(),
        )
        .unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Validation);
        assert!(error.message().contains("8388608-byte limit"));
    }

    #[test]
    fn recovery_context_literal_arrays_are_not_domain_criteria() {
        let fields = erp_sdk::views::expression_references("{'default_filter': ['internal_memo', '=', 'literal'], 'group_by': ['create_date:month', 'priority']}", erp_sdk::views::ExpressionKind::Context).unwrap();
        assert_eq!(
            fields,
            BTreeSet::from(["create_date".to_owned(), "priority".to_owned()])
        );
    }

    /// encoder; the other D7 caps are covered elsewhere)
    #[test]
    fn recovery_encoded_response_ceiling_refuses_with_the_full_message() {
        let mut admission = ResponseAdmission::new();
        let error = admission
            .wire(&"x".repeat(MAX_ENCODED_RESPONSE_BYTES))
            .unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Validation);
        assert_eq!(
            error.message(),
            "Encoded response exceeds the 8388608-byte limit."
        );
    }

    #[test]
    fn description_options_have_explicit_false_defaults() {
        for (entries, expected) in [
            (vec![], false),
            (vec![("mobile", Value::Bool(true))], true),
            (
                vec![
                    ("toolbar", Value::Bool(false)),
                    ("load_filters", Value::Bool(false)),
                ],
                false,
            ),
        ] {
            let options = entries
                .into_iter()
                .map(|(key, value)| (key.to_owned(), value))
                .collect();
            assert_eq!(description_mobile(&options).unwrap(), expected);
        }
    }

    #[test]
    fn description_option_refusals_name_the_offending_key_and_expected_type() {
        for key in ["mobile", "toolbar", "load_filters"] {
            for value in [Value::False, Value::Int(1)] {
                let options = [(key.to_owned(), value)].into_iter().collect();
                let error = description_mobile(&options).unwrap_err();
                assert_eq!(error.kind(), ErrorKind::Validation);
                assert_eq!(
                    error.message(),
                    format!("get_views option '{key}' must be boolean.")
                );
            }
        }
        let options = [("unexpected".to_owned(), Value::Bool(false))]
            .into_iter()
            .collect();
        let error = description_mobile(&options).unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Validation);
        assert_eq!(error.message(), "Unknown get_views option 'unexpected'.");
    }

    #[test]
    fn final_transport_tree_is_reserved_before_json_materialization() {
        let mut admission = ResponseAdmission::with_limits(32, MAX_ENCODED_RESPONSE_BYTES).unwrap();
        let error = admission.wire(&true).unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Validation);
        assert_eq!(
            error.message(),
            ViewError::LimitExceeded {
                limit: ViewLimitKind::DescriptionBuildBytes,
                max: 32,
                actual: Some(4 + transport_arc_bytes().unwrap()),
            }
            .to_string()
        );
        assert!(admission.budget.check().is_err());
    }

    #[test]
    fn multi_view_request_shares_one_build_budget_and_returns_no_partial_result() {
        let node = std::mem::size_of::<wire::ViewNode>();
        // Each view alone fits; together they exceed the lowered limit.
        let limit = node * 3;
        let mut admission = ResponseAdmission::with_limits(limit, MAX_ENCODED_RESPONSE_BYTES)
            .expect("lowered limits are valid");
        let mut admitted = Vec::new();
        let results: Vec<_> = [2usize, 2]
            .into_iter()
            .map(|nodes| {
                admission
                    .charge_view_nodes(nodes)
                    .map(|()| admitted.push(nodes))
            })
            .collect();
        assert!(results[0].is_ok());
        let error = results[1].as_ref().unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Validation);
        assert_eq!(
            error.message(),
            erp_sdk::views::ViewError::LimitExceeded {
                limit: ViewLimitKind::DescriptionBuildBytes,
                max: limit,
                actual: Some(
                    node * 4
                        + admission.held.capacity() * std::mem::size_of::<DescriptionReservation>()
                ),
            }
            .to_string()
        );
        assert_eq!(admitted, [2], "the refused view is never admitted");
        // No partial description: nothing can be serialized or hashed afterwards.
        assert!(admission.wire(&"ok").is_err());
        assert!(description_hash(&mut admission, "m", &["v"], &BTreeMap::new()).is_err());
    }

    #[test]
    fn a_caught_build_limit_error_cannot_be_followed_by_a_successful_serialization() {
        let mut admission = ResponseAdmission::with_limits(1, MAX_ENCODED_RESPONSE_BYTES)
            .expect("lowered limits are valid");
        let caught = admission.charge_view_nodes(1).unwrap_err();
        assert_eq!(caught.kind(), ErrorKind::Validation);
        let later = admission.wire(&1u8).unwrap_err();
        assert_eq!(later.kind(), ErrorKind::Validation);
        assert_eq!(
            later.message(),
            erp_sdk::views::ViewError::LimitExceeded {
                limit: ViewLimitKind::DescriptionBuildBytes,
                max: 1,
                actual: None,
            }
            .to_string()
        );
        assert!(admission.charge_field_catalogue(0, 0).is_err());
    }

    #[test]
    fn declared_page_limits_have_an_explicit_safety_boundary() {
        for value in [1, 200, 250, 9_007_199_254_740_991] {
            assert_eq!(
                u64::from(admitted_page_limit(value).unwrap()),
                u64::try_from(value).unwrap()
            );
        }
        for value in [-1, 0, 9_007_199_254_740_992, i64::MAX] {
            let error = admitted_page_limit(value).unwrap_err();
            assert_eq!(error.kind(), ErrorKind::Validation);
            assert_eq!(
                error.message(),
                "List page limit must be a positive browser-safe integer."
            );
        }
    }

    #[test]
    fn list_filter_choices_are_bounded_and_never_inferred() {
        let selection = |key: &str, label: &str| FieldType::Selection {
            values: Some(vec![(key.to_owned(), label.to_owned())]),
            method: None,
        };
        assert!(
            validate_list_filter_choices(true, &selection(&"é".repeat(128), &"🦀".repeat(64)))
                .is_ok()
        );
        for kind in [
            selection(" ", "Label"),
            selection("key", " "),
            selection(&"é".repeat(129), "Label"),
            selection("key", &"🦀".repeat(65)),
        ] {
            let error = validate_list_filter_choices(true, &kind).unwrap_err();
            assert_eq!(error.kind(), ErrorKind::Validation);
            assert_eq!(
                error.message(),
                "Search panel selection category choices require distinct nonempty keys and nonempty labels of at most 256 bytes."
            );
        }
        let choices = FieldType::Selection {
            values: Some(vec![("draft".into(), "Draft".into())]),
            method: None,
        };
        assert!(validate_list_filter_choices(true, &choices).is_ok());
        for (stored, kind, message) in [
            (
                false,
                choices.clone(),
                "Search panel selection category requires a stored selection field.",
            ),
            (
                true,
                FieldType::Text,
                "Search panel selection category requires a static selection field.",
            ),
            (
                true,
                FieldType::Selection {
                    values: None,
                    method: Some("choices".into()),
                },
                "Search panel selection category requires a static selection field.",
            ),
            (
                true,
                FieldType::Selection {
                    values: Some(Vec::new()),
                    method: None,
                },
                "Search panel selection category requires 1–32 selection choices.",
            ),
            (
                true,
                FieldType::Selection {
                    values: Some((0..33).map(|n| (n.to_string(), n.to_string())).collect()),
                    method: None,
                },
                "Search panel selection category requires 1–32 selection choices.",
            ),
            (
                true,
                FieldType::Selection {
                    values: Some(vec![("x".into(), "X".into()), ("x".into(), "Other".into())]),
                    method: None,
                },
                "Search panel selection category choices require distinct nonempty keys and nonempty labels of at most 256 bytes.",
            ),
        ] {
            let error = validate_list_filter_choices(stored, &kind).unwrap_err();
            assert_eq!(error.kind(), ErrorKind::Validation);
            assert_eq!(error.message(), message);
        }
    }

    fn first_widget(xml: &str) -> Result<wire::ViewNode> {
        let descriptor = ViewRow::new(1, ViewMode::Primary)
            .arch(xml)
            .into_descriptor()
            .map_err(|error| fail(error.to_string()))?;
        let catalogue =
            ViewCatalogue::new(vec![descriptor]).map_err(|error| fail(error.to_string()))?;
        let combined = catalogue
            .combine(1)
            .map_err(|error| fail(error.to_string()))?;
        let child = combined
            .arch()
            .root()
            .children()
            .next()
            .ok_or_else(|| fail("test view has no widget node"))?;
        service_widget(&child)
    }

    fn check_layout(xml: &str) -> Result<Vec<wire::ViewNode>> {
        let descriptor = ViewRow::new(1, ViewMode::Primary)
            .arch(xml)
            .into_descriptor()
            .map_err(|error| fail(error.to_string()))?;
        let catalogue =
            ViewCatalogue::new(vec![descriptor]).map_err(|error| fail(error.to_string()))?;
        let combined = catalogue
            .combine(1)
            .map_err(|error| fail(error.to_string()))?;
        let root = combined.arch().root();
        let mode = if root.attr("record_mode") == Some("service") {
            wire::RecordMode::Service
        } else {
            wire::RecordMode::Record
        };
        let plan = layout_plan(root, root.tag() != "list", mode)?;
        let mut result = Vec::new();
        let tags: Vec<&str> = plan.iter().map(|(node, _)| node.tag()).collect();
        for (node, parent) in plan.into_iter().skip(1) {
            if node.tag() == "button" {
                result.push(button_plan(&node, tags[parent] == "footer")?.node);
            } else if !matches!(node.tag(), "field" | "widget") {
                result.push(layout_node(&node)?);
                parse_bool_attr(&node, "invisible", false).map_err(layout_error)?;
            }
        }
        Ok(result)
    }

    #[test]
    fn footer_is_one_last_form_child_of_buttons() {
        let nodes = check_layout(
            r#"<form><sheet><group/></sheet><footer><button name="confirm" type="object" string="Confirm" class="btn-primary" confirm="Sure?"/><button special="cancel" string="Cancel"/></footer></form>"#,
        )
        .unwrap();
        assert!(matches!(&nodes[2], wire::ViewNode::Footer { .. }));
        assert!(matches!(
            &nodes[3],
            wire::ViewNode::Button { name, variant: wire::ButtonVariant::Primary, confirm: Some(_), .. } if name == "confirm"
        ));
        assert!(matches!(
            &nodes[4],
            wire::ViewNode::CancelButton { label, variant: wire::ButtonVariant::Secondary, .. } if label == "Cancel"
        ));
        for (xml, message) in [
            (
                r#"<form><footer/><sheet/></form>"#,
                "<footer> must be the single last child of <form>, after the sheet and content. Unsupported form layout requires a later release.",
            ),
            (
                r#"<form><footer/><footer/></form>"#,
                "<footer> must be the single last child of <form>, after the sheet and content. Unsupported form layout requires a later release.",
            ),
            (
                r#"<form><sheet><footer/></sheet></form>"#,
                "View element <footer> is not allowed under <sheet>. Unsupported form layout requires a later release.",
            ),
            (
                r#"<form record_mode="service"><footer/></form>"#,
                "View element <footer> is not allowed under <form>. Unsupported form layout requires a later release.",
            ),
            (
                r#"<form><footer><field name="a"/></footer></form>"#,
                "View element <field> is not allowed under <footer>. Unsupported form layout requires a later release.",
            ),
            (
                r#"<list><footer/></list>"#,
                "View element <footer> is not allowed under <list>. Unsupported form layout requires a later release.",
            ),
        ] {
            assert_eq!(check_layout(xml).unwrap_err().to_string(), message, "{xml}");
        }
    }

    #[test]
    fn cancel_button_has_no_method_and_refuses_contradictions() {
        for (xml, message) in [
            (
                r#"<form><header><button special="cancel" string="Cancel"/></header></form>"#,
                "View attribute 'special' is unsupported by the current human UI contract. Unsupported form layout requires a later release.",
            ),
            (
                r#"<form><footer><button special="cancel" name="x" string="Cancel"/></footer></form>"#,
                "A special='cancel' button cannot combine with 'name'. Unsupported form layout requires a later release.",
            ),
            (
                r#"<form><footer><button special="cancel" type="object" string="Cancel"/></footer></form>"#,
                "A special='cancel' button cannot combine with 'type'. Unsupported form layout requires a later release.",
            ),
            (
                r#"<form><footer><button special="cancel" confirm="Sure?" string="Cancel"/></footer></form>"#,
                "A special='cancel' button cannot combine with 'confirm'. Unsupported form layout requires a later release.",
            ),
            (
                r#"<form><footer><button special="save" string="Save"/></footer></form>"#,
                "Button special 'save' is unsupported; only special='cancel' is available. Unsupported form layout requires a later release.",
            ),
            (
                r#"<form><footer><button special="cancel" string="A"/><button special="cancel" string="B"/></footer></form>"#,
                "A <footer> allows at most one special button. Unsupported form layout requires a later release.",
            ),
            (
                r#"<form><footer><button special="cancel"/></footer></form>"#,
                "Button requires a nonempty string. Unsupported form layout requires a later release.",
            ),
        ] {
            assert_eq!(check_layout(xml).unwrap_err().to_string(), message, "{xml}");
        }
    }

    #[test]
    fn form_layout_preserves_static_tracks_and_page_identity() {
        let nodes = check_layout(r#"<form><sheet><group string="概要"><group col="12" colspan="4"/></group><notebook><page name="first"/><page string="Résumé"/></notebook><separator string="Notes"/></sheet></form>"#).unwrap();
        assert!(
            matches!(&nodes[1], wire::ViewNode::Group { label, columns: 4, colspan: 2, .. } if label == "概要")
        );
        assert!(matches!(
            &nodes[2],
            wire::ViewNode::Group {
                columns: 12,
                colspan: 4,
                ..
            }
        ));
        assert!(
            matches!(&nodes[4], wire::ViewNode::Page { name, label, .. } if name == "first" && label == "first")
        );
        assert!(
            matches!(&nodes[5], wire::ViewNode::Page { name, label, .. } if name == "Résumé" && label == "Résumé")
        );
        assert!(matches!(&nodes[6], wire::ViewNode::Separator { label } if label == "Notes"));
    }

    #[test]
    fn form_aside_has_a_closed_root_only_grammar() {
        let nodes = check_layout(
            "<form><sheet/><aside><group string='Record information'/></aside></form>",
        )
        .unwrap();
        assert!(matches!(&nodes[1], wire::ViewNode::Aside { .. }));
        for (xml, message) in [
            (
                "<form><aside/><aside/></form>",
                "A record form allows at most one <aside>.",
            ),
            (
                "<form><sheet><aside/></sheet></form>",
                "View element <aside> is not allowed under <sheet>.",
            ),
            (
                "<form><aside><notebook/></aside></form>",
                "View element <notebook> is not allowed under <aside>.",
            ),
            (
                "<form record_mode='service'><aside/></form>",
                "View element <aside> is not allowed under <form>.",
            ),
            (
                "<form><aside class='arbitrary'/></form>",
                "View attribute 'class' is unsupported by the current human UI contract.",
            ),
        ] {
            assert_eq!(
                check_layout(xml).unwrap_err().to_string(),
                format!("{message} Unsupported form layout requires a later release.")
            );
        }
    }

    #[test]
    fn form_layout_refuses_invalid_grammar_and_resource_limits() {
        for (xml, message) in [
            (
                "<form><page name='a'/></form>",
                "View element <page> is not allowed under <form>.",
            ),
            (
                "<form><notebook><field name='a'/></notebook></form>",
                "View element <field> is not allowed under <notebook>.",
            ),
            (
                "<form><group><sheet/></group></form>",
                "View element <sheet> is not allowed under <group>.",
            ),
            (
                "<form><separator><field name='a'/></separator></form>",
                "View element <field> is not allowed under <separator>.",
            ),
            (
                "<list><group/></list>",
                "View element <group> is not allowed under <list>.",
            ),
            (
                "<form record_mode='service'><sheet/></form>",
                "View element <sheet> is not allowed under <form>.",
            ),
            (
                "<form><group class='arbitrary'/></form>",
                "View attribute 'class' is unsupported by the current human UI contract.",
            ),
            (
                "<form><group invisible='state == 1'/></form>",
                "Dynamic 'invisible' modifier is unsupported by the current human UI contract.",
            ),
            (
                "<form><notebook><page/></notebook></form>",
                "Page requires a nonempty name or string.",
            ),
            (
                "<form><group/>untrusted</form>",
                "Text between view elements is unsupported.",
            ),
        ] {
            let error = check_layout(xml).unwrap_err();
            assert_eq!(error.kind(), ErrorKind::Validation);
            assert_eq!(
                error.to_string(),
                format!("{message} Unsupported form layout requires a later release.")
            );
        }
        for key in ["col", "colspan"] {
            for value in [
                "",
                "0",
                "1",
                "3",
                "13",
                "14",
                "4294967296",
                "-2",
                "2.0",
                " 2",
                "state",
            ] {
                let error =
                    check_layout(&format!("<form><group {key}='{value}'/></form>")).unwrap_err();
                assert_eq!(error.kind(), ErrorKind::Validation);
                assert_eq!(
                    error.to_string(),
                    format!(
                        "<group> {key} must be an even integer from 2 through 12. Unsupported form layout requires a later release."
                    )
                );
            }
        }
        let deep = |groups: usize| {
            format!(
                "<form>{}{}</form>",
                "<group>".repeat(groups),
                "</group>".repeat(groups)
            )
        };
        assert_eq!(check_layout(&deep(31)).unwrap().len(), 31);
        let error = check_layout(&deep(32)).unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Validation);
        assert_eq!(error.to_string(), "View exceeds the 32-level depth limit.");
        let wide = |count: usize| format!("<form>{}</form>", "<separator/>".repeat(count));
        assert_eq!(check_layout(&wide(4095)).unwrap().len(), 4095);
        let error = check_layout(&wide(4096)).unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Validation);
        assert_eq!(error.to_string(), "View exceeds the 4096-node limit.");
    }

    #[test]
    fn raw_form_layout_refuses_text_in_denied_subtrees() {
        for (xml, tag) in [
            (
                "<form><group invisible='1'><group>discarded text<field name='name'/></group></group></form>",
                "group",
            ),
            (
                "<form><group invisible='1'><separator>caption</separator></group></form>",
                "separator",
            ),
        ] {
            let descriptor = ViewRow::new(1, ViewMode::Primary)
                .arch(xml)
                .into_descriptor()
                .unwrap();
            let catalogue = ViewCatalogue::new(vec![descriptor]).unwrap();
            let combined = catalogue.combine(1).unwrap();
            let error =
                layout_plan(combined.arch().root(), true, wire::RecordMode::Record).unwrap_err();
            assert_eq!(error.kind(), ErrorKind::Validation);
            assert_eq!(
                error.to_string(),
                format!(
                    "Text content in <{tag}> is unsupported. Unsupported form layout requires a later release."
                )
            );
        }
    }

    fn combine_nodes(rows: Vec<(i64, ViewMode, Option<i64>, &str)>) -> Result<Vec<wire::ViewNode>> {
        let descriptors = rows
            .into_iter()
            .map(|(id, mode, parent, arch)| {
                ViewRow::new(id, mode)
                    .arch(arch)
                    .inherit_id(parent)
                    .into_descriptor()
                    .map_err(|error| fail(error.to_string()))
            })
            .collect::<Result<Vec<_>>>()?;
        let catalogue = ViewCatalogue::new(descriptors).map_err(|error| fail(error.to_string()))?;
        let combined = catalogue
            .combine(1)
            .map_err(|error| fail(error.to_string()))?;
        let plan = layout_plan(combined.arch().root(), true, wire::RecordMode::Record)?;
        let mut result = Vec::new();
        for (node, _) in plan.into_iter().skip(1) {
            if node.tag() == "button" {
                result.push(header_button(&node)?.node);
            } else if node.tag() != "field" {
                result.push(layout_node(&node)?);
            }
        }
        Ok(result)
    }

    #[test]
    fn form_title_area_preserves_declared_fields() {
        let nodes = check_layout(r#"<form><sheet><div class="oe_title"><h1><field name="display_label"/></h1><h2><field name="caption"/></h2></div><group><field name="body"/></group></sheet></form>"#).unwrap();
        let actual = serde_json::to_value(nodes).unwrap();
        assert_eq!(actual[1]["kind"], "title");
        assert_eq!(actual[2]["kind"], "heading");
        assert_eq!(actual[2]["level"], 1);
        assert_eq!(actual[3]["level"], 2);
    }

    #[test]
    fn form_title_area_extends_through_existing_xml_inheritance() {
        let nodes = combine_nodes(vec![
            (1, ViewMode::Primary, None, r#"<form><sheet><div class="oe_title"><h1><field name="display_label"/></h1></div><group><field name="body"/></group></sheet></form>"#),
            (2, ViewMode::Extension, Some(1), r#"<data><xpath expr="//div[@class='oe_title']" position="inside"><h2><field name="caption"/></h2></xpath></data>"#),
        ]).unwrap();
        assert!(
            nodes
                .iter()
                .any(|node| matches!(node, wire::ViewNode::Title { .. }))
        );
        assert_eq!(
            nodes
                .iter()
                .filter(|node| matches!(node, wire::ViewNode::Heading { .. }))
                .count(),
            2
        );
    }

    #[test]
    fn form_title_area_rejects_unadmitted_markup() {
        for arch in [
            r#"<form><sheet><div class="other"><h1><field name="name"/></h1></div></sheet></form>"#,
            r#"<form><sheet><h1><field name="name"/></h1></sheet></form>"#,
            r#"<form><sheet><div class="oe_title"><h3><field name="name"/></h3></div></sheet></form>"#,
            r#"<form><sheet><div class="oe_title"><h1 class="custom"><field name="name"/></h1></div></sheet></form>"#,
            r#"<form><sheet><div class="oe_title"><h1><span>Title</span></h1></div></sheet></form>"#,
        ] {
            assert!(check_layout(arch).is_err(), "unadmitted markup: {arch}");
        }
    }

    #[test]
    fn form_header_admits_statusbar_fields() {
        let nodes = check_layout(
            r#"<form><header><field name="phase" widget="statusbar" statusbar_visible="draft,done"/></header><sheet/></form>"#,
        )
        .unwrap();
        assert!(matches!(&nodes[0], wire::ViewNode::Header { .. }));
    }

    #[test]
    fn inherited_smart_buttons_keep_the_declared_box() {
        let nodes = combine_nodes(vec![
            (1, ViewMode::Primary, None, "<form><sheet><div name='button_box' class='oe_button_box'/><group/></sheet></form>"),
            (2, ViewMode::Extension, Some(1), r#"<xpath expr="//div[@name='button_box']" position='inside'><button name='open_lines' type='object' class='oe_stat_button'><field name='line_count' widget='statinfo' string='Lines'/></button></xpath>"#),
            (3, ViewMode::Extension, Some(1), r#"<xpath expr="//button[@name='open_lines']" position='attributes'><attribute name='invisible'>line_count == 0</attribute></xpath>"#),
        ]).unwrap();
        assert!(matches!(&nodes[1], wire::ViewNode::ButtonBox { .. }));
        assert!(
            matches!(&nodes[2], wire::ViewNode::StatButton { name, label, modifiers, .. } if name == "open_lines" && label == "Lines" && modifiers.invisible.is_some())
        );
        let invalid = check_layout("<form><sheet><div name='arbitrary'><button name='open_lines' type='object' string='Lines'/></div></sheet></form>").unwrap_err();
        assert_eq!(invalid.kind(), ErrorKind::Validation);
        assert!(invalid.to_string().contains("not allowed under <sheet>"));
    }

    #[test]
    fn form_header_accepts_object_buttons() {
        let nodes = check_layout(
            r#"<form><header><button name="lend" type="object" string="Lend" class="btn-primary" confirm="Lend this book?" invisible="state != 'available'"/><button name="give_back" type="object" string="Return"/></header><sheet/></form>"#,
        )
        .unwrap();
        assert!(matches!(&nodes[0], wire::ViewNode::Header { .. }));
        match &nodes[1] {
            wire::ViewNode::Button {
                name,
                button_type: wire::ButtonType::Object,
                label,
                variant: wire::ButtonVariant::Primary,
                confirm: Some(confirm),
                modifiers,
                details: None,
            } => {
                assert_eq!((name.as_str(), label.as_str()), ("lend", "Lend"));
                assert_eq!(confirm, "Lend this book?");
                assert!(modifiers.invisible.is_some());
                assert!(modifiers.readonly.is_none() && modifiers.required.is_none());
            }
            other => panic!("unexpected node {other:?}"),
        }
        assert!(matches!(
            &nodes[2],
            wire::ViewNode::Button {
                variant: wire::ButtonVariant::Secondary,
                confirm: None,
                ..
            }
        ));
        assert!(matches!(&nodes[3], wire::ViewNode::Sheet { .. }));
    }

    #[test]
    fn button_invisible_is_static_or_a_bounded_expression_over_names() {
        let plan = |arch: &str| {
            let descriptor = ViewRow::new(1, ViewMode::Primary)
                .arch(format!("<form><header>{arch}</header></form>"))
                .into_descriptor()
                .map_err(|error| fail(error.to_string()))?;
            let catalogue =
                ViewCatalogue::new(vec![descriptor]).map_err(|e| fail(e.to_string()))?;
            let combined = catalogue.combine(1).map_err(|e| fail(e.to_string()))?;
            let button = combined
                .arch()
                .root()
                .children()
                .next()
                .and_then(|header| header.children().next())
                .ok_or_else(|| fail("no button"))?;
            header_button(&button)
        };
        let hidden = plan(r#"<button name="a" type="object" string="A" invisible="1"/>"#).unwrap();
        assert!(hidden.hidden && hidden.names.is_empty());
        let shown =
            plan(r#"<button name="a" type="object" string="A" invisible="False"/>"#).unwrap();
        assert!(!shown.hidden);
        let dynamic = plan(
            r#"<button name="a" type="object" string="A" invisible="state != 'available' or not active"/>"#,
        )
        .unwrap();
        assert_eq!(
            dynamic.names.iter().map(String::as_str).collect::<Vec<_>>(),
            ["active", "state"]
        );
        let call = plan(r#"<button name="a" type="object" string="A" invisible="foo(state)"/>"#)
            .err()
            .unwrap();
        assert_eq!(call.kind(), ErrorKind::Validation);
        assert!(
            call.to_string().ends_with("requires a later release."),
            "{call}"
        );
    }

    #[test]
    fn form_header_refuses_everything_outside_the_object_button_grammar() {
        for (xml, message) in [
            (
                "<form><sheet/><header/></form>",
                "<header> must be the first child of <form>, before the sheet and content.",
            ),
            (
                "<form><header/><header/></form>",
                "<header> must be the first child of <form>, before the sheet and content.",
            ),
            (
                "<form><header><field name='state'/></header></form>",
                "View element <field> is not allowed under <header>.",
            ),
            (
                "<form><header><group/></header></form>",
                "View element <group> is not allowed under <header>.",
            ),
            (
                "<form><sheet><header/></sheet></form>",
                "View element <header> is not allowed under <sheet>.",
            ),
            (
                "<list><header/></list>",
                "View element <header> is not allowed under <list>.",
            ),
            (
                "<form><header><button name='a' type='object' string='A'><field name='x'/></button></header></form>",
                "View element <field> is not allowed under <button>.",
            ),
            (
                "<form><header class='x'/></form>",
                "View attribute 'class' is unsupported by the current human UI contract.",
            ),
            (
                "<form><header><button name='a' type='action' string='A'/></header></form>",
                "Button type 'action' is unsupported; only type='object' is available.",
            ),
            (
                "<form><header><button name='a' string='A'/></header></form>",
                "Button requires type='object'.",
            ),
            (
                "<form><header><button type='object' string='A'/></header></form>",
                "Button requires a name.",
            ),
            (
                "<form><header><button name='_private' type='object' string='A'/></header></form>",
                "Button name '_private' is not a public method name.",
            ),
            (
                "<form><header><button name='a.b' type='object' string='A'/></header></form>",
                "Button name 'a.b' is not a public method name.",
            ),
            (
                "<form><header><button name='a' type='object'/></header></form>",
                "Button requires a nonempty string.",
            ),
            (
                "<form><header><button name='a' type='object' string='A' confirm=' '/></header></form>",
                "Button confirm cannot be empty.",
            ),
            (
                "<form><header><button name='a' type='object' string='A' class='fancy'/></header></form>",
                "Button class 'fancy' is unsupported; use btn-primary, btn-secondary or btn-danger.",
            ),
            (
                "<form><header><button name='a' type='object' string='A' context='{}'/></header></form>",
                "View attribute 'context' is unsupported by the current human UI contract.",
            ),
            (
                "<form><header><button name='a' type='object' string='A' special='cancel'/></header></form>",
                "View attribute 'special' is unsupported by the current human UI contract.",
            ),
            (
                "<form><header><button name='a' type='object' string='A'>text</button></header></form>",
                "Text content in <button> is unsupported.",
            ),
        ] {
            let error = check_layout(xml).unwrap_err();
            assert_eq!(error.kind(), ErrorKind::Validation, "{xml}");
            assert_eq!(
                error.to_string(),
                format!("{message} Unsupported form layout requires a later release."),
                "{xml}"
            );
        }
        let record = check_layout("<form record_mode='service'><header/></form>").unwrap_err();
        assert_eq!(
            record.to_string(),
            "View element <header> is not allowed under <form>. Unsupported form layout requires a later release."
        );
    }

    #[test]
    fn extension_views_add_and_modify_header_buttons() {
        let base = (
            1,
            ViewMode::Primary,
            None,
            "<form><sheet><group/></sheet></form>",
        );
        // A package adds a header before the sheet, then another package adds a button inside
        // it and a third inserts one before the first and one after it.
        let nodes = combine_nodes(vec![
            base,
            (
                2,
                ViewMode::Extension,
                Some(1),
                r#"<xpath expr="//sheet" position="before"><header><button name="lend" type="object" string="Lend"/></header></xpath>"#,
            ),
            (
                3,
                ViewMode::Extension,
                Some(1),
                r#"<xpath expr="//header" position="inside"><button name="give_back" type="object" string="Return"/></xpath>"#,
            ),
            (
                4,
                ViewMode::Extension,
                Some(1),
                r#"<xpath expr="//button[@name='lend']" position="before"><button name="reserve" type="object" string="Reserve"/></xpath>"#,
            ),
            (
                5,
                ViewMode::Extension,
                Some(1),
                r#"<xpath expr="//button[@name='lend']" position="after"><button name="renew" type="object" string="Renew"/></xpath>"#,
            ),
        ])
        .unwrap();
        let names: Vec<&str> = nodes
            .iter()
            .filter_map(|node| match node {
                wire::ViewNode::Button { name, .. } => Some(name.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(names, ["reserve", "lend", "renew", "give_back"]);
        assert!(matches!(&nodes[0], wire::ViewNode::Header { .. }));
    }

    #[test]
    fn inheritance_cannot_place_a_header_after_the_content() {
        let error = combine_nodes(vec![
            (1, ViewMode::Primary, None, "<form><sheet/></form>"),
            (
                2,
                ViewMode::Extension,
                Some(1),
                r#"<xpath expr="//sheet" position="after"><header/></xpath>"#,
            ),
        ])
        .unwrap_err();
        assert_eq!(
            error.to_string(),
            "<header> must be the first child of <form>, before the sheet and content. Unsupported form layout requires a later release."
        );
    }

    #[test]
    fn inherited_attribute_changes_are_revalidated() {
        let nodes = combine_nodes(vec![
            (
                1,
                ViewMode::Primary,
                None,
                r#"<form><header><button name="lend" type="object" string="Lend"/></header></form>"#,
            ),
            (
                2,
                ViewMode::Extension,
                Some(1),
                r#"<xpath expr="//button[@name='lend']" position="attributes"><attribute name="string">Borrow</attribute><attribute name="class">btn-danger</attribute></xpath>"#,
            ),
        ])
        .unwrap();
        assert!(matches!(
            &nodes[1],
            wire::ViewNode::Button { label, variant: wire::ButtonVariant::Danger, .. } if label == "Borrow"
        ));
        let error = combine_nodes(vec![
            (
                1,
                ViewMode::Primary,
                None,
                r#"<form><header><button name="lend" type="object" string="Lend"/></header></form>"#,
            ),
            (
                2,
                ViewMode::Extension,
                Some(1),
                r#"<xpath expr="//button" position="attributes"><attribute name="type">action</attribute></xpath>"#,
            ),
        ])
        .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("Button type 'action' is unsupported")
        );
    }

    #[test]
    fn parses_canonical_menu_action_reference() {
        assert_eq!(
            parse_action_ref("ir.actions.act_window,42").unwrap(),
            ("ir.actions.act_window", 42)
        );
        for invalid in [
            "",
            "ir.actions.act_window",
            "ir.actions.act_window,0",
            "ir.actions.act_window,-1",
            "ir.actions.act_window,1x",
        ] {
            assert!(
                parse_action_ref(invalid).is_err(),
                "accepted malformed action reference {invalid:?}"
            );
        }
    }

    #[test]
    fn service_settings_widget_requires_closed_valid_scope_vocabulary() {
        assert!(matches!(
            first_widget(
                r#"<form><widget name="web.settings" scope_kinds="global,company"/></form>"#
            )
            .unwrap(),
            wire::ViewNode::Widget {
                widget: wire::ServiceWidget::Settings,
                ..
            }
        ));
        let duplicate = first_widget(
            r#"<form><widget name="web.settings" scope_kinds="global,global"/></form>"#,
        )
        .unwrap_err();
        assert_eq!(duplicate.kind(), ErrorKind::Validation);
        assert_eq!(
            duplicate.to_string(),
            "Duplicate web.settings scope kind 'global'."
        );
        let unknown = first_widget(
            r#"<form><widget name="web.settings" scope_kinds="global,tenant"/></form>"#,
        )
        .unwrap_err();
        assert_eq!(unknown.kind(), ErrorKind::Validation);
        assert_eq!(
            unknown.to_string(),
            "Unknown web.settings scope kind 'tenant'."
        );
        let extra =
            first_widget(r#"<form><widget name="web.addons" scope_kinds="global"/></form>"#)
                .unwrap_err();
        assert_eq!(extra.kind(), ErrorKind::Validation);
        assert_eq!(extra.to_string(), "web.addons does not accept scope_kinds.");
    }

    /// An addon's own settings menu opens the shared page on its app.
    #[test]
    fn service_settings_widget_carries_a_validated_addon_app() {
        let options = |xml: &str| match first_widget(xml).unwrap() {
            wire::ViewNode::Widget {
                options: wire::WidgetOptions::Settings(options),
                ..
            } => options,
            other => panic!("expected a settings widget, got {other:?}"),
        };
        let opened = options(
            r#"<form><widget name="web.settings" scope_kinds="global" app="acceptance_notes"/></form>"#,
        );
        assert_eq!(opened.app.as_deref(), Some("acceptance_notes"));
        let longest = format!("a{}", "b_9".repeat(21));
        assert_eq!(longest.len(), 64);
        let boundary = options(&format!(
            r#"<form><widget name="web.settings" scope_kinds="global" app="{longest}"/></form>"#
        ));
        assert_eq!(boundary.app.as_deref(), Some(longest.as_str()));
        // The wire always carries the member: null opens the first app.
        let first = options(r#"<form><widget name="web.settings" scope_kinds="global"/></form>"#);
        assert_eq!(first.app, None);
        assert_eq!(
            serde_json::to_value(&first).unwrap(),
            serde_json::json!({"scope_kinds": ["global"], "app": null})
        );
        let long = "a".repeat(65);
        for bad in [
            "Sale",
            "1sale",
            "sale.order",
            "sale-order",
            "",
            long.as_str(),
        ] {
            let error = first_widget(&format!(
                r#"<form><widget name="web.settings" scope_kinds="global" app="{bad}"/></form>"#
            ))
            .unwrap_err();
            assert_eq!(error.kind(), ErrorKind::Validation, "accepted app {bad:?}");
            assert_eq!(
                error.to_string(),
                format!("web.settings app '{bad}' is not an addon technical name.")
            );
        }
        let addons =
            first_widget(r#"<form><widget name="web.addons" app="sale"/></form>"#).unwrap_err();
        assert_eq!(addons.kind(), ErrorKind::Validation);
        assert_eq!(addons.to_string(), "web.addons does not accept app.");
    }
}

/// Run the real menu producer under a trusted lower test allowance, without transport encoding.
#[cfg(feature = "test-support")]
pub async fn test_load_menus(
    env: &Env,
    tx: &mut Transaction,
    bytes: usize,
) -> Result<wire::MenuTree> {
    let mut admission = ResponseAdmission::with_limits(bytes, MAX_ENCODED_RESPONSE_BYTES)?;
    load_menus(env, tx, &mut admission).await
}

/// Run the real action producer under a trusted lower test allowance, without transport encoding.
#[cfg(feature = "test-support")]
pub async fn test_load_action(
    env: &Env,
    tx: &mut Transaction,
    id: i64,
    bytes: usize,
) -> Result<wire::WindowAction> {
    let mut admission = ResponseAdmission::with_limits(bytes, MAX_ENCODED_RESPONSE_BYTES)?;
    load_action(
        &env.records("ir.actions.act_window")?.browse([id])?,
        tx,
        &mut admission,
    )
    .await
}

/// Trusted integration test of the actual action-plus-view orchestration; never an RPC.
#[cfg(feature = "test-support")]
pub async fn test_load_action_views(
    env: &Env,
    tx: &mut Transaction,
    id: i64,
    bytes: usize,
) -> Result<wire::WindowActionViews> {
    let mut admission = ResponseAdmission::with_limits(bytes, MAX_ENCODED_RESPONSE_BYTES)?;
    load_action_views(
        &env.records("ir.actions.act_window")?.browse([id])?,
        tx,
        1,
        None,
        &mut admission,
    )
    .await
}

/// Trusted integration test of the actual aggregate view producer; never an RPC.
#[cfg(feature = "test-support")]
pub async fn test_get_views(
    env: &Env,
    tx: &mut Transaction,
    model: &str,
    views: Vec<Value>,
    bytes: usize,
) -> Result<wire::ViewDescription> {
    let mut admission = ResponseAdmission::with_limits(bytes, MAX_ENCODED_RESPONSE_BYTES)?;
    get_views(
        env,
        tx,
        ViewsRequest {
            model,
            views,
            wire_major: 1,
            options: ValueMap::new(),
            action_presentation: None,
        },
        &mut admission,
    )
    .await
}

/// Trusted test instrumentation: the actual menu result and executed static producer checkpoints.
#[cfg(feature = "test-support")]
pub async fn test_load_menus_probed(
    env: &Env,
    tx: &mut Transaction,
    bytes: usize,
) -> Result<(Result<wire::MenuTree>, Vec<&'static str>)> {
    let mut admission = ResponseAdmission::with_limits(bytes, MAX_ENCODED_RESPONSE_BYTES)?;
    let result = load_menus(env, tx, &mut admission).await;
    Ok((result, admission.stages))
}

/// Trusted test instrumentation: the actual action result and executed static producer checkpoints.
#[cfg(feature = "test-support")]
pub async fn test_load_action_probed(
    env: &Env,
    tx: &mut Transaction,
    id: i64,
    bytes: usize,
) -> Result<(Result<wire::WindowAction>, Vec<&'static str>)> {
    let mut admission = ResponseAdmission::with_limits(bytes, MAX_ENCODED_RESPONSE_BYTES)?;
    let result = load_action(
        &env.records("ir.actions.act_window")?.browse([id])?,
        tx,
        &mut admission,
    )
    .await;
    Ok((result, admission.stages))
}

#[cfg(test)]
mod producer_admission_tests {
    #![allow(clippy::unwrap_used, reason = "admission assertions")]
    use super::*;
    #[test]
    fn unsupported_action_source_is_refused_without_changing_empty_semantics() {
        for (domain, context) in [
            (None, None),
            (Some(""), Some("")),
            (Some(" [] "), Some(" {} ")),
        ] {
            assert!(ensure_action_sources(domain, context).unwrap().is_none());
        }
        for (domain, context) in [
            (Some("[('name','=','x')]"), None),
            (Some("[('parent_id', '=', False)]"), None),
        ] {
            assert!(ensure_action_sources(domain, context).unwrap().is_some());
        }
        for (domain, context) in [
            (None, Some(" ")),
            (None, Some("{ }")),
            (None, Some("[]")),
            (None, Some("{'key': 'value'}")),
        ] {
            let error = ensure_action_sources(domain, context).unwrap_err();
            assert_eq!(error.kind(), ErrorKind::Validation);
            assert_eq!(
                error.message(),
                "Non-empty window-action context expressions are not supported by the current human UI client; additional action expressions require a later release."
            );
        }
        for (domain, context) in [(Some(" "), None), (Some("{invalid"), None)] {
            let error = ensure_action_sources(domain, context).unwrap_err();
            assert_eq!(error.kind(), ErrorKind::Validation);
        }
    }
    #[test]
    fn source_row_slots_refuse_before_any_row_materialization() {
        let mut admission = ResponseAdmission::with_limits(1, MAX_ENCODED_RESPONSE_BYTES).unwrap();
        let error = admission
            .source_rows(1, &["name"], 4096, 1, 0, 0)
            .unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Validation);
        assert_eq!(
            error.message(),
            ViewError::LimitExceeded {
                limit: ViewLimitKind::DescriptionBuildBytes,
                max: 1,
                actual: Some(std::mem::size_of::<BTreeMap<String, Value>>())
            }
            .to_string()
        );
        assert!(admission.budget.check().is_err());
    }
}
