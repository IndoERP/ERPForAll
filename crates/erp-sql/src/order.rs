//! Order specifications (`_order`, `search(order=…)`, later `sorted()`) and their `ORDER BY`
//! compilation (SPEC-DOM §4.14).
//!
//! Callers map a
//! parse failure to their own error (`search`: `UserError` "Invalid "order" specified …",
//! `sorted()`: "Invalid order '…' to sort"). [`Direction::descending`] and
//! [`Nulls::first_for`] give the PostgreSQL defaults both the SQL and the in-memory sort use.
//!
//! [`compile_order`] renders terms on the model's table: plain columns with the given direction and
//! NULLS clause (PostgreSQL defaults otherwise), nullable booleans as `COALESCE(f, FALSE)`, static
//! selections by declaration order (`array_position`), and many2one fields by the comodel's own order
//! through a join (by their raw column when the comodel's order is `id` or the term is `f.id`).
//! A field inherited through `_inherits` is ordered on the parent's table, joined through the
//! delegate many2one (at every level of a multi-hop path), by the same join a condition on the
//! inherited field uses; the delegate column is among the fields to flush.

use erp_fields::{Error, FieldType, Result};
use erp_registry::{FieldId, ModelId, ModelSpec, Registry};

use crate::filter::{can_be_null, column};
use crate::{literal, quote, shorten_identifier};

/// Sort direction of a term.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    /// Not given (ascending, NULLs last).
    Default,
    /// `ASC`.
    Asc,
    /// `DESC`.
    Desc,
}

impl Direction {
    /// `DESC`.
    pub fn descending(self) -> bool {
        self == Self::Desc
    }
}

/// NULLS clause of a term.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Nulls {
    /// Not given (PostgreSQL default: last for ascending, first for descending).
    Default,
    /// `NULLS FIRST`.
    First,
    /// `NULLS LAST`.
    Last,
}

impl Nulls {
    pub fn first_for(self, direction: Direction) -> bool {
        match self {
            Self::First => true,
            Self::Last => false,
            Self::Default => direction.descending(),
        }
    }
}

/// One parsed term of an order specification.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OrderTerm<'a> {
    /// Field name.
    pub field: &'a str,
    /// `.property` suffix (`country_id.id`, date parts …).
    pub property: Option<&'a str>,
    pub func: Option<&'a str>,
    /// Direction.
    pub direction: Direction,
    /// NULLS clause.
    pub nulls: Nulls,
}

impl std::fmt::Display for OrderTerm<'_> {
    /// The term in the grammar [`parse_order`] reads (`field[.property][:func][ asc|desc][ nulls
    /// first|last]`): parsing the text gives the term back.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.field)?;
        if let Some(property) = self.property {
            write!(f, ".{property}")?;
        }
        if let Some(func) = self.func {
            write!(f, ":{func}")?;
        }
        match self.direction {
            Direction::Default => {}
            Direction::Asc => f.write_str(" asc")?,
            Direction::Desc => f.write_str(" desc")?,
        }
        match self.nulls {
            Nulls::Default => Ok(()),
            Nulls::First => f.write_str(" nulls first"),
            Nulls::Last => f.write_str(" nulls last"),
        }
    }
}

fn is_name_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_'
}

/// Split a leading run of characters matching `pred`.
fn take_while(s: &str, pred: impl Fn(char) -> bool) -> (&str, &str) {
    let end = s.find(|c: char| !pred(c)).unwrap_or(s.len());
    s.split_at(end)
}

/// Strip a case-insensitive ASCII keyword.
fn strip_keyword<'a>(s: &'a str, keyword: &str) -> Option<&'a str> {
    let head = s.get(..keyword.len())?;
    head.eq_ignore_ascii_case(keyword)
        .then(|| &s[keyword.len()..])
}

