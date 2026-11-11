//! Errors and notices of the data loader (design).

use std::fmt;

use erp_fields::ErrorKind;

use crate::ast::{DataOperation, RecordOp};
use crate::error::ParseError;

/// The start tag of an operation as the error message prints it, e.g.
/// `<record id="cat_a" model="res.partner.category">`. Operations do not keep their source text,
/// so the tag is rebuilt from what they keep.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpLabel(String);

impl OpLabel {
    /// A label with the given text.
    pub fn new(text: impl Into<String>) -> Self {
        Self(text.into())
    }

    /// The label of an operation.
    pub fn of(op: &DataOperation) -> Self {
        let text = match op {
            DataOperation::Record(r) => return Self::of_record(r),
            DataOperation::MenuItem(m) => format!("<menuitem id=\"{}\">", m.id()),
            DataOperation::Delete(d) => format!("<delete model=\"{}\">", d.model()),
            DataOperation::Function(f) => {
                format!("<function model=\"{}\" name=\"{}\">", f.model(), f.name())
            }
            DataOperation::Template(t) => format!("<template id=\"{}\">", t.id()),
            DataOperation::Asset(a) => format!("<asset id=\"{}\">", a.id()),
            DataOperation::Csv(c) => format!(
                "CSV file {} (model {})",
                c.source_file(),
                c.import().model()
            ),
        };
        Self(text)
    }

    /// The label of a `<record>`.
    pub fn of_record(record: &RecordOp) -> Self {
        // a record desugared from a `<template>` is named as the template
        if let Some(origin) = record.origin() {
            return Self(origin.to_owned());
        }
        Self(match record.id() {
            Some(id) => format!("<record id=\"{id}\" model=\"{}\">", record.model()),
            None => format!("<record model=\"{}\">", record.model()),
        })
    }

    /// The text.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for OpLabel {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Where in a data file a [`LoadError`] happened.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Place {
    file: String,
    line: usize,
    op: OpLabel,
}

/// What went wrong while loading.
#[derive(Debug)]
#[non_exhaustive]
pub enum LoadErrorKind {
    /// The file did not parse (an error the operations iterator yielded).
    Parse(ParseError),
    /// An ORM or database error; its kind and message are the source's.
    Orm(erp_fields::Error),
    /// An external ID string was invalid ([`InvalidXmlId`]).
    InvalidExternalId(crate::ast::InvalidXmlId),
    /// `ref="…"`, `uid="…"` or `ref()` named an external id that does not exist
    /// .
    ExternalIdNotFound {
        /// The qualified external id.
        xid: String,
    },
    /// The operation's model is not in the registry of this install step.
    ModelMissing {
        /// What names the model (`<record>`, `CSV file`).
        what: String,
        /// The model name.
        model: String,
    },
    /// A CSV file had messages of type error; they abort the file.
    CsvImport {
        /// The module.
        module: String,
        /// The file as listed in the manifest.
        file: String,
        /// Every message of the import, errors and warnings, in order.
        messages: Vec<String>,
    },
    /// An `eval`, `context` expression failed.
    Eval {
        /// The expression source.
        expr: String,
        /// The evaluator's error, with its Python class name.
        error: erp_expr::ExprError,
    },
    /// An expression produced a value that cannot be stored in a field.
    NotAFieldValue {
        /// The model.
        model: String,
        /// The field, or `context` for a context expression.
        field: String,
        /// What the expression produced, e.g. `a timedelta`.
        got: String,
    },
    /// A text or number cannot be converted to the field's type.
    TypeMismatch {
        /// The model.
        model: String,
        /// The field.
        field: String,
        /// The field type name.
        ty: String,
        /// What was given.
        given: String,
    },
    /// `uid="…"` names a record that is not a user.
    NotAUser {
        /// The qualified external id.
        xid: String,
        /// The model the external id points at.
        model: String,
    },
    /// A record of another module's external id that does not exist and may not be created
    /// .
    MissingForeignRecord {
        /// The qualified external id.
        xid: String,
    },
    /// A record's external id names a module that is not part of this install step
    /// .
    UninstalledModule {
        /// The qualified external id.
        xid: String,
    },
    /// A `<field>` holds `<record>` elements but is not a one2many field (
    /// only collects them for one2many fields).
    NotOne2many {
        /// The model.
        model: String,
        /// The field.
        field: String,
    },
    /// A `<function>` named a method that no implementation declared data-callable.
    NotDataCallable {
        /// The model.
        model: String,
        /// The method.
        method: String,
    },
    /// A `<function>` named a method the model does not have.
    UnknownMethod {
        /// The model.
        model: String,
        /// The method.
        method: String,
    },
    /// The arguments of a `<function>` do not fit the method: too many
    /// positional arguments, an unexpected or repeated keyword, a value of the wrong type, a
    /// required argument that is missing.
    FunctionArguments {
        /// `model.method`.
        call: String,
        /// What is wrong, in the binder's words.
        message: String,
    },
    /// A `<delete id="…">` named an external id of a record of another model than the `<delete>`'s.
    DeleteWrongModel {
        /// The qualified external id.
        xid: String,
        /// The model the external id points at.
        xid_model: String,
        /// The model of the `<delete>`.
        model: String,
    },
    /// The loader's own state is inconsistent (a defect of the loader, never of the data): it
    /// fails the operation, also in demo data.
    Internal(String),
    /// A feature that a later sub-slice adds.
    Unsupported {
        /// What is not supported.
        what: String,
        /// The board task that adds it (never a merged one).
        slice: String,
    },
    /// The model an operation writes does not exist yet; the task that brings it is named
    /// (`<menuitem>` needs `ir.ui.menu`, `<template>` needs `ir.ui.view`).
    ModelArrivesLater {
        /// What needs the model (`<menuitem>`).
        what: String,
        /// The model name.
        model: String,
        /// The board task that adds the model.
        slice: String,
    },
    /// A `search=` value without a model, on a field that is not relational
    /// .
    SearchNeedsModel,
    /// An addon file a `<field>` names could not be read.
    File(crate::load::FileReadError),
    /// A file read as text is not UTF-8.
    FileNotText {
        /// The module.
        module: String,
        /// The path inside the module.
        path: String,
    },
}

