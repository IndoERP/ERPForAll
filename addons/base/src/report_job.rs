// SPDX-License-Identifier: MIT OR Apache-2.0
//! Printing reports, few records or many.
//!
//! The web client asks [`ReportJob::erp_print`] to print a report for some records. Up to
//! `base.report_sync_max_records` records print at once: the answer is the address of the
//! synchronous download, which streams the PDF. More become a `report.job` row and a background
//! job on the durable queue. Each chunk of the job renders
//! `base.report_job_chunk_records` records in its own transaction, so a restart continues at the
//! next chunk; a document that fails is listed with its reason and the others still print; the
//! last chunk merges the parts into one plain PDF attachment, which the user downloads through
//! the streaming content route `/erp/content/<id>`. The client reads the row for progress.
//!
//! Rendering, the synchronous route and the merge are not done here: a [`ReportBackend`] that the
//! report runtime installs does them and writes its output into attachments, never into this
//! process's memory as a whole document (`ir.actions.report` and its routes are slice,
//! the worker runner that renders and merges is). Without a backend every print request
//! fails with an error that names those slices.

use std::sync::{Arc, OnceLock};

use erp_sdk::jobs::{EnqueueRequest, HandlerRef, JobKindDecl, StepArgs, StepOutcome};
use erp_sdk::prelude::*;
use erp_sdk::report::DocTree;
use erp_sdk::serde_json::{self, Value as Json, json};
use erp_sdk::{BoxFuture, ErrorKind};

/// The model name.
pub const MODEL: &str = "report.job";
/// The job kind of a background print.
pub const KIND: &str = "base.report_print";
/// Most records printed synchronously; more go to a background job.
pub const SYNC_MAX_KEY: &str = "base.report_sync_max_records";
/// Records one chunk of a background print renders.
pub const CHUNK_KEY: &str = "base.report_job_chunk_records";
/// Most records one print request takes.
pub const JOB_MAX_KEY: &str = "base.report_job_max_records";
/// How often the client reads the progress of a background print (milliseconds).
pub const POLL_KEY: &str = "base.report_job_poll_ms";
/// Pixels per inch a printed picture is embedded at; a picture with more is scaled down.
pub const IMAGE_DPI_KEY: &str = "base.report_image_dpi";
/// Quality a picture is encoded at when it is scaled down or converted to a JPEG.
pub const IMAGE_JPEG_QUALITY_KEY: &str = "base.report_image_jpeg_quality";
const PICTURE_DPI: (u16, u16, u16) = erp_sdk::report::PictureQuality::DPI;
const PICTURE_JPEG: (u8, u8, u8) = erp_sdk::report::PictureQuality::JPEG_QUALITY;
/// The six parameters: key, label, default, floor, hard bound (declared in
/// `system_parameters.rs`). The two picture values take theirs from the document model, so a
/// stored value is always one a document accepts.
pub(crate) const PARAMETERS: [(&str, &str, i64, i64, i64); 6] = [
    (
        SYNC_MAX_KEY,
        "Printing: most records printed at once (more print in the background)",
        50,
        1,
        500,
    ),
    (
        CHUNK_KEY,
        "Printing: records per background chunk",
        20,
        1,
        500,
    ),
    (
        JOB_MAX_KEY,
        "Printing: most records in one print",
        10_000,
        1,
        100_000,
    ),
    (
        POLL_KEY,
        "Printing: progress refresh of a background print (milliseconds)",
        1_000,
        250,
        60_000,
    ),
    (
        IMAGE_DPI_KEY,
        "Printing: resolution of pictures (pixels per inch; larger pictures are scaled down)",
        PICTURE_DPI.0 as i64,
        PICTURE_DPI.1 as i64,
        PICTURE_DPI.2 as i64,
    ),
    (
        IMAGE_JPEG_QUALITY_KEY,
        "Printing: JPEG quality of scaled-down pictures",
        PICTURE_JPEG.0 as i64,
        PICTURE_JPEG.1 as i64,
        PICTURE_JPEG.2 as i64,
    ),
];
/// Whether a printed document is shown in the page (the document viewer) or saved as a file.
pub const PREVIEW_KEY: &str = "base.report_print_preview";
/// Label of [`PREVIEW_KEY`] (declared in `system_parameters.rs`).
pub(crate) const PREVIEW_LABEL: &str =
    "Printing: show the printed document in the page instead of saving it as a file";
