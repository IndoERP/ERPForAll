//! Host-owned HTML admission and lossless, savepoint-scoped removal reports.
use crate::Transaction;
use erp_fields::convert::FieldRef;
use erp_fields::html::{self, Limits, RemovalReport, SanitizeMode};
use erp_fields::{Cell, Error, FieldType, Result};
use erp_registry::{FieldSpec, ModelSpec};
use std::sync::Arc;

/// Maximum retained diagnostic bytes in one transaction attempt.
pub const MAX_HTML_REPORT_BYTES: usize = 1_000_000;

/// Sanitizer diagnostics for one field value, without raw content or URLs.
#[derive(Debug, Clone)]
pub struct HtmlFieldReport {
    model: &'static str,
    field: &'static str,
    language: Option<Arc<str>>,
    removals: RemovalReport,
}
impl HtmlFieldReport {
    /// Model owning the field.
    pub fn model(&self) -> &str {
        self.model
    }
    /// Field whose input was cleaned.
    pub fn field(&self) -> &str {
        self.field
    }
    /// Translation language when a language map was admitted.
    pub fn language(&self) -> Option<&str> {
        self.language.as_deref()
    }
    /// Complete bounded removal report.
    pub fn removals(&self) -> &RemovalReport {
        &self.removals
    }
}

#[derive(Default)]
pub(crate) struct Reports {
    values: Vec<HtmlFieldReport>,
    bytes: usize,
    marks: Vec<(usize, usize)>,
}
impl Reports {
    pub(crate) fn push(&mut self, value: HtmlFieldReport) -> Result<()> {
        if value.removals.is_empty() {
            return Ok(());
        }
        let charge = size_of::<HtmlFieldReport>()
            .saturating_add(value.removals.retained_bytes())
            .saturating_add(value.language.as_ref().map_or(0, |s| s.len()));
        let total = self
            .bytes
            .checked_add(charge)
            .filter(|n| *n <= MAX_HTML_REPORT_BYTES)
            .ok_or_else(|| {
                Error::validation(
                    "HTML transaction removal reports exceed the limit of 1000000 bytes",
                )
            })?;
        self.values
            .try_reserve_exact(1)
            .map_err(|_| Error::programming("Cannot allocate HTML transaction report storage"))?;
        self.values.push(value);
        self.bytes = total;
        Ok(())
    }
    pub(crate) fn mark(&mut self) {
        self.marks.push((self.values.len(), self.bytes));
    }
    pub(crate) fn release(&mut self, depth: usize) {
        self.marks.truncate(depth);
    }
    pub(crate) fn rollback(&mut self, depth: usize) {
        if let Some(&(len, bytes)) = self.marks.get(depth) {
            self.values.truncate(len);
            self.bytes = bytes;
        }
        self.marks.truncate(depth);
    }
    pub(crate) fn clear(&mut self) {
        *self = Self::default();
    }
    /// The retained reports and their charge, to restore when an isolated scope (onchange) ends.
    pub(crate) fn position(&self) -> (usize, usize) {
        (self.values.len(), self.bytes)
    }
    /// Drop every report pushed after `position`: nothing an isolated scope did persists, so
    /// neither do the reports about values it admitted.
    pub(crate) fn restore(&mut self, (len, bytes): (usize, usize)) {
        if self.values.len() > len {
            self.values.truncate(len);
            self.bytes = bytes;
        }
    }
    pub(crate) fn take(&mut self) -> Vec<HtmlFieldReport> {
        self.bytes = 0;
        self.marks.fill((0, 0));
        std::mem::take(&mut self.values)
    }
}

pub(crate) fn field_ref<'a>(spec: &'a ModelSpec, field: &'a FieldSpec) -> FieldRef<'a> {
    FieldRef {
        model: spec.name,
        field: field.name,
        label: &field.string,
    }
}

/// Admit one scalar using final metadata; no cache mutation occurs here.
pub(crate) fn admit_scalar(
    reports: &mut Reports,
    spec: &ModelSpec,
    field: &FieldSpec,
    cell: Cell,
    language: Option<&str>,
    mode: SanitizeMode,
) -> Result<Cell> {
    let policy = field.html_policy.as_ref().ok_or_else(|| {
        Error::programming(format!(
            "{}.{}: missing resolved HTML policy",
            spec.name, field.name
        ))
    })?;
    if language.is_some_and(|s| s.len() > 128) {
        return Err(Error::validation("HTML report language exceeds 128 bytes"));
    }
    let converted = html::convert_cell(
        field_ref(spec, field),
        cell,
        policy,
        &Limits::recommended(),
        mode,
    )
    .map_err(|error| {
        if let Some(language) = language {
            let mut context = match error.context() {
                Some(erp_fields::Value::Map(map)) => (**map).clone(),
                _ => erp_fields::ValueMap::new(),
            };
            context.insert("html_language".to_owned(), erp_fields::Value::str(language));
            error.with_context(erp_fields::Value::Map(Arc::new(context)))
        } else {
            error
        }
    })?;
    let (cell, removals) = converted.into_parts();
    reports.push(HtmlFieldReport {
        model: spec.name,
        field: field.name,
        language: language.map(Arc::from),
        removals,
    })?;
    Ok(cell)
}