impl LoadErrorKind {
    fn message(&self) -> String {
        match self {
            // printed by the error itself
            Self::Parse(_) | Self::Orm(_) => String::new(),
            Self::InvalidExternalId(e) => e.to_string(),
            Self::ExternalIdNotFound { xid } => {
                format!("External ID not found in the system: {xid}")
            }
            Self::ModelMissing { what, model } => {
                format!("{what} model '{model}' is not in the registry of this install step")
            }
            Self::CsvImport {
                module,
                file,
                messages,
            } => format!(
                "Module loading {module} failed: file {file} could not be processed:\n{}",
                messages.join("\n")
            ),
            Self::Eval { expr, error } => format!("eval \"{expr}\" failed: {error}"),
            Self::NotDataCallable { model, method } => format!(
                "<function> {model}.{method}: the method is not data-callable (declare it with \
                 #[data_callable] or #[rpc])"
            ),
            Self::UnknownMethod { model, method } => {
                format!("<function> {model}.{method}: the model has no such method")
            }
            Self::FunctionArguments { call, message } => format!("<function> {call}: {message}"),
            Self::DeleteWrongModel {
                xid,
                xid_model,
                model,
            } => format!(
                "<delete> the external ID \"{xid}\" is a record of model '{xid_model}', not of \
                 '{model}'"
            ),
            Self::NotAFieldValue { model, field, got } => format!(
                "field '{field}' of model '{model}': an expression produced {got}, which is not \
                 a field value"
            ),
            Self::TypeMismatch {
                model,
                field,
                ty,
                given,
            } => format!("field '{field}' of model '{model}' ({ty}) cannot take {given}"),
            Self::NotAUser { xid, model } => {
                format!("uid \"{xid}\" is a record of model '{model}', not of 'res.users'")
            }
            Self::MissingForeignRecord { xid } => format!("Cannot update missing record '{xid}'"),
            Self::UninstalledModule { xid } => {
                format!("The ID \"{xid}\" refers to an uninstalled module")
            }
            Self::NotOne2many { model, field } => format!(
                "field '{field}' of model '{model}' holds <record> elements but is not a one2many \
                 field"
            ),
            Self::Unsupported { what, slice } => format!("{what} is not supported until {slice}"),
            Self::ModelArrivesLater { what, model, slice } => {
                format!("{what} needs model '{model}', which arrives with {slice}")
            }
            Self::Internal(message) => format!("internal error of the data loader: {message}"),
            Self::SearchNeedsModel => {
                "Define an attribute model=\"...\" in your .XML file!".to_owned()
            }
            Self::File(error) => error.to_string(),
            Self::FileNotText { module, path } => {
                format!("{module}/{path} is not valid UTF-8 text")
            }
        }
    }
}

/// An error of the data loader.
///
/// An ORM error is reachable through
/// [`std::error::Error::source`] and keeps its own kind ([`LoadError::orm_kind`]); its message is
/// printed once, by the source.
#[derive(Debug)]
#[non_exhaustive]
pub struct LoadError {
    place: Option<Place>,
    kind: Box<LoadErrorKind>,
}

impl LoadError {
    /// An error of operation `op`.
    pub fn new(op: &DataOperation, kind: LoadErrorKind) -> Self {
        Self::at(op.source_file(), op.line(), OpLabel::of(op), kind)
    }