/// The reference client saves a printed document as a file; a database that has not chosen does
/// the same.
pub(crate) const PREVIEW_DEFAULT: bool = false;
/// Failures and warnings a job keeps listed (the counts stay exact): a storage bound of the row,
/// not a policy value.
pub const MAX_LISTED: usize = 100;
/// Longest stored reason of one failure or warning (bytes).
pub const MAX_REASON_BYTES: usize = 512;
/// Longest report or model name a request takes (bytes).
const MAX_NAME_BYTES: usize = 128;

const NO_BACKEND: &str = "Printing is not available on this server: no report renderer is \
                          installed. The report runtime (ir.actions.report, slice a later release) and \
                          its worker runner (a later release) provide it.";

/// A report that can print records of a model.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PrintableReport {
    /// The technical report name (`ir.actions.report.report_name`).
    pub report: String,
    /// The name shown in the Print menu and used for the file name.
    pub name: String,
}

/// What one chunk of a background print produced.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RenderedChunk {
    /// The attachment holding this chunk's PDF; `None` when every document of the chunk failed.
    pub part: Option<i64>,
    /// The records that could not be printed, each with the reason the user is shown. A
    /// document that hits the render deadline belongs here: it is not retried.
    pub failures: Vec<(i64, String)>,
    /// Warnings about degraded content (for example a terms field printed without an image).
    pub warnings: Vec<String>,
}

/// The report runtime this module drives. Every call runs in the caller's transaction and with
/// the caller's rights; output goes into attachments, streamed, never returned as bytes.
pub trait ReportBackend: Send + Sync + 'static {
    /// The reports bound to `model` that the caller may print.
    fn reports<'a>(
        &'a self,
        tx: &'a mut Transaction,
        model: &'a str,
    ) -> BoxFuture<'a, Result<Vec<PrintableReport>>>;

    /// Refuse unless `report` exists, prints `model` and the caller may read every record.
    fn check<'a>(
        &'a self,
        tx: &'a mut Transaction,
        report: &'a str,
        model: &'a str,
        ids: &'a [i64],
    ) -> BoxFuture<'a, Result<PrintableReport>>;

    /// The address of the synchronous, streamed download of `ids`.
    ///
    /// # Errors
    /// A report or id list the route cannot address.
    fn download_url(&self, report: &str, ids: &[i64]) -> Result<String>;

    /// Render `ids` (one chunk of print job `job`) into one part attachment of that job.
    fn render<'a>(
        &'a self,
        tx: &'a mut Transaction,
        job: i64,
        report: &'a str,
        ids: &'a [i64],
    ) -> BoxFuture<'a, Result<RenderedChunk>>;

    /// Lay a finished `document` out as one PDF, in memory: the dynamic report export builds its
    /// own document and has no `ir.actions.report` record. Bounded by the renderer's own limits.
    ///
    /// # Errors
    /// The renderer's refusals; by default a backend that cannot do it says so.
    fn layout(&self, document: DocTree) -> BoxFuture<'_, Result<Vec<u8>>> {
        drop(document);
        Box::pin(async { Err(Error::user(NO_BACKEND)) })
    }

    /// Merge the parts, in order, into one plain PDF attachment named `name` on job `job`;
    /// returns its id. The parts are the backend's to keep or delete.
    fn merge<'a>(
        &'a self,
        tx: &'a mut Transaction,
        job: i64,
        name: &'a str,
        parts: &'a [i64],
    ) -> BoxFuture<'a, Result<i64>>;
}

// shortcut: one backend per process, not per database registry; upgrade when two report runtimes
// must coexist in one server.
static BACKEND: OnceLock<Arc<dyn ReportBackend>> = OnceLock::new();

/// Install the report runtime (once per process, by the binary that composes it).
///
/// # Errors
/// `Programming` when a backend is already installed.
pub fn install_backend(backend: Arc<dyn ReportBackend>) -> Result<()> {
    BACKEND
        .set(backend)
        .map_err(|_| Error::programming("A report backend is already installed in this process."))
}

fn backend() -> Result<&'static dyn ReportBackend> {
    BACKEND
        .get()
        .map(AsRef::as_ref)
        .ok_or_else(|| Error::user(NO_BACKEND))
}

