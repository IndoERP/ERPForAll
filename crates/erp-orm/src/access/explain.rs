//! Explain requests (DN §6): what the engine's error builder asks the policy,
//! so enforcement and explanation cannot drift. fills explanations from the
//! compiled rules; `erp-orm` gathers the per-record evidence with one statement.
//!
//! A record-level denial is explained in two steps: asked about ids without evidence, the policy
//! answers the [`EvidenceColumn`]s to evaluate (one predicate per applicable rule); the engine
//! evaluates them on the denied records with one statement (`SELECT id, (<col 1>) IS TRUE, …`)
//! and asks again with the [`RecordEvidence`]: the policy then blames the failing rules and
//! renders the SEC-009 text ([`Explanation::message`]).

use erp_domain::Domain;
use erp_fields::{Error, Result, Value, ValueMap};
use erp_registry::ModelSpec;

use super::domain::RuleRef;
use super::{Operation, PolicyGeneration};
use crate::principal::{CompanyId, PrincipalBinding};

/// Most record ids one explanation covers (the six records SEC-009 lists).
pub const MAX_EXPLAIN_IDS: usize = 6;

/// Whose access is explained.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum ExplainTarget {
    /// The asking principal itself (always allowed at the `Message` detail).
    Myself,
}

/// How much an explanation says.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum ExplainDetail {
    Message,
    /// Per-record results (the debug listing).
    Records,
}

/// What to explain: `op` on `model` (and on `ids`, at most [`MAX_EXPLAIN_IDS`]) for a binding.
#[derive(Debug, Clone, Copy)]
pub struct ExplainRequest<'a> {
    binding: &'a PrincipalBinding<'a>,
    target: ExplainTarget,
    model: &'a ModelSpec,
    op: Operation,
    ids: &'a [i64],
    detail: ExplainDetail,
    evidence: Option<&'a RecordEvidence>,
}

impl<'a> ExplainRequest<'a> {
    /// Explain `op` on `model` for `binding` itself at the `Message` detail, for no record.
    pub fn new(binding: &'a PrincipalBinding<'a>, model: &'a ModelSpec, op: Operation) -> Self {
        Self {
            binding,
            target: ExplainTarget::Myself,
            model,
            op,
            ids: &[],
            detail: ExplainDetail::Message,
            evidence: None,
        }
    }

    /// The same request about `ids`.
    ///
    /// # Errors
    ///
    /// `Programming` for more than [`MAX_EXPLAIN_IDS`] ids (explain never probes more records
    /// than an error message lists).
    pub fn ids(mut self, ids: &'a [i64]) -> Result<Self> {
        if ids.len() > MAX_EXPLAIN_IDS {
            return Err(Error::programming(format!(
                "an explanation covers at most {MAX_EXPLAIN_IDS} records, {} were asked",
                ids.len()
            )));
        }
        self.ids = ids;
        Ok(self)
    }

    /// The same request at `detail`.
    #[must_use]
    pub fn detail(mut self, detail: ExplainDetail) -> Self {
        self.detail = detail;
        self
    }

    /// The asking binding.
    pub fn binding(&self) -> &'a PrincipalBinding<'a> {
        self.binding
    }

    /// Whose access.
    pub fn target(&self) -> ExplainTarget {
        self.target
    }

    /// The model.
    pub fn model(&self) -> &'a ModelSpec {
        self.model
    }

    /// The operation.
    pub fn op(&self) -> Operation {
        self.op
    }

    /// The records (empty: the model-level decision).
    pub fn record_ids(&self) -> &'a [i64] {
        self.ids
    }

    /// The detail.
    pub fn detail_level(&self) -> ExplainDetail {
        self.detail
    }

    /// The same request with the evidence the engine gathered for its ids.
    #[must_use]
    pub fn evidence(mut self, evidence: &'a RecordEvidence) -> Self {
        self.evidence = Some(evidence);
        self
    }

    /// The evidence, when the engine gathered it.
    pub fn record_evidence(&self) -> Option<&'a RecordEvidence> {
        self.evidence
    }
}

