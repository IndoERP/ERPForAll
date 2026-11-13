//! The environment-dependent domain optimization levels (SPEC-DOM §4.4, §4.5, §4.7) on top of
//! `erp-domain`'s pure BASIC level.
//!
//! - [`RegistrySchema`] implements `erp_domain::Schema` on the registry: field types, comodels,
//!   `NOT NULL` columns (`id` and required stored fields), the registry generation as version.
//! - [`ZoneTz`] implements `erp_domain::TimeZone` on the IANA database (`env.tz`).
//!
//! DYNAMIC_VALUES resolves relative dates against "now" in the user's timezone and turns selection
//! `not in` into `in`.
//! Relational rules at FULL
//! (DOM §4.5): fields inherited through `_inherits` search their parent (`delegate any (…)`);
//! `any`/`not any` become `any!`/`not any!` in superuser mode or on `bypass_search_access`
//! fields; sub-domains are optimized to FULL for their comodel; a many2one `any!` on `id`
//! collapses to the column; x2many `in`/`not in` become `any!`/`not any!` of a records query.
//! Hierarchy operators are resolved before optimization (`crate::hierarchy`, they need queries);
//! `access` and field `search=` methods need record rules (M3) and are refused with a clear
//! "not supported until" error.
//!
//! Optimization runs on every search; only the SQL text of the result's shape is memoized
//! (`crate::search`), so "now", record rules and the context are always current.

use erp_domain::{
    Condition, DateContext, Domain, DomainKind, FieldInfo, FieldType as DomainFieldType,
    ModelContext, Operator, OptimizationLevel, Schema, TimeZone, Value, ValueSet, parse_date,
};
use erp_fields::{Error, FieldType, Result};
use erp_registry::{FieldSpec, ModelSpec, Registry};
use erp_sql::param::{ScalarKind, primitive_of, to_scalar};
use erp_sql::reference::ReferenceQuery;
use time::PrimitiveDateTime;

/// The registry seen by the domain optimizer.
pub(crate) struct RegistrySchema<'a>(pub(crate) &'a Registry);

fn domain_type(ty: &FieldType) -> (DomainFieldType, Option<&str>) {
    let t = match ty {
        FieldType::Id => DomainFieldType::Id,
        FieldType::Boolean => DomainFieldType::Boolean,
        FieldType::Integer => DomainFieldType::Integer,
        FieldType::Float { .. } => DomainFieldType::Float,
        FieldType::Monetary { .. } => DomainFieldType::Monetary,
        FieldType::Char { .. } => DomainFieldType::Char,
        FieldType::Text => DomainFieldType::Text,
        FieldType::Html => DomainFieldType::Html,
        FieldType::Selection { .. } => DomainFieldType::Selection,
        FieldType::Date => DomainFieldType::Date,
        FieldType::Datetime => DomainFieldType::Datetime,
        FieldType::Uuid => DomainFieldType::Uuid,
        FieldType::Xid8 => DomainFieldType::Xid8,
        FieldType::TimestampTz => DomainFieldType::TimestampTz,

        FieldType::Json => DomainFieldType::Json,
        FieldType::Many2one { comodel, .. } => {
            return (DomainFieldType::Many2one, Some(comodel.as_str()));
        }
        FieldType::One2many { comodel, .. } => {
            return (DomainFieldType::One2many, Some(comodel.as_str()));
        }
        FieldType::Many2many { comodel, .. } => {
            return (DomainFieldType::Many2many, Some(comodel.as_str()));
        }
        FieldType::Reference { .. } => DomainFieldType::Reference,
        FieldType::Many2oneReference { .. } => DomainFieldType::Many2oneReference,
        // compared as an integer (an `int8` column, never NULL)
        FieldType::RowVersion => DomainFieldType::Integer,
        FieldType::DateRange { .. } => DomainFieldType::DateRange,
        FieldType::DatetimeRange { .. } => DomainFieldType::DatetimeRange,
    };
    (t, None)
}

fn is_null_value(value: &Value) -> bool {
    match value {
        Value::Domain(_) | Value::Opaque(_) | Value::Param(_) => false,
        other => !other.is_truthy(),
    }
}

pub(crate) fn not_null(field: &FieldSpec) -> bool {
    !erp_sql::filter::can_be_null(field)
}

impl Schema for RegistrySchema<'_> {
    fn field(&self, model: &str, field: &str) -> Option<FieldInfo<'_>> {
        let spec = self.0.model(model)?.field(field)?;
        let (ty, comodel) = domain_type(&spec.ty);
        let mut info = FieldInfo::new(spec.name, ty).with_not_null(not_null(spec));
        if let Some(comodel) = comodel {
            info = info.with_comodel(comodel);
        }
        Some(info)
    }

    fn has_model(&self, model: &str) -> bool {
        self.0.contains(model)
    }

    fn version(&self) -> u64 {
        self.0.generation()
    }
}

/// An IANA timezone (`env.tz`), for date values compared with datetime fields and relative dates.
pub(crate) struct ZoneTz {
    name: String,
    zone: jiff::tz::TimeZone,
}