/// Lay `document` out as a PDF with the installed report runtime (no record, no attachment).
///
/// # Errors
/// `User` when no report runtime is installed; the renderer's refusals.
pub async fn layout(document: DocTree) -> Result<Vec<u8>> {
    backend()?.layout(document).await
}

/// Where a print request goes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Dispatch {
    /// Printed at once through the synchronous download.
    Sync,
    /// Printed by a background job.
    Job,
}

/// Decide where a request for `count` records goes.
///
/// # Errors
/// `Validation` for no record or more than `job_max`, naming the parameter.
pub fn plan(count: usize, sync_max: i64, job_max: i64) -> Result<Dispatch> {
    let count_i = i64::try_from(count).unwrap_or(i64::MAX);
    if count == 0 {
        return Err(Error::validation("Select at least one record to print."));
    }
    if count_i > job_max {
        return Err(Error::validation(format!(
            "{count} records were selected, more than the {job_max} one print takes \
             ({JOB_MAX_KEY}). Select fewer records or ask an administrator to raise the parameter."
        )));
    }
    Ok(if count_i <= sync_max {
        Dispatch::Sync
    } else {
        Dispatch::Job
    })
}

fn check_name(what: &str, value: &str) -> Result<()> {
    if value.is_empty() || value.len() > MAX_NAME_BYTES {
        return Err(Error::validation(format!(
            "The {what} name must be 1 to {MAX_NAME_BYTES} bytes."
        )));
    }
    Ok(())
}

/// The checks a direct report address (`/erp/report/...`) shares with the Print menu: saved, distinct
/// records and no more than the synchronous limit, so the address cannot print more than the menu
/// would print at once (review cycle 21, security M1).
pub async fn check_direct_print(tx: &mut Transaction, ids: &[i64]) -> Result<()> {
    check_ids(ids)?;
    let sync_max = param(tx, SYNC_MAX_KEY).await?;
    if i64::try_from(ids.len()).unwrap_or(i64::MAX) > sync_max {
        return Err(Error::validation(format!(
            "This address prints {} records; at most {sync_max} print at once ({SYNC_MAX_KEY}). \
             Print more from the Print menu, which prints them in the background.",
            ids.len()
        )));
    }
    Ok(())
}

fn check_ids(ids: &[i64]) -> Result<()> {
    let mut seen = std::collections::BTreeSet::new();
    for id in ids {
        if *id <= 0 {
            return Err(Error::validation(format!(
                "{id} is not a record id: a print takes saved records only."
            )));
        }
        if !seen.insert(*id) {
            return Err(Error::validation(format!(
                "Record {id} is listed twice in the print request."
            )));
        }
    }
    Ok(())
}

/// `text` cut to [`MAX_REASON_BYTES`] at a character boundary.
fn bounded(text: &str) -> String {
    let mut end = text.len().min(MAX_REASON_BYTES);
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    text[..end].to_owned()
}

fn stored(text: Option<&str>, field: &str) -> Result<Json> {
    match text {
        None | Some("") => Ok(Json::Array(Vec::new())),
        Some(text) => serde_json::from_str(text)
            .map_err(|e| Error::invariant(format!("report.job {field} is not valid JSON: {e}"))),
    }
}

fn corrupt(field: &str) -> Error {
    Error::invariant(format!("report.job {field} is not the list it should be"))
}

fn ids_of(text: Option<&str>, field: &str) -> Result<Vec<i64>> {
    let value = stored(text, field)?;
    value
        .as_array()
        .ok_or_else(|| corrupt(field))?
        .iter()
        .map(|id| id.as_i64().ok_or_else(|| corrupt(field)))
        .collect()
}

fn texts_of(text: Option<&str>, field: &str) -> Result<Vec<String>> {
    let value = stored(text, field)?;
    value
        .as_array()
        .ok_or_else(|| corrupt(field))?
        .iter()
        .map(|item| {
            item.as_str()
                .map(str::to_owned)
                .ok_or_else(|| corrupt(field))
        })
        .collect()
}

fn failures_of(text: Option<&str>) -> Result<Vec<Failure>> {
    let value = stored(text, "failures")?;
    value
        .as_array()
        .ok_or_else(|| corrupt("failures"))?
        .iter()
        .map(|item| match (item["id"].as_i64(), item["error"].as_str()) {
            (Some(id), Some(error)) => Ok(Failure {
                id,
                error: error.to_owned(),
            }),
            _ => Err(corrupt("failures")),
        })
        .collect()
}