/// Whether an evidence column is a permission or a restriction.
///
// Deliberately not `#[non_exhaustive]`: blaming depends on the kind (permissions fail as a whole,
// restrictions one by one), so a new kind must be decided wherever blame is computed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RuleKind {
    /// A rule with a group: permissions are ORed, so they fail as a whole.
    Permission,
    /// A rule without group: restrictions are ANDed, each fails on its own.
    Restriction,
}

/// One rule the evidence statement evaluates: its predicate compiled for the principal
/// (parameters bound per statement, `access` conditions resolved).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EvidenceColumn {
    rule: RuleRef,
    kind: RuleKind,
    domain: Domain,
    mentions_company: bool,
}

impl EvidenceColumn {
    /// The column of `rule`.
    pub fn new(rule: RuleRef, kind: RuleKind, domain: Domain) -> Self {
        Self {
            rule,
            kind,
            domain,
            mentions_company: false,
        }
    }

    #[must_use]
    pub fn with_mentions_company(mut self, mentions: bool) -> Self {
        self.mentions_company = mentions;
        self
    }

    /// Whether the rule names the company (see [`EvidenceColumn::with_mentions_company`]).
    pub fn mentions_company(&self) -> bool {
        self.mentions_company
    }

    /// The rule.
    pub fn rule(&self) -> &RuleRef {
        &self.rule
    }

    /// Permission or restriction.
    pub fn kind(&self) -> RuleKind {
        self.kind
    }

    /// The predicate to evaluate (`(<domain>) IS TRUE` per record).
    pub fn domain(&self) -> &Domain {
        &self.domain
    }
}

/// What the evidence statement found for one denied record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EvidenceRow {
    id: i64,
    display_name: Option<String>,
    results: Vec<bool>,
    company: Option<EvidenceCompany>,
    name_redacted: bool,
}

impl EvidenceRow {
    /// Record `id` with one result per column (`(<rule>) IS TRUE`: `NULL` is `false`) and, for
    /// the debug listing, its display name (`None` when it has none).
    pub fn new(id: i64, display_name: Option<String>, results: Vec<bool>) -> Self {
        Self {
            id,
            display_name,
            results,
            company: None,
            name_redacted: false,
        }
    }

    /// The same row with the company of the record as the evidence statement read it (a model
    /// with a `company_id` field; `None` for a record without company).
    #[must_use]
    pub fn with_company(mut self, company: Option<EvidenceCompany>) -> Self {
        self.company = company;
        self
    }

    /// The same row, saying the display name is withheld: the record belongs to a company outside
    /// the principal's allowed companies. The debug listing then shows the
    /// record without its name.
    #[must_use]
    pub fn with_name_redacted(mut self, redacted: bool) -> Self {
        self.name_redacted = redacted;
        self
    }

    /// The company of the record (`company_id`), when the model has one and it is set.
    pub fn company(&self) -> Option<&EvidenceCompany> {
        self.company.as_ref()
    }

    /// Whether the display name was withheld (see [`EvidenceRow::with_name_redacted`]).
    pub fn name_redacted(&self) -> bool {
        self.name_redacted
    }

    /// The record id.
    pub fn id(&self) -> i64 {
        self.id
    }

    /// The display name, if any.
    pub fn display_name(&self) -> Option<&str> {
        self.display_name.as_deref()
    }

    /// One result per column.
    pub fn results(&self) -> &[bool] {
        &self.results
    }
}

/// The evidence of a record-level denial: the columns, the rows of at most [`MAX_EXPLAIN_IDS`]
/// denied records, and the subject's display name (the statement reads it with the records).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecordEvidence {
    columns: Vec<EvidenceColumn>,
    rows: Vec<EvidenceRow>,
    subject_name: String,
    first_company: Option<EvidenceCompany>,
}