impl ZoneTz {
    /// The zone named `name`, or UTC when the name is unknown (`env.tz` already validates it).
    pub(crate) fn new(name: &str) -> Self {
        match jiff::tz::TimeZone::get(name) {
            Ok(zone) => ZoneTz {
                name: name.to_owned(),
                zone,
            },
            Err(_) => ZoneTz {
                name: "UTC".to_owned(),
                zone: jiff::tz::TimeZone::UTC,
            },
        }
    }
}

fn to_civil(dt: PrimitiveDateTime) -> Option<jiff::civil::DateTime> {
    jiff::civil::DateTime::new(
        i16::try_from(dt.year()).ok()?,
        i8::try_from(u8::from(dt.month())).ok()?,
        i8::try_from(dt.day()).ok()?,
        i8::try_from(dt.hour()).ok()?,
        i8::try_from(dt.minute()).ok()?,
        i8::try_from(dt.second()).ok()?,
        i32::try_from(dt.nanosecond()).ok()?,
    )
    .ok()
}

fn from_civil(dt: jiff::civil::DateTime) -> Option<PrimitiveDateTime> {
    let date = time::Date::from_calendar_date(
        i32::from(dt.year()),
        time::Month::try_from(u8::try_from(dt.month()).ok()?).ok()?,
        u8::try_from(dt.day()).ok()?,
    )
    .ok()?;
    let time = time::Time::from_hms_nano(
        u8::try_from(dt.hour()).ok()?,
        u8::try_from(dt.minute()).ok()?,
        u8::try_from(dt.second()).ok()?,
        u32::try_from(dt.subsec_nanosecond()).ok()?,
    )
    .ok()?;
    Some(PrimitiveDateTime::new(date, time))
}

impl TimeZone for ZoneTz {
    fn id(&self) -> &str {
        &self.name
    }

    /// Ambiguous local times take the earlier offset and skipped ones the offset before the gap,
    /// like Python `zoneinfo` with `fold=0` (jiff's "compatible" disambiguation).
    fn local_to_utc(&self, local: PrimitiveDateTime) -> Option<PrimitiveDateTime> {
        let zoned = self
            .zone
            .to_ambiguous_zoned(to_civil(local)?)
            .compatible()
            .ok()?;
        from_civil(jiff::tz::TimeZone::UTC.to_datetime(zoned.timestamp()))
    }

    fn utc_to_local(&self, utc: PrimitiveDateTime) -> Option<PrimitiveDateTime> {
        let ts = to_civil(utc)?.to_zoned(jiff::tz::TimeZone::UTC).ok()?;
        from_civil(self.zone.to_datetime(ts.timestamp()))
    }
}

/// The current time as the naive UTC datetime relative dates start from.
pub(crate) fn now_utc() -> Result<PrimitiveDateTime> {
    primitive_of(erp_fields::DateTime::now())
        .ok_or_else(|| Error::programming("the current time is outside the supported date range"))
}

const MAX_ROUNDS: usize = 1000;

/// First day of the week for `=week_start` in relative dates.
const WEEK_START: time::Weekday = time::Weekday::Sunday;

/// What the `access` operator resolves against: the access policy of a
/// transaction for the environment's principal.
pub(crate) trait AccessLookup {
    /// # Errors
    ///
    /// The policy's errors; `Programming` for an `access` chain that comes back to a
    /// (model, operation) being resolved.
    fn resolve(&self, model: &ModelSpec, op: crate::access::Operation) -> Result<Option<Domain>>;
}

/// The environment of an optimization: registry, timezone, "now", superuser mode (relational
/// conditions bypass access then) and the `access` operator's lookup (`None` outside
/// a transaction, where an `access` condition is an error).
#[derive(Clone, Copy)]
pub(crate) struct OptimizeCtx<'a> {
    pub(crate) registry: &'a Registry,
    pub(crate) tz: &'a ZoneTz,
    pub(crate) now: PrimitiveDateTime,
    pub(crate) su: bool,
    pub(crate) access: Option<&'a dyn AccessLookup>,
}