fn to_text(value: &Json) -> Result<Option<Arc<str>>> {
    serde_json::to_string(value)
        .map(|text| Some(Arc::from(text)))
        .map_err(|e| Error::internal(format!("report.job value: {e}")))
}

fn some(text: &str) -> Option<Arc<str>> {
    Some(Arc::from(text))
}

/// A listed failure of a background print.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Failure {
    /// The record.
    pub id: i64,
    /// Why it was not printed.
    pub error: String,
}

/// The progress of one chunk folded into what the job stored.
#[derive(Debug, Default, PartialEq, Eq)]
struct Tally {
    failures: Vec<Failure>,
    failed: usize,
    warnings: Vec<String>,
    parts: Vec<i64>,
}

impl Tally {
    fn add(&mut self, chunk: RenderedChunk) {
        self.failed += chunk.failures.len();
        for (id, error) in chunk.failures {
            if self.failures.len() < MAX_LISTED {
                self.failures.push(Failure {
                    id,
                    error: bounded(&error),
                });
            }
        }
        for warning in chunk.warnings {
            let warning = bounded(&warning);
            if self.warnings.len() < MAX_LISTED && !self.warnings.contains(&warning) {
                self.warnings.push(warning);
            }
        }
        self.parts.extend(chunk.part);
    }

    fn failures_json(&self) -> Json {
        Json::Array(
            self.failures
                .iter()
                .map(|f| json!({"id": f.id, "error": f.error}))
                .collect(),
        )
    }
}

/// The records an elevated set reads and writes: a user has read access to the own rows only and
/// never writes them; only the request and the job do, for that user.
fn elevated(this: &RecordSet<ReportJob>) -> RecordSet<ReportJob> {
    this.sudo(true)
}

/// A background print: what is printed, for whom, how far it got and the merged result.
#[model(
    name = "report.job",
    description = "Print Job",
    order = "id desc",
    company_access = unrestricted(reason = "a print job belongs to the user who asked for it; the records it prints keep their own company rules, checked when they are rendered")
)]
pub struct ReportJob {
    /// The file name of the result.
    #[field(char, required = true, string = "Name")]
    pub name: Option<Arc<str>>,
    /// The user who asked for the print; the job runs as that user.
    #[field(
        many2one,
        comodel = "res.users",
        required = true,
        ondelete = "cascade",
        index = true,
        string = "User"
    )]
    pub user_id: Option<RecId>,
    /// The technical report name.
    #[field(char, required = true, string = "Report")]
    pub report_name: Option<Arc<str>>,
    /// The model of the printed records.
    #[field(char, required = true, string = "Model")]
    pub res_model: Option<Arc<str>>,
    /// The printed record ids, in print order (a JSON list).
    #[field(text, required = true, string = "Records")]
    pub res_ids: Option<Arc<str>>,
    /// `queued`, `running`, then `done` or `failed`.
    #[field(
        selection,
        selection = [("queued", "Queued"), ("running", "Printing"), ("done", "Done"), ("failed", "Failed")],
        required = true,
        default = "queued",
        string = "State"
    )]
    pub state: Option<Arc<str>>,
    /// Records in the print.
    #[field(integer, string = "Records to Print")]
    pub total: i32,
    /// Records handled so far, printed or failed.
    #[field(integer, string = "Records Handled")]
    pub processed: i32,
    /// Records that could not be printed.
    #[field(integer, string = "Records Failed")]
    pub failed: i32,
    /// The first [`MAX_LISTED`] failures (a JSON list of `{id, error}`).
    #[field(text, string = "Failures")]
    pub failures: Option<Arc<str>>,
    /// The first [`MAX_LISTED`] distinct warnings (a JSON list of text).
    #[field(text, string = "Warnings")]
    pub warnings: Option<Arc<str>>,
    /// The part attachments rendered so far (a JSON list of ids); internal.
    #[field(text, string = "Parts", groups = "base.group_system")]
    pub parts: Option<Arc<str>>,
    /// Why the whole print failed.
    #[field(text, string = "Error")]
    pub error: Option<Arc<str>>,
    /// The merged PDF.
    #[field(
        many2one,
        comodel = "ir.attachment",
        ondelete = "set null",
        string = "Result"
    )]
    pub attachment_id: Option<RecId>,
}