impl RecordEvidence {
    /// Evidence of `rows` over `columns`, for a subject named `subject_name`.
    ///
    /// # Errors
    ///
    /// `Programming` for more than [`MAX_EXPLAIN_IDS`] rows or a row without one result per
    /// column.
    pub fn new(
        columns: Vec<EvidenceColumn>,
        rows: Vec<EvidenceRow>,
        subject_name: impl Into<String>,
    ) -> Result<Self> {
        if rows.len() > MAX_EXPLAIN_IDS {
            return Err(Error::programming(format!(
                "access evidence covers at most {MAX_EXPLAIN_IDS} records, {} were given",
                rows.len()
            )));
        }
        if let Some(row) = rows.iter().find(|r| r.results.len() != columns.len()) {
            return Err(Error::programming(format!(
                "access evidence of record {} has {} results for {} rules",
                row.id,
                row.results.len(),
                columns.len()
            )));
        }
        Ok(Self {
            columns,
            rows,
            subject_name: subject_name.into(),
            first_company: None,
        })
    }

    /// Not for models with a `company_id`: their rows carry the
    /// company.
    #[must_use]
    pub fn with_first_company(mut self, company: Option<EvidenceCompany>) -> Self {
        self.first_company = company;
        self
    }

    /// ```
    /// use erp_orm::{CompanyHint, CompanyId, EvidenceCompany, EvidenceRow, RecordEvidence};
    /// let b = CompanyId::new(2)?;
    /// let row = EvidenceRow::new(7, None, vec![])
    ///     .with_company(Some(EvidenceCompany::own(b, "Company B".into())));
    /// let evidence = RecordEvidence::new(vec![], vec![row], "Bob")?;
    /// assert_eq!(
    ///     evidence.company_hint(),
    ///     CompanyHint::Suggested { company: b, display_name: "Company B".into() }
    /// );
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    pub fn company_hint(&self) -> CompanyHint {
        if let Some(first) = &self.first_company {
            return CompanyHint::of(first);
        }
        let mut found: Option<&EvidenceCompany> = None;
        for company in self.rows.iter().filter_map(EvidenceRow::company) {
            match found {
                None => found = Some(company),
                Some(seen) if seen.id == company.id => {}
                Some(_) => return CompanyHint::Ambiguous,
            }
        }
        found.map_or(CompanyHint::None, CompanyHint::of)
    }

    /// The columns.
    pub fn columns(&self) -> &[EvidenceColumn] {
        &self.columns
    }

    /// The rows.
    pub fn rows(&self) -> &[EvidenceRow] {
        &self.rows
    }

    /// The subject's display name (`res.users.name`).
    pub fn subject_name(&self) -> &str {
        &self.subject_name
    }
}

/// The company of a denied record as the evidence statement read it: its id, and its name only
/// when it is one of the subject's own companies (a foreign company is never named,
/// decision 8).
///
/// ```
/// use erp_orm::{CompanyId, EvidenceCompany};
/// let own = EvidenceCompany::own(CompanyId::new(2)?, "Company B".into());
/// assert_eq!(own.display_name(), Some("Company B"));
/// assert_eq!(EvidenceCompany::foreign(CompanyId::new(3)?).display_name(), None);
/// # Ok::<(), Box<dyn std::error::Error>>(())
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EvidenceCompany {
    id: CompanyId,
    display_name: Option<String>,
}

impl EvidenceCompany {
    /// A company the subject is in: its name may be shown.
    pub fn own(id: CompanyId, display_name: String) -> Self {
        Self {
            id,
            display_name: Some(display_name),
        }
    }

    /// A company the subject is not in: it is never named, so there is no
    /// way to give it a name.
    pub fn foreign(id: CompanyId) -> Self {
        Self {
            id,
            display_name: None,
        }
    }

    /// The company id.
    pub fn id(&self) -> CompanyId {
        self.id
    }

