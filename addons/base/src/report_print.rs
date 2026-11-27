// SPDX-License-Identifier: MIT OR Apache-2.0
//! Turning a report and its records into a document (
//! §4).
//!
//! A print request names a report (its technical name, its database id or the external id of the
//! action) and the records. This module finds the report as the caller may see it, checks that
//! the caller may read every record, and runs the report library under the caller's rights:
//! compile the template and the templates it calls, plan the reads, load the records, evaluate.
//! The result is a [`DocTree`], the engine-neutral document. Laying it out into a PDF is the
//! node's work (the render worker pool), as is the HTTP route that answers with it: an addon has
//! no process to start.
//!
//! The template context is `docs` (the printed records), `doc` and `o` are the loop names the
//! templates choose, and `company` (for the layouts: the printed records' own `company_id` when
//! their model has one and it is set, else the caller's current company; see [`company_runs`]).

use std::sync::Arc;

use erp_sdk::auth::user_context;
use erp_sdk::prelude::*;
use erp_sdk::report::{
    DataError, DocTree, DocumentOptions, EvalContext, EvalOptions, HtmlImagePolicy, LangFormat,
    Language, Limits, LoadOptions, Node, PageSetup, PictureQuality, PlanContext, ReportError,
    ReportLimits, ReportProgram, TemplateSource, field_info, load_records_with_options,
};
use erp_sdk::{FieldType, Operation, access::GroupId};

use crate::ir_actions::report::paperformat_of;
use crate::ir_ui_view::report_templates;

/// Longest downloaded file name, in bytes, before the extension.
const MAX_FILE_NAME_BYTES: usize = 200;
/// Points per millimetre.
const POINTS_PER_MM: f64 = 72.0 / 25.4;

/// A report action as a print reads it.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct PrintReport {
    /// The action's id.
    pub id: i64,
    /// The action's name: the Print menu entry and the default file name.
    pub name: String,
    /// The model it prints.
    pub model: String,
    /// `qweb-pdf`, `qweb-html` or `qweb-text`.
    pub report_type: String,
    /// The key of its template.
    pub report_name: String,
    print_report_name: Option<String>,
    html_image_policy: HtmlImagePolicy,
}

fn report_error(report: &PrintReport, error: &ReportError) -> Error {
    Error::validation(format!(
        "Report '{}' ({}): {error}",
        report.name, report.report_name
    ))
}

/// Run compile and evaluation on the blocking pool: they are CPU work, and `erp server` may run
/// every request on one runtime thread.
async fn blocking<T: Send + 'static>(
    report: &PrintReport,
    work: impl FnOnce() -> std::result::Result<T, ReportError> + Send + 'static,
) -> Result<T> {
    tokio::task::spawn_blocking(work)
        .await
        .map_err(|e| Error::internal(format!("Report '{}': the task failed: {e}", report.name)))?
        .map_err(|e| report_error(report, &e))
}

fn data_error(report: &PrintReport, error: DataError) -> Error {
    match error {
        // the caller's own refusal (no read access to a model, record or field) stays what it is
        DataError::Orm(error) => error,
        other => Error::validation(format!(
            "Report '{}' ({}): {other}",
            report.name, report.report_name
        )),
    }
}

