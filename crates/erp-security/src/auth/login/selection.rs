//! Login selection types, validation and contracts for dispatched `_get_login_domain`
//! and `_get_login_order` hooks (H58 login selection).

use erp_domain::{Condition, Domain, DomainKind, Operator, Value};
use erp_fields::{Error, FieldType, Result};
use erp_orm::{Transaction, UserId};
use erp_registry::Registry;
use sha2::{Digest, Sha256};
use std::collections::HashSet;
use std::fmt;
use tokio_postgres::types::ToSql;
use zeroize::{Zeroize, Zeroizing};

/// Maximum bounds for login selection (Contract §2).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LoginSelectionLimits {
    /// Maximum length of encoded login identifier string in bytes.
    max_encoded_bytes: usize,
    /// Maximum depth of login selection domain AST.
    max_domain_depth: usize,
    /// Maximum number of leaf condition terms in selection domain.
    max_conditions: usize,
    /// Maximum number of ordering terms.
    max_effective_order: usize,
    /// Maximum number of AST nodes.
    max_ast_nodes: usize,
    /// Maximum number of elements in in/not in membership lists.
    max_list_elements: usize,
    /// Maximum number of distinct referenced field names.
    max_distinct_fields: usize,
    /// Maximum size in bytes of selection witness payload.
    max_witness_bytes: usize,
}

/// The candidate user record selected by candidate selection:
/// `(uid, stored_password, lang, tz, canonical_login, witness)`. The stored hash and the
/// canonical login are [`Zeroizing`] so the copies handed to the login attempt are wiped when it
/// ends, like the witness's own (LD3).
pub(super) type CandidateRecord = (
    UserId,
    Option<Zeroizing<String>>,
    Option<String>,
    Option<String>,
    Zeroizing<String>,
    SelectionWitness,
);

impl Default for LoginSelectionLimits {
    fn default() -> Self {
        let query = erp_domain::query::QueryLimits::default();
        Self {
            max_encoded_bytes: query.bytes(),
            max_domain_depth: query.domain_depth(),
            max_conditions: query.domain_items(),
            max_effective_order: query.order_terms(),
            max_ast_nodes: 1_024,
            max_list_elements: 512,
            max_distinct_fields: query.fields(),
            max_witness_bytes: query.bytes(),
        }
    }
}

impl LoginSelectionLimits {
    /// Lower the query-derived bounds and auth-only node/list/witness ceilings.
    /// Zero or an auth ceiling above its default is a Validation error.
    pub fn new(
        query: &erp_domain::query::QueryLimits,
        nodes: usize,
        list_elements: usize,
        witness_bytes: usize,
    ) -> Result<Self> {
        if nodes == 0
            || nodes > 1_024
            || list_elements == 0
            || list_elements > 512
            || witness_bytes == 0
            || witness_bytes > erp_domain::query::QueryLimits::default().bytes()
        {
            return Err(Error::validation(
                "login selection limits must be positive and no higher than framework ceilings",
            ));
        }
        Ok(Self {
            max_encoded_bytes: query.bytes(),
            max_domain_depth: query.domain_depth(),
            max_conditions: query.domain_items(),
            max_effective_order: query.order_terms(),
            max_ast_nodes: nodes,
            max_list_elements: list_elements,
            max_distinct_fields: query.fields(),
            max_witness_bytes: witness_bytes,
        })
    }
    /// Maximum total encoded plan bytes.
    pub const fn encoded_bytes(&self) -> usize {
        self.max_encoded_bytes
    }
}

/// The bounded, submitted login text passed to `_get_login_domain`.
///
/// Preserves exact bytes: no implicit trimming, normalization or case folding.
/// Redacts `Debug` to avoid leaking sensitive or malformed inputs.
#[derive(Clone, PartialEq, Eq)]
pub struct LoginLookup {
    raw: String,
}

impl LoginLookup {
    /// Create a new `LoginLookup`, validating against `max_bytes`.
    pub fn new(lookup: impl AsRef<str>, max_bytes: usize) -> Result<Self> {
        let raw = lookup.as_ref();
        if raw.is_empty() {
            return Err(Error::validation("submitted login cannot be empty"));
        }
        if raw.len() > max_bytes {
            return Err(Error::validation(format!(
                "submitted login length {} exceeds admission limit of {} bytes",
                raw.len(),
                max_bytes
            )));
        }
        Ok(Self {
            raw: raw.to_owned(),
        })
    }

    /// Access the submitted login string slice.
    pub fn as_str(&self) -> &str {
        &self.raw
    }
}

impl fmt::Debug for LoginLookup {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("LoginLookup([REDACTED])")
    }
}

/// Direction for candidate ordering.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OrderDirection {
    /// Ascending sort order.
    Asc,
    /// Descending sort order.
    Desc,
}

impl OrderDirection {
    /// Return the SQL representation for this direction.
    pub fn as_sql(&self) -> &'static str {
        match self {
            Self::Asc => "ASC",
            Self::Desc => "DESC",
        }
    }
}

/// A raw ordering term admitted by the framework before login candidate selection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoginOrderTerm {
    /// Field name to order by.
    pub field: String,
    /// Direction to order by.
    pub direction: OrderDirection,
}

impl LoginOrderTerm {
    /// Construct an ascending order term for `field`.
    pub fn asc(field: impl Into<String>) -> Self {
        Self {
            field: field.into(),
            direction: OrderDirection::Asc,
        }
    }

    /// Construct a descending order term for `field`.
    pub fn desc(field: impl Into<String>) -> Self {
        Self {
            field: field.into(),
            direction: OrderDirection::Desc,
        }
    }
}

/// A candidate ordering producer; the framework admits it before execution.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoginOrder {
    terms: Vec<LoginOrderTerm>,
}

impl LoginOrder {
    /// Base order: `id ASC`.
    pub fn base() -> Self {
        Self {
            terms: vec![LoginOrderTerm::asc("id")],
        }
    }

    /// Construct a custom unvalidated `LoginOrder` (validated by framework admission).
    pub fn custom(terms: Vec<LoginOrderTerm>) -> Self {
        Self { terms }
    }

    /// Construct a `LoginOrder` from terms, validating limits and registry constraints.
    pub fn from_terms(
        terms: Vec<LoginOrderTerm>,
        limits: &LoginSelectionLimits,
        registry: &Registry,
    ) -> Result<Self> {
        if terms.is_empty() {
            return Err(Error::validation("login selection order cannot be empty"));
        }

        if terms.len() > limits.max_effective_order {
            return Err(Error::validation(
                "login selection effective order exceeds maximum limit",
            ));
        }
        let mut seen = HashSet::new();
        let mut has_id = false;
        let mut validated_terms = Vec::with_capacity(terms.len());

        for term in terms {
            validate_field_identifier(&term.field)?;
            validate_scalar_user_field(&term.field, registry)?;

            if !seen.insert(term.field.clone()) {
                return Err(Error::validation(format!(
                    "duplicate field '{}' in login selection order",
                    term.field
                )));
            }
            if term.field == "id" {
                has_id = true;
            }
            validated_terms.push(term);
        }

        if !has_id {
            validated_terms.push(LoginOrderTerm::asc("id"));
        }

        if validated_terms.len() > limits.max_effective_order {
            return Err(Error::validation(format!(
                "login selection order terms {} exceeds maximum allowed limit of {}",
                validated_terms.len(),
                limits.max_effective_order
            )));
        }

        Ok(Self {
            terms: validated_terms,
        })
    }

    pub(crate) fn admit(self, limits: &LoginSelectionLimits, registry: &Registry) -> Result<Self> {
        Self::from_terms(self.terms, limits, registry)
    }

    /// Return the list of order terms.
    pub fn terms(&self) -> &[LoginOrderTerm] {
        &self.terms
    }
}