/// Optimize `domain` for `spec` up to `level` (BASIC, DYNAMIC_VALUES or FULL).
pub(crate) fn optimize_domain(
    octx: &OptimizeCtx<'_>,
    spec: &ModelSpec,
    domain: &Domain,
    level: OptimizationLevel,
) -> Result<Domain> {
    let OptimizeCtx {
        registry,
        tz,
        now,
        su,
        access,
    } = *octx;
    let schema = RegistrySchema(registry);
    let ctx = ModelContext::new(&schema, spec.name).with_tz(tz);
    let mut current = domain.optimize(&ctx).map_err(Error::from)?;
    if level <= OptimizationLevel::Basic {
        return Ok(current);
    }
    let rewriter = Rewriter {
        registry,
        spec,
        tz,
        now,
        su,
        access,
        dates: DateContext::new(now, tz, WEEK_START),
        full: level == OptimizationLevel::Full,
        external: domain.is_external(),
        programming_context: domain.programming_context().map(AsRef::as_ref),
    };
    for _ in 0..MAX_ROUNDS {
        let mut failure: Option<Error> = None;
        let next = current.map_conditions(|node| {
            if failure.is_some() {
                return node.clone();
            }
            let Some(condition) = node.as_condition() else {
                return node.clone();
            };
            let node_rewriter = Rewriter {
                external: node.is_external() || rewriter.external,
                programming_context: node
                    .programming_context()
                    .map(AsRef::as_ref)
                    .or(rewriter.programming_context),
                ..rewriter
            };
            match node_rewriter.rewrite(condition) {
                Ok(Some(rewritten)) => rewritten,
                Ok(None) => node.clone(),
                Err(error) => {
                    failure = Some(error);
                    node.clone()
                }
            }
        });
        if let Some(error) = failure {
            return Err(error);
        }
        if next == current {
            return Ok(current);
        }
        current = next.optimize(&ctx).map_err(Error::from)?;
    }
    Err(Error::programming("Domain.optimize: too many loops"))
}

/// Reuse the existing related rewrite before any callback can be introduced by FULL.
pub(crate) fn framework_search_domain(
    env: &crate::Env,
    model: &ModelSpec,
    field: &FieldSpec,
    condition: &Condition,
    external: bool,
    programming_context: Option<&std::sync::Arc<str>>,
) -> Result<Domain> {
    let tz = ZoneTz::new(env.tz());
    let now = now_utc()?;
    let schema = RegistrySchema(env.registry());
    let input = Domain::condition(
        condition.field_expr(),
        condition.operator(),
        condition.value().clone(),
    )?;
    let input = if let Some(context) = programming_context {
        input.with_programming_context(std::sync::Arc::clone(context))
    } else if external {
        input.into_external()
    } else {
        input
    };
    let basic = input
        .optimize(&ModelContext::new(&schema, model.name).with_tz(&tz))
        .map_err(Error::from)?;
    let Some(condition) = basic.as_condition() else {
        return Ok(basic);
    };
    let rewriter = Rewriter {
        registry: env.registry(),
        spec: model,
        tz: &tz,
        now,
        su: env.su(),
        access: None,
        dates: DateContext::new(now, &tz, WEEK_START),
        full: true,
        external: basic.is_external(),
        programming_context: basic.programming_context().map(AsRef::as_ref),
    };
    if let Some(inherited) = field.inherited {
        let delegate = model.field_by_id(inherited.delegate).name;
        return rewriter.condition(delegate, Operator::Any, Value::Domain(basic.clone()));
    }
    if let Some(path) = field.related {
        return rewriter.search_related(field, path, condition.operator(), condition.value());
    }
    if field.name == "display_name" && field.compute == Some("_compute_display_name") {
        return rewriter.search_display_name(condition.operator(), condition.value());
    }
    Err(Error::programming(
        "framework search rewrite requires a delegated, related or display-name field",
    ))
}

#[derive(Clone, Copy)]
struct Rewriter<'a> {
    registry: &'a Registry,
    spec: &'a ModelSpec,
    tz: &'a ZoneTz,
    now: PrimitiveDateTime,
    su: bool,
    access: Option<&'a dyn AccessLookup>,
    dates: DateContext<'a>,
    full: bool,
    external: bool,
    programming_context: Option<&'a str>,
}

impl<'a> Rewriter<'a> {
    /// An error caused by the domain itself: `Validation` for external input.
    fn invalid(&self, message: String) -> Error {
        if let Some(context) = self.programming_context {
            Error::programming(format!("{context}: {message}"))
        } else if self.external {
            Error::validation(message)
        } else {
            Error::programming(message)
        }
    }