/// The report `reference` names, if the caller may print it: a database id, the technical name
/// (`report_name`) or the external id of the action (`module.name`), tried in that order.
///
/// # Errors
///
/// `Missing` naming the reference when no report matches; `Access` when the report is limited to
/// groups the caller is not in; a database error.
pub async fn find(tx: &mut Transaction, reference: &str) -> Result<PrintReport> {
    tx.flush().await?;
    let by_id = reference.parse::<i64>().ok().filter(|id| *id > 0);
    let rows = tx
        .query(
            "SELECT r.id, r.name, r.model, r.report_type, r.report_name, r.print_report_name, \
                    ARRAY(SELECT g.gid::int8 FROM res_groups_report_rel g WHERE g.uid = r.id \
                          ORDER BY 1), \
                    r.attachment, r.allow_attachment_group_fields \
               FROM ir_act_report_xml r \
              WHERE r.id = $1 OR r.report_name = $2 \
                 OR r.id = (SELECT d.res_id FROM ir_model_data d \
                             WHERE d.model = 'ir.actions.report' \
                               AND d.module || '.' || d.name = $2) \
              ORDER BY (r.id = $1) DESC, (r.report_name = $2) DESC, r.id LIMIT 1",
            &[&by_id.unwrap_or(0), &reference],
        )
        .await?;
    let Some(row) = rows.first() else {
        return Err(Error::missing(format!(
            "There is no report '{reference}': no report action has that id, technical name or \
             external id."
        )));
    };
    let bad = |what: &str| Error::db(format!("unexpected value in ir_act_report_xml.{what}"));
    let text = |i: usize, what: &str| -> Result<String> {
        Ok(row
            .try_get::<_, Option<String>>(i)
            .map_err(|_| bad(what))?
            .unwrap_or_default())
    };
    let report = PrintReport {
        html_image_policy: {
            let attachment: Option<String> = row.try_get(7).map_err(|_| bad("attachment"))?;
            let acknowledged: Option<bool> = row
                .try_get(8)
                .map_err(|_| bad("allow_attachment_group_fields"))?;
            // Absent acknowledgement is false, as the declared boolean field default.
            if attachment.is_some_and(|s| !s.trim().is_empty()) && acknowledged != Some(true) {
                HtmlImagePolicy::UnrestrictedFields
            } else {
                HtmlImagePolicy::CallerRights
            }
        },
        id: row.try_get(0).map_err(|_| bad("id"))?,
        name: text(1, "name")?,
        model: text(2, "model")?,
        report_type: text(3, "report_type")?,
        report_name: text(4, "report_name")?,
        print_report_name: row
            .try_get::<_, Option<String>>(5)
            .map_err(|_| bad("print_report_name"))?
            .filter(|expr| !expr.trim().is_empty()),
    };
    let groups: Vec<i64> = row.try_get(6).map_err(|_| bad("group_ids"))?;
    if !groups.is_empty() {
        let groups = groups
            .into_iter()
            .map(|g| GroupId::new(g).map_err(|e| Error::db(format!("report group {g}: {e}"))))
            .collect::<Result<Vec<GroupId>>>()?;
        let env = tx.env();
        if !tx.group_membership(&env).await?.has_any(&groups)? {
            return Err(Error::access(format!(
                "Report '{}' is limited to groups you are not a member of.",
                report.name
            )));
        }
    }
    Ok(report)
}

/// Refuse unless `ids` are records of the report's model that exist and that the caller may
/// read. `model`, when given, must be the report's model.
///
/// # Errors
///
/// `Validation` for another model or no id; `Missing` naming the ids that do not exist; `Access`
/// for a record the caller may not read.
pub async fn check_records(
    tx: &mut Transaction,
    report: &PrintReport,
    model: Option<&str>,
    ids: &[i64],
) -> Result<()> {
    if let Some(model) = model.filter(|model| *model != report.model) {
        return Err(Error::validation(format!(
            "Report '{}' prints {} records, not {model} records.",
            report.name, report.model
        )));
    }
    if ids.is_empty() {
        return Err(Error::validation("Select at least one record to print."));
    }
    let records = tx
        .env()
        .records(&report.model)?
        .browse(ids.iter().copied())?;
    let existing = records.exists(tx).await?.ids();
    let gone: Vec<String> = ids
        .iter()
        .filter(|id| !existing.contains(id))
        .map(ToString::to_string)
        .collect();
    if !gone.is_empty() {
        return Err(Error::missing(format!(
            "{} record(s) {} do not exist any more.",
            report.model,
            gone.join(", ")
        )));
    }
    records.check_access(tx, Operation::Read).await
}