/// A login domain producer; the framework admits it before execution.
#[derive(Clone, PartialEq, Eq)]
pub struct LoginDomain {
    domain: Domain,
    referenced_fields: HashSet<String>,
}

impl fmt::Debug for LoginDomain {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("LoginDomain")
            .field("referenced_fields", &self.referenced_fields)
            .finish()
    }
}

impl LoginDomain {
    /// Base domain: exact equality `login = lookup`.
    pub fn exact_login(lookup: &str) -> Result<Self> {
        let lookup = LoginLookup::new(lookup, erp_domain::query::QueryLimits::default().bytes())?;
        let domain = Domain::condition("login", Operator::Eq, lookup.as_str())
            .map_err(|_| Error::validation("invalid exact login selection domain"))?;
        let mut fields = HashSet::new();
        fields.insert("login".to_string());
        Ok(Self {
            domain,
            referenced_fields: fields,
        })
    }

    /// Construct a custom unvalidated `LoginDomain` (validated by framework admission).
    pub fn custom(domain: Domain) -> Self {
        Self {
            domain,
            referenced_fields: HashSet::new(),
        }
    }

    pub(crate) fn admit(self, limits: &LoginSelectionLimits, registry: &Registry) -> Result<Self> {
        Self::from_domain(self.domain, limits, registry)
    }

    /// Return the inner domain AST.
    pub fn domain(&self) -> &Domain {
        &self.domain
    }

    /// Return the set of referenced field names.
    pub fn referenced_fields(&self) -> &HashSet<String> {
        &self.referenced_fields
    }

    /// Validate and construct a `LoginDomain` from an arbitrary `Domain`.
    pub fn from_domain(
        domain: Domain,
        limits: &LoginSelectionLimits,
        registry: &Registry,
    ) -> Result<Self> {
        let mut validator = DomainValidator {
            limits: *limits,
            registry,
            nodes_count: 2,
            conditions_count: 1,
            list_elements_count: 0,
            encoded_bytes: 64,
            referenced_fields: HashSet::new(),
        };

        validator.validate_node(&domain, 2)?;

        let mut all_fields = validator.referenced_fields;
        all_fields.insert("id".to_string());
        all_fields.insert("login".to_string());
        all_fields.insert("active".to_string());
        all_fields.insert("password".to_string());

        if all_fields.len() > limits.max_distinct_fields {
            return Err(Error::validation(format!(
                "login selection referenced fields {} exceeds maximum limit of {}",
                all_fields.len(),
                limits.max_distinct_fields
            )));
        }

        Ok(Self {
            domain,
            referenced_fields: all_fields,
        })
    }
}

struct DomainValidator<'a> {
    limits: LoginSelectionLimits,
    registry: &'a Registry,
    nodes_count: usize,
    conditions_count: usize,
    list_elements_count: usize,
    encoded_bytes: usize,
    referenced_fields: HashSet<String>,
}

impl<'a> DomainValidator<'a> {
    fn validate_node(&mut self, root: &Domain, root_depth: usize) -> Result<()> {
        let mut pending = vec![(root, root_depth)];
        while let Some((domain, depth)) = pending.pop() {
            if depth > self.limits.max_domain_depth {
                return Err(Error::validation(
                    "login selection domain depth exceeds maximum limit",
                ));
            }
            self.nodes_count = self
                .nodes_count
                .checked_add(1)
                .ok_or_else(|| Error::validation("login selection node count overflow"))?;
            if self.nodes_count > self.limits.max_ast_nodes {
                return Err(Error::validation(
                    "login selection AST node count exceeds maximum limit",
                ));
            }
            self.charge(8)?;
            match domain.kind() {
                DomainKind::True | DomainKind::False => {}
                DomainKind::Not(child) => pending.push((child, depth + 1)),
                DomainKind::And(children) | DomainKind::Or(children) => {
                    if children.len() > self.limits.max_ast_nodes.saturating_sub(self.nodes_count)
                        || pending
                            .len()
                            .checked_add(children.len())
                            .is_none_or(|n| n > self.limits.max_ast_nodes)
                    {
                        return Err(Error::validation(
                            "login selection AST node count exceeds maximum limit",
                        ));
                    }
                    for child in children.iter().rev() {
                        pending.push((child, depth + 1));
                    }
                }
                DomainKind::Condition(condition) => {
                    self.conditions_count =
                        self.conditions_count.checked_add(1).ok_or_else(|| {
                            Error::validation("login selection condition count overflow")
                        })?;
                    if self.conditions_count > self.limits.max_conditions {
                        return Err(Error::validation(
                            "login selection condition count exceeds maximum limit",
                        ));
                    }
                    self.validate_condition(condition)?;
                }
                DomainKind::Custom(_) => {
                    return Err(Error::validation(
                        "custom login selection nodes are unsupported until a later release",
                    ));
                }
            }
        }
        self.check_bytes()
    }

    fn charge(&mut self, bytes: usize) -> Result<()> {
        self.encoded_bytes = self
            .encoded_bytes
            .checked_add(bytes)
            .ok_or_else(|| Error::validation("login selection encoded byte count overflow"))?;
        self.check_bytes()
    }

    fn check_bytes(&self) -> Result<()> {
        if self.encoded_bytes > self.limits.max_encoded_bytes {
            return Err(Error::validation(format!(
                "login selection encoded bytes {} exceeds maximum limit of {}",
                self.encoded_bytes, self.limits.max_encoded_bytes
            )));
        }
        Ok(())
    }

    fn validate_condition(&mut self, cond: &Condition) -> Result<()> {
        let field = cond.field_expr();
        validate_field_identifier(field)?;
        validate_scalar_user_field(field, self.registry)?;
        self.charge(field.len())?;
        self.charge(32)?;
        self.referenced_fields.insert(field.to_owned());
        if self.referenced_fields.len() > self.limits.max_distinct_fields {
            return Err(Error::validation(
                "login selection referenced field count exceeds maximum limit",
            ));
        }
        match cond.operator() {
            Operator::Eq
            | Operator::Ne
            | Operator::Lt
            | Operator::Le
            | Operator::Gt
            | Operator::Ge => self.scalar(cond.value()),
            Operator::In | Operator::NotIn => {
                let count = match cond.value() {
                    Value::List(items) => items.len(),
                    Value::Set(items) => items.len(),
                    _ => {
                        return Err(Error::validation(
                            "login selection membership operand must be a scalar collection",
                        ));
                    }
                };
                self.list_elements_count =
                    self.list_elements_count.checked_add(count).ok_or_else(|| {
                        Error::validation("login selection membership count overflow")
                    })?;
                if self.list_elements_count > self.limits.max_list_elements {
                    return Err(Error::validation(
                        "login selection membership list exceeds maximum limit",
                    ));
                }
                match cond.value() {
                    Value::List(items) => {
                        for value in items {
                            self.scalar(value)?;
                        }
                    }
                    Value::Set(items) => {
                        for value in items.iter() {
                            self.scalar(value)?;
                        }
                    }
                    _ => {
                        return Err(Error::validation(
                            "login selection membership operand must be a scalar collection",
                        ));
                    }
                }
                Ok(())
            }
            _ => Err(Error::validation(
                "operator unsupported for login selection until a later release",
            )),
        }
    }

    fn scalar(&mut self, value: &Value) -> Result<()> {
        match value {
            // Worst-case JSON escaping plus framing/type tags; checked before copies.
            Value::Str(text) => self.charge(
                text.len()
                    .checked_mul(6)
                    .and_then(|n| n.checked_add(8))
                    .ok_or_else(|| {
                        Error::validation("login selection operand byte count overflow")
                    })?,
            ),
            Value::Int(_) => self.charge(24),
            Value::Bool(_) => self.charge(8),
            _ => Err(Error::validation(
                "unsupported scalar value in login selection domain",
            )),
        }
    }
}

