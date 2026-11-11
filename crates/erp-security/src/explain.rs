//! Explanations and the access error texts (DN §6): the SEC-008 model-level
//! and SEC-009 record-level messages are rendered here from what the policy decided, so
//! enforcement and explanation cannot drift.
//!
//! The record-level message needs per-record evidence that only the database has: which of the
//! applicable rules each denied record satisfies.

use std::fmt;

use erp_domain::Operation;
use erp_fields::Value;
use erp_orm::{CompanyHint, EvidenceCompany, RecordEvidence, RuleKind, RuleRef};
use erp_registry::ModelSpec;

fn model_verb(op: Operation) -> &'static str {
    match op {
        Operation::Read => "access",
        Operation::Write => "modify",
        Operation::Create => "create",
        Operation::Unlink => "delete",
    }
}

pub fn operation_name(op: Operation) -> &'static str {
    match op {
        Operation::Read => "read",
        Operation::Write => "write",
        Operation::Create => "create",
        Operation::Unlink => "delete",
    }
}

fn description(model: &ModelSpec) -> &str {
    if model.description.is_empty() {
        model.name
    } else {
        &model.description
    }
}

/// The SEC-008 message: `op` on `model` is allowed to no record.
///
/// ```
/// # use erp_registry::RegistryBuilder;
/// let registry = RegistryBuilder::new([]).linked(false).build()?;
/// let users = registry.model("res.users").unwrap();
/// let text = erp_security::explain::model_message(users, erp_orm::Operation::Write, &[]);
/// assert_eq!(
///     text,
///     "You are not allowed to modify 'User' (res.users) records.\n\nNo group currently allows \
///      this operation.\n\nContact your administrator to request access if necessary."
/// );
/// # Ok::<(), erp_fields::Error>(())
/// ```
pub fn model_message(model: &ModelSpec, op: Operation, groups: &[String]) -> String {
    let group_info = if groups.is_empty() {
        "No group currently allows this operation.".to_owned()
    } else {
        let list: Vec<String> = groups.iter().map(|g| format!("\t- {g}")).collect();
        format!(
            "This operation is allowed for the following groups:\n{}",
            list.join("\n")
        )
    };
    format!(
        "You are not allowed to {} '{}' ({}) records.\n\n{group_info}\n\nContact your \
         administrator to request access if necessary.",
        model_verb(op),
        description(model),
        model.name
    )
}

pub fn blame(evidence: &RecordEvidence) -> Vec<RuleRef> {
    let columns = evidence.columns();
    let permissions_fail = evidence.rows().iter().any(|row| {
        !columns
            .iter()
            .zip(row.results())
            .any(|(c, ok)| c.kind() == RuleKind::Permission && *ok)
    });
    columns
        .iter()
        .enumerate()
        .filter(|(i, column)| match column.kind() {
            RuleKind::Permission => permissions_fail,
            RuleKind::Restriction => evidence
                .rows()
                .iter()
                .any(|row| row.results().get(*i).is_some_and(|ok| !ok)),
        })
        .map(|(_, column)| column.rule().clone())
        .collect()
}

/// The SEC-009 record-level message of one denial, for the subject `uid`
/// named in the evidence. With `debug` (an internal user with `base.group_no_one` in debug mode),
/// the denied records and the rules to blame are listed instead of the model.
///
/// ```
/// use erp_orm::{CompanyId, EvidenceColumn, EvidenceCompany, EvidenceRow, RecordEvidence, RuleRef};
/// use erp_security::explain::RecordMessage;
/// # let registry = erp_registry::RegistryBuilder::new([]).linked(false).build()?;
/// let users = registry.model("res.users").unwrap();
/// let column = EvidenceColumn::new(
///     RuleRef::new(1, "Own company only")?,
///     erp_orm::RuleKind::Restriction,
///     erp_domain::Domain::TRUE,
/// )
/// .with_mentions_company(true);
/// let row = EvidenceRow::new(7, None, vec![false])
///     .with_company(Some(EvidenceCompany::own(CompanyId::new(2)?, "Company B".into())));
/// let evidence = RecordEvidence::new(vec![column], vec![row], "Bob")?;
/// let message = RecordMessage::new(users, erp_orm::Operation::Write, 5, &evidence);
/// assert!(message.render().ends_with("switching to the company: Company B."));
/// assert!(message.context().is_some());
/// # Ok::<(), Box<dyn std::error::Error>>(())
/// ```
#[derive(Debug, Clone, Copy)]
pub struct RecordMessage<'a> {
    model: &'a ModelSpec,
    op: Operation,
    uid: i64,
    evidence: &'a RecordEvidence,
    debug: bool,
}