/// The reports bound to the Print menu of `model` that the caller may see: `(report_name, name)`.
///
/// # Errors
///
/// `Validation` for a model the registry does not have; a database error.
pub async fn printable(tx: &mut Transaction, model: &str) -> Result<Vec<(String, String)>> {
    let env = tx.env();
    let bindings = crate::ir_actions::get_bindings(tx, &env, model).await?;
    let ids: Vec<i64> = bindings.report().iter().map(|b| b.id().get()).collect();
    if ids.is_empty() {
        return Ok(Vec::new());
    }
    let rows = tx
        .query(
            "SELECT report_name, name FROM ir_act_report_xml WHERE id = ANY($1) ORDER BY id",
            &[&ids],
        )
        .await?;
    rows.iter()
        .map(|row| {
            let read = |i| {
                row.try_get::<_, Option<String>>(i)
                    .map_err(|_| Error::db("unexpected value in ir_act_report_xml"))
                    .map(Option::unwrap_or_default)
            };
            Ok((read(0)?, read(1)?))
        })
        .collect()
}

/// The page of the report for the caller's company, from its paper format, each margin as set.
async fn page_of(
    tx: &mut Transaction,
    report: &PrintReport,
    company: Option<i64>,
) -> Result<PageSetup> {
    const FIELDS: [&str; 6] = [
        "print_page_width",
        "print_page_height",
        "margin_left",
        "margin_right",
        "margin_top",
        "margin_bottom",
    ];
    let format = paperformat_of(tx, report.id, company).await?;
    // the paper format is configuration of the report, not data of the caller
    let rows = tx
        .env()
        .sudo(true)
        .records("report.paperformat")?
        .browse([format])?
        .read(tx, &FIELDS)
        .await?;
    let row = rows
        .first()
        .ok_or_else(|| Error::missing(format!("report.paperformat {format} does not exist")))?;
    let mut values = [0.0_f64; 6];
    for (value, field) in values.iter_mut().zip(FIELDS) {
        *value = match row.get(field) {
            Some(Value::Float(v)) => *v,
            #[allow(clippy::cast_precision_loss, reason = "a margin in millimetres")]
            Some(Value::Int(v)) => *v as f64,
            other => {
                return Err(Error::invariant(format!(
                    "report.paperformat.{field} read as {other:?}"
                )));
            }
        };
    }
    let [width, height, left, right, top, bottom] = values;
    PageSetup::with_margins(
        width * POINTS_PER_MM,
        height * POINTS_PER_MM,
        [top, right, bottom, left].map(|mm| mm * POINTS_PER_MM),
    )
    .map_err(|e| report_error(report, &e))
}

/// The company each record of `ids` prints under, as runs of consecutive records of one company
/// in the order given: the record's own `company_id` when the report's model has that field and it
/// is set, else the caller's current company. A selection that mixes companies prints run by run,
/// each document with its own company's letterhead.
///
/// # Errors
///
/// The caller's `Access` refusal of the read; a database error.
pub async fn company_runs(
    tx: &mut Transaction,
    report: &PrintReport,
    ids: &[i64],
) -> Result<Vec<(Option<i64>, Vec<i64>)>> {
    let env = tx.env();
    let session = env.company_id()?;
    let own = env
        .registry()
        .model(&report.model)
        .and_then(|model| model.field("company_id"))
        .is_some_and(|field| {
            matches!(&field.ty, FieldType::Many2one { comodel, .. } if comodel.as_str() == "res.company")
        });
    let companies = if own {
        let cells = env
            .records(&report.model)?
            .browse(ids.iter().copied())?
            .cells(tx, "company_id")
            .await?;
        cells
            .iter()
            .map(|cell| {
                <Option<RecId> as erp_sdk::FieldValue>::from_cell(cell)
                    .map(|company| company.map(RecId::get).or(session))
            })
            .collect::<Result<Vec<_>>>()?
    } else {
        vec![session; ids.len()]
    };
    if companies.len() != ids.len() {
        return Err(Error::invariant(format!(
            "{} company_id read {} values for {} records",
            report.model,
            companies.len(),
            ids.len()
        )));
    }
    Ok(runs_of(ids, companies))
}

/// `ids` grouped into runs of consecutive records of one company, in the order given.
fn runs_of(ids: &[i64], companies: Vec<Option<i64>>) -> Vec<(Option<i64>, Vec<i64>)> {
    let mut runs: Vec<(Option<i64>, Vec<i64>)> = Vec::new();
    for (id, company) in ids.iter().zip(companies) {
        match runs.last_mut() {
            Some((last, run)) if *last == company => run.push(*id),
            _ => runs.push((company, vec![*id])),
        }
    }
    runs
}