fn validate_field_identifier(name: &str) -> Result<()> {
    if name.is_empty() {
        return Err(Error::validation("field identifier cannot be empty"));
    }
    if name.contains('.') {
        return Err(Error::validation(
            "relational traversal and property paths are unsupported until a later release",
        ));
    }
    if name.contains('(')
        || name.contains(')')
        || name.contains(' ')
        || name.contains(';')
        || name.contains('"')
    {
        return Err(Error::validation(format!(
            "invalid field identifier '{name}'"
        )));
    }
    Ok(())
}

fn validate_scalar_user_field(name: &str, registry: &Registry) -> Result<()> {
    let model = registry
        .model("res.users")
        .ok_or_else(|| Error::programming("model 'res.users' not found in registry"))?;

    let field = model.field(name).ok_or_else(|| {
        Error::validation(format!("res.users has no field '{name}' (login selection)"))
    })?;

    if field.inherited.is_some() || field.translate || field.company_dependent {
        return Err(Error::validation(format!(
            "field '{name}' is inherited, translated or company-dependent; unsupported until a later release"
        )));
    }
    if !field.store {
        return Err(Error::validation(format!(
            "field '{name}' on res.users is not stored; non-stored fields are unsupported until a later release"
        )));
    }

    if field.compute.is_some() || field.related.is_some() || field.search_method.is_some() {
        return Err(Error::validation(format!(
            "field '{name}' on res.users is computed or related; computed/related fields are unsupported until a later release"
        )));
    }

    match &field.ty {
        FieldType::Id | FieldType::Integer | FieldType::Boolean | FieldType::Char { .. } => Ok(()),
        FieldType::Selection { method: None, .. } => Ok(()),
        _ => Err(Error::validation(format!(
            "field '{name}' on res.users has unsupported type; only physical scalar fields (integer, boolean, char, static selection) are supported until a later release"
        ))),
    }
}

/// Compiled selection plan for primary execution.
#[derive(Debug, Clone)]
pub(super) struct LoginSelectionPlan {
    /// The admitted login domain.
    pub(crate) domain: LoginDomain,
    /// The admitted login order.
    pub(crate) order: LoginOrder,
    /// Dependencies (non-standard fields needed for witness check).
    dependencies: Vec<String>,
    /// Cryptographic digest of this selection plan.
    digest: [u8; 32],
}

impl LoginSelectionPlan {
    pub(super) fn admit(
        domain: LoginDomain,
        order: LoginOrder,
        limits: &LoginSelectionLimits,
        registry: &Registry,
    ) -> Result<Self> {
        // Revisit a bounded admitted domain without cloning it, adding the complete
        // order and union of referenced fields to the same request budget.
        let mut validator = DomainValidator {
            limits: *limits,
            registry,
            nodes_count: 2,
            conditions_count: 1,
            list_elements_count: 0,
            encoded_bytes: 64,
            referenced_fields: HashSet::new(),
        };
        validator.validate_node(domain.domain(), 2)?;
        for term in order.terms() {
            validator.charge(term.field.len())?;
            validator.charge(32)?;
            validator.referenced_fields.insert(term.field.clone());
        }
        for core in ["id", "login", "active", "password", "lang", "tz"] {
            validator.referenced_fields.insert(core.to_owned());
        }
        if validator.referenced_fields.len() > limits.max_distinct_fields {
            return Err(Error::validation(
                "login selection referenced field count exceeds maximum limit",
            ));
        }
        Ok(Self::new(domain, order))
    }

    /// Create the private plan only after complete framework admission.
    fn new(domain: LoginDomain, order: LoginOrder) -> Self {
        let mut deps_set = HashSet::new();
        for f in domain.referenced_fields() {
            if f != "id"
                && f != "login"
                && f != "active"
                && f != "password"
                && f != "lang"
                && f != "tz"
            {
                deps_set.insert(f.clone());
            }
        }
        for term in order.terms() {
            if term.field != "id"
                && term.field != "login"
                && term.field != "active"
                && term.field != "password"
                && term.field != "lang"
                && term.field != "tz"
            {
                deps_set.insert(term.field.clone());
            }
        }

        let mut dependencies: Vec<String> = deps_set.into_iter().collect();
        dependencies.sort();

        let mut hasher = Sha256::new();
        hasher.update(b"LOGIN_SELECTION_PLAN_V2");
        // Runtime admission bounds this canonical representation before plan creation.
        hasher.update(domain.domain().to_string().as_bytes());
        hasher.update(b"\0");
        hasher.update((dependencies.len() as u64).to_be_bytes());
        for dep in &dependencies {
            hasher.update(dep.as_bytes());
            hasher.update(b"\0");
        }
        for term in order.terms() {
            hasher.update(term.field.as_bytes());
            hasher.update(b":");
            hasher.update(term.direction.as_sql().as_bytes());
            hasher.update(b"\0");
        }
        let digest: [u8; 32] = hasher.finalize().into();

        Self {
            domain,
            order,
            dependencies,
            digest,
        }
    }

    /// Compile an admitted plan using the ordinary typed scalar/null compiler.
    fn compile_sql(
        &self,
        env: &erp_orm::Env,
        limits: &LoginSelectionLimits,
    ) -> Result<(String, Vec<DynamicParam>)> {
        use erp_sql::filter::{Unaccent, WhereContext, compile_where};
        let registry = env.registry();
        let records = env.records("res.users")?;
        // Only already-admitted scalar literals reach the shared optimizer. A fixed
        // epoch avoids reading wall time; dynamic/date/context inputs were rejected.
        let optimized = records
            .optimize_domain(
                self.domain.domain(),
                erp_domain::OptimizationLevel::Full,
                Some(erp_fields::DateTime::from_micros(0)),
            )
            .map_err(|_| Error::validation("invalid scalar operand in login selection domain"))?;
        let optimized = LoginDomain::from_domain(optimized, limits, registry)?;
        let model = registry
            .model("res.users")
            .ok_or_else(|| Error::programming("res.users missing for login selection"))?;
        let filter = compile_where(
            optimized.domain(),
            &WhereContext::new(registry, model, Unaccent::Off),
        )
        // Compiler diagnostics may contain operand values. Keep this auth boundary redacted.
        .map_err(|_| Error::validation("invalid scalar operand in login selection domain"))?;
        if !filter.joins.is_empty() {
            return Err(Error::validation(
                "joined login selection is unsupported until a later release",
            ));
        }
        let params = filter
            .params
            .iter()
            .map(DynamicParam::from_compiled)
            .collect::<Result<Vec<_>>>()?;
        // octet_length on stored text is evaluated on the server. Oversized text is
        // never sent in a row or decoded/copied by the application.
        let mut size = String::from(
            "128::int8 + COALESCE(octet_length(u.password),0)::int8 + octet_length(u.login)::int8 + COALESCE(octet_length(u.lang),0)::int8 + COALESCE(octet_length(u.tz),0)::int8",
        );
        for dep in &self.dependencies {
            size.push_str(&format!(
                " + {}::int8 + COALESCE(octet_length(u.\"{dep}\"::text),0)::int8",
                dep.len() + 16
            ));
        }
        let admitted = format!("({size}) <= {}::int8", limits.max_witness_bytes);
        let mut sql = format!(
            "SELECT u.id::int8, CASE WHEN {admitted} THEN u.password END, CASE WHEN {admitted} THEN u.lang END, CASE WHEN {admitted} THEN u.tz END, CASE WHEN {admitted} THEN u.login END, u.active"
        );
        for dep in &self.dependencies {
            sql.push_str(&format!(
                ", CASE WHEN {admitted} THEN u.\"{dep}\"::text END"
            ));
        }
        sql.push_str(&format!(
            ", ({admitted}) FROM res_users u WHERE u.active AND ({}) ORDER BY ",
            filter.sql.replace("\"res_users\".", "u.")
        ));
        for (i, term) in self.order.terms().iter().enumerate() {
            if i > 0 {
                sql.push_str(", ");
            }
            sql.push_str(&format!("u.\"{}\" {}", term.field, term.direction.as_sql()));
        }
        sql.push_str(" LIMIT 1");
        Ok((sql, params))
    }
}