    /// One condition's DYNAMIC/FULL rewrite, or `None` when nothing applies.
    fn rewrite(&self, condition: &Condition) -> Result<Option<Domain>> {
        let expr = condition.field_expr();
        if expr.contains('.') {
            if condition.operator() == Operator::Access {
                return Err(self.invalid(format!(
                    "The 'access' operator does not work for properties in condition \
                     {condition:?}"
                )));
            }
            // Paths on relational fields are `any` after BASIC; what remains is a property.
            return Ok(None);
        }
        let Some(field) = self.spec.field(expr) else {
            return Ok(None);
        };
        let operator = condition.operator();
        let value = condition.value();

        // ----- DYNAMIC_VALUES -----
        if operator == Operator::Access {
            return self.access(field, condition).map(Some);
        }
        let comparison = matches!(
            operator,
            Operator::In
                | Operator::NotIn
                | Operator::Lt
                | Operator::Gt
                | Operator::Le
                | Operator::Ge
        );
        if comparison && matches!(field.ty, FieldType::Date | FieldType::Datetime) {
            let datetime = matches!(field.ty, FieldType::Datetime);
            if let Some(resolved) = self.resolve_dates(value, datetime)? {
                return self.condition(expr, operator, resolved).map(Some);
            }
        }
        if let (
            Operator::NotIn,
            FieldType::Selection {
                values: Some(keys), ..
            },
            Value::Set(excluded),
        ) = (operator, &field.ty, value)
            && excluded.iter().any(Value::is_truthy)
        {
            let included: ValueSet = keys
                .iter()
                .map(|(k, _)| Value::Str(k.clone()))
                .chain(std::iter::once(Value::FALSE))
                .filter(|v| !excluded.contains(v))
                .collect();
            return self
                .condition(expr, Operator::In, Value::Set(included))
                .map(Some);
        }
        if !self.full {
            return Ok(None);
        }

        // ----- FULL -----
        if operator.is_any()
            && let Some(query) = ReferenceQuery::of(value)
        {
            return self.reference_query(field, condition, query);
        }
        if let Some(inherited) = field.inherited {
            // a field of an `_inherits` parent searches the parent (the link bypasses access)
            let delegate = self.spec.field_by_id(inherited.delegate).name;
            let sub = self.condition(expr, operator, value.clone())?;
            return self
                .condition(delegate, Operator::Any, Value::Domain(sub))
                .map(Some);
        }
        if field.name == "display_name" && field.compute == Some("_compute_display_name") {
            return self.search_display_name(operator, value).map(Some);
        }
        if matches!(operator, Operator::ChildOf | Operator::ParentOf) {
            // resolved before optimization (`crate::hierarchy`, comodel rules in
            // `crate::prepare`): one that arrives here would reach SQL unprepared
            return Err(Error::programming(format!(
                "the hierarchy condition {condition:?} on {}.{} reached FULL optimization \
                 unresolved",
                self.spec.name, field.name
            )));
        }
        if field.ty.is_x2many()
            && field.related.is_some()
            && !field.store
            && let Some(path) = field.related
        {
            // a related x2many has no records of its own to compare: `in` / `any` go to the last
            // field of its path whole, not through the comodel's ids (which it has no column for)
            return self.search_related(field, path, operator, value).map(Some);
        }
        if field.ty.is_relational() {
            if let Some(rewritten) = self.relational(field, condition)? {
                return Ok(Some(rewritten));
            }
            if operator.is_any() {
                return Ok(None);
            }
        }
        if !matches!(field.ty, FieldType::Id)
            && (!field.store || (!field.has_column() && !field.ty.is_x2many()))
        {
            if let Some(path) = field.related {
                return self.search_related(field, path, operator, value).map(Some);
            }
            return Err(self.invalid(format!(
                "Field has no SQL representation because it is not stored in condition \
                 {condition:?}"
            )));
        }
        if matches!(field.ty, FieldType::Boolean)
            && matches!(operator, Operator::In | Operator::NotIn)
            && let Value::Set(set) = value
            && set.iter().any(Value::is_truthy)
            && set.iter().any(|v| !v.is_truthy())
        {
            // `active in [True, False]` survives BASIC (it disables the active test) and only
            // becomes a constant here.
            return Ok(Some(Domain::constant(operator == Operator::In)));
        }
        if !operator.is_standard() {
            return Err(self.invalid(format!(
                "Not standard operator left in condition {condition:?}"
            )));
        }
        Ok(None)
    }

    fn condition(&self, field: &str, operator: Operator, value: Value) -> Result<Domain> {
        Domain::condition(field, operator, value).map_err(Error::from)
    }

    /// <comodel's access domain>`;
    /// `FALSE` when that domain is `FALSE`, `f != False` when it is `TRUE` or the principal is the
    /// superuser.
    fn access(&self, field: &FieldSpec, condition: &Condition) -> Result<Domain> {
        let comodel = match &field.ty {
            FieldType::Id => self.spec,
            FieldType::Many2one { comodel, .. } => {
                self.registry.model(comodel).ok_or_else(|| {
                    Error::programming(format!("Model '{comodel}' does not exist in registry."))
                })?
            }
            _ => {
                return Err(self.invalid(format!(
                    "The 'access' operator works only for many2one and 'id' fields in \
                     condition {condition:?}"
                )));
            }
        };
        let op = match condition.value() {
            Value::Str(name) => crate::access::Operation::parse(name),
            _ => None,
        }
        .ok_or_else(|| {
            self.invalid(format!(
                "Invalid value for 'access' operator in condition {condition:?}"
            ))
        })?;
        let Some(lookup) = self.access else {
            return Err(Error::programming(format!(
                "The 'access' operator in {condition:?} needs the access policy of a \
                 transaction: optimize with Records::optimize_domain_in"
            )));
        };
        let set = || self.condition(field.name, Operator::Ne, Value::FALSE);
        match lookup.resolve(comodel, op)? {
            None => set(),
            Some(domain) if domain.is_false() => Ok(Domain::FALSE),
            Some(domain) if domain.is_true() => set(),
            Some(domain) => self.condition(field.name, Operator::AnyBypass, Value::Domain(domain)),
        }
    }