/// Compile, plan, load and evaluate `report` for `ids` (already checked with
/// [`check_records`]) into one document, under the caller's rights, in the caller's language,
/// number and date formats and timezone. The letterhead and the paper format are those of the
/// records' company ([`company_runs`]); records of several companies are refused (see
/// [`prepare_runs`]). The pages are counted over the whole document.
///
/// # Errors
///
/// `Validation` naming the report for a template the library refuses (missing, outside the print
/// profile, over a limit), data it cannot print, or records of several companies; the caller's
/// `Access` refusal of a read; a database error.
pub async fn prepare(tx: &mut Transaction, report: &PrintReport, ids: &[i64]) -> Result<DocTree> {
    prepare_for_language(tx, report, ids, None).await
}

/// Prepare the approved document using an installed recipient language and the caller's rights.
///
/// # Errors
/// Returns the same validation, database and access errors as [`prepare`].
pub async fn prepare_for_language(
    tx: &mut Transaction,
    report: &PrintReport,
    ids: &[i64],
    language: Option<&str>,
) -> Result<DocTree> {
    let company = match company_runs(tx, report, ids).await?.as_slice() {
        [] => tx.env().company_id()?,
        [(company, _)] => *company,
        _ => {
            return Err(Error::validation(format!(
                "Report '{}': the selected records belong to several companies and one document \
                 prints with one company's letterhead. Print the records of each company apart.",
                report.name
            )));
        }
    };
    let locale = locale(tx, report, language).await?;
    let program = compile(tx, report, &report.report_name, None, &locale.language).await?;
    let (_, mut documents) = evaluate_run(
        tx,
        report,
        program,
        &locale,
        company,
        ids,
        vec![ids.to_vec()],
    )
    .await?;
    documents
        .pop()
        .ok_or_else(|| Error::internal("printing: the document was not evaluated"))
}

/// One document per run of records of one company in `ids` (already checked with
/// [`check_records`]), in order, each with its company's letterhead and paper format
/// ([`company_runs`]). The template is compiled once and the companies are read once.
///
/// # Errors
///
/// As [`prepare`], except that records of several companies are printed.
pub async fn prepare_runs(
    tx: &mut Transaction,
    report: &PrintReport,
    ids: &[i64],
) -> Result<Vec<DocTree>> {
    let runs = company_runs(tx, report, ids).await?;
    let locale = locale(tx, report, None).await?;
    let mut program = compile(tx, report, &report.report_name, None, &locale.language).await?;
    let mut documents = Vec::with_capacity(runs.len());
    for (company, run) in runs {
        let documents_of_run = vec![run.clone()];
        let (compiled, evaluated) = evaluate_run(
            tx,
            report,
            program,
            &locale,
            company,
            &run,
            documents_of_run,
        )
        .await?;
        program = compiled;
        documents.extend(evaluated);
    }
    Ok(documents)
}

/// Load `ids` of one `company` and evaluate one document per entry of `documents` (its records
/// bound as `docs`). The program comes back for the next run.
async fn evaluate_run(
    tx: &mut Transaction,
    report: &PrintReport,
    program: ReportProgram,
    locale: &Locale,
    company: Option<i64>,
    ids: &[i64],
    documents: Vec<Vec<i64>>,
) -> Result<(ReportProgram, Vec<DocTree>)> {
    let companies: Vec<i64> = company.into_iter().collect();
    let store = load(tx, report, &program, ids, &companies, &[]).await?;
    let page = page_of(tx, report, company).await?;
    let options = locale.options.clone();
    let pictures = locale.pictures;
    let model = report.model.clone();
    let id = format!("report-{}", report.id);
    let name = report.name.clone();
    blocking(report, move || {
        let evaluated = documents
            .iter()
            .map(|docs| {
                let context = EvalContext::standard()
                    .with_var("docs", store.recordset(&model, docs))
                    .with_var("company", store.recordset("res.company", &companies));
                let document = DocumentOptions::new(id.clone(), name.clone(), page.clone())
                    .with_pictures(pictures);
                program.evaluate(&context, &store, &options, document)
            })
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok((program, evaluated))
    })
    .await
}