impl<'a> RecordMessage<'a> {
    /// The message of `op` on `model` denied to `uid`, from `evidence` (not the debug listing).
    pub fn new(
        model: &'a ModelSpec,
        op: Operation,
        uid: i64,
        evidence: &'a RecordEvidence,
    ) -> Self {
        Self {
            model,
            op,
            uid,
            evidence,
            debug: false,
        }
    }

    /// The same message as the debug listing (`debug` true) or the plain text.
    #[must_use]
    pub fn debug(mut self, debug: bool) -> Self {
        self.debug = debug;
        self
    }

    /// The company hint, when a blamed rule names the company.
    fn hint(&self) -> CompanyHint {
        if blamed_names_company(self.evidence) {
            self.evidence.company_hint()
        } else {
            CompanyHint::None
        }
    }

    /// The error context for the client: the suggested company, when there is one.
    pub fn context(&self) -> Option<Value> {
        self.hint().error_context()
    }

    /// The text.
    pub fn render(&self) -> String {
        let (model, evidence) = (self.model, self.evidence);
        let operation_error = format!(
            "Uh-oh! Looks like you have stumbled upon some top-secret records.\n\nSorry, {} \
             (id={}) doesn't have '{}' access to:",
            evidence.subject_name(),
            self.uid,
            operation_name(self.op)
        );
        let mut resolution_info = "If you really, really need access, perhaps you can win over \
                                   your friendly administrator with a batch of freshly baked \
                                   cookies."
            .to_owned();
        let hint = self.hint();
        if let Some(text) = hint.text() {
            resolution_info.push_str("\n\n");
            resolution_info.push_str(&text);
        }
        if !self.debug {
            return format!(
                "{operation_error}\n- {} ({})\n\n{resolution_info}",
                description(model),
                model.name
            );
        }
        let company_related = blamed_names_company(evidence);
        let records: Vec<String> = evidence
            .rows()
            .iter()
            .map(|row| {
                let company = row
                    .company()
                    .and_then(EvidenceCompany::display_name)
                    .filter(|_| company_related)
                    .map(|name| format!(", company={name}"))
                    .unwrap_or_default();
                if row.name_redacted() {
                    format!(
                        "- {} ({}: {}{company})",
                        description(model),
                        model.name,
                        row.id()
                    )
                } else {
                    format!(
                        "- {}, {} ({}: {}{company})",
                        description(model),
                        Shown(row.display_name()),
                        model.name,
                        row.id()
                    )
                }
            })
            .collect();
        let rules: Vec<String> = blame(evidence)
            .iter()
            .map(|rule| format!("- {}", rule.name()))
            .collect();
        format!(
            "{operation_error}\n{}\n\nBlame the following accesses:\n{}\n\n{resolution_info}",
            records.join("\n"),
            rules.join("\n")
        )
    }
}

fn blamed_names_company(evidence: &RecordEvidence) -> bool {
    let blamed = blame(evidence);
    evidence
        .columns()
        .iter()
        .any(|c| c.mentions_company() && blamed.contains(c.rule()))
}

struct Shown<'a>(Option<&'a str>);

impl fmt::Display for Shown<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.0.unwrap_or("False"))
    }
}