    /// The company's name, when the subject is in the company.
    pub fn display_name(&self) -> Option<&str> {
        self.display_name.as_deref()
    }
}

/// ```
/// use erp_orm::{CompanyHint, CompanyId};
/// let hint = CompanyHint::Suggested { company: CompanyId::new(2)?, display_name: "Company B".into() };
/// assert_eq!(
///     hint.text().as_deref(),
///     Some("To open the record, switch to its company: Company B.")
/// );
/// assert!(CompanyHint::None.text().is_none());
/// # Ok::<(), Box<dyn std::error::Error>>(())
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum CompanyHint {
    /// The records have no company: nothing is said.
    None,
    /// The denied records belong to several companies.
    Ambiguous,
    /// One company, one of the subject's: it can be switched to.
    Suggested {
        /// The company.
        company: CompanyId,
        /// Its display name.
        display_name: String,
    },
    /// One company the subject is not in; it is never named.
    OutsideYourCompanies,
}

impl CompanyHint {
    fn of(company: &EvidenceCompany) -> Self {
        match company.display_name() {
            Some(name) => Self::Suggested {
                company: company.id,
                display_name: name.to_owned(),
            },
            None => Self::OutsideYourCompanies,
        }
    }

    pub fn text(&self) -> Option<String> {
        match self {
            Self::None => None,
            Self::Ambiguous => Some(
                "The records belong to several companies: switching the current company may help."
                    .to_owned(),
            ),
            Self::Suggested { display_name, .. } => Some(format!(
                "To open the record, switch to its company: {display_name}."
            )),
            Self::OutsideYourCompanies => {
                Some("The record belongs to a company outside your allowed companies.".to_owned())
            }
        }
    }

    /// The error context of a suggestion, `{"suggested_company": {"id", "display_name"}}`, which
    /// the web client uses to switch company.
    pub fn error_context(&self) -> Option<Value> {
        let Self::Suggested {
            company,
            display_name,
        } = self
        else {
            return None;
        };
        let mut inner = ValueMap::new();
        inner.insert("id".to_owned(), Value::Int(company.get()));
        inner.insert("display_name".to_owned(), Value::str(display_name));
        let mut outer = ValueMap::new();
        outer.insert("suggested_company".to_owned(), Value::Map(inner.into()));
        Some(Value::Map(outer.into()))
    }
}

/// The outcome of an explained decision.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum AccessDecision {
    /// Allowed.
    Allow,
    /// Denied.
    Deny,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Explanation {
    decision: AccessDecision,
    generation: PolicyGeneration,
    blamed: Vec<RuleRef>,
    columns: Vec<EvidenceColumn>,
    message: Option<String>,
    context: Option<Value>,
    lists_records: bool,
}

impl Explanation {
    /// An explanation of `decision` under `generation`, blaming `blamed`.
    pub fn new(
        decision: AccessDecision,
        generation: PolicyGeneration,
        blamed: Vec<RuleRef>,
    ) -> Self {
        Self {
            decision,
            generation,
            blamed,
            columns: Vec::new(),
            message: None,
            context: None,
            lists_records: false,
        }
    }

    /// The same explanation with the error context for the client (a suggested company).
    #[must_use]
    pub fn with_context(mut self, context: Option<Value>) -> Self {
        self.context = context;
        self
    }

    /// The error context to attach to the access error, when the policy has one.
    pub fn context(&self) -> Option<&Value> {
        self.context.as_ref()
    }

    /// Without it
    /// (the default) no name is read.
    #[must_use]
    pub fn with_record_listing(mut self, lists: bool) -> Self {
        self.lists_records = lists;
        self
    }

    /// Whether the text lists the denied records by display name, so the evidence needs them.
    pub fn lists_records(&self) -> bool {
        self.lists_records
    }

    /// The same explanation with the evidence columns the engine must evaluate (a record-level
    /// request without evidence).
    #[must_use]
    pub fn with_columns(mut self, columns: Vec<EvidenceColumn>) -> Self {
        self.columns = columns;
        self
    }