/// A dynamic SQL parameter for candidate selection query.
#[derive(Debug, Clone)]
enum DynamicParam {
    /// String parameter.
    Text(String),
    /// 64-bit integer parameter.
    Int(i64),
    /// Array of strings.
    TextArray(Vec<String>),
    /// Array of integers.
    IntArray(Vec<i64>),
}

impl DynamicParam {
    /// Cast to a dynamic `ToSql` trait reference.
    fn as_to_sql(&self) -> &(dyn ToSql + Sync) {
        match self {
            Self::Text(s) => s,
            Self::Int(i) => i,
            Self::TextArray(a) => a,
            Self::IntArray(a) => a,
        }
    }
}

impl DynamicParam {
    fn from_compiled(value: &erp_sql::param::Param) -> Result<Self> {
        use erp_sql::param::{Param, Scalar, ScalarKind};
        match value {
            Param::Scalar(Scalar::Text(text)) => Ok(Self::Text(text.to_string())),
            Param::Scalar(Scalar::Int8(value)) => Ok(Self::Int(*value)),
            Param::Array(ScalarKind::Text, values) => Ok(Self::TextArray(
                values
                    .iter()
                    .map(|value| match value {
                        Scalar::Text(text) => Ok(text.to_string()),
                        _ => Err(Error::programming(
                            "invalid text array in admitted login plan",
                        )),
                    })
                    .collect::<Result<Vec<_>>>()?,
            )),
            Param::Array(ScalarKind::Int8, values) => Ok(Self::IntArray(
                values
                    .iter()
                    .map(|value| match value {
                        Scalar::Int8(value) => Ok(*value),
                        _ => Err(Error::programming(
                            "invalid integer array in admitted login plan",
                        )),
                    })
                    .collect::<Result<Vec<_>>>()?,
            )),
            _ => Err(Error::validation(
                "unsupported scalar parameter in login selection",
            )),
        }
    }
}

/// Query the candidate user and selection witness in `tx` matching `plan`.
pub(super) async fn select_candidate(
    tx: &mut Transaction,
    plan: &LoginSelectionPlan,
    limits: &LoginSelectionLimits,
) -> Result<Option<CandidateRecord>> {
    let env = tx.env();
    let (sql, params) = plan.compile_sql(&env, limits)?;
    let param_refs: Vec<&(dyn ToSql + Sync)> = params.iter().map(|p| p.as_to_sql()).collect();
    let rows = tx.query(&sql, &param_refs).await?;
    let Some(row) = rows.first() else {
        return Ok(None);
    };

    let admitted: bool = row.try_get(6 + plan.dependencies.len()).map_err(|error| {
        Error::db("invalid login selection size-admission result").with_source(error)
    })?;
    if !admitted {
        return Err(Error::validation(
            "login selection witness exceeds byte limit",
        ));
    }
    let id: i64 = row
        .try_get(0)
        .map_err(|e| Error::db(format!("res_users.id: {e}")))?;
    let uid = UserId::new(id)?;
    let stored_hash: Option<Zeroizing<String>> = row
        .try_get::<_, Option<String>>(1)
        .map_err(|e| Error::db(format!("res_users.password: {e}")))?
        .map(Zeroizing::new);
    let lang: Option<String> = row
        .try_get(2)
        .map_err(|e| Error::db(format!("res_users.lang: {e}")))?;
    let tz: Option<String> = row
        .try_get(3)
        .map_err(|e| Error::db(format!("res_users.tz: {e}")))?;
    let canonical_login: Zeroizing<String> = Zeroizing::new(
        row.try_get::<_, String>(4)
            .map_err(|e| Error::db(format!("res_users.login: {e}")))?,
    );
    let active: bool = row
        .try_get(5)
        .map_err(|e| Error::db(format!("res_users.active: {e}")))?;

    let mut dependency_values = Vec::with_capacity(plan.dependencies.len());
    for (idx, _) in plan.dependencies.iter().enumerate() {
        let val: Option<String> = row
            .try_get(6 + idx)
            .map_err(|e| Error::db(format!("res_users dependency: {e}")))?;
        dependency_values.push(val);
    }

    let witness = SelectionWitness {
        uid,
        canonical_login: String::clone(&canonical_login),
        active,
        stored_hash: stored_hash.as_deref().cloned(),
        dependency_values,
        plan_digest: plan.digest,
    };
    witness.validate_size(limits.max_witness_bytes)?;

    Ok(Some((uid, stored_hash, lang, tz, canonical_login, witness)))
}

/// Witness of the selected candidate and plan.
#[derive(Clone, PartialEq, Eq)]
pub struct SelectionWitness {
    /// Candidate user ID.
    pub uid: UserId,
    /// Canonical login of candidate user.
    pub canonical_login: String,
    /// Active state of candidate user.
    pub active: bool,
    /// Stored password hash of candidate user.
    pub stored_hash: Option<String>,
    /// Dependent field values observed during selection.
    pub dependency_values: Vec<Option<String>>,
    /// SHA256 digest of selection plan.
    pub plan_digest: [u8; 32],
}

impl fmt::Debug for SelectionWitness {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("SelectionWitness(<redacted>)")
    }
}
impl Drop for SelectionWitness {
    fn drop(&mut self) {
        self.canonical_login.zeroize();
        if let Some(hash) = self.stored_hash.as_mut() {
            hash.zeroize();
        }
        for value in self.dependency_values.iter_mut().flatten() {
            value.zeroize();
        }
        self.plan_digest.zeroize();
    }
}

impl SelectionWitness {
    /// Validate size of witness payload against `max_bytes`.
    pub fn validate_size(&self, max_bytes: usize) -> Result<()> {
        let bytes = self.dependency_values.iter().try_fold(
            128usize
                .checked_add(self.canonical_login.len())
                .and_then(|n| n.checked_add(self.stored_hash.as_ref().map_or(0, String::len)))
                .ok_or_else(|| Error::validation("login selection witness byte count overflow"))?,
            |bytes, value| {
                bytes
                    .checked_add(16)
                    .and_then(|n| n.checked_add(value.as_ref().map_or(0, String::len)))
                    .ok_or_else(|| Error::validation("login selection witness byte count overflow"))
            },
        )?;
        if bytes > max_bytes {
            return Err(Error::validation(format!(
                "selection witness size {} exceeds limit of {} bytes",
                bytes, max_bytes
            )));
        }
        Ok(())
    }
}

/// Context supplied to selection hook callers.
#[derive(Debug, Clone)]
pub struct LoginSelectionContext {
    limits: LoginSelectionLimits,
}

impl LoginSelectionContext {
    /// Create a new selection context with the given limits.
    pub fn new(limits: LoginSelectionLimits) -> Self {
        Self { limits }
    }

    /// Access the selection limits.
    pub fn limits(&self) -> &LoginSelectionLimits {
        &self.limits
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "tests")]
mod tests {
    use super::{LoginDomain, LoginOrder, LoginSelectionPlan};