fn parse_term(part: &str) -> Option<OrderTerm<'_>> {
    let rest = part.trim_start();
    let (field, rest) = take_while(rest, is_name_char);
    if field.is_empty() {
        return None;
    }
    let (property, rest) = match rest.strip_prefix('.') {
        Some(after) => {
            let (p, r) = take_while(after, is_name_char);
            if p.is_empty() {
                return None;
            }
            (Some(p), r)
        }
        None => (None, rest),
    };
    let (func, rest) = match rest.strip_prefix(':') {
        Some(after) => {
            let (f, r) = take_while(after, |c| c.is_ascii_alphabetic() || c == '_');
            if f.is_empty() {
                return None;
            }
            (Some(f), r)
        }
        None => (None, rest),
    };
    let mut direction = Direction::Default;
    let mut nulls = Nulls::Default;
    let mut rest = rest;
    // `\s+(asc|desc)`
    let trimmed = rest.trim_start();
    if trimmed.len() < rest.len() {
        if let Some(r) = strip_keyword(trimmed, "desc").filter(|r| !r.starts_with(is_name_char)) {
            direction = Direction::Desc;
            rest = r;
        } else if let Some(r) =
            strip_keyword(trimmed, "asc").filter(|r| !r.starts_with(is_name_char))
        {
            direction = Direction::Asc;
            rest = r;
        }
    }
    let trimmed = rest.trim_start();
    if trimmed.len() < rest.len() {
        if let Some(r) =
            strip_keyword(trimmed, "nulls first").filter(|r| !r.starts_with(is_name_char))
        {
            nulls = Nulls::First;
            rest = r;
        } else if let Some(r) =
            strip_keyword(trimmed, "nulls last").filter(|r| !r.starts_with(is_name_char))
        {
            nulls = Nulls::Last;
            rest = r;
        }
    }
    if !rest.trim().is_empty() {
        return None;
    }
    Some(OrderTerm {
        field,
        property,
        func,
        direction,
        nulls,
    })
}

/// `None` when it does not match
/// (empty terms, a trailing comma, unknown words …).
pub fn parse_order(spec: &str) -> Option<Vec<OrderTerm<'_>>> {
    spec.split(',').map(parse_term).collect()
}

/// The `UserError` `search` raises for an order specification that does not parse.
pub fn invalid_order_error(spec: &str) -> Error {
    Error::user(format!(
        "Invalid \"order\" specified ({spec}). A valid \"order\" specification is a \
         comma-separated list of valid field names (optionally followed by asc/desc for the \
         direction)"
    ))
}

/// A compiled `ORDER BY` list (without the keywords).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OrderBy {
    /// The terms, comma-separated; empty when there is nothing to order by.
    pub sql: String,
    /// Fields the order reads, with their model (to flush first).
    pub fields: Vec<(ModelId, FieldId)>,
    /// `LEFT JOIN`/`JOIN`s of comodels ordered by their own order, `(alias, text)` in order.
    pub joins: Vec<(String, String)>,
}

/// A table the order reads: the main table or a joined comodel.
struct Table<'a> {
    model: &'a ModelSpec,
    alias: String,
    /// Reached through `JOIN`s only (the main table included).
    inner: bool,
}

struct OrderCompiler<'a> {
    registry: &'a Registry,
    fields: Vec<(ModelId, FieldId)>,
    joins: Vec<(String, String)>,
    lang: Option<&'a str>,
    company_slot: Option<usize>,
}

fn reversed(direction: Direction, nulls: Nulls, reverse: bool) -> (Direction, Nulls) {
    if !reverse {
        return (direction, nulls);
    }
    let direction = match direction {
        Direction::Desc => Direction::Asc,
        Direction::Default | Direction::Asc => Direction::Desc,
    };
    let nulls = match nulls {
        Nulls::First => Nulls::Last,
        Nulls::Last => Nulls::First,
        Nulls::Default => Nulls::Default,
    };
    (direction, nulls)
}