    /// The same explanation with the access error text (SEC-008 or SEC-009).
    #[must_use]
    pub fn with_message(mut self, message: impl Into<String>) -> Self {
        self.message = Some(message.into());
        self
    }

    /// The evidence columns to evaluate (empty unless asked about records without evidence).
    pub fn columns(&self) -> &[EvidenceColumn] {
        &self.columns
    }

    /// The access error text of a denial, when the policy rendered one.
    pub fn message(&self) -> Option<&str> {
        self.message.as_deref()
    }

    /// The decision.
    pub fn decision(&self) -> AccessDecision {
        self.decision
    }

    /// The generations of the snapshot that decided.
    pub fn generation(&self) -> PolicyGeneration {
        self.generation
    }

    /// The rules to blame (empty when allowed, or when no rule applies at all).
    pub fn blamed(&self) -> &[RuleRef] {
        &self.blamed
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn company(id: i64, name: Option<&str>) -> EvidenceCompany {
        let id = CompanyId::new(id).unwrap();
        match name {
            Some(name) => EvidenceCompany::own(id, name.to_owned()),
            None => EvidenceCompany::foreign(id),
        }
    }

    fn evidence(companies: &[Option<EvidenceCompany>]) -> RecordEvidence {
        let rows = companies
            .iter()
            .enumerate()
            .map(|(i, c)| EvidenceRow::new(i as i64 + 1, None, vec![]).with_company(c.clone()))
            .collect();
        RecordEvidence::new(vec![], rows, "Bob").unwrap()
    }

    #[test]
    fn records_without_company_say_nothing() {
        assert_eq!(evidence(&[None, None]).company_hint(), CompanyHint::None);
        assert_eq!(evidence(&[]).company_hint(), CompanyHint::None);
        assert_eq!(CompanyHint::None.text(), None);
        assert_eq!(CompanyHint::None.error_context(), None);
    }

    #[test]
    fn one_company_of_the_subject_is_suggested_and_a_foreign_one_is_not_named() {
        let own = evidence(&[
            Some(company(2, Some("B"))),
            None,
            Some(company(2, Some("B"))),
        ]);
        assert_eq!(
            own.company_hint(),
            CompanyHint::Suggested {
                company: CompanyId::new(2).unwrap(),
                display_name: "B".into()
            }
        );
        let foreign = evidence(&[Some(company(3, None))]);
        assert_eq!(foreign.company_hint(), CompanyHint::OutsideYourCompanies);
        assert_eq!(foreign.company_hint().error_context(), None);
        assert!(!format!("{:?}", foreign.company_hint().text()).contains('3'));
    }

    #[test]
    fn several_companies_are_ambiguous_even_when_one_is_foreign() {
        let mixed = evidence(&[Some(company(2, Some("B"))), Some(company(3, None))]);
        assert_eq!(mixed.company_hint(), CompanyHint::Ambiguous);
        assert_eq!(mixed.company_hint().error_context(), None);
    }

    #[test]
    fn the_first_company_of_a_many_model_decides() {
        let many = evidence(&[None, None]).with_first_company(Some(company(5, Some("E"))));
        assert_eq!(
            many.company_hint(),
            CompanyHint::Suggested {
                company: CompanyId::new(5).unwrap(),
                display_name: "E".into()
            }
        );
        let context = many.company_hint().error_context().unwrap();
        let Value::Map(outer) = context else {
            panic!("not a map");
        };
        let Some(Value::Map(inner)) = outer.get("suggested_company") else {
            panic!("no suggestion");
        };
        assert_eq!(inner.get("id"), Some(&Value::Int(5)));
        assert_eq!(inner.get("display_name"), Some(&Value::str("E")));
        assert_eq!(
            evidence(&[None]).with_first_company(None).company_hint(),
            CompanyHint::None
        );
    }
}