    /// An error at an explicit place (a nested record, a CSV row).
    pub fn at(file: &str, line: usize, op: OpLabel, kind: LoadErrorKind) -> Self {
        Self {
            place: Some(Place {
                file: file.to_owned(),
                line,
                op,
            }),
            kind: Box::new(kind),
        }
    }

    /// An error that belongs to no operation (the end-of-run cleanup, the uninstall purge).
    pub fn without_place(kind: LoadErrorKind) -> Self {
        Self {
            place: None,
            kind: Box::new(kind),
        }
    }

    /// A feature a later sub-slice adds: `what` is not supported until `slice`.
    pub fn unsupported(op: &DataOperation, what: impl Into<String>, slice: &str) -> Self {
        Self::new(
            op,
            LoadErrorKind::Unsupported {
                what: what.into(),
                slice: slice.to_owned(),
            },
        )
    }

    /// The file, when the error belongs to an operation.
    pub fn file(&self) -> Option<&str> {
        self.place.as_ref().map(|p| p.file.as_str())
    }

    /// The line, when the error belongs to an operation.
    pub fn line(&self) -> Option<usize> {
        self.place.as_ref().map(|p| p.line)
    }

    /// The operation's start tag, when the error belongs to an operation.
    pub fn op(&self) -> Option<&OpLabel> {
        self.place.as_ref().map(|p| &p.op)
    }

    /// What went wrong.
    pub fn kind(&self) -> &LoadErrorKind {
        &self.kind
    }

    /// Takes the error kind out of this error.
    pub fn into_kind(self) -> LoadErrorKind {
        *self.kind
    }

    /// The kind of the ORM error behind this one, if it is one.
    pub fn orm_kind(&self) -> Option<ErrorKind> {
        match &*self.kind {
            LoadErrorKind::Orm(e) => Some(e.kind()),
            LoadErrorKind::InvalidExternalId(_) => Some(ErrorKind::Validation),
            _ => None,
        }
    }

    #[must_use]
    pub fn relabel(mut self, op: OpLabel) -> Self {
        if let Some(place) = &mut self.place {
            place.op = op;
        }
        self
    }

    /// The file's own parse error, if this is one (the module system reports it as such).
    pub fn into_parse_error(self) -> Result<ParseError, Self> {
        match *self.kind {
            LoadErrorKind::Parse(e) => Ok(e),
            kind => Err(Self {
                place: self.place,
                kind: Box::new(kind),
            }),
        }
    }
}

impl From<ParseError> for LoadError {
    fn from(error: ParseError) -> Self {
        Self {
            place: None,
            kind: Box::new(LoadErrorKind::Parse(error)),
        }
    }
}

impl From<erp_fields::Error> for LoadError {
    fn from(error: erp_fields::Error) -> Self {
        Self {
            place: None,
            kind: Box::new(LoadErrorKind::Orm(error)),
        }
    }
}

impl fmt::Display for LoadError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if let LoadErrorKind::Parse(e) = &*self.kind {
            return fmt::Display::fmt(e, f);
        }
        let message = self.kind.message();
        match &self.place {
            Some(p) => {
                write!(
                    f,
                    "while parsing {}:{}, somewhere inside\n{}",
                    p.file, p.line, p.op
                )?;
                if !message.is_empty() {
                    write!(f, "\n{message}")?;
                }
                Ok(())
            }
            None if message.is_empty() => f.write_str("the data loader failed"),
            None => f.write_str(&message),
        }
    }
}

impl std::error::Error for LoadError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match &*self.kind {
            // only the kinds whose own text is empty: the others print their source themselves, and
            // a chain walker would print it twice
            LoadErrorKind::Orm(e) => Some(e),
            LoadErrorKind::InvalidExternalId(e) => Some(e),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct DataNotice {
    module: String,
    file: String,
    kind: NoticeKind,
}

impl DataNotice {
    /// The module whose file it concerns (its technical name). A purge notice concerns every module
    /// of the uninstall: their names joined with `", "`, and an empty file.
    pub fn module(&self) -> &str {
        &self.module
    }

    /// The file, as listed in the manifest (empty for a notice about a module as a whole).
    pub fn file(&self) -> &str {
        &self.file
    }

    /// What happened.
    pub fn kind(&self) -> &NoticeKind {
        &self.kind
    }

    /// A notice about `file` of `module`.
    pub fn new(module: impl Into<String>, file: impl Into<String>, kind: NoticeKind) -> Self {
        Self {
            module: module.into(),
            file: file.into(),
            kind,
        }
    }
}

impl fmt::Display for DataNotice {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "module {}, {}: {}", self.module, self.file, self.kind)
    }
}