fn with_direction(mut expr: String, direction: Direction, nulls: Nulls) -> String {
    match direction {
        Direction::Asc => expr.push_str(" ASC"),
        Direction::Desc => expr.push_str(" DESC"),
        Direction::Default => {}
    }
    match nulls {
        Nulls::First => expr.push_str(" NULLS FIRST"),
        Nulls::Last => expr.push_str(" NULLS LAST"),
        Nulls::Default => {}
    }
    expr
}

fn join_body(text: &str) -> &str {
    text.strip_prefix(" LEFT JOIN ")
        .or_else(|| text.strip_prefix(" JOIN "))
        .unwrap_or(text)
}

fn join_clash(alias: &str, first: &str, second: &str) -> Error {
    Error::programming(format!(
        "Cannot change the JOIN condition of the alias {alias}: {} against {}",
        first.trim(),
        second.trim()
    ))
}

/// The joins of several lists (the predicate's and the order's of one statement) name each alias
/// for one table and condition only: a second, different join under an alias would be dropped
/// when the lists are merged and its columns read from the wrong table.
///
/// # Errors
pub fn check_join_conflicts(lists: &[&[(String, String)]]) -> Result<()> {
    let mut seen: Vec<(&str, &str)> = Vec::new();
    for list in lists {
        for (alias, text) in *list {
            match seen.iter().find(|(a, _)| *a == alias.as_str()) {
                Some((_, first)) if join_body(first) != join_body(text) => {
                    return Err(join_clash(alias, first, text));
                }
                Some(_) => {}
                None => seen.push((alias.as_str(), text.as_str())),
            }
        }
    }
    Ok(())
}

impl<'a> OrderCompiler<'a> {
    /// Records the join `text` under `alias`; the same alias twice is one join when the table and
    /// condition agree.
    fn add_join(&mut self, alias: &str, text: String) -> Result<()> {
        match self.joins.iter().find(|(a, _)| a.as_str() == alias) {
            Some((_, first)) if join_body(first) != join_body(&text) => {
                Err(join_clash(alias, first, &text))
            }
            Some(_) => Ok(()),
            None => {
                self.joins.push((alias.to_owned(), text));
                Ok(())
            }
        }
    }

