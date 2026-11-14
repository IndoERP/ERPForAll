//! What a predicate reads (DN §2.2 `AccessDomain::reads`, R-S5): every (model, field) its
//! conditions, paths, `any!` sub-domains, `_inherits` delegates and user paths touch, the models
//! it sub-selects (`reads_own_table`: no create fusion when it sub-selects its own table) and its
//! `access` conditions (DOM-035). One walk serves compiled rules and combined predicates (with
//! their grants and overrides), so the engine flushes exactly what the statement reads.

use erp_domain::{Condition, Domain, DomainKind, Operation, Operator, PrincipalParam, Value};
use erp_fields::{Error, FieldType, Result};
use erp_registry::{FieldId, FieldSpec, ModelId, ModelSpec, Registry};
use erp_sql::reference::ReferenceQuery;

use crate::rule::USERS_MODEL;
use crate::rule_op::{Operators, RuleOperatorLeaf};

/// Most `_inherits` levels an inherited field may cross.
pub(crate) const MAX_INHERITS_DEPTH: usize = 64;

/// An `access` condition: deciding it needs the predicate of `target` for `op`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) struct AccessLeaf {
    /// The model whose access domain the condition asks for.
    pub(crate) target: ModelId,
    /// The operation asked.
    pub(crate) op: Operation,
}

/// The result of [`analyze`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Analysis {
    /// (model, field) pairs read, each once, in first-seen order.
    pub(crate) reads: Vec<(ModelId, FieldId)>,
    /// Models sub-selected (traversals, hierarchy operators, user paths), each once.
    pub(crate) subselects: Vec<ModelId>,
    /// `access` conditions, each once.
    pub(crate) access: Vec<AccessLeaf>,
    /// The models a declared rule operator selects from (Amendment 1): the decision
    /// depends on which of their records exist, whatever fields their predicates read.
    pub(crate) operator_targets: Vec<ModelId>,
}

impl Analysis {
    fn read(&mut self, model: ModelId, field: FieldId) {
        if !self.reads.contains(&(model, field)) {
            self.reads.push((model, field));
        }
    }

    fn subselect(&mut self, model: ModelId) {
        if !self.subselects.contains(&model) {
            self.subselects.push(model);
        }
    }
}

/// The relational comodel of a field type.
pub(crate) fn comodel_of(ty: &FieldType) -> Option<&str> {
    // exhaustive on purpose: a new relational type must say whether rules traverse it
    match ty {
        FieldType::Many2one { comodel, .. }
        | FieldType::One2many { comodel, .. }
        | FieldType::Many2many { comodel, .. } => Some(comodel.as_str()),
        FieldType::Id
        | FieldType::Boolean
        | FieldType::BigInt
        | FieldType::Integer
        | FieldType::Float { .. }
        | FieldType::Monetary { .. }
        | FieldType::Char { .. }
        | FieldType::Text
        | FieldType::Html
        | FieldType::Selection { .. }
        | FieldType::Date
        | FieldType::Datetime
        | FieldType::Uuid
        | FieldType::Xid8
        | FieldType::TimestampTz
        | FieldType::RowVersion
        | FieldType::DateRange { .. }
        | FieldType::DatetimeRange { .. }
        | FieldType::Json
        | FieldType::Reference { .. }
        | FieldType::Many2oneReference { .. } => None,
    }
}

/// Analyze `domain`, a predicate on `model`.
///
/// # Errors
///
/// `Programming` for a field, comodel or path the registry does not have, or a node whose reads
/// cannot be known (a custom node): a predicate whose reads are unknown could be checked against
/// stale values.
pub(crate) fn analyze(registry: &Registry, model: &ModelSpec, domain: &Domain) -> Result<Analysis> {
    let mut out = Analysis::default();
    Walk { registry }.domain(model, domain, &mut out)?;
    Ok(out)
}

struct Walk<'r> {
    registry: &'r Registry,
}