    #[test]
    fn lc07_deterministic_plan_digest() {
        let left = LoginSelectionPlan::new(
            LoginDomain::exact_login("alice").unwrap(),
            LoginOrder::base(),
        );
        let right = LoginSelectionPlan::new(
            LoginDomain::exact_login("alice").unwrap(),
            LoginOrder::base(),
        );
        assert_eq!(left.digest, right.digest);
    }

    #[test]
    fn recovery_plan_digest_distinguishes_operands() {
        let left = LoginSelectionPlan::new(
            LoginDomain::exact_login("left").unwrap(),
            LoginOrder::base(),
        );
        let right = LoginSelectionPlan::new(
            LoginDomain::exact_login("right").unwrap(),
            LoginOrder::base(),
        );
        assert_ne!(left.digest, right.digest);
    }
}

/// LOGIN-R5/R7 boundary and type matrices. Every table below is a
/// list of cells; one cell is one input and the exact `(kind, message)` it must produce. The
/// admission budgets are exercised on both sides of each limit, with the expected numbers computed
/// in the test from the per-item charges the validator documents (node 8, field name length,
/// condition 32, string operand 6 per byte + 8, integer 24, boolean 8, base 64); no cell compares a
/// value read back from the code under test with itself.
#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, reason = "tests")]
mod matrix_tests {
    use super::{
        LoginDomain, LoginLookup, LoginOrder, LoginOrderTerm, LoginSelectionLimits,
        LoginSelectionPlan, SelectionWitness,
    };
    use erp_domain::query::{QueryLimits, QueryLimitsBuilder};
    use erp_domain::{Domain, Operator, PrincipalParam, Value};
    use erp_fields::{Error, ErrorKind, OnDelete, Result};
    use erp_orm::UserId;
    use erp_registry::{
        AddonDescriptor, FieldDecl, KindDecl, ModelDecl, Registry, RegistryBuilder,
    };
    use std::fmt::Debug;
    use std::sync::OnceLock;

    static ADDON: AddonDescriptor = AddonDescriptor {
        name: "test_login_matrix",
        depends: &["base"],
        exact_only: false,
    };

    /// A `res.users` with every field shape the type matrix needs, over a delegate parent so one
    /// field is `inherited`.
    static DECLS: [ModelDecl; 2] = [
        ModelDecl::new("test_login_matrix", "res.partner")
            .description("Parent")
            .fields(const { &[FieldDecl::char("pname")] }),
        ModelDecl::extension("test_login_matrix", "res.users", &["res.users"])
            .inherits(&[("res.partner", "partner_id")])
            .fields(
                const {
                    &[
                        FieldDecl::many2one("partner_id", "res.partner")
                            .required(true)
                            .ondelete(OnDelete::Cascade),
                        // the real `password` is computed with an inverse and not stored
                        FieldDecl::char("password")
                            .compute("_compute_password")
                            .inverse("_set_password"),
                        FieldDecl::char("lang"),
                        FieldDecl::char("tz"),
                        FieldDecl::char("f_char"),
                        FieldDecl::integer("f_int"),
                        FieldDecl::boolean("f_bool"),
                        FieldDecl::selection("f_sel", &[("a", "A"), ("b", "B")]),
                        FieldDecl::new(
                            "f_dynsel",
                            KindDecl::Selection {
                                values: None,
                                method: Some("_selection_dyn"),
                                add: &[],
                                ondelete: &[],
                            },
                        ),
                        FieldDecl::float("f_float"),
                        FieldDecl::text("f_text"),
                        FieldDecl::date("f_date"),
                        FieldDecl::datetime("f_datetime"),
                        FieldDecl::many2one("f_m2o", "res.partner"),
                        FieldDecl::char("f_translate").translate(true),
                        FieldDecl::char("f_company").company_dependent(true),
                        FieldDecl::char("f_computed")
                            .compute("_compute_x")
                            .store(false),
                        FieldDecl::char("f_computed_stored")
                            .compute("_compute_x")
                            .store(true),
                        FieldDecl::char("f_related")
                            .related("partner_id.pname")
                            .store(false),
                        FieldDecl::char("f_related_stored")
                            .related("partner_id.pname")
                            .store(true),
                    ]
                },
            ),
    ];