erp_sdk::jobs::register_job_kind!(
    PRINT_JOB_KIND,
    JobKindDecl::new(
        "base",
        KIND,
        HandlerRef::Step {
            model: MODEL,
            method: "run_chunk",
        },
    )
    .max_attempts(3)
);

/// The `open` value of a print answer: what the client does with the printed document.
fn open_as(preview: bool) -> &'static str {
    if preview { "preview" } else { "download" }
}

async fn param(tx: &mut Transaction, key: &str) -> Result<i64> {
    erp_sdk::params::get_int(tx, &crate::ADDON, key).await
}

/// Run `work` in a savepoint; a failure other than a concurrency error (which the queue retries)
/// comes back as its message, so the job can record it instead of retrying forever.
async fn attempt<T, F>(tx: &mut Transaction, work: F) -> Result<std::result::Result<T, String>>
where
    F: AsyncFnOnce(&mut Transaction) -> Result<T>,
{
    match tx.savepoint(work).await {
        Ok(value) => Ok(Ok(value)),
        Err(error) if error.kind() == ErrorKind::Concurrency => Err(error),
        Err(error) => Ok(Err(bounded(error.message()))),
    }
}

#[methods(ReportJob, addon = "base")]
impl ReportJob {
    /// The reports the caller may print for `res_model`, for the Print menu: a list of
    /// `{report, name}`. Without a report runtime no report exists, so the list is empty.
    #[method]
    #[rpc(effect = read, model)]
    async fn erp_print_reports(
        _this: &RecordSet<ReportJob>,
        tx: &mut Transaction,
        res_model: String,
    ) -> Result<Value> {
        check_name("model", &res_model)?;
        let Some(backend) = BACKEND.get() else {
            return Ok(Value::Json(Arc::new(json!([]))));
        };
        let reports = backend.reports(tx, &res_model).await?;
        let list: Vec<Json> = reports
            .into_iter()
            .map(|r| json!({"report": r.report, "name": r.name}))
            .collect();
        Ok(Value::Json(Arc::new(Json::Array(list))))
    }

    /// Print `report` for `res_ids` of `res_model`. Every answer carries `open`: `"preview"` when
    /// `base.report_print_preview` is on (the client shows the document in its viewer), else
    /// `"download"`. Answers `{mode: "sync", url}` for a small
    /// request, or `{mode: "job", id, poll_ms}` for a background print whose `report.job` row
    /// the caller reads for progress.
    ///
    /// # Errors
    /// `Validation` for bad names or ids and more records than `base.report_job_max_records`;
    /// `UserError` without a report runtime; the backend's refusal (unknown report, no read
    /// access).
    #[method]
    #[rpc(effect = write, model)]
    async fn erp_print(
        this: &RecordSet<ReportJob>,
        tx: &mut Transaction,
        report: String,
        res_model: String,
        res_ids: Vec<i64>,
    ) -> Result<Value> {
        check_name("report", &report)?;
        check_name("model", &res_model)?;
        check_ids(&res_ids)?;
        let sync_max = param(tx, SYNC_MAX_KEY).await?;
        let job_max = param(tx, JOB_MAX_KEY).await?;
        let dispatch = plan(res_ids.len(), sync_max, job_max)?;
        let backend = backend()?;
        let printable = backend.check(tx, &report, &res_model, &res_ids).await?;
        let open = open_as(erp_sdk::params::get_bool(tx, &crate::ADDON, PREVIEW_KEY).await?);
        let answer = match dispatch {
            Dispatch::Sync => {
                json!({"mode": "sync", "url": backend.download_url(&report, &res_ids)?, "open": open})
            }
            Dispatch::Job => {
                let user = RecId::from_real(tx.env().uid())
                    .ok_or_else(|| Error::access("A background print needs a signed-in user."))?;
                let total = i32::try_from(res_ids.len())
                    .map_err(|_| Error::validation("Too many records in one print."))?;
                let vals = this
                    .vals()
                    .set(
                        ReportJob::NAME,
                        Some(Arc::from(format!("{}.pdf", printable.name))),
                    )
                    .set(ReportJob::USER_ID, Some(user))
                    .set(ReportJob::REPORT_NAME, some(&report))
                    .set(ReportJob::RES_MODEL, some(&res_model))
                    .set(ReportJob::RES_IDS, to_text(&json!(res_ids))?)
                    .set(ReportJob::TOTAL, total);
                let created = elevated(this).create(tx, vec![vals]).await?;
                let id = *created
                    .ids()
                    .first()
                    .ok_or_else(|| Error::internal("the print job was not created"))?;
                let env = tx.env();
                tx.enqueue(
                    &env,
                    EnqueueRequest::new(KIND).payload(&json!({ "id": id }))?,
                )
                .await?;
                json!({"mode": "job", "id": id, "poll_ms": param(tx, POLL_KEY).await?, "open": open})
            }
        };
        Ok(Value::Json(Arc::new(answer)))
    }