/// What a document prints in: the printing user's language with the number and date formats of
/// its language record, and the user's timezone, as the web client shows them. A print job runs
/// as the user who asked for it, so a queued print reads the same values as a direct one.
struct Locale {
    language: Language,
    options: EvalOptions,
    /// The resolution and the JPEG quality pictures are embedded at: two system parameters of the
    /// database (`base.report_image_dpi`, `base.report_image_jpeg_quality`), read once per print.
    pictures: PictureQuality,
}

/// The [`Locale`] of the user `tx` runs as.
///
/// # Errors
///
/// `Validation` naming the report when the user's language is not installed, its formats are not
/// printable or the user's timezone is unknown; a database error.
async fn locale(
    tx: &mut Transaction,
    report: &PrintReport,
    language: Option<&str>,
) -> Result<Locale> {
    let uid = tx.env().user_id();
    let context = user_context(tx, uid).await?.ok_or_else(|| {
        Error::missing(format!(
            "Report '{}': the printing user {} does not exist.",
            report.name,
            uid.get()
        ))
    })?;
    let code = language.unwrap_or(context.lang());
    let rows = tx
        .query(
            "SELECT decimal_point, thousands_sep, grouping, date_format, time_format \
               FROM res_lang WHERE code = $1 AND active",
            &[&code],
        )
        .await?;
    let row = rows.first().ok_or_else(|| {
        Error::validation(format!(
            "Report '{}': the language {code} of your preferences is not installed.",
            report.name
        ))
    })?;
    let text = |i: usize, what: &str| -> Result<String> {
        Ok(row
            .try_get::<_, Option<String>>(i)
            .map_err(|_| Error::db(format!("unexpected value in res_lang.{what}")))?
            .unwrap_or_default())
    };
    let grouping = crate::parse_grouping(&text(2, "grouping")?)?
        .into_iter()
        .map(i8::try_from)
        .collect::<std::result::Result<Vec<i8>, _>>()
        .map_err(|_| {
            Error::validation(format!(
                "Report '{}': the digit grouping of the language {code} is out of range.",
                report.name
            ))
        })?;
    let formats = LangFormat::new(
        &text(0, "decimal_point")?,
        &text(1, "thousands_sep")?,
        &grouping,
        &text(3, "date_format")?,
        &text(4, "time_format")?,
    )
    .map_err(|e| report_error(report, &e))?;
    let formats = formats
        .with_language(code)
        .map_err(|e| report_error(report, &e))?;
    let tz = context.tz().filter(|tz| !tz.is_empty()).unwrap_or("UTC");
    let zone = jiff::tz::TimeZone::get(tz).map_err(|_| {
        Error::validation(format!(
            "Report '{}': the timezone {tz} of your preferences is not known.",
            report.name
        ))
    })?;
    let language = Language::new(code).map_err(|e| report_error(report, &e))?;
    let options = EvalOptions::default()
        .with_code_grid(crate::report_settings::barcode_grid(tx).await?)
        .with_formats(formats)
        .with_utc_offsets(move |unix| {
            jiff::Timestamp::from_second(unix)
                .ok()
                .map(|instant| zone.to_offset(instant).seconds())
        });
    let dpi = erp_sdk::params::get_int(tx, &crate::ADDON, crate::report_job::IMAGE_DPI_KEY).await?;
    let quality =
        erp_sdk::params::get_int(tx, &crate::ADDON, crate::report_job::IMAGE_JPEG_QUALITY_KEY)
            .await?;
    // The declarations hold both inside the floor and the bound of a document; a value that is
    // not (a declaration changed without the document model) is refused here, never clamped.
    let pictures = match (u16::try_from(dpi), u8::try_from(quality)) {
        (Ok(dpi), Ok(quality)) => {
            PictureQuality::new(dpi, quality).map_err(|e| report_error(report, &e))?
        }
        _ => {
            return Err(Error::validation(format!(
                "Report '{}': the picture resolution {dpi} or JPEG quality {quality} of the \
                 system parameters is out of range.",
                report.name
            )));
        }
    };
    Ok(Locale {
        language,
        options,
        pictures,
    })
}