    /// A negative operator
    /// with a non-empty value is the negation of the positive condition.
    fn search_related(
        &self,
        field: &FieldSpec,
        path: &str,
        operator: Operator,
        value: &Value,
    ) -> Result<Domain> {
        let empty = |v: &Value| match v {
            Value::Set(set) => set.iter().any(is_null_value),
            other => is_null_value(other),
        };
        if operator.is_negative() && !empty(value) {
            return Ok(!self.search_related(field, path, operator.positive(), value)?);
        }
        let matches_empty = !operator.is_negative() && empty(value);
        let names: Vec<&str> = path.split('.').collect();
        // the steps' fields, to know which are nullable many2one fields
        let mut steps: Vec<&FieldSpec> = Vec::with_capacity(names.len());
        let mut model = self.spec;
        for (i, name) in names.iter().enumerate() {
            let step = model.field(name).ok_or_else(|| {
                Error::programming(format!(
                    "Field {}.{} is related to '{path}', which does not resolve",
                    self.spec.name, field.name
                ))
            })?;
            steps.push(step);
            if i + 1 < names.len() {
                model = step
                    .ty
                    .comodel()
                    .and_then(|c| self.registry.model(c))
                    .ok_or_else(|| {
                        Error::programming(format!(
                            "Field {}.{} is related to '{path}', which does not resolve",
                            self.spec.name, field.name
                        ))
                    })?;
            }
        }
        let any = if field.compute_sudo() {
            Operator::AnyBypass
        } else {
            Operator::Any
        };
        // a company-bound field keeps the superuser traversal, but each
        // company-scoped hop only offers the records of the reader's active companies
        let bound = field.company_bound() && !self.su;
        let last_name = names[names.len() - 1];
        // `in [False, F]` holds for an empty value or for F: the empty part needs no bound, the
        // rest keeps it (review FX2), so the set is split when both are present
        let split = match value {
            Value::Set(set) if matches_empty && set.iter().any(|x| !is_null_value(x)) => {
                let (mut nulls, mut rest) = (ValueSet::new(), ValueSet::new());
                for item in set.iter() {
                    if is_null_value(item) {
                        nulls.insert(item.clone());
                    } else {
                        rest.insert(item.clone());
                    }
                }
                Some((nulls, rest))
            }
            _ => None,
        };
        let mut domain = match &split {
            Some((nulls, rest)) => {
                self.condition(last_name, operator, Value::Set(nulls.clone()))?
                    | self.condition(last_name, operator, Value::Set(rest.clone()))?
            }
            None => self.condition(last_name, operator, value.clone())?,
        };
        // a relational last field of a bound path points to one more hop (review SC4): a value
        // is only found when it points to a record of the reader's companies. The read refuses a
        // record with any target outside them, so for several targets the search leaves out
        // every record that has one (review SD1): a record with a foreign and an own target is
        // found by neither
        let last_hop = match steps.last() {
            Some(last) if bound && !last.delegate => self.comodel_spec(last)?,
            _ => None,
        };
        if bound
            && (!matches_empty || split.is_some())
            && let Some(last) = steps.last()
            && let Some(hop) = last_hop
            && let Some(within) = self.active_companies(hop)?
        {
            let apply = |part: Domain| -> Result<Domain> {
                if last.ty.is_x2many() {
                    let foreign = self.condition(
                        last.name,
                        Operator::AnyBypass,
                        Value::Domain(!within.clone()),
                    )?;
                    Ok(part & !foreign)
                } else {
                    Ok(part
                        & self.condition(
                            last.name,
                            Operator::AnyBypass,
                            Value::Domain(within.clone()),
                        )?)
                }
            };
            domain = match &split {
                Some((nulls, rest)) => {
                    self.condition(last_name, operator, Value::Set(nulls.clone()))?
                        | apply(self.condition(last_name, operator, Value::Set(rest.clone()))?)?
                }
                None => apply(domain)?,
            };
        }
        for step in steps[..steps.len() - 1].iter().rev() {
            if bound
                && !step.delegate
                && let Some(hop) = self.comodel_spec(step)?
                && let Some(within) = self.active_companies(hop)?
            {
                domain = domain & within;
            }
            domain = self.condition(step.name, any, Value::Domain(domain))?;
            if matches_empty && matches!(step.ty, FieldType::Many2one { .. }) && !not_null(step) {
                domain = domain | self.condition(step.name, Operator::Eq, Value::FALSE)?;
            }
        }
        Ok(domain)
    }