    /// One chunk of a background print (job kind [`KIND`]), as the user who asked for it.
    ///
    /// # Errors
    /// `Invariant` for a payload that is not `{id}` or a job that runs as another user; a
    /// concurrency error (the queue retries the chunk). Any other failure of the backend ends the
    /// print as `failed` with its message, without a retry.
    #[method]
    async fn run_chunk(
        this: &RecordSet<ReportJob>,
        tx: &mut Transaction,
        args: StepArgs,
    ) -> Result<StepOutcome> {
        let Some(id) = args.payload["id"].as_i64() else {
            return Err(Error::invariant("the print job payload is not {id}"));
        };
        let job = elevated(this)
            .search(tx, &Domain::condition("id", Operator::Eq, id)?)
            .await?;
        if job.is_empty() {
            return Ok(StepOutcome::done(json!({"outcome": "gone"}), 0));
        }
        let owner = job
            .get(tx, ReportJob::USER_ID)
            .await?
            .map(|user| user.get());
        if owner != Some(tx.env().uid()) {
            return Err(Error::invariant(format!(
                "print job {id} belongs to user {owner:?} but runs as user {}",
                tx.env().uid()
            )));
        }
        let state = job.get(tx, ReportJob::STATE).await?;
        if matches!(state.as_deref(), Some("done" | "failed")) {
            return Ok(StepOutcome::done(json!({"outcome": "finished"}), 0));
        }
        let ids = ids_of(job.get(tx, ReportJob::RES_IDS).await?.as_deref(), "res_ids")?;
        let report = job
            .get(tx, ReportJob::REPORT_NAME)
            .await?
            .unwrap_or_default();
        let name = job.get(tx, ReportJob::NAME).await?.unwrap_or_default();
        let start = usize::try_from(job.get(tx, ReportJob::PROCESSED).await?).unwrap_or(0);
        let mut tally = Tally {
            failures: failures_of(job.get(tx, ReportJob::FAILURES).await?.as_deref())?,
            failed: usize::try_from(job.get(tx, ReportJob::FAILED).await?).unwrap_or(0),
            warnings: texts_of(
                job.get(tx, ReportJob::WARNINGS).await?.as_deref(),
                "warnings",
            )?,
            parts: ids_of(job.get(tx, ReportJob::PARTS).await?.as_deref(), "parts")?,
        };
        let chunk = usize::try_from(param(tx, CHUNK_KEY).await?)
            .unwrap_or(1)
            .max(1);
        let end = ids.len().min(start.saturating_add(chunk));
        let backend = backend()?;
        let slice = ids.get(start..end).unwrap_or_default();
        let rendered = attempt(tx, async |tx| backend.render(tx, id, &report, slice).await).await?;
        let mut vals = job.vals().set(ReportJob::STATE, some("running"));
        let finished = match rendered {
            Err(error) => {
                vals = vals
                    .set(ReportJob::STATE, some("failed"))
                    .set(ReportJob::ERROR, Some(Arc::from(error)));
                true
            }
            Ok(chunk) => {
                tally.add(chunk);
                if end < ids.len() {
                    false
                } else if tally.parts.is_empty() {
                    let error = format!("None of the {} documents could be printed.", ids.len());
                    vals = vals
                        .set(ReportJob::STATE, some("failed"))
                        .set(ReportJob::ERROR, Some(Arc::from(error)));
                    true
                } else {
                    let parts = tally.parts.clone();
                    match attempt(tx, async |tx| backend.merge(tx, id, &name, &parts).await).await?
                    {
                        Ok(attachment) => {
                            vals = vals
                                .set(ReportJob::STATE, some("done"))
                                .set(ReportJob::ATTACHMENT_ID, RecId::from_real(attachment));
                        }
                        Err(error) => {
                            vals = vals
                                .set(ReportJob::STATE, some("failed"))
                                .set(ReportJob::ERROR, Some(Arc::from(error)));
                        }
                    }
                    true
                }
            }
        };
        let to_i32 = |n: usize| i32::try_from(n).unwrap_or(i32::MAX);
        vals = vals
            .set(ReportJob::PROCESSED, to_i32(end))
            .set(ReportJob::FAILED, to_i32(tally.failed))
            .set(ReportJob::FAILURES, to_text(&tally.failures_json())?)
            .set(ReportJob::WARNINGS, to_text(&json!(tally.warnings))?)
            .set(ReportJob::PARTS, to_text(&json!(tally.parts))?);
        job.write(tx, vals).await?;
        let done = u64::try_from(end - start).unwrap_or(0);
        Ok(if finished {
            StepOutcome::done(json!({ "id": id }), done)
        } else {
            let left = u64::try_from(ids.len() - end).unwrap_or(0);
            StepOutcome::more(json!({ "id": id, "processed": end }), done, Some(left))
        })
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "tests")]
mod tests {
    use super::*;