    /// The terms of `spec` on `table`; `reverse` flips every direction (a many2one ordered
    /// `DESC`), `seen` holds the many2one fields being ordered already (loop guard).
    fn terms(
        &mut self,
        table: &Table<'a>,
        terms: &[OrderTerm<'_>],
        reverse: bool,
        seen: &[(ModelId, FieldId)],
        parts: &mut Vec<OrderPart>,
    ) -> Result<()> {
        for term in terms {
            self.term(table, term, reverse, seen, parts)?;
        }
        Ok(())
    }

    fn term(
        &mut self,
        table: &Table<'a>,
        term: &OrderTerm<'_>,
        reverse: bool,
        seen: &[(ModelId, FieldId)],
        parts: &mut Vec<OrderPart>,
    ) -> Result<()> {
        let model = table.model;
        let field = model.field(term.field).ok_or_else(|| {
            Error::programming(format!(
                "Invalid field '{}' on model '{}'",
                term.field, model.name
            ))
        })?;
        if field.company_dependent && self.company_slot.is_none() {
            return Err(crate::company::missing(model, field));
        }

        if let Some(inherited) = field.inherited {
            // Preserve the generated exposure for caller-side target authorization, including
            // when this term comes from a comodel's implicit order.
            let exposed = (model.id, field.id);
            if field.is_generated() && !self.fields.contains(&exposed) {
                self.fields.push(exposed);
            }
            let delegate = model.field_by_id(inherited.delegate);
            let parent = self.registry.model_by_id(inherited.parent);
            let delegate_entry = (model.id, delegate.id);
            if !self.fields.contains(&delegate_entry) {
                self.fields.push(delegate_entry);
            }
            let alias = shorten_identifier(&format!("{}__{}", table.alias, delegate.name));
            let inner = table.inner && !can_be_null(delegate);
            let kind = if inner { "JOIN" } else { "LEFT JOIN" };
            self.add_join(
                &alias,
                format!(
                    " {kind} {} AS {} ON ({}.{} = {}.{})",
                    quote(&parent.table),
                    quote(&alias),
                    quote(&table.alias),
                    quote(delegate.name),
                    quote(&alias),
                    quote("id")
                ),
            )?;
            let parent_table = Table {
                model: parent,
                alias,
                inner,
            };
            let parent_term = OrderTerm {
                field: parent.field_by_id(inherited.parent_field).name,
                ..*term
            };
            return self.term(&parent_table, &parent_term, reverse, seen, parts);
        }
        let col = if field.company_dependent {
            crate::company::expression(
                self.registry,
                model,
                field,
                &table.alias,
                self.company_slot
                    .ok_or_else(|| crate::company::missing(model, field))?,
            )?
        } else {
            format!("{}.{}", quote(&table.alias), quote(field.name))
        };
        let (direction, nulls) = reversed(term.direction, term.nulls, reverse);
        let entry = (model.id, field.id);
        let read = |c: &mut Self| {
            if !matches!(field.ty, FieldType::Id) && !c.fields.contains(&entry) {
                c.fields.push(entry);
            }
        };
        let expr = match (&field.ty, term.property) {
            (FieldType::Many2one { comodel, .. }, property) => {
                if seen.contains(&entry) {
                    // ordering a many2one already being ordered: skipped (loop guard)
                    return Ok(());
                }
                let comodel = self.registry.model(comodel).ok_or_else(|| {
                    Error::programming(format!(
                        "Field {}.{} refers to the unknown model '{comodel}'",
                        model.name, field.name
                    ))
                })?;
                let coorder = comodel.order.trim();
                let coterms = match parse_order(coorder) {
                    Some(t) if !coorder.is_empty() && coorder != "id" => t,
                    _ => Vec::new(),
                };
                match property {
                    Some("id") => {}
                    Some(p) => {
                        return Err(Error::programming(format!(
                            "Cannot order by comodel fields in '{}.{p}'",
                            term.field
                        )));
                    }
                    None => {}
                }
                read(self);
                if property.is_some() || coterms.is_empty() {
                    col
                } else {
                    match nulls {
                        Nulls::First => parts.push(OrderPart::bare(format!("{col} IS NOT NULL"))),
                        Nulls::Last => parts.push(OrderPart::bare(format!("{col} IS NULL"))),
                        Nulls::Default => {}
                    }
                    let inner = table.inner && !can_be_null(field);
                    let link = if field.company_dependent {
                        format!(
                            "{}__p{}",
                            field.name,
                            self.company_slot
                                .ok_or_else(|| crate::company::missing(model, field))?
                        )
                    } else {
                        field.name.to_owned()
                    };
                    let alias = shorten_identifier(&format!("{}__{link}", table.alias));
                    let kind = if inner { "JOIN" } else { "LEFT JOIN" };
                    self.add_join(
                        &alias,
                        format!(
                            " {kind} {} AS {} ON ({col} = {}.{})",
                            quote(&comodel.table),
                            quote(&alias),
                            quote(&alias),
                            quote("id")
                        ),
                    )?;
                    let cotable = Table {
                        model: comodel,
                        alias,
                        inner,
                    };
                    let mut seen = seen.to_vec();
                    seen.push(entry);
                    self.terms(
                        &cotable,
                        &coterms,
                        direction == Direction::Desc,
                        &seen,
                        parts,
                    )?;
                    return Ok(());
                }
            }
            (_, Some(property)) => {
                return Err(Error::programming(format!(
                    "Ordering by the property '{property}' of {}.{} is not supported until a \
                     later milestone",
                    model.name, field.name
                )));
            }
            (FieldType::Id, None) => col,
            _ if !field.has_column() => {
                return Err(Error::programming(format!(
                    "Cannot convert {}.{} to SQL because it is not stored",
                    model.name, field.name
                )));
            }
            (FieldType::Boolean, None) if can_be_null(field) => {
                read(self);
                format!("COALESCE({col}, FALSE)")
            }
            (FieldType::Selection { values, method }, None) => match values {
                Some(values) => {
                    read(self);
                    let keys: Vec<String> = values.iter().map(|(k, _)| literal(k)).collect();
                    format!(
                        "array_position(ARRAY[{}]::text[], {col}::text)",
                        keys.join(", ")
                    )
                }
                None => {
                    return Err(Error::programming(format!(
                        "Ordering by {}.{} whose values come from the method {} is not supported \
                         until a later milestone",
                        model.name,
                        field.name,
                        method.as_deref().unwrap_or("?")
                    )));
                }
            },
            _ if field.translate => {
                read(self);
                let lang_code = self.lang.filter(|l| !l.is_empty()).unwrap_or("en_US");
                if lang_code == "en_US" {
                    format!("{col}->>'en_US'")
                } else {
                    format!("COALESCE({col}->>{}, {col}->>'en_US')", literal(lang_code))
                }
            }
            (_, None) => {
                read(self);
                col
            }
        };
        let sql = with_direction(expr.clone(), direction, nulls);
        parts.push(OrderPart { expr, sql });
        Ok(())
    }
}

/// Compile `spec` (or the model's `_order` when `spec` is empty) on `model`'s table.
///
/// Many2one terms order by the comodel's own `_order` through a join (`DESC` reverses every inner
/// term and swaps its NULLS clause; an explicit NULLS clause on the many2one becomes a leading
/// `f IS [NOT] NULL` term); `f.id`, or a comodel ordered by `id`, orders by the raw column.
/// A many2one already being ordered in the chain is skipped, so self-referencing
/// orders terminate.
///
/// Unless the order already contains `id`, `id` is appended as the last term, so rows that tie on
/// every term come back in a stable order and pages do not overlap.
pub fn compile_order(
    registry: &Registry,
    model: &ModelSpec,
    spec: &str,
    lang: Option<&str>,
) -> Result<OrderBy> {
    compile_order_inner(registry, model, spec, lang, None)
}

/// Physical order dependencies, including implicit comodel order fields.
/// # Errors
/// As [`compile_order`], except preparation is not required for discovery.
pub fn order_fields(
    registry: &Registry,
    model: &ModelSpec,
    spec: &str,
    lang: Option<&str>,
) -> Result<Vec<(ModelId, FieldId)>> {
    Ok(compile_order_inner(registry, model, spec, lang, Some(1))?.fields)
}

/// Compile with the prepared-company document bound at `slot` by the caller.
/// # Errors
/// As [`compile_order`], plus a missing prepared fallback or invalid slot.
pub fn compile_order_with_company(
    registry: &Registry,
    model: &ModelSpec,
    spec: &str,
    lang: Option<&str>,
    values: &crate::company::CompanyValues,
    slot: usize,
) -> Result<OrderBy> {
    let order = compile_order_inner(registry, model, spec, lang, Some(slot))?;
    values.validate(registry, &order.fields)?;
    Ok(order)
}

fn compile_order_inner(
    registry: &Registry,
    model: &ModelSpec,
    spec: &str,
    lang: Option<&str>,
    company_slot: Option<usize>,
) -> Result<OrderBy> {
    crate::validate_language(lang)?;
    let spec = if spec.trim().is_empty() {
        model.order
    } else {
        spec
    };
    let terms = if spec.trim().is_empty() {
        Vec::new()
    } else {
        parse_order(spec).ok_or_else(|| invalid_order_error(spec))?
    };
    let has_id = terms
        .iter()
        .any(|t| t.field == "id" && model.field("id").is_some() && t.property.is_none());
    let mut compiler = OrderCompiler {
        registry,
        fields: Vec::new(),
        joins: Vec::new(),
        lang,
        company_slot,
    };
    let table = Table {
        model,
        alias: model.table.clone(),
        inner: true,
    };
    let mut parts = Vec::with_capacity(terms.len() + 1);
    compiler.terms(&table, &terms, false, &[], &mut parts)?;
    if !has_id {
        parts.push(OrderPart::bare(column(model, "id")));
    }
    Ok(OrderBy {
        sql: parts
            .iter()
            .map(|p| p.sql.as_str())
            .collect::<Vec<_>>()
            .join(", "),
        fields: compiler.fields,
        joins: compiler.joins,
    })
}

/// One compiled order term: its bare expression and the rendered term (with direction and NULLS
/// clause).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OrderPart {
    /// The expression ordered by (what a `GROUP BY` must contain to order by it).
    pub expr: String,
    /// The `ORDER BY` term.
    pub sql: String,
}