    /// The model a relational `field` points to; `None` for a field that is not relational.
    ///
    /// # Errors
    ///
    /// `Programming` when the comodel is not in the registry: the bound cannot be skipped for it
    /// (review SF5).
    fn comodel_spec(&self, field: &FieldSpec) -> Result<Option<&'a ModelSpec>> {
        let Some(comodel) = field.ty.comodel() else {
            return Ok(None);
        };
        self.registry.model(comodel).map(Some).ok_or_else(|| {
            Error::programming(format!(
                "Field {}.{} points to model {comodel}, which is not in the registry",
                self.spec.name, field.name
            ))
        })
    }

    /// The condition "this record of `hop` belongs to the reader's active companies, or to none"
    /// by the model's own company match, over the parameter `company_ids` (bound per statement from
    /// the validated companies of the environment); `None` for a model without
    /// company scope.
    ///
    /// # Errors
    ///
    /// `Programming` for a company match this version does not know (fail closed).
    fn active_companies(&self, hop: &ModelSpec) -> Result<Option<Domain>> {
        use erp_registry::{CompanyMatch, CompanyScope};
        let (field, matching) = match hop.company_scope {
            Some(CompanyScope::Single { field, matching }) => {
                (hop.field_by_id(field), CompanyMatch::from(matching))
            }
            Some(CompanyScope::Many { field, matching }) => {
                (hop.field_by_id(field), CompanyMatch::from(matching))
            }
            None | Some(CompanyScope::Itself) => return Ok(None),
            Some(other) => return Err(crate::related_company::unknown_scope(other, hop.name)),
        };
        let active = Value::Param(erp_domain::PrincipalParam::CompanyIds);
        let none = self.condition(field.name, Operator::Eq, Value::FALSE)?;
        let some = match matching {
            CompanyMatch::Exact | CompanyMatch::AnyOf => {
                self.condition(field.name, Operator::In, active)?
            }
            CompanyMatch::ParentOf => {
                self.condition(field.name, Operator::In, self.active_ancestors()?)?
            }
            CompanyMatch::ParentOfAny => {
                // a shared record is usable when one of its companies is an ancestor of an
                // active one
                let ancestors = self.condition("id", Operator::In, self.active_ancestors()?)?;
                self.condition(field.name, Operator::AnyBypass, Value::Domain(ancestors))?
            }
            other => {
                return Err(Error::programming(format!(
                    "company-bound related searches do not know the company match {}",
                    other.as_str()
                )));
            }
        };
        Ok(Some(none | some))
    }

    /// The active companies and their ancestors as a hierarchy query selected in the statement
    /// itself (what `('company_id', 'parent_of', company_ids)` resolves to, `crate::hierarchy`):
    /// the optimizer runs after that resolution, so the resolved form is built here.
    fn active_ancestors(&self) -> Result<Value> {
        let companies = self.registry.model("res.company").ok_or_else(|| {
            Error::programming("a parent company match needs the res.company model")
        })?;
        let parent = companies.parent_name.ok_or_else(|| {
            Error::programming("a parent company match needs res.company to have a parent field")
        })?;
        Ok(erp_sql::relational::HierarchyQuery {
            model: companies.id,
            parent,
            descendants: false,
            start: erp_sql::relational::HierarchyStart::Selected {
                ids: Vec::new(),
                params: vec![erp_domain::PrincipalParam::CompanyIds],
                by_path: false,
            },
        }
        .into_value())
    }

    /// The FULL rule of a reference query (Amendment 1): its predicate is optimized to
    /// FULL on its target model, as the sub-domain of a relational condition is, and the query is
    /// marked optimized. An already optimized query is left alone, so the fixed point holds. A
    /// predicate that is `FALSE` makes the condition `FALSE`.
    ///
    /// # Errors
    ///
    /// `Programming` for a query on a field that is not a `many2one_reference`, for the negated
    /// operators (a negated query could let rows through) or for a target outside the registry.
    fn reference_query(
        &self,
        field: &FieldSpec,
        condition: &Condition,
        query: &ReferenceQuery,
    ) -> Result<Option<Domain>> {
        if !matches!(field.ty, FieldType::Many2oneReference { .. }) {
            return Err(Error::programming(format!(
                "reference query in condition {condition:?} on {}.{}: a reference query applies \
                 to a many2one_reference field",
                self.spec.name, field.name
            )));
        }
        if condition.operator() != Operator::AnyBypass {
            return Err(Error::programming(format!(
                "reference query in condition {condition:?} on {}.{}: only the positive 'any!' \
                 form is supported, a negated reference query could let rows through",
                self.spec.name, field.name
            )));
        }
        if query.is_optimized() {
            return Ok(None);
        }
        let target = crate::search_method::reference_target(self.registry, query)?;
        let optimized = optimize_domain(
            &OptimizeCtx {
                registry: self.registry,
                tz: self.tz,
                now: self.now,
                su: self.su,
                access: self.access,
            },
            target,
            query.sub(),
            OptimizationLevel::Full,
        )?;
        if optimized.is_false() {
            return Ok(Some(Domain::FALSE));
        }
        self.condition(
            condition.field_expr(),
            condition.operator(),
            query.optimized(optimized).into_value(),
        )
        .map(Some)
    }

    /// FULL rules of relational fields (DOM §4.5); `None` when none applies.
    fn relational(
        &self,
        field: &erp_registry::FieldSpec,
        condition: &Condition,
    ) -> Result<Option<Domain>> {
        let expr = condition.field_expr();
        let operator = condition.operator();
        let value = condition.value();
        // any rights
        if matches!(operator, Operator::Any | Operator::NotAny)
            && (self.su || field.bypass_search_access)
        {
            return self
                .condition(expr, operator.bypass(), value.clone())
                .map(Some);
        }
        let many2one = matches!(field.ty, FieldType::Many2one { .. });
        if operator.is_any() {
            if let Some(query) = crate::prepare::RecordsQuery::of(value) {
                if !many2one {
                    return Ok(None);
                }
                // a many2one compared with a records query is a comparison of the column
                let positive = operator == Operator::AnyBypass;
                let domain = match &query.ids {
                    Some(ids) => {
                        self.condition(expr, Operator::In, Value::list(ids.iter().copied()))?
                    }
                    None => self.condition(expr, Operator::Ne, Value::FALSE)?,
                };
                return Ok(Some(if positive { domain } else { !domain }));
            }
            let Value::Domain(sub) = value else {
                return Ok(None);
            };
            let Some(comodel) = field.ty.comodel().and_then(|c| self.registry.model(c)) else {
                return Ok(None);
            };
            let optimized = optimize_domain(
                &OptimizeCtx {
                    registry: self.registry,
                    tz: self.tz,
                    now: self.now,
                    su: self.su,
                    access: self.access,
                },
                comodel,
                sub,
                OptimizationLevel::Full,
            )?;
            if optimized != *sub {
                return self
                    .condition(expr, operator, Value::Domain(optimized))
                    .map(Some);
            }
            if many2one && matches!(operator, Operator::AnyBypass | Operator::NotAnyBypass) {
                return self.many2one_id_lookup(expr, operator, sub);
            }
            return Ok(None);
        }
        if field.ty.is_x2many()
            && matches!(operator, Operator::In | Operator::NotIn)
            && let Value::Set(set) = value
            && let Some(comodel) = field.ty.comodel().and_then(|c| self.registry.model_id(c))
        {
            // x2m in ids  =>  x2m any! (those records); a False member: x2m not any! (all)
            let mut parts = Vec::new();
            if set.iter().any(Value::is_false_literal) {
                parts.push(self.condition(
                    expr,
                    Operator::NotAnyBypass,
                    crate::prepare::RecordsQuery::value(comodel, None),
                )?);
            }
            let ids: Vec<i64> = set
                .iter()
                .filter_map(|v| match v {
                    Value::Int(i) => Some(*i),
                    _ => None,
                })
                .collect();
            // principal parameters (`[user.partner_id.id]` in an access rule) are ids known in
            // SQL only: they become a sub-domain on the comodel's ids instead of a records query
            let params: ValueSet = set
                .iter()
                .filter(|v| matches!(v, Value::Param(_)))
                .cloned()
                .collect();
            if set
                .iter()
                .any(|v| !v.is_false_literal() && !matches!(v, Value::Int(_) | Value::Param(_)))
            {
                return Err(self.invalid(format!(
                    "Invalid value for an x2many 'in' condition: {condition:?}"
                )));
            }
            if !ids.is_empty() {
                parts.push(self.condition(
                    expr,
                    Operator::AnyBypass,
                    crate::prepare::RecordsQuery::value(comodel, Some(ids)),
                )?);
            }
            if !params.is_empty() {
                let by_id = self.condition("id", Operator::In, Value::Set(params))?;
                parts.push(self.condition(expr, Operator::AnyBypass, Value::Domain(by_id))?);
            }
            let domain = Domain::or(parts);
            return Ok(Some(if operator == Operator::In {
                domain
            } else {
                !domain
            }));
        }
        Ok(None)
    }

    /// `m2o any!
    fn many2one_id_lookup(
        &self,
        expr: &str,
        operator: Operator,
        sub: &Domain,
    ) -> Result<Option<Domain>> {
        let DomainKind::Condition(inner) = sub.kind() else {
            return Ok(None);
        };
        if inner.field_expr() != "id" {
            return Ok(None);
        }
        let without_false = |set: &ValueSet| -> ValueSet {
            set.iter()
                .filter(|v| !v.is_false_literal())
                .cloned()
                .collect()
        };
        let domain = match (inner.operator(), inner.value()) {
            (Operator::In, Value::Set(set)) => {
                self.condition(expr, Operator::In, Value::Set(without_false(set)))?
            }
            (Operator::NotIn, Value::Set(set)) => {
                let mut with = without_false(set);
                with.insert(Value::FALSE);
                self.condition(expr, Operator::NotIn, Value::Set(with))?
            }
            (Operator::AnyBypass, value) => {
                self.condition(expr, Operator::AnyBypass, value.clone())?
            }
            (Operator::NotAnyBypass, value) => {
                self.condition(expr, Operator::Ne, Value::FALSE)?
                    & self.condition(expr, Operator::NotAnyBypass, value.clone())?
            }
            _ => return Ok(None),
        };
        Ok(Some(if operator == Operator::NotAnyBypass {
            !domain
        } else {
            domain
        }))
    }

    /// Relative date strings in a value, resolved (`None` when there are none).
    fn resolve_dates(&self, value: &Value, datetime: bool) -> Result<Option<Value>> {
        match value {
            Value::Str(s) => self.resolve_date(s, datetime).map(Some),
            Value::Set(set) if set.iter().any(|v| matches!(v, Value::Str(_))) => {
                let mut out = ValueSet::new();
                for item in set.iter() {
                    out.insert(match item {
                        Value::Str(s) => self.resolve_date(s, datetime)?,
                        other => other.clone(),
                    });
                }
                Ok(Some(Value::Set(out)))
            }
            _ => Ok(None),
        }
    }

    fn resolve_date(&self, text: &str, datetime: bool) -> Result<Value> {
        let parsed = parse_date(text, &self.dates)
            .map_err(|error| self.invalid(error.to_string()).with_source(error))?;
        Ok(match (parsed, datetime) {
            (Value::Datetime(dt), false) => Value::Date(dt.date()),
            (Value::Date(d), true) => {
                let midnight = d.midnight();
                let utc = if matches!(d.year(), 1 | 9999) {
                    midnight
                } else {
                    self.tz
                        .local_to_utc(midnight)
                        .ok_or_else(|| self.invalid(format!("date value out of range: {text}")))?
                };
                Value::Datetime(utc)
            }
            (other, _) => other,
        })
    }

    fn search_display_name(&self, operator: Operator, value: &Value) -> Result<Domain> {
        let names: Vec<&str> = match &self.spec.rec_names_search {
            Some(names) => names.clone(),
            None => self
                .spec
                .rec_name
                .map(|f| vec![self.spec.field_by_id(f).name])
                .unwrap_or_default(),
        };
        if names.is_empty() {
            tracing::warn!(
                target: "erp::orm",
                model = self.spec.name,
                "Cannot search on display_name, no _rec_name or _rec_names_search defined"
            );
            return Ok(Domain::TRUE);
        }
        let exact_like = matches!(
            operator,
            Operator::EqLike | Operator::NotEqLike | Operator::EqILike | Operator::NotEqILike
        );
        if operator.is_like() && !value.is_truthy() && !exact_like {
            return Ok(Domain::constant(!operator.is_negative()));
        }
        let mut parts = Vec::with_capacity(names.len());
        for name in names {
            let field = self.spec.field(name.split('.').next().unwrap_or(name));
            let relational = field.is_some_and(|f| matches!(f.ty, FieldType::Many2one { .. }));
            if relational || operator.is_like() || field.is_none() {
                let expr = if relational && !name.contains('.') {
                    format!("{name}.display_name")
                } else {
                    name.to_owned()
                };
                parts.push(self.condition(&expr, operator, value.clone())?);
                continue;
            }
            let Some(field) = field else { continue };
            let convertible = |v: &Value| match ScalarKind::of_field(&field.ty) {
                _ if matches!(v, Value::Bool(false)) => true,
                Some(kind) => to_scalar(kind, v).is_ok(),
                None => true,
            };
            match value {
                Value::Set(set) => {
                    let typed: Vec<Value> =
                        set.iter().filter(|v| convertible(v)).cloned().collect();
                    parts.push(self.condition(name, operator, Value::List(typed))?);
                }
                Value::List(list) => {
                    let typed: Vec<Value> =
                        list.iter().filter(|v| convertible(v)).cloned().collect();
                    parts.push(self.condition(name, operator, Value::List(typed))?);
                }
                other if convertible(other) => {
                    parts.push(self.condition(name, operator, other.clone())?);
                }
                _ => {}
            }
        }
        Ok(if operator.is_negative() {
            Domain::and(parts)
        } else {
            Domain::or(parts)
        })
    }
}