    fn registry() -> &'static Registry {
        static REGISTRY: OnceLock<Registry> = OnceLock::new();
        REGISTRY.get_or_init(|| {
            RegistryBuilder::new([&ADDON])
                .linked(false)
                .with_decls(DECLS.iter())
                .build()
                .expect("matrix registry")
        })
    }

    fn limits_with(
        shape: impl FnOnce(QueryLimitsBuilder) -> QueryLimitsBuilder,
    ) -> LoginSelectionLimits {
        let query = shape(QueryLimits::builder()).build().unwrap();
        LoginSelectionLimits::new(&query, 1_024, 512, 65_536).unwrap()
    }

    fn cond(field: &str, op: Operator, value: impl Into<Value>) -> Domain {
        Domain::condition(field, op, value).unwrap()
    }

    fn refused<T: Debug>(result: Result<T>) -> (ErrorKind, String) {
        let error: Error = result.unwrap_err();
        (error.kind(), error.message().to_owned())
    }

    fn validation(message: impl Into<String>) -> (ErrorKind, String) {
        (ErrorKind::Validation, message.into())
    }

    fn admit_domain(domain: Domain, limits: &LoginSelectionLimits) -> Result<LoginDomain> {
        LoginDomain::from_domain(domain, limits, registry())
    }

    // ---- R5: limits -------------------------------------------------------------------------

    fn nested(levels: usize) -> Domain {
        let mut domain = cond("login", Operator::Eq, "u");
        for level in 0..levels {
            let leaf = cond("active", Operator::Eq, true);
            domain = if level % 2 == 0 {
                Domain::or([domain, leaf])
            } else {
                Domain::and([domain, leaf])
            };
        }
        domain
    }

    #[test]
    fn r5_depth_boundary() {
        for max_depth in [3usize, 5, 16] {
            let limits = limits_with(|q| q.max_domain_depth(max_depth));
            // the root sits at depth 2, each nesting level adds one
            assert!(
                admit_domain(nested(max_depth - 2), &limits).is_ok(),
                "depth {max_depth} at the limit"
            );
            assert_eq!(
                refused(admit_domain(nested(max_depth - 1), &limits)),
                validation("login selection domain depth exceeds maximum limit"),
                "depth {max_depth} one over"
            );
        }
    }

    #[test]
    fn r5_ast_node_boundary() {
        for nodes in [4usize, 10, 500] {
            let query = QueryLimits::default();
            let limits = LoginSelectionLimits::new(&query, nodes, 512, 65_536).unwrap();
            // nodes = 2 (framework) + 1 (the OR) + one per leaf
            let leaves =
                |n: usize| Domain::or((0..n).map(|i| cond("f_int", Operator::Eq, i as i64)));
            assert!(
                admit_domain(leaves(nodes - 3), &limits).is_ok(),
                "{nodes} nodes at the limit"
            );
            assert_eq!(
                refused(admit_domain(leaves(nodes - 2), &limits)),
                validation("login selection AST node count exceeds maximum limit"),
                "{nodes} nodes one over"
            );
        }
        // a huge child list is refused before any child is walked
        let huge = Domain::or((0..5_000).map(|i| cond("f_int", Operator::Eq, i as i64)));
        assert_eq!(
            refused(admit_domain(huge, &LoginSelectionLimits::default())),
            validation("login selection AST node count exceeds maximum limit")
        );
    }

    #[test]
    fn r5_condition_boundary() {
        for items in [2usize, 3, 8] {
            let limits = limits_with(|q| q.max_domain_items(items));
            let leaves =
                |n: usize| Domain::or((0..n).map(|i| cond("f_int", Operator::Eq, i as i64)));
            // the framework counts one condition of its own
            assert!(
                admit_domain(leaves(items - 1), &limits).is_ok(),
                "{items} at the limit"
            );
            assert_eq!(
                refused(admit_domain(leaves(items), &limits)),
                validation("login selection condition count exceeds maximum limit"),
                "{items} one over"
            );
        }
    }

    /// aggregate over several conditions)
    #[test]
    fn r5_membership_boundary() {
        let over = || validation("login selection membership list exceeds maximum limit");
        for list_max in [1usize, 4, 512] {
            let limits =
                LoginSelectionLimits::new(&QueryLimits::default(), 1_024, list_max, 65_536)
                    .unwrap();
            for op in [Operator::In, Operator::NotIn] {
                let list = |n: usize| Value::list((0..n).map(|i| i as i64));
                let set = |n: usize| Value::set((0..n).map(|i| i as i64));
                assert!(admit_domain(cond("f_int", op, list(list_max)), &limits).is_ok());
                assert!(admit_domain(cond("f_int", op, set(list_max)), &limits).is_ok());
                assert_eq!(
                    refused(admit_domain(cond("f_int", op, list(list_max + 1)), &limits)),
                    over(),
                    "{op:?} list {list_max}"
                );
                assert_eq!(
                    refused(admit_domain(cond("f_int", op, set(list_max + 1)), &limits)),
                    over(),
                    "{op:?} set {list_max}"
                );
            }
        }
        // two lists that fit separately and not together
        let limits = LoginSelectionLimits::new(&QueryLimits::default(), 1_024, 4, 65_536).unwrap();
        let split = |a: usize, b: usize| {
            Domain::and([
                cond("f_int", Operator::In, Value::list((0..a).map(|i| i as i64))),
                cond(
                    "f_bool",
                    Operator::NotIn,
                    Value::list((0..b).map(|i| i % 2 == 0)),
                ),
            ])
        };
        assert!(admit_domain(split(2, 2), &limits).is_ok());
        assert_eq!(refused(admit_domain(split(3, 2), &limits)), over());
    }

    fn base_cost(nodes: usize) -> usize {
        64 + 8 * nodes
    }

    /// kind and across a membership list)
    #[test]
    fn r5_encoded_byte_boundary() {
        // (domain, total charge computed from the documented per-item charges)
        let field = "f_char";
        let cases: Vec<(&str, Domain, usize)> = vec![
            (
                "empty-ish string",
                cond(field, Operator::Eq, "x"),
                base_cost(1) + field.len() + 32 + (6 + 8),
            ),
            (
                "multibyte string charges its bytes",
                cond(field, Operator::Eq, "é".repeat(10)),
                base_cost(1) + field.len() + 32 + (6 * 20 + 8),
            ),
            (
                "integer",
                cond("f_int", Operator::Gt, 7_i64),
                base_cost(1) + "f_int".len() + 32 + 24,
            ),
            (
                "boolean",
                cond("f_bool", Operator::Ne, true),
                base_cost(1) + "f_bool".len() + 32 + 8,
            ),
            (
                "list of strings",
                cond(
                    field,
                    Operator::In,
                    Value::list(["a", "bb", "ccc"].map(str::to_owned)),
                ),
                base_cost(1) + field.len() + 32 + (6 + 8) + (12 + 8) + (18 + 8),
            ),
            (
                "two conditions add two nodes",
                Domain::and([
                    cond("f_int", Operator::Eq, 1_i64),
                    cond(field, Operator::Eq, "y"),
                ]),
                base_cost(3) + "f_int".len() + 32 + 24 + field.len() + 32 + (6 + 8),
            ),
        ];
        for (name, domain, cost) in cases {
            let at = limits_with(|q| q.max_bytes(cost));
            assert!(
                admit_domain(domain.clone(), &at).is_ok(),
                "{name}: at {cost}"
            );
            let under = limits_with(|q| q.max_bytes(cost - 1));
            assert_eq!(
                refused(admit_domain(domain, &under)),
                validation(format!(
                    "login selection encoded bytes {cost} exceeds maximum limit of {}",
                    cost - 1
                )),
                "{name}: one under"
            );
        }
    }

    /// private plan)
    #[test]
    fn r5_plan_aggregates_domain_and_order_bytes() {
        let domain = || LoginDomain::custom(cond("login", Operator::Eq, "x"));
        let domain_cost = base_cost(1) + "login".len() + 32 + (6 + 8);
        let order = || LoginOrder::base();
        // one `id` term charges its name and 32
        let total = domain_cost + "id".len() + 32;
        let at = limits_with(|q| q.max_bytes(total));
        assert!(LoginSelectionPlan::admit(domain(), order(), &at, registry()).is_ok());
        let under = limits_with(|q| q.max_bytes(total - 1));
        assert_eq!(
            refused(LoginSelectionPlan::admit(
                domain(),
                order(),
                &under,
                registry()
            )),
            validation(format!(
                "login selection encoded bytes {total} exceeds maximum limit of {}",
                total - 1
            ))
        );
        // the domain alone would have fitted
        assert!(admit_domain(cond("login", Operator::Eq, "x"), &under).is_ok());
    }

    /// counted, in the domain alone and in the plan with its order)
    #[test]
    fn r5_referenced_field_boundary() {
        // `id`, `login`, `active`, `password` always count; a domain on `f_char` adds one
        let one = || cond("f_char", Operator::Eq, "x");
        assert!(admit_domain(one(), &limits_with(|q| q.max_fields(5))).is_ok());
        assert_eq!(
            refused(admit_domain(one(), &limits_with(|q| q.max_fields(4)))),
            validation("login selection referenced fields 5 exceeds maximum limit of 4")
        );
        // the running count of the conditions' own fields is checked first
        let two = Domain::and([one(), cond("f_int", Operator::Eq, 1_i64)]);
        assert_eq!(
            refused(admit_domain(two, &limits_with(|q| q.max_fields(1)))),
            validation("login selection referenced field count exceeds maximum limit")
        );
        // the plan unions domain fields, order fields, id and the six core fields (lang, tz too)
        let plan = |fields: usize| {
            LoginSelectionPlan::admit(
                LoginDomain::custom(one()),
                LoginOrder::custom(vec![LoginOrderTerm::asc("f_int")])
                    .admit(&LoginSelectionLimits::default(), registry())
                    .unwrap(),
                &limits_with(|q| q.max_fields(fields)),
                registry(),
            )
        };
        // {f_char, f_int, id, login, active, password, lang, tz}
        assert!(plan(8).is_ok());
        assert_eq!(
            refused(plan(7)),
            validation("login selection referenced field count exceeds maximum limit")
        );
    }

    /// duplicates)
    #[test]
    fn r5_order_matrix() {
        let default = LoginSelectionLimits::default();
        let terms = |names: &[&str]| -> Vec<LoginOrderTerm> {
            names.iter().map(|n| LoginOrderTerm::asc(*n)).collect()
        };
        let from = |t: Vec<LoginOrderTerm>, l: &LoginSelectionLimits| {
            LoginOrder::from_terms(t, l, registry())
        };
        assert_eq!(
            refused(from(vec![], &default)),
            validation("login selection order cannot be empty")
        );
        for max in [1usize, 3, 8] {
            let limits = limits_with(|q| q.max_order_terms(max));
            let names = [
                "f_char", "f_int", "f_bool", "f_sel", "lang", "tz", "login", "active", "name",
            ];
            // `max` terms ending in id: effective order is exactly `max`
            let mut with_id = terms(&names[..max - 1]);
            with_id.push(LoginOrderTerm::desc("id"));
            let order = from(with_id, &limits).unwrap();
            assert_eq!(order.terms().len(), max);
            assert_eq!(order.terms()[max - 1], LoginOrderTerm::desc("id"));
            // `max` terms without id: the appended id is one over
            assert_eq!(
                refused(from(terms(&names[..max]), &limits)),
                validation(format!(
                    "login selection order terms {} exceeds maximum allowed limit of {max}",
                    max + 1
                )),
                "max {max}"
            );
            // more than `max` supplied terms are refused before any is walked
            assert_eq!(
                refused(from(terms(&names[..max + 1]), &limits)),
                validation("login selection effective order exceeds maximum limit"),
                "max {max} supplied"
            );
        }
        // an absent id is appended ascending, a present one is never duplicated
        let appended = from(terms(&["f_char"]), &default).unwrap();
        assert_eq!(
            appended.terms(),
            [LoginOrderTerm::asc("f_char"), LoginOrderTerm::asc("id")]
        );
        let first = from(
            vec![LoginOrderTerm::desc("id"), LoginOrderTerm::asc("f_char")],
            &default,
        )
        .unwrap();
        assert_eq!(first.terms().len(), 2);
        assert_eq!(
            refused(from(terms(&["f_char", "f_char"]), &default)),
            validation("duplicate field 'f_char' in login selection order")
        );
        assert_eq!(
            refused(from(terms(&["id", "id"]), &default)),
            validation("duplicate field 'id' in login selection order")
        );
        assert_eq!(LoginOrder::base().terms(), [LoginOrderTerm::asc("id")]);
        // framework admission of a custom order is the same check
        assert_eq!(
            LoginOrder::custom(terms(&["f_char"]))
                .admit(&default, registry())
                .unwrap(),
            appended
        );
    }

    #[test]
    fn r5_limits_constructor_matrix() {
        let query = QueryLimits::default();
        let message =
            "login selection limits must be positive and no higher than framework ceilings";
        // (nodes, list elements, witness bytes)
        for (nodes, list, witness, ok) in [
            (1, 1, 1, true),
            (1_024, 512, 65_536, true),
            (0, 512, 65_536, false),
            (1_025, 512, 65_536, false),
            (1_024, 0, 65_536, false),
            (1_024, 513, 65_536, false),
            (1_024, 512, 0, false),
            (1_024, 512, 65_537, false),
        ] {
            let made = LoginSelectionLimits::new(&query, nodes, list, witness);
            if ok {
                assert!(made.is_ok(), "{nodes}/{list}/{witness}");
            } else {
                assert_eq!(
                    refused(made),
                    validation(message),
                    "{nodes}/{list}/{witness}"
                );
            }
        }
        assert_eq!(
            LoginSelectionLimits::default(),
            LoginSelectionLimits::new(&query, 1_024, 512, 65_536).unwrap()
        );
        let lowered = limits_with(|q| q.max_bytes(1_000));
        assert_eq!(lowered.encoded_bytes(), 1_000);
        // a query ceiling above the framework default is refused by the query limits themselves
        assert!(QueryLimits::builder().max_bytes(65_537).build().is_err());
    }

    #[test]
    fn r5_lookup_matrix() {
        let too_long = |len: usize, max: usize| {
            validation(format!(
                "submitted login length {len} exceeds admission limit of {max} bytes"
            ))
        };
        assert_eq!(
            refused(LoginLookup::new("", 8)),
            validation("submitted login cannot be empty")
        );
        assert_eq!(
            LoginLookup::new("12345678", 8).unwrap().as_str(),
            "12345678"
        );
        assert_eq!(refused(LoginLookup::new("123456789", 8)), too_long(9, 8));
        // bytes, not characters: two 2-byte characters are 4 bytes
        assert_eq!(LoginLookup::new("éé", 4).unwrap().as_str(), "éé");
        assert_eq!(refused(LoginLookup::new("éé", 3)), too_long(4, 3));
        // exact bytes: no trimming, folding or normalization
        for exact in [" ", "  a ", "A.B", "e\u{301}"] {
            assert_eq!(LoginLookup::new(exact, 64).unwrap().as_str(), exact);
        }
        assert_eq!(refused(LoginLookup::new("x", 0)), too_long(1, 0));
        assert_eq!(
            format!("{:?}", LoginLookup::new("secret-user", 64).unwrap()),
            "LoginLookup([REDACTED])"
        );
    }

    fn witness(login: &str, hash: Option<&str>, deps: &[Option<&str>]) -> SelectionWitness {
        SelectionWitness {
            uid: UserId::new(10).unwrap(),
            canonical_login: login.to_owned(),
            active: true,
            stored_hash: hash.map(str::to_owned),
            dependency_values: deps.iter().map(|d| d.map(str::to_owned)).collect(),
            plan_digest: [0; 32],
        }
    }

    #[test]
    fn r5_witness_size_boundary() {
        // 128 fixed + login + hash, then 16 per dependency plus its value
        let cases: Vec<(&str, SelectionWitness, usize)> = vec![
            ("login only", witness("abc", None, &[]), 128 + 3),
            (
                "with hash",
                witness("abc", Some("hashhash"), &[]),
                128 + 3 + 8,
            ),
            (
                "dependencies, set and unset",
                witness("abc", None, &[Some("xy"), None, Some("")]),
                128 + 3 + (16 + 2) + 16 + 16,
            ),
            ("multibyte login", witness("é", None, &[]), 128 + 2),
        ];
        for (name, witness, bytes) in cases {
            assert!(witness.validate_size(bytes).is_ok(), "{name}: at {bytes}");
            assert_eq!(
                refused(witness.validate_size(bytes - 1)),
                validation(format!(
                    "selection witness size {bytes} exceeds limit of {} bytes",
                    bytes - 1
                )),
                "{name}: one under"
            );
        }
    }

    // ---- R7: fields, identifiers, operators, operands --------------------------------------

    fn unsupported_until_finish(what: &str) -> String {
        format!("{what} until a later release")
    }

    fn field_refusal(name: &str) -> (ErrorKind, String) {
        let reason = match name {
            "f_translate" | "f_company" | "pname" => unsupported_until_finish(&format!(
                "field '{name}' is inherited, translated or company-dependent; unsupported"
            )),
            "password" | "f_computed" | "f_related" => unsupported_until_finish(&format!(
                "field '{name}' on res.users is not stored; non-stored fields are unsupported"
            )),
            "f_computed_stored" | "f_related_stored" => unsupported_until_finish(&format!(
                "field '{name}' on res.users is computed or related; computed/related fields are unsupported"
            )),
            "f_float" | "f_text" | "f_date" | "f_datetime" | "f_m2o" | "f_dynsel" => {
                unsupported_until_finish(&format!(
                    "field '{name}' on res.users has unsupported type; only physical scalar fields (integer, boolean, char, static selection) are supported"
                ))
            }
            other => format!("res.users has no field '{other}' (login selection)"),
        };
        validation(reason)
    }

    const ACCEPTED_FIELDS: [&str; 9] = [
        "id", "login", "active", "lang", "tz", "f_char", "f_int", "f_bool", "f_sel",
    ];
    const REFUSED_FIELDS: [&str; 16] = [
        "pname",
        "password",
        "f_translate",
        "f_company",
        "f_computed",
        "f_related",
        "f_computed_stored",
        "f_related_stored",
        "f_float",
        "f_text",
        "f_date",
        "f_datetime",
        "f_m2o",
        "f_dynsel",
        "does_not_exist",
        "F_CHAR",
    ];

    #[test]
    fn r7_field_matrix() {
        let default = LoginSelectionLimits::default();
        for name in ACCEPTED_FIELDS {
            assert!(
                admit_domain(cond(name, Operator::Eq, 1_i64), &default).is_ok(),
                "domain on {name}"
            );
            assert!(
                LoginOrder::from_terms(vec![LoginOrderTerm::asc(name)], &default, registry())
                    .is_ok(),
                "order on {name}"
            );
        }
        for name in REFUSED_FIELDS {
            assert_eq!(
                refused(admit_domain(cond(name, Operator::Eq, 1_i64), &default)),
                field_refusal(name),
                "domain on {name}"
            );
            assert_eq!(
                refused(LoginOrder::from_terms(
                    vec![LoginOrderTerm::asc(name)],
                    &default,
                    registry()
                )),
                field_refusal(name),
                "order on {name}"
            );
        }
    }

    #[test]
    fn r7_identifier_matrix() {
        let default = LoginSelectionLimits::default();
        let traversal = validation(unsupported_until_finish(
            "relational traversal and property paths are unsupported",
        ));
        let cells: Vec<(&str, (ErrorKind, String))> = vec![
            ("a.b", traversal.clone()),
            ("f_char.x", traversal.clone()),
            ("f_m2o.pname", traversal),
            ("a(b", validation("invalid field identifier 'a(b'")),
            ("a)b", validation("invalid field identifier 'a)b'")),
            ("a b", validation("invalid field identifier 'a b'")),
            ("a;b", validation("invalid field identifier 'a;b'")),
            ("a\"b", validation("invalid field identifier 'a\"b'")),
            ("a'b", field_refusal("a'b")),
            ("a-b", field_refusal("a-b")),
            ("f_char\t", field_refusal("f_char\t")),
        ];
        for (name, expected) in cells {
            assert_eq!(
                refused(admit_domain(cond(name, Operator::Eq, 1_i64), &default)),
                expected,
                "domain {name:?}"
            );
            assert_eq!(
                refused(LoginOrder::from_terms(
                    vec![LoginOrderTerm::asc(name)],
                    &default,
                    registry()
                )),
                expected,
                "order {name:?}"
            );
        }
        // an empty name cannot be built into a condition; an order term can carry it
        assert!(Domain::condition("", Operator::Eq, 1_i64).is_err());
        assert_eq!(
            refused(LoginOrder::from_terms(
                vec![LoginOrderTerm::asc("")],
                &default,
                registry()
            )),
            validation("field identifier cannot be empty")
        );
    }

    /// The operators the selection supports; written without a wildcard so a new operator makes
    /// this test fail to compile until its cell is decided.
    fn operator_supported(op: Operator) -> bool {
        match op {
            Operator::Eq
            | Operator::Ne
            | Operator::Lt
            | Operator::Le
            | Operator::Gt
            | Operator::Ge
            | Operator::In
            | Operator::NotIn => true,
            Operator::EqIfSet
            | Operator::Like
            | Operator::NotLike
            | Operator::ILike
            | Operator::NotILike
            | Operator::EqLike
            | Operator::NotEqLike
            | Operator::EqILike
            | Operator::NotEqILike
            | Operator::Any
            | Operator::NotAny
            | Operator::AnyBypass
            | Operator::NotAnyBypass
            | Operator::ChildOf
            | Operator::ParentOf
            | Operator::Access
            | Operator::Overlaps
            | Operator::Contains => false,
        }
    }

    fn is_membership(op: Operator) -> bool {
        matches!(op, Operator::In | Operator::NotIn)
    }

    #[test]
    fn r7_operator_by_scalar_matrix() {
        let default = LoginSelectionLimits::default();
        let scalars = [Value::Bool(true), Value::Int(1), Value::Str("x".into())];
        for op in Operator::ALL {
            for operand in &scalars {
                let result = admit_domain(cond("f_int", op, operand.clone()), &default);
                if !operator_supported(op) {
                    assert_eq!(
                        refused(result),
                        validation(unsupported_until_finish(
                            "operator unsupported for login selection"
                        )),
                        "{op:?} {operand:?}"
                    );
                } else if is_membership(op) {
                    assert_eq!(
                        refused(result),
                        validation(
                            "login selection membership operand must be a scalar collection"
                        ),
                        "{op:?} {operand:?}: a scalar is not a collection"
                    );
                } else {
                    assert!(result.is_ok(), "{op:?} {operand:?}");
                }
            }
        }
    }

    /// operators, including nested and null-like operands)
    #[test]
    fn r7_operand_kind_matrix() {
        let default = LoginSelectionLimits::default();
        let date = time::Date::from_calendar_date(2026, time::Month::November, 8).unwrap();
        let datetime = time::PrimitiveDateTime::new(date, time::Time::MIDNIGHT);
        let unsupported = || validation("unsupported scalar value in login selection domain");
        let not_collection =
            || validation("login selection membership operand must be a scalar collection");
        // not a scalar: refused for every comparison operator
        let non_scalars: Vec<(&str, Value)> = vec![
            ("float", Value::Float(1.5)),
            ("date", Value::Date(date)),
            ("datetime", Value::Datetime(datetime)),
            ("list", Value::list([1_i64])),
            ("set", Value::set([1_i64])),
            ("domain", Value::Domain(Domain::TRUE)),
            ("param", Value::Param(PrincipalParam::Uid)),
        ];
        for op in [
            Operator::Eq,
            Operator::Ne,
            Operator::Lt,
            Operator::Le,
            Operator::Gt,
            Operator::Ge,
        ] {
            for (name, value) in &non_scalars {
                assert_eq!(
                    refused(admit_domain(cond("f_int", op, value.clone()), &default)),
                    unsupported(),
                    "{op:?} {name}"
                );
            }
        }
        // membership: only List/Set of scalars; a non-collection or a non-scalar element is refused
        for op in [Operator::In, Operator::NotIn] {
            for (name, value) in &non_scalars {
                let expected = match name {
                    &"list" | &"set" => None,
                    _ => Some(not_collection()),
                };
                let result = admit_domain(cond("f_int", op, value.clone()), &default);
                match expected {
                    None => assert!(result.is_ok(), "{op:?} {name}"),
                    Some(expected) => assert_eq!(refused(result), expected, "{op:?} {name}"),
                }
            }
            let bad_elements: Vec<(&str, Value)> = vec![
                ("float element", Value::list([Value::Float(1.0)])),
                ("nested list", Value::list([Value::list([1_i64])])),
                ("domain element", Value::list([Value::Domain(Domain::TRUE)])),
                ("date element", Value::set([Value::Date(date)])),
            ];
            for (name, value) in bad_elements {
                assert_eq!(
                    refused(admit_domain(cond("f_int", op, value), &default)),
                    unsupported(),
                    "{op:?} {name}"
                );
            }
            // empty, Boolean and mixed scalar members are accepted by admission
            for value in [
                Value::list(Vec::<i64>::new()),
                Value::list([true, false]),
                Value::list([Value::Int(1), Value::Bool(false), Value::Str("x".into())]),
            ] {
                assert!(
                    admit_domain(cond("f_bool", op, value), &default).is_ok(),
                    "{op:?}"
                );
            }
        }
        // a new-record reference becomes an empty membership list
        for op in [Operator::Eq, Operator::Ne] {
            assert!(
                admit_domain(cond("f_int", op, Value::NewId), &default).is_ok(),
                "{op:?}"
            );
        }
    }

    #[test]
    fn r7_node_kind_matrix() {
        #[derive(Debug)]
        struct Opaque;
        impl erp_domain::CustomDomain for Opaque {
            fn as_any(&self) -> &dyn std::any::Any {
                self
            }
        }
        let default = LoginSelectionLimits::default();
        for constant in [Domain::TRUE, Domain::FALSE] {
            let admitted = admit_domain(constant, &default).unwrap();
            // only the four core fields are referenced
            assert_eq!(admitted.referenced_fields().len(), 4);
        }
        assert_eq!(
            refused(admit_domain(
                Domain::custom(std::sync::Arc::new(Opaque)),
                &default
            )),
            validation("custom login selection nodes are unsupported until a later release")
        );
    }
}