impl Transaction {
    /// Exact HTML reports produced by this transaction attempt. No content is logged.
    pub fn html_reports(&self) -> &[HtmlFieldReport] {
        &self.html_reports.values
    }
    /// Take reports before commit; commit/reset/rollback clears attempt-local reports.
    /// A caller must not publish these until its transaction successfully commits.
    ///
    /// # Errors
    /// A programming error inside a savepoint or an isolated scope (onchange): what is inside may
    /// still be rolled back, and the scope could not drop the reports about it afterwards.
    pub fn take_html_reports(&mut self) -> Result<Vec<HtmlFieldReport>> {
        if self.inside_scope() {
            return Err(Error::programming(
                "HTML reports are taken outside savepoints and onchange scopes, before commit",
            ));
        }
        Ok(self.html_reports.take())
    }
    /// Make HTML writes strict for the rest of this transaction attempt.
    /// There is no API to relax an already strict attempt.
    pub fn require_strict_html(&mut self) {
        self.html_mode = SanitizeMode::Strict;
    }

    pub(crate) fn admit_html_cell(
        &mut self,
        spec: &ModelSpec,
        field: &FieldSpec,
        cell: Cell,
    ) -> Result<Cell> {
        if !matches!(field.ty, FieldType::Html) {
            return Ok(cell);
        }
        if field.translate
            && let Cell::Json(value) = &cell
        {
            let map = value.as_object().ok_or_else(|| {
                Error::validation(format!(
                    "{}.{}: expected an HTML translation map",
                    spec.name, field.name
                ))
            })?;
            let mut result = serde_json::Map::new();
            for (language, value) in map {
                let input = match value {
                    serde_json::Value::String(text) => Cell::Str(Arc::from(text.as_str())),
                    serde_json::Value::Null | serde_json::Value::Bool(false) => Cell::Null,
                    _ => {
                        return Err(Error::validation(format!(
                            "{}.{}: HTML translations require text or null",
                            spec.name, field.name
                        )));
                    }
                };
                let admitted = admit_scalar(
                    &mut self.html_reports,
                    spec,
                    field,
                    input,
                    Some(language),
                    self.html_mode,
                )?;
                let output = match admitted {
                    Cell::Str(text) => serde_json::Value::String(text.to_string()),
                    Cell::Null => serde_json::Value::Null,
                    _ => {
                        return Err(Error::programming(
                            "HTML admission produced a non-text cell",
                        ));
                    }
                };
                result.insert(language.clone(), output);
            }
            if field.required
                && !result
                    .get("en_US")
                    .and_then(serde_json::Value::as_str)
                    .is_some_and(|s| !s.is_empty())
            {
                return Err(Error::validation(format!(
                    "{}.{}: required HTML base translation is empty after sanitation",
                    spec.name, field.name
                )));
            }
            return Ok(Cell::Json(Arc::new(serde_json::Value::Object(result))));
        }
        admit_scalar(
            &mut self.html_reports,
            spec,
            field,
            cell,
            None,
            self.html_mode,
        )
    }
}

/// Prepare an untrusted RawVals value without silently cleaning away diagnostics.
/// All consumers must call transaction admission before cache publication.
pub(crate) fn input_cell(
    field: FieldRef<'_>,
    ty: &FieldType,
    value: &erp_fields::Value,
) -> Result<Cell> {
    if !matches!(ty, FieldType::Html) {
        return erp_fields::convert::to_cell(field, ty, value);
    }
    html::convert_value(
        field,
        value,
        &html::FieldPolicy::UnsanitisedText,
        &Limits::recommended(),
        SanitizeMode::Clean,
    )
    .map(|result| result.into_parts().0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use html::{DefaultPolicy, SanitizePolicy};
    fn report() -> HtmlFieldReport {
        let result = erp_fields::html::convert_value(
            FieldRef {
                model: "test",
                field: "body",
                label: "Body",
            },
            &erp_fields::Value::from("<p onclick='x'>hello</p>"),
            &html::FieldPolicy::Sanitize(SanitizePolicy::Default(DefaultPolicy::new())),
            &Limits::recommended(),
            SanitizeMode::Clean,
        )
        .unwrap();
        HtmlFieldReport {
            model: "test",
            field: "body",
            language: None,
            removals: result.into_parts().1,
        }
    }
    #[test]
    fn reports_follow_nested_savepoint_lifetime() {
        let mut reports = Reports::default();
        reports.push(report()).unwrap();
        reports.mark();
        reports.push(report()).unwrap();
        reports.mark();
        reports.push(report()).unwrap();
        reports.release(1);
        assert_eq!(reports.values.len(), 3);
        reports.rollback(0);
        assert_eq!(reports.values.len(), 1);
        assert_eq!(reports.values[0].removals.total_count(), 1);
        assert!(reports.marks.is_empty());
    }
    #[test]
    fn report_overflow_refuses_without_losing_prior_entries() {
        let mut reports = Reports::default();
        let template = report();
        let mut accepted = 0;
        let error = loop {
            match reports.push(template.clone()) {
                Ok(()) => accepted += 1,
                Err(error) => break error,
            }
            assert!(accepted <= MAX_HTML_REPORT_BYTES);
        };
        assert_eq!(error.kind(), erp_fields::ErrorKind::Validation);
        assert_eq!(
            error.message(),
            "HTML transaction removal reports exceed the limit of 1000000 bytes"
        );
        assert_eq!(reports.values.len(), accepted);
        assert!(reports.bytes <= MAX_HTML_REPORT_BYTES);
        assert!(reports.values.iter().all(|r| r.removals.total_count() == 1));
        let transferred = reports.take();
        assert_eq!(transferred.len(), accepted);
        reports.push(template).unwrap();
        assert_eq!(reports.values.len(), 1);
    }
    #[test]
    fn taken_reports_are_not_resurrected_by_savepoint_rollback() {
        let mut reports = Reports::default();
        reports.push(report()).unwrap();
        reports.mark();
        reports.push(report()).unwrap();
        assert_eq!(reports.take().len(), 2);
        reports.push(report()).unwrap();
        reports.rollback(0);
        assert!(reports.values.is_empty());
        assert_eq!(reports.bytes, 0);
    }
}