/// Compile `root` from the database templates, or from `inline` (key, arch) when given.
async fn compile(
    tx: &mut Transaction,
    report: &PrintReport,
    root: &str,
    inline: Option<(&str, &str)>,
    language: &Language,
) -> Result<ReportProgram> {
    let sources = match inline {
        Some((key, arch)) => vec![(key.to_owned(), arch.to_owned())],
        None => report_templates::template_sources(tx, &report.name, root).await?,
    };
    let refs: Vec<_> = sources
        .iter()
        .map(|(key, arch)| TemplateSource::new(key, arch))
        .collect();
    let conditions = ReportProgram::group_conditions(&refs, ReportLimits::default())
        .map_err(|err| report_error(report, &err))?;
    let mut groups = std::collections::BTreeMap::new();
    let env = tx.env();
    for condition in conditions {
        let allowed = tx.has_groups(&env, &condition).await?;
        groups.insert(condition, allowed);
    }
    let mut language = language.clone();
    if inline.is_none() {
        let keys: Vec<_> = sources.iter().map(|(key, _)| key.as_str()).collect();
        let rows = tx
            .query(
                "SELECT DISTINCT ON (key) key, template_translations->$2::text FROM ir_ui_view
                WHERE key=ANY($1) AND type='qweb' AND active IS TRUE
                ORDER BY key, (mode='primary') DESC, priority, id",
                &[&keys, &language.code()],
            )
            .await?;
        for row in rows {
            let key: String = row
                .try_get(0)
                .map_err(|_| Error::db("invalid report template identity"))?;
            let terms: Option<erp_sdk::serde_json::Value> = row
                .try_get(1)
                .map_err(|_| Error::db("invalid report template catalogue"))?;
            if let Some(terms) = terms {
                for (source, translated) in terms.as_object().ok_or_else(|| {
                    Error::validation("report catalogue must be a term dictionary")
                })? {
                    language = language.with_template_term(
                        &key,
                        source,
                        translated
                            .as_str()
                            .ok_or_else(|| Error::validation("report translation must be text"))?,
                    );
                }
            }
        }
    }
    let root = root.to_owned();
    blocking(report, move || {
        let templates: Vec<TemplateSource<'_>> = sources
            .iter()
            .map(|(key, arch)| TemplateSource::new(key, arch))
            .collect();
        ReportProgram::compile_for_groups(
            &root,
            &templates,
            &language,
            ReportLimits::default(),
            &groups,
        )
    })
    .await
}

/// Plan and run the reads of `program` for `ids` and the `company` ids, with `extra` record
/// bindings of the report's model.
async fn load(
    tx: &mut Transaction,
    report: &PrintReport,
    program: &ReportProgram,
    ids: &[i64],
    companies: &[i64],
    extra: &[&str],
) -> Result<Arc<erp_sdk::report::RecordStore>> {
    let env = tx.env();
    let registry = Arc::clone(env.registry());
    let mut bindings: Vec<(&str, Option<&str>)> = vec![("company", Some("res.company"))];
    bindings.extend(
        extra
            .iter()
            .map(|name| (*name, Some(report.model.as_str()))),
    );
    let helpers = erp_sdk::report::report_helpers(tx);
    let plan = program
        .prefetch_plan(
            &PlanContext::new(&report.model)
                .with_bindings(&bindings)
                .with_helpers(&helpers),
            &|model: &str, field: &str| field_info(&registry, model, field),
        )
        .map_err(|e| report_error(report, &e))?;
    let mut roots: Vec<(&str, &[i64])> = vec![("docs", ids), ("company", companies)];
    roots.extend(extra.iter().map(|name| (*name, ids)));
    let store = load_records_with_options(
        tx,
        &env,
        &plan,
        &roots,
        LoadOptions::new(ReportLimits::default(), Limits::default())
            .with_html_image_policy(report.html_image_policy),
    )
    .await
    .map_err(|e| data_error(report, e))?;
    Ok(Arc::new(store))
}