impl OrderPart {
    fn bare(expr: String) -> Self {
        Self {
            sql: expr.clone(),
            expr,
        }
    }
}

/// The compiled terms of an order specification, without the `id` tiebreaker of
/// [`compile_order`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OrderTerms {
    /// The terms in order.
    pub parts: Vec<OrderPart>,
    /// Fields the order reads, with their model (to flush first).
    pub fields: Vec<(ModelId, FieldId)>,
    /// Joins of comodels ordered by their own order, `(alias, text)` in order.
    pub joins: Vec<(String, String)>,
}

/// The order of `comodel`'s records referenced by `fk` (an SQL expression holding their ids, e.g.
/// a column of a grouped sub-query): a `LEFT JOIN comodel AS alias ON (fk = alias.id)` and the
/// comodel's `_order` terms on it, every direction flipped when `descending`, an explicit NULLS
/// clause becoming a leading `fk IS [NOT] NULL` term (the rule of [`compile_order`] for many2one
/// terms). A comodel ordered by `id` orders by `fk` itself, without a join. `_read_group` orders
/// many2one groups this way after grouping by the ids alone.
pub fn compile_comodel_order(
    registry: &Registry,
    comodel: &ModelSpec,
    fk: &str,
    alias: &str,
    descending: bool,
    nulls: Nulls,
    lang: Option<&str>,
) -> Result<OrderTerms> {
    compile_comodel_order_inner(
        registry,
        comodel,
        fk,
        ComodelOrderOptions::new(alias)
            .descending(descending)
            .nulls(nulls)
            .with_lang(lang),
        None,
    )
}