/// A `time` datetime as an ERPForAll datetime (for callers handing "now" in).
pub(crate) fn primitive_now(now: erp_fields::DateTime) -> Result<PrimitiveDateTime> {
    primitive_of(now)
        .ok_or_else(|| Error::programming("the given time is outside the supported date range"))
}

#[cfg(test)]
mod tests {
    use erp_sql::param::datetime_of;

    use super::*;

    fn pdt(y: i32, m: u8, d: u8, h: u8, mi: u8) -> PrimitiveDateTime {
        PrimitiveDateTime::new(
            time::Date::from_calendar_date(y, time::Month::try_from(m).unwrap(), d).unwrap(),
            time::Time::from_hms(h, mi, 0).unwrap(),
        )
    }

    #[test]
    fn brussels_midnight_is_previous_evening_in_utc() {
        let tz = ZoneTz::new("Europe/Brussels");
        assert_eq!(tz.id(), "Europe/Brussels");
        assert_eq!(
            tz.local_to_utc(pdt(2024, 7, 2, 0, 0)),
            Some(pdt(2024, 7, 1, 22, 0))
        );
        assert_eq!(
            tz.local_to_utc(pdt(2024, 1, 2, 0, 0)),
            Some(pdt(2024, 1, 1, 23, 0))
        );
        assert_eq!(
            tz.utc_to_local(pdt(2024, 1, 1, 23, 0)),
            Some(pdt(2024, 1, 2, 0, 0))
        );
        assert_eq!(ZoneTz::new("Nowhere/Land").id(), "UTC");
        let dt = pdt(2024, 3, 4, 5, 6);
        assert_eq!(primitive_of(datetime_of(dt).unwrap()), Some(dt));
    }
}