    #[test]
    fn plan_sends_up_to_the_threshold_sync_and_more_to_a_job() {
        assert_eq!(plan(1, 50, 10_000).unwrap(), Dispatch::Sync);
        assert_eq!(plan(50, 50, 10_000).unwrap(), Dispatch::Sync);
        assert_eq!(plan(51, 50, 10_000).unwrap(), Dispatch::Job);
        assert_eq!(
            plan(0, 50, 10_000).unwrap_err().message(),
            "Select at least one record to print."
        );
        let over = plan(10_001, 50, 10_000).unwrap_err();
        assert_eq!(over.kind(), ErrorKind::Validation);
        assert!(over.message().contains(JOB_MAX_KEY), "{}", over.message());
    }

    #[test]
    fn ids_must_be_saved_and_distinct() {
        assert!(check_ids(&[1, 2, 3]).is_ok());
        assert!(check_ids(&[0]).is_err());
        assert_eq!(
            check_ids(&[4, 4]).unwrap_err().message(),
            "Record 4 is listed twice in the print request."
        );
    }

    #[test]
    fn the_tally_counts_every_failure_and_lists_a_bounded_few() {
        let mut tally = Tally::default();
        let many: Vec<(i64, String)> = (1..=150).map(|id| (id, "x".repeat(600))).collect();
        tally.add(RenderedChunk {
            part: Some(9),
            failures: many,
            warnings: vec!["w".into(), "w".into()],
        });
        tally.add(RenderedChunk {
            part: None,
            ..RenderedChunk::default()
        });
        assert_eq!(tally.failed, 150);
        assert_eq!(tally.failures.len(), MAX_LISTED);
        assert_eq!(tally.failures[0].error.len(), MAX_REASON_BYTES);
        assert_eq!(tally.warnings, vec!["w".to_owned()]);
        assert_eq!(tally.parts, vec![9]);
    }

    /// and bound of the document model, so every value they can hold is one a document accepts;
    /// the refusal of a value outside them at save time is the parameter registry's)
    #[test]
    fn the_picture_parameters_take_their_bounds_from_the_document_model() {
        let declared = |key: &str| {
            PARAMETERS
                .iter()
                .find(|(declared, ..)| *declared == key)
                .map(|(_, _, default, floor, bound)| (*default, *floor, *bound))
        };
        assert_eq!(declared(IMAGE_DPI_KEY), Some((200, 72, 600)));
        assert_eq!(declared(IMAGE_JPEG_QUALITY_KEY), Some((85, 50, 100)));
        for (dpi, quality) in [(72, 50), (200, 85), (600, 100)] {
            let set = erp_sdk::report::PictureQuality::new(dpi, quality).unwrap();
            assert_eq!((set.dpi(), set.jpeg_quality()), (dpi, quality));
        }
    }

    #[test]
    fn the_print_answer_names_how_the_document_opens() {
        assert_eq!(open_as(PREVIEW_DEFAULT), "download");
        assert_eq!(open_as(true), "preview");
    }

    #[test]
    fn every_parameter_default_lies_inside_its_bounds() {
        for (key, _, default, floor, hard) in PARAMETERS {
            assert!(floor >= 1 && floor <= default && default <= hard, "{key}");
        }
    }
}