/// Presentation options for a comodel's order around grouped records.
#[derive(Debug, Clone, Copy)]
#[non_exhaustive]
pub struct ComodelOrderOptions<'a> {
    alias: &'a str,
    descending: bool,
    nulls: Nulls,
    lang: Option<&'a str>,
}
impl<'a> ComodelOrderOptions<'a> {
    /// Default ascending order at a quoted table alias.
    pub fn new(alias: &'a str) -> Self {
        Self {
            alias,
            descending: false,
            nulls: Nulls::Default,
            lang: None,
        }
    }
    /// Reverse every term of the comodel's order.
    pub fn descending(mut self, descending: bool) -> Self {
        self.descending = descending;
        self
    }
    /// Place empty relation groups first or last.
    pub fn nulls(mut self, nulls: Nulls) -> Self {
        self.nulls = nulls;
        self
    }
    /// Select the language of translated order fields.
    pub fn with_lang(mut self, lang: Option<&'a str>) -> Self {
        self.lang = lang;
        self
    }
}

/// Comodel order with a previously prepared company document.
/// # Errors
/// As [`compile_comodel_order`], plus missing property preparation.
pub fn compile_comodel_order_with_company(
    registry: &Registry,
    comodel: &ModelSpec,
    fk: &str,
    options: ComodelOrderOptions<'_>,
    company: Option<(&crate::company::CompanyValues, usize)>,
) -> Result<OrderTerms> {
    let terms = compile_comodel_order_inner(
        registry,
        comodel,
        fk,
        options,
        company.map(|(_, slot)| slot),
    )?;
    if let Some((values, _)) = company {
        values.validate(registry, &terms.fields)?;
    }
    Ok(terms)
}