impl<'r> Walk<'r> {
    fn model(&self, name: &str) -> Result<&'r ModelSpec> {
        self.registry.model(name).ok_or_else(|| {
            Error::programming(format!(
                "access predicate: the model {name} is not in the registry"
            ))
        })
    }

    fn domain(&self, model: &ModelSpec, domain: &Domain, out: &mut Analysis) -> Result<()> {
        match domain.kind() {
            DomainKind::True | DomainKind::False => Ok(()),
            DomainKind::Not(child) => self.domain(model, child, out),
            DomainKind::And(children) | DomainKind::Or(children) => {
                for child in children {
                    self.domain(model, child, out)?;
                }
                Ok(())
            }
            DomainKind::Condition(condition) => self.condition(model, condition, out),
            // a declared rule operator in a compiled rule (Amendment 1): it reads its
            // reference column and the model column, and the access of every model it ranges
            // over (the models are known from the registry; which of them a principal may read
            // is decided per policy, so the whole set is the safe side for invalidation)
            DomainKind::Custom(node) if let Some(leaf) = RuleOperatorLeaf::of(node.as_ref()) => {
                let decl = leaf.decl();
                if decl.model() != model.name {
                    return Err(Error::programming(format!(
                        "access predicate of {}: the '{}' operator is declared for {}",
                        model.name,
                        decl.name(),
                        decl.model()
                    )));
                }
                let spec = self.field(model, decl.field())?;
                self.read_field(model, spec, out)?;
                if let FieldType::Many2oneReference { model_field } = &spec.ty {
                    let named = self.field(model, model_field)?;
                    self.read_field(model, named, out)?;
                }
                for target in Operators::targets(self.registry, decl) {
                    out.subselect(target.id);
                    if !out.operator_targets.contains(&target.id) {
                        out.operator_targets.push(target.id);
                    }
                    let access = AccessLeaf {
                        target: target.id,
                        op: leaf.op(),
                    };
                    if !out.access.contains(&access) {
                        out.access.push(access);
                    }
                }
                Ok(())
            }
            DomainKind::Custom(node) => Err(Error::programming(format!(
                "access predicate of {}: the custom domain node {node:?} has unknown reads; \
                 access domains are built from conditions",
                model.name
            ))),
        }
    }

    fn condition(
        &self,
        model: &ModelSpec,
        condition: &Condition,
        out: &mut Analysis,
    ) -> Result<()> {
        let field_expr = condition.field_expr();
        let (first, rest) = match field_expr.split_once('.') {
            Some((first, rest)) => (first, Some(rest)),
            None => (field_expr, None),
        };
        let spec = self.field(model, first)?;
        self.read_field(model, spec, out)?;
        // a dotted path: every relational segment is a sub-select; after a non-relational field
        // the rest is a property or date part of it
        let mut current = (model, spec);
        for segment in rest.into_iter().flat_map(|rest| rest.split('.')) {
            let Some(comodel) = comodel_of(&current.1.ty) else {
                break;
            };
            let comodel = self.model(comodel)?;
            out.subselect(comodel.id);
            let next = self.field(comodel, segment)?;
            self.read_field(comodel, next, out)?;
            current = (comodel, next);
        }
        let (holder, last) = current;
        match condition.operator() {
            // a reference query holds a predicate on its target model (Amendment 1)
            op if op.is_any()
                && let Some(query) = ReferenceQuery::of(condition.value()) =>
            {
                if !matches!(last.ty, FieldType::Many2oneReference { .. }) {
                    return Err(Error::programming(format!(
                        "access predicate of {}: a reference query on '{field_expr}', which is \
                         not a many2one_reference",
                        model.name
                    )));
                }
                let target = self.registry.models().get(query.target().index()).ok_or_else(|| {
                    Error::programming(format!(
                        "access predicate of {}: a reference query names a model that is not in \
                         the registry",
                        model.name
                    ))
                })?;
                out.subselect(target.id);
                self.domain(target, query.sub(), out)
            }
            Operator::Any | Operator::NotAny | Operator::AnyBypass | Operator::NotAnyBypass => {
                let Some(comodel) = comodel_of(&last.ty) else {
                    return Err(Error::programming(format!(
                        "access predicate of {}: '{field_expr}' is not relational in an 'any' \
                         condition",
                        model.name
                    )));
                };
                let comodel = self.model(comodel)?;
                out.subselect(comodel.id);
                match condition.value() {
                    Value::Domain(sub) => self.domain(comodel, sub, out),
                    other => self.value(holder, other, out),
                }
            }
            Operator::Access => {
                let target = match comodel_of(&last.ty) {
                    Some(comodel) => self.model(comodel)?,
                    None => holder,
                };
                let op = match condition.value() {
                    Value::Str(op) => Operation::parse(op),
                    _ => None,
                };
                let Some(op) = op else {
                    return Err(Error::programming(format!(
                        "access predicate of {}: invalid value for 'access' operator in condition \
                         on '{field_expr}'",
                        model.name
                    )));
                };
                let leaf = AccessLeaf {
                    target: target.id,
                    op,
                };
                if !out.access.contains(&leaf) {
                    out.access.push(leaf);
                }
                Ok(())
            }
            Operator::ChildOf | Operator::ParentOf => {
                match comodel_of(&last.ty) {
                    Some(comodel) => {
                        let comodel = self.model(comodel)?;
                        out.subselect(comodel.id);
                    }
                    None => out.subselect(holder.id),
                }
                self.value(holder, condition.value(), out)
            }
            _ => self.value(holder, condition.value(), out),
        }
    }

    /// The parameters (user paths) and sub-queries of a condition value on `model`.
    fn value(&self, model: &ModelSpec, value: &Value, out: &mut Analysis) -> Result<()> {
        match value {
            Value::Param(PrincipalParam::UserPath(path)) => {
                let mut current = self.model(USERS_MODEL)?;
                out.subselect(current.id);
                let fields = path.fields();
                // the last field of a path is its terminal (`id`/`ids`) or a field read on the
                // record reached
                for (i, name) in fields.iter().enumerate() {
                    let last = i + 1 == fields.len();
                    if last && matches!(&**name, "id" | "ids") {
                        break;
                    }
                    let spec = self.field(current, name)?;
                    out.read(current.id, spec.id);
                    match comodel_of(&spec.ty) {
                        Some(comodel) => {
                            current = self.model(comodel)?;
                            out.subselect(current.id);
                        }
                        None if last => {}
                        None => {
                            return Err(Error::programming(format!(
                                "access predicate: user.{path} continues after the                                  non-relational field {}.{name}",
                                current.name
                            )));
                        }
                    }
                }
                Ok(())
            }
            Value::List(items) => {
                for item in items {
                    self.value(model, item, out)?;
                }
                Ok(())
            }
            Value::Set(set) => {
                for item in set.iter() {
                    self.value(model, item, out)?;
                }
                Ok(())
            }
            // a sub-query value: what it reads is not known here; it may read this model's table
            Value::Opaque(_) | Value::Domain(_) => {
                out.subselect(model.id);
                Ok(())
            }
            _ => Ok(()),
        }
    }

    fn field<'m>(&self, model: &'m ModelSpec, name: &str) -> Result<&'m FieldSpec> {
        model.field(name).ok_or_else(|| {
            Error::programming(format!(
                "access predicate: {} has no field '{name}'",
                model.name
            ))
        })
    }

    /// `spec` of `model`, and for an inherited field its delegate and the parent's field.
    fn read_field(&self, model: &ModelSpec, spec: &FieldSpec, out: &mut Analysis) -> Result<()> {
        out.read(model.id, spec.id);
        let mut owner = model;
        let mut current = spec.inherited;
        // `_inherits` chains are acyclic in a composed registry; the bound guards a damaged one
        for _ in 0..MAX_INHERITS_DEPTH {
            let Some(inherited) = current else {
                return Ok(());
            };
            out.read(owner.id, inherited.delegate);
            let parent = self.registry.model_by_id(inherited.parent);
            out.read(parent.id, inherited.parent_field);
            current = parent.field_by_id(inherited.parent_field).inherited;
            owner = parent;
        }
        Err(Error::programming(format!(
            "access predicate: the inherited field {}.{} goes through more than              {MAX_INHERITS_DEPTH} parents",
            model.name, spec.name
        )))
    }
}