/// What a [`DataNotice`] reports.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum NoticeKind {
    /// The file is listed twice in the same manifest list and was loaded twice
    /// .
    RepeatedFile,
    /// A `.js` file in a data list: valid, not loaded.
    IgnoredFile,
    /// A file that was not loaded, with the reason.
    SkippedFile {
        /// Why.
        reason: String,
    },
    /// A `<record forcecreate="0">` was skipped because a `ref=` did not resolve
    /// .
    SkippedUnresolvedRef {
        /// The record's external id, when it has one.
        xid: Option<String>,
        /// The field holding the reference.
        field: String,
        /// The external id that did not resolve, qualified.
        reference: String,
    },
    /// A `<delete id="…">` found nothing to delete: the external id does not exist, or its record
    /// is already gone.
    DeleteTargetMissing {
        /// The qualified external id.
        xid: String,
    },
    /// The demo files of the module failed and were rolled back.
    DemoFailed {
        /// The failure, as the module system printed it.
        error: String,
    },
    /// An html field was written with `type="xml"`; the value was loaded.
    HtmlDeclaredXml {
        /// The model.
        model: String,
        /// The field.
        field: String,
    },
    PurgeUnavailableModel {
        /// The model.
        model: String,
    },
    /// A view named by an external id of a module that stays was deleted with the copied view
    /// of a removed module it was built on (deletes by key with the
    /// views built on the copies; reviews A3-S3, A4-S1).
    PurgeCascadedNamedView {
        /// The deleted view.
        view_id: i64,
        /// The external id that named it.
        xmlid: String,
    },
    /// A view named by an external id of a module that stays was deleted with a removed
    /// module's own view it was built on (the view model's `force_delete` cascade,
    /// ; review S-4).
    PurgeViewCascaded {
        /// The deleted view.
        view_id: i64,
        /// The external id that named it.
        xmlid: String,
    },
    /// A CSV row named an external id of another module that does not exist; the record was
    /// created under it.
    ForeignRecordCreated {
        /// The qualified external id.
        xid: String,
    },
    DeprecatedBase64 {
        /// The model.
        model: String,
        /// The field.
        field: String,
    },
}

impl fmt::Display for NoticeKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::RepeatedFile => f.write_str("file is imported twice"),
            Self::IgnoredFile => f.write_str(".js files are valid but not loaded here"),
            Self::SkippedFile { reason } => write!(f, "file not loaded: {reason}"),
            Self::SkippedUnresolvedRef {
                xid,
                field,
                reference,
            } => match xid {
                Some(xid) => write!(
                    f,
                    "skipping creation of '{xid}' because {field}='{reference}' could not be \
                     resolved"
                ),
                None => write!(
                    f,
                    "skipping creation of a record without external id because \
                     {field}='{reference}' could not be resolved"
                ),
            },
            Self::PurgeUnavailableModel { model } => write!(
                f,
                "external ids of model '{model}' skipped: the model is not in the registry"
            ),
            Self::PurgeCascadedNamedView { view_id, xmlid } => write!(
                f,
                "view {view_id} ('{xmlid}', of a module that stays) was deleted with the copied \
                 view of a removed module it was built on"
            ),
            Self::PurgeViewCascaded { view_id, xmlid } => write!(
                f,
                "view {view_id} ('{xmlid}', of a module that stays) was deleted with the view of \
                 a removed module it was built on"
            ),
            Self::ForeignRecordCreated { xid } => write!(
                f,
                "creating record with external id '{xid}' of a module other than the one loading it"
            ),
            Self::DeleteTargetMissing { xid } => {
                write!(f, "skipping deletion for missing external ID '{xid}'")
            }
            Self::DemoFailed { error } => {
                write!(
                    f,
                    "demo data failed to install, installed without demo data: {error}"
                )
            }
            Self::HtmlDeclaredXml { model, field } => {
                write!(
                    f,
                    "HTML field '{field}' of '{model}' is declared as type=\"xml\""
                )
            }
            Self::DeprecatedBase64 { model, field } => write!(
                f,
                "field '{field}' of '{model}': type=\"base64\" is deprecated since 20.0, use \
                 type=\"bytes\""
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::error::Error as StdError;

    use super::*;
    use crate::ast::XmlId;

    #[test]
    fn invalid_external_id_kind_preserves_source_and_maps_to_validation() {
        let invalid = XmlId::parse("bad..name").unwrap_err();
        let load_err = LoadError::without_place(LoadErrorKind::InvalidExternalId(invalid.clone()));
        assert_eq!(load_err.orm_kind(), Some(ErrorKind::Validation));
        assert!(load_err.to_string().contains("maximum one dot"));
        let src = load_err.source().expect("source exists");
        let downcasted = src.downcast_ref::<crate::ast::InvalidXmlId>().unwrap();
        assert_eq!(downcasted, &invalid);
    }
}