/// The downloaded file's name for `ids`, with `extension` (`pdf`, `html`): the report's
/// `print_report_name` evaluated over `object` for one record, else the report's name. Path
/// separators and control characters are replaced, the length is bounded.
///
/// # Errors
///
/// `Validation` naming the report when the expression does not evaluate (it may read the
/// record's fields; `time` is not available); the caller's `Access` refusal of a read.
pub async fn file_name(
    tx: &mut Transaction,
    report: &PrintReport,
    ids: &[i64],
    extension: &str,
) -> Result<String> {
    let base = match (&report.print_report_name, ids) {
        (Some(expr), [_]) => {
            let arch = format!(
                "<t t-name=\"report.file_name\"><span t-out=\"{}\"/></t>",
                escape_attribute(expr)
            );
            let inline = Some(("report.file_name", arch.as_str()));
            // the name is an expression over the record: it has no text to translate
            let language = Language::new("en_US").map_err(|e| report_error(report, &e))?;
            let program = compile(tx, report, "report.file_name", inline, &language).await?;
            let store = load(tx, report, &program, ids, &[], &["object"]).await?;
            let context = EvalContext::standard()
                .with_var("docs", store.recordset(&report.model, ids))
                .with_var("company", store.recordset("res.company", &[]))
                .with_var("object", store.recordset(&report.model, ids));
            let page = PageSetup::new(595.0, 842.0, 0.0).map_err(|e| report_error(report, &e))?;
            let document = blocking(report, move || {
                program.evaluate(
                    &context,
                    &store,
                    &EvalOptions::default(),
                    DocumentOptions::new("file-name", "file name", page),
                )
            })
            .await?;
            document
                .nodes()
                .iter()
                .filter_map(|node| match node {
                    Node::Text(text) => Some(text.as_str()),
                    _ => None,
                })
                .collect::<String>()
        }
        _ => report.name.clone(),
    };
    Ok(format!("{}.{extension}", clean_file_name(&base)))
}

fn escape_attribute(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('"', "&quot;")
        .replace('<', "&lt;")
}

/// `name` without path separators or control characters, trimmed, at most
/// [`MAX_FILE_NAME_BYTES`] bytes; `report` when nothing is left.
fn clean_file_name(name: &str) -> String {
    let cleaned: String = name
        .chars()
        .map(|c| {
            if c.is_control() || matches!(c, '/' | '\\') {
                '_'
            } else {
                c
            }
        })
        .collect();
    let mut cleaned = cleaned.trim().to_owned();
    let mut end = cleaned.len().min(MAX_FILE_NAME_BYTES);
    while !cleaned.is_char_boundary(end) {
        end -= 1;
    }
    cleaned.truncate(end);
    if cleaned.is_empty() {
        "report".to_owned()
    } else {
        cleaned
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "tests")]
mod tests {
    use super::*;

    #[test]
    fn file_names_lose_separators_and_stay_bounded() {
        assert_eq!(clean_file_name("Book / Sheet\n"), "Book _ Sheet_");
        assert_eq!(clean_file_name("  "), "report");
        let long = "é".repeat(300);
        let cleaned = clean_file_name(&long);
        assert!(cleaned.len() <= MAX_FILE_NAME_BYTES);
        assert!(cleaned.chars().all(|c| c == 'é'));
        assert_eq!(
            escape_attribute("'a' + \"b\" & <c>"),
            "'a' + &quot;b&quot; &amp; &lt;c>"
        );
    }

    #[test]
    fn records_print_in_runs_of_one_company_in_the_order_given() {
        assert_eq!(
            runs_of(&[4, 5, 6, 7], vec![Some(1), Some(1), Some(2), Some(1)]),
            vec![
                (Some(1), vec![4, 5]),
                (Some(2), vec![6]),
                (Some(1), vec![7])
            ]
        );
        assert_eq!(runs_of(&[8], vec![None]), vec![(None, vec![8])]);
        assert!(runs_of(&[], Vec::new()).is_empty());
    }
}