fn compile_comodel_order_inner(
    registry: &Registry,
    comodel: &ModelSpec,
    fk: &str,
    options: ComodelOrderOptions<'_>,
    company_slot: Option<usize>,
) -> Result<OrderTerms> {
    let ComodelOrderOptions {
        alias,
        descending,
        nulls,
        lang,
    } = options;
    crate::validate_language(lang)?;
    let coorder = comodel.order.trim();
    let coterms = match parse_order(coorder) {
        Some(t) if !coorder.is_empty() && coorder != "id" => t,
        _ => Vec::new(),
    };
    let direction = if descending {
        Direction::Desc
    } else {
        Direction::Asc
    };
    if coterms.is_empty() {
        return Ok(OrderTerms {
            parts: vec![OrderPart {
                expr: fk.to_owned(),
                sql: with_direction(fk.to_owned(), direction, nulls),
            }],
            fields: Vec::new(),
            joins: Vec::new(),
        });
    }
    let mut parts = Vec::with_capacity(coterms.len() + 1);
    match nulls {
        Nulls::First => parts.push(OrderPart::bare(format!("{fk} IS NOT NULL"))),
        Nulls::Last => parts.push(OrderPart::bare(format!("{fk} IS NULL"))),
        Nulls::Default => {}
    }
    let mut compiler = OrderCompiler {
        registry,
        fields: Vec::new(),
        joins: vec![(
            alias.to_owned(),
            format!(
                " LEFT JOIN {} AS {} ON ({fk} = {}.{})",
                quote(&comodel.table),
                quote(alias),
                quote(alias),
                quote("id")
            ),
        )],
        lang,
        company_slot,
    };
    let table = Table {
        model: comodel,
        alias: alias.to_owned(),
        inner: false,
    };
    compiler.terms(&table, &coterms, descending, &[], &mut parts)?;
    Ok(OrderTerms {
        parts,
        fields: compiler.fields,
        joins: compiler.joins,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A term renders back to text that parses to the same term (renders the kept
    /// terms of a comodel order this way).
    #[test]
    fn a_term_renders_to_text_that_parses_back_to_it() {
        let spec =
            "a, b desc, c.id asc nulls first, d:day, e.year:month desc nulls last, f NULLS LAST";
        let terms = parse_order(spec).unwrap();
        let rendered: Vec<String> = terms.iter().map(ToString::to_string).collect();
        assert_eq!(
            rendered,
            [
                "a",
                "b desc",
                "c.id asc nulls first",
                "d:day",
                "e.year:month desc nulls last",
                "f nulls last"
            ]
        );
        assert_eq!(parse_order(&rendered.join(", ")).unwrap(), terms);
    }

    #[test]
    fn spec_dom_053_order_grammar() {
        for valid in [
            "name",
            "name asc",
            "name DESC",
            "id desc, name",
            " name desc nulls first , id",
            "email asc nulls last",
            "country_id.id DESC",
            "create_date:month desc",
            "Name",
        ] {
            assert!(parse_order(valid).is_some(), "{valid}");
        }
        for invalid in [
            "",
            "name,",
            ",name",
            "name; drop",
            "name DESCENDING",
            "name desc desc",
            "name nulls  first",
            "name.",
            "na-me",
            "name asc nulls",
        ] {
            assert!(parse_order(invalid).is_none(), "{invalid:?}");
        }
        let terms = parse_order("email desc nulls last, id").unwrap();
        assert_eq!(terms[0].field, "email");
        assert_eq!(terms[0].direction, Direction::Desc);
        assert_eq!(terms[0].nulls, Nulls::Last);
        assert_eq!(terms[1].direction, Direction::Default);
        let t = parse_order("country_id.id:max ASC").unwrap();
        assert_eq!(t[0].property, Some("id"));
        assert_eq!(t[0].func, Some("max"));
        assert_eq!(
            invalid_order_error("name; drop").kind(),
            erp_fields::ErrorKind::UserError
        );
    }
}
