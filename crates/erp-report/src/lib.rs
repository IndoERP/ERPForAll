// SPDX-License-Identifier: MIT OR Apache-2.0
//! The report library, in four database-free parts that share one error type:
//!
//! - template admission: pure QWeb print-profile and attachment field validation. No rendering,
//!   filesystem or network I/O; static success retains explicit obligations for values that only an
//!   evaluator can determine;
//! - the report compiler and evaluator: validated archs become a closed `ReportProgram` per
//!   language, and a program, a context and the loaded records become a `DocTree`, with `t-field`
//!   values formatted by the language's settings;
//! - the data stage without I/O: the prefetch plan of a program (which fields of which models it
//!   reads, and the report helpers it may call) and the reads of a batch, one per model and
//!   relation depth, that fill the record store. The caller runs the reads; this crate has no
//!   database;
//! - the engine-neutral, bounded native report document (`DocTree`) and the `LayoutEngine` interface a
//!   renderer implements. No layout engine is linked by this crate;
//! - the `qweb-html` converter ([`render_html`]): the same `DocTree` as an escaped HTML preview page.
mod asset;
mod attachment;
mod code;
mod corpus;
mod doc_limits;
mod document;
mod duration;
mod engine;
mod error;
mod evaluate;
mod format;
mod helper;
mod html;
mod html_images;
mod limits;
mod load;
mod locale;
mod look;
mod plan;
mod precision;
mod profile;
mod program;
mod records;
mod rich_text;
mod style;
mod validate;
mod warning;
mod widget;
pub use asset::{ImageFit, ImageFormat, ImageInfo, Vector, probe_image};
pub use attachment::{
    AttachmentPolicy, FieldInfo, HelperDependencies, RestrictedField, TemplateSource,
    check_attachment,
};
pub use code::{Code, CodeGraphic, ModuleGrid, QrLevel, Symbology};
pub use corpus::{CorpusError, CorpusReport, scan_corpus};
pub use doc_limits::{LimitKind, Limits};
pub use document::{
    Alignment, CODE_CAPTION_POINTS, CrossAlignment, DocTree, DocumentBuilder, DocumentOptions,
    Node, NodeId, PageCounter, PageSetup, PdfStandard, PictureQuality, RenderDate, Style,
    TableLook,
};
pub use engine::{ImageFailure, LayoutEngine, RenderIdentity, RenderResult};
pub use error::{Diagnostic, LayoutFailure, ProfileKind, ReportError, WorkerCrash};
pub use evaluate::EvalOptions;
pub use format::LangFormat;
pub use helper::{HelperRead, HelperRequest};
pub use html::{HTML_STYLESHEET, render_html};
pub use html_images::{HtmlImageRequest, HtmlImageResult};
pub use limits::ReportLimits;
pub use load::{DataStage, LoadedRecord, ReadRequest};
pub use plan::{PlanContext, PlanLevel, PrefetchPlan, ReportHelper};
pub use precision::LoadedPrecision;
pub use profile::{HTML_DEGRADATION_RULES, PrintProfile, TokenMeaning, class_meaning};
pub use program::{Language, ReportProgram};
pub use records::{FieldValue, RecordStore};
pub use rich_text::RichText;
pub use validate::{
    DynamicValue, Validation, inspect, inspect_arch, validate, validate_arch, validate_attribute,
};
pub use warning::{HtmlSource, ReportWarning, ReportWarningKind};
