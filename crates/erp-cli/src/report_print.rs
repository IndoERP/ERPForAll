// SPDX-License-Identifier: MIT OR Apache-2.0
//! The report runtime of a served node (§11).
//!
//! `base` turns a report and its records into a document under the caller's rights
//! ([`erp_addon_base::report_print`]); this module lays the document out in the render worker
//! pool (`erp report-worker` processes) and hands the result on:
//!
//! - to the report route (`/erp/report/<pdf|html>/<report>/<ids>`, [`WebReport`]), which answers
//!   with the file;
//! - to the print jobs of `base` ([`ReportBackend`]): each chunk becomes one PDF attachment, one
//!   document per record so a failing record is listed and the others print, and the last chunk
//!   merges the parts into one PDF attachment the user downloads.
//!
//! The pool's settings are node configuration (`[report]`, read once at startup, each refused by
//! name outside its floor and bound); the pool itself starts on the first print, so a node
//! without a font store still starts and serves everything but printing.

use std::io::Cursor;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, PoisonError, RwLock};

use erp_addon_base::attachments::{self, AttachmentContent, NewAttachment, Uploader};
use erp_addon_base::report_job::{PrintableReport, RenderedChunk, ReportBackend};
use erp_addon_base::report_print::{self, PrintReport};
use erp_fields::{Error, Result};
use erp_orm::{BoxFuture, Orm, Transaction};
use erp_report::{DocTree, LayoutEngine, RenderResult, ReportError, render_html};
use erp_report_worker::{PoolSettings, Setting, WorkerCommand, WorkerPool};
use erp_server::config::ServerConfig;
use erp_server::web::{PrintedReport, ReportFormat, WebReport};
use tokio::io::AsyncReadExt;

/// Where the route sends a synchronous print.
const ROUTE: &str = "/erp/report/pdf";
/// The shipped font store's directory name in an addons path.
const FONT_ADDON: &str = "report_fonts";

/// The request ORM of the serving generation: the uploader of the print results stages its
/// bytes in a transaction of its own.
// shortcut: one ORM per process, replaced at each recomposition; upgrade when a node serves
// several databases.
static SERVE_ORM: RwLock<Option<Orm>> = RwLock::new(None);

/// Record the request ORM a recomposition built (the print jobs stage their PDFs with it).
pub(crate) fn serve_with(orm: &Orm) {
    *SERVE_ORM.write().unwrap_or_else(PoisonError::into_inner) = Some(orm.clone());
}

fn serve_orm() -> Result<Orm> {
    SERVE_ORM
        .read()
        .unwrap_or_else(PoisonError::into_inner)
        .clone()
        .ok_or_else(|| Error::internal("printing: the serving ORM is not composed yet"))
}

/// The render pool, started on the first document.
struct Pool {
    /// The checked settings, or why this node cannot print.
    settings: std::result::Result<PoolSettings, String>,
    started: Mutex<Option<Arc<WorkerPool>>>,
}

impl Pool {
    fn get(&self) -> Result<Arc<WorkerPool>> {
        let settings = self
            .settings
            .as_ref()
            .map_err(|why| Error::user(why.clone()))?;
        let mut started = self.started.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(pool) = started.as_ref() {
            return Ok(Arc::clone(pool));
        }
        let pool = Arc::new(WorkerPool::new(settings.clone()).map_err(|e| unavailable(&e))?);
        *started = Some(Arc::clone(&pool));
        Ok(pool)
    }

    fn render(&self, document: &DocTree) -> Result<RenderResult> {
        self.get()?.render(document).map_err(|e| layout_error(&e))
    }
}

fn unavailable(error: &ReportError) -> Error {
    Error::internal(format!(
        "Printing is not available on this server: the render workers cannot start ({error})."
    ))
}

/// A document's own failure (too long, too large) is the user's to fix, and so is a document the
/// worker frame or the worker refuses: printing it again fails the same way. Workers that do not
/// start are the server being unavailable; a worker that crashed or an export that failed is the
/// server's error.
fn layout_error(error: &ReportError) -> Error {
    match error {
        ReportError::Deadline { .. }
        | ReportError::DocumentLimit { .. }
        | ReportError::Limit { .. }
        | ReportError::Image { .. }
        | ReportError::Layout { .. } => {
            Error::validation(format!("The document could not be laid out: {error}"))
        }
        ReportError::Unsupported { .. }
        | ReportError::InvalidDocument { .. }
        | ReportError::WorkerRefused { .. } => {
            Error::user(format!("This document cannot be printed: {error}."))
        }
        ReportError::WorkerUnavailable { .. } => unavailable(error),
        other => Error::internal(format!("Printing failed on this server ({other}).")),
    }
}

/// The settings of `[report]`: the font store (the configured directory, else the
/// `report_fonts` directory of an addons path), the optional `report_fonts_cjk` package where
/// `cjk_font_directory` names it, and each named pool setting.
///
/// # Errors
///
/// `Validation` naming an unknown setting or a value outside its floor and bound (the node does
/// not start with them). A missing font store is not an error here: printing then fails with a
/// message that names `report.font_directory`.
fn pool_settings(config: &ServerConfig) -> Result<std::result::Result<PoolSettings, String>> {
    let report = config.report();
    let fonts: Option<PathBuf> = match &report.font_directory {
        Some(directory) => Some(directory.clone()),
        None => config
            .addons()
            .iter()
            .map(|path| path.join(FONT_ADDON))
            .find(|path| path.is_dir()),
    };
    let command = WorkerCommand::current_exe()
        .map_err(|e| Error::internal(format!("printing: the server binary is unknown: {e}")))?;
    // the bounds are checked even without fonts: a wrong value stops the node at startup
    let mut settings = PoolSettings::new(command, fonts.clone().unwrap_or_default());
    for (name, value) in &report.settings {
        let setting = Setting::from_name(name).ok_or_else(|| {
            let known: Vec<&str> = Setting::ALL.iter().map(|s| s.name()).collect();
            Error::validation(format!(
                "[report] {name} is not a report setting; the settings are font_directory, \
                 cjk_font_directory, {}",
                known.join(", ")
            ))
        })?;
        settings = settings
            .with(setting, *value)
            .map_err(|e| Error::validation(format!("[report] {e}")))?;
    }
    // The optional package is installed where it is configured; without it a document that names
    // one of its families is refused by name and every other document prints.
    let cjk = erp_report_typst::OptionalFonts::Cjk.package();
    if let Some(path) = &report.cjk_font_directory {
        if !path.is_dir() {
            return Ok(Err(format!(
                "Printing is not available on this server: the optional font package {cjk} {} \
                 is not a directory (report.cjk_font_directory).",
                path.display()
            )));
        }
        settings = settings.with_optional_fonts(cjk, path);
    }
    Ok(match fonts {
        Some(path) if path.is_dir() => Ok(settings),
        Some(path) => Err(format!(
            "Printing is not available on this server: the font store {} does not exist \
             (report.font_directory).",
            path.display()
        )),
        None => Err(format!(
            "Printing is not available on this server: no font store is configured \
             (report.font_directory) and no addons path has a {FONT_ADDON} directory."
        )),
    })
}

/// The node's report runtime: the route's printer and the print jobs' backend.
pub(crate) struct ReportRuntime {
    pool: Arc<Pool>,
}

impl ReportRuntime {
    /// Read `[report]` (no worker starts here) and install the runtime as the print jobs'
    /// backend of this process.
    ///
    /// # Errors
    ///
    /// As [`pool_settings`]; `Programming` when a backend is already installed.
    pub(crate) fn install(config: &ServerConfig) -> Result<Arc<Self>> {
        static INSTALLED: Mutex<Option<Arc<ReportRuntime>>> = Mutex::new(None);
        let mut installed = INSTALLED.lock().unwrap_or_else(PoisonError::into_inner);
        // shortcut: the print jobs' backend is process-global, so a second composition in the same
        // process (a recomposed generation, a test) shares the first runtime and its settings;
        // upgrade when one process serves nodes with different report settings.
        if let Some(runtime) = installed.as_ref() {
            return Ok(Arc::clone(runtime));
        }
        let runtime = Arc::new(Self {
            pool: Arc::new(Pool {
                settings: pool_settings(config)?,
                started: Mutex::new(None),
            }),
        });
        let backend: Arc<dyn ReportBackend> = Arc::clone(&runtime) as Arc<dyn ReportBackend>;
        erp_addon_base::report_job::install_backend(backend)?;
        *installed = Some(Arc::clone(&runtime));
        Ok(runtime)
    }

    /// Lay `document` out on the blocking pool (the workers are separate processes; the call
    /// waits for them).
    async fn layout(&self, document: DocTree) -> Result<RenderResult> {
        let pool = Arc::clone(&self.pool);
        tokio::task::spawn_blocking(move || pool.render(&document))
            .await
            .map_err(|e| Error::internal(format!("printing: the layout task failed: {e}")))?
    }

    /// One PDF of every record of `ids`, each laid out alone, in order; the failures by record.
    async fn render_each(
        &self,
        tx: &mut Transaction,
        report: &PrintReport,
        ids: &[i64],
    ) -> Result<(Vec<Vec<u8>>, Vec<(i64, String)>, Vec<String>)> {
        let mut parts = Vec::with_capacity(ids.len());
        let mut failures = Vec::new();
        let mut warnings = Vec::new();
        for id in ids {
            let one = std::slice::from_ref(id);
            let prepared = tx
                .savepoint(async |tx| report_print::prepare(tx, report, one).await)
                .await;
            let rendered = match prepared {
                Ok(document) => self.layout(document).await,
                Err(error) => Err(error),
            };
            match rendered {
                Ok(output) => {
                    warnings.extend(output.warnings().iter().cloned());
                    parts.push(output.into_bytes());
                }
                // a retry of the whole chunk would fail the same way: the queue retries only these
                Err(error) if error.kind() == erp_fields::ErrorKind::Concurrency => {
                    return Err(error);
                }
                Err(error) => failures.push((*id, error.message().to_owned())),
            }
        }
        Ok((parts, failures, warnings))
    }

    /// Keep `bytes` as a PDF attachment `name` of the caller, inside the job's transaction (which
    /// started before the bytes existed, so they are attached with [`Uploader::create`], not
    /// staged and then claimed).
    async fn keep(tx: &mut Transaction, job: i64, name: &str, bytes: Vec<u8>) -> Result<i64> {
        let length = u64::try_from(bytes.len()).unwrap_or(u64::MAX);
        let mut item = NewAttachment::new(name, AttachmentContent::Empty);
        item.declared_mimetype = Some("application/pdf".to_owned());
        item.description = Some(format!("Print job {job}"));
        let created = Uploader::new(&serve_orm()?)
            .create(tx, item, &mut Cursor::new(bytes), Some(length))
            .await?;
        Ok(created.get())
    }
}

/// `inputs` merged into one plain PDF, on the blocking pool.
async fn merge(inputs: Vec<Vec<u8>>) -> Result<Vec<u8>> {
    tokio::task::spawn_blocking(move || {
        let mut output = Vec::new();
        erp_pdf::merge(inputs, &erp_pdf::PdfLimits::default(), &mut |bytes| {
            output.extend_from_slice(bytes);
            Ok(())
        })
        .map_err(|e| {
            Error::validation(format!("The printed documents could not be merged: {e}"))
        })?;
        Ok(output)
    })
    .await
    .map_err(|e| Error::internal(format!("printing: the merge task failed: {e}")))?
}

impl ReportBackend for ReportRuntime {
    fn reports<'a>(
        &'a self,
        tx: &'a mut Transaction,
        model: &'a str,
    ) -> BoxFuture<'a, Result<Vec<PrintableReport>>> {
        Box::pin(async move {
            Ok(report_print::printable(tx, model)
                .await?
                .into_iter()
                .map(|(report, name)| PrintableReport { report, name })
                .collect())
        })
    }

    fn check<'a>(
        &'a self,
        tx: &'a mut Transaction,
        report: &'a str,
        model: &'a str,
        ids: &'a [i64],
    ) -> BoxFuture<'a, Result<PrintableReport>> {
        Box::pin(async move {
            let found = report_print::find(tx, report).await?;
            report_print::check_records(tx, &found, Some(model), ids).await?;
            Ok(PrintableReport {
                report: found.report_name,
                name: found.name,
            })
        })
    }

    fn download_url(&self, report: &str, ids: &[i64]) -> Result<String> {
        if !report
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'_' | b'.'))
        {
            return Err(Error::validation(format!(
                "The report name '{report}' cannot be printed through the download: it may hold \
                 only letters, digits, '_' and '.'."
            )));
        }
        let ids: Vec<String> = ids.iter().map(ToString::to_string).collect();
        Ok(format!("{ROUTE}/{report}/{}?download=1", ids.join(",")))
    }

    fn render<'a>(
        &'a self,
        tx: &'a mut Transaction,
        job: i64,
        report: &'a str,
        ids: &'a [i64],
    ) -> BoxFuture<'a, Result<RenderedChunk>> {
        Box::pin(async move {
            let found = report_print::find(tx, report).await?;
            let (parts, failures, warnings) = self.render_each(tx, &found, ids).await?;
            let part = if parts.is_empty() {
                None
            } else {
                let bytes = merge(parts).await?;
                Some(Self::keep(tx, job, &format!("print-job-{job}-part.pdf"), bytes).await?)
            };
            Ok(RenderedChunk {
                part,
                failures,
                warnings,
            })
        })
    }

    fn layout(&self, document: DocTree) -> BoxFuture<'_, Result<Vec<u8>>> {
        Box::pin(async move { Ok(ReportRuntime::layout(self, document).await?.into_bytes()) })
    }

    fn merge<'a>(
        &'a self,
        tx: &'a mut Transaction,
        job: i64,
        name: &'a str,
        parts: &'a [i64],
    ) -> BoxFuture<'a, Result<i64>> {
        Box::pin(async move {
            // shortcut: the parts are read whole before the merge (bounded by the job's record
            // limit and the attachment size limit); upgrade to a streamed merge when erp-pdf
            // takes readers.
            let mut inputs = Vec::with_capacity(parts.len());
            for part in parts {
                let mut read = attachments::open(tx, *part).await?.open(None).await?;
                let mut bytes = Vec::new();
                read.reader
                    .read_to_end(&mut bytes)
                    .await
                    .map_err(|e| Error::internal(format!("printing: part {part}: {e}")))?;
                inputs.push(bytes);
            }
            let merged = if inputs.len() == 1 {
                inputs.pop().unwrap_or_default()
            } else {
                merge(inputs).await?
            };
            let result = Self::keep(tx, job, name, merged).await?;
            // the parts are this job's own intermediate files
            tx.env()
                .sudo(true)
                .records("ir.attachment")?
                .browse(parts.iter().copied())?
                .unlink(tx)
                .await?;
            Ok(result)
        })
    }
}

impl WebReport for ReportRuntime {
    fn print<'a>(
        &'a self,
        tx: &'a mut Transaction,
        format: ReportFormat,
        report: &'a str,
        ids: &'a [i64],
    ) -> BoxFuture<'a, Result<PrintedReport>> {
        Box::pin(async move {
            erp_addon_base::report_job::check_direct_print(tx, ids).await?;
            let found = report_print::find(tx, report).await?;
            report_print::check_records(tx, &found, None, ids).await?;
            match format {
                ReportFormat::Pdf => {
                    // each company's records print as their own document, with that company's
                    // letterhead, and the documents are merged in the order given
                    let documents = report_print::prepare_runs(tx, &found, ids).await?;
                    let mut parts = Vec::with_capacity(documents.len());
                    for document in documents {
                        parts.push(self.layout(document).await?.into_bytes());
                    }
                    let bytes = if parts.len() == 1 {
                        parts.pop().unwrap_or_default()
                    } else {
                        merge(parts).await?
                    };
                    Ok(PrintedReport {
                        file_name: report_print::file_name(tx, &found, ids, "pdf").await?,
                        bytes,
                    })
                }
                ReportFormat::Html => {
                    let document = report_print::prepare(tx, &found, ids).await?;
                    Ok(PrintedReport {
                        file_name: report_print::file_name(tx, &found, ids, "html").await?,
                        // CPU work over up to the document limits: off the runtime thread.
                        bytes: tokio::task::spawn_blocking(move || render_html(&document, &[]))
                            .await
                            .map_err(|e| {
                                Error::internal(format!("printing: the HTML task failed: {e}"))
                            })?
                            .map_err(|e| {
                                Error::validation(format!("Report '{}': {e}", found.name))
                            })?
                            .into_bytes(),
                    })
                }
                other => Err(Error::programming(format!(
                    "the report route asked for {other:?}, which this node does not print"
                ))),
            }
        })
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, reason = "tests")]
    use super::*;
    use erp_fields::ErrorKind;
    use erp_report_typst::OptionalFonts;

    /// A document the frame or the worker refuses is the user's error and says what cannot be
    /// printed; only workers that do not start read as the server being unavailable
    /// (C33-FIX-PRINT-CELL: the journal entry's refusal was shown as a temporary outage).
    #[test]
    fn a_refused_document_is_a_user_error_and_a_start_failure_is_the_servers() {
        for (error, feature) in [
            (
                ReportError::Unsupported {
                    feature: "document node of a newer kind over the render worker protocol",
                    task: "a later release",
                },
                "document node of a newer kind",
            ),
            (
                ReportError::InvalidDocument {
                    reason: "table has more than 1000 columns",
                },
                "more than 1000 columns",
            ),
            (
                ReportError::WorkerRefused {
                    document: "account.report_move".into(),
                    message: "invalid report document: table cell spans past the last column"
                        .into(),
                },
                "spans past the last column",
            ),
        ] {
            let mapped = layout_error(&error);
            assert_eq!(mapped.kind(), ErrorKind::UserError, "{error}");
            assert!(
                mapped
                    .message()
                    .starts_with("This document cannot be printed: ")
                    && mapped.message().contains(feature),
                "{}",
                mapped.message()
            );
        }
        let start = layout_error(&ReportError::WorkerUnavailable {
            cause: "the worker exited at start".into(),
        });
        assert_eq!(start.kind(), ErrorKind::Internal);
        assert!(
            start.message().contains("cannot start"),
            "{}",
            start.message()
        );
        let failed = layout_error(&ReportError::Pdf {
            document: "account.report_move".into(),
            reason: "export".into(),
        });
        assert_eq!(failed.kind(), ErrorKind::Internal);
        assert!(
            !failed.message().contains("cannot start"),
            "{}",
            failed.message()
        );
    }

    /// the printing-unavailable message naming the package and the key, and an existing one is
    /// handed to the pool settings)
    #[test]
    fn a_missing_cjk_font_directory_is_named_by_package_and_key() {
        let dir = tempfile::tempdir().unwrap();
        for (name, value) in [
            ("runtime-url", "postgresql://r:x@127.0.0.1:5548/erp\n"),
            ("owner-url", "postgresql://o:x@127.0.0.1:5548/erp\n"),
            (
                "proxy-token",
                "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef\n",
            ),
        ] {
            let path = dir.path().join(name);
            std::fs::write(&path, value).unwrap();
            std::fs::set_permissions(&path, std::os::unix::fs::PermissionsExt::from_mode(0o600))
                .unwrap();
        }
        std::fs::create_dir(dir.path().join("fonts")).unwrap();
        let load = |report: &str| {
            let text = format!(
                "database='erp'\ndatabase_url_file='runtime-url'\nowner_database_url_file='owner-url'\n\
                 bind='127.0.0.1:18069'\npublic_origin='https://erp.example.test'\n\
                 proxy_mode='authenticated'\ntrusted_proxies=['127.0.0.1/32']\n\
                 proxy_token_file='proxy-token'\naddons_path=['addons']\nassets_manifest='m.json'\n\
                 [report]\nfont_directory='fonts'\n{report}"
            );
            let path = dir.path().join("server.toml");
            std::fs::write(&path, text).unwrap();
            ServerConfig::load(&path).unwrap()
        };
        let message = pool_settings(&load("cjk_font_directory='absent'\n"))
            .unwrap()
            .err()
            .unwrap();
        assert!(
            message.contains(OptionalFonts::Cjk.package())
                && message.contains("is not a directory (report.cjk_font_directory)"),
            "{message}"
        );
        std::fs::create_dir(dir.path().join("cjk")).unwrap();
        let settings = pool_settings(&load("cjk_font_directory='cjk'\n"))
            .unwrap()
            .unwrap();
        assert_eq!(settings.optional_fonts().len(), 1);
    }

    /// A PDF of `len` bytes that the type detection reads as `application/pdf`.
    fn pdf_of(len: usize) -> Vec<u8> {
        let mut bytes = b"%PDF-1.7\n%".to_vec();
        bytes.resize(len.saturating_sub(7), b'x');
        bytes.extend_from_slice(b"\n%%EOF\n");
        bytes
    }

    /// A print job keeps each PDF inside the job's own transaction, whose snapshot was taken by
    /// its first statement, before the bytes were staged. Staging and then claiming the upload in
    /// that transaction could not work: the upload row committed after the snapshot is invisible
    /// to the claim, so every queued print failed with "The upload is not usable".
    #[tokio::test]
    async fn keep_stores_the_pdf_in_the_jobs_older_transaction() {
        let store: Arc<dyn erp_sdk::blob::AttachmentStore> =
            Arc::new(erp_sdk::blob::MemoryStore::new());
        let runtime = Arc::new(erp_sdk::blob::AttachmentRuntime::single(
            "main",
            store,
            erp_sdk::blob::AttachmentLimits::default(),
        ));
        let h = erp_testkit::Harness::builder([
            &erp_test_models::base_users::ADDON,
            &erp_addon_base::ADDON,
        ])
        .attach(runtime)
        .unlink_hook(attachments::UNLINK_HOOK)
        .build()
        .await
        .unwrap();
        serve_with(h.orm());
        // above the 2 KiB inline threshold: the bytes go through the store
        let pdf = pdf_of(26 * 1024);
        let job = async || {
            let mut tx = h.begin().await.unwrap();
            // the job's first statement takes its snapshot, as run_chunk's read of the job does
            tx.query("SELECT count(*) FROM ir_blob_upload", &[])
                .await
                .unwrap();
            tx
        };
        // the first file ever stored registers the store after the snapshot: the queue retries
        let mut tx = job().await;
        let error = ReportRuntime::keep(&mut tx, 7, "print-job-7-part.pdf", pdf.clone())
            .await
            .unwrap_err();
        assert_eq!(
            error.kind(),
            erp_fields::ErrorKind::Concurrency,
            "{error:?}"
        );
        tx.rollback().await.unwrap();
        drop(tx);
        let mut tx = job().await;
        let id = ReportRuntime::keep(&mut tx, 7, "print-job-7-part.pdf", pdf.clone())
            .await
            .unwrap();
        let handle = attachments::open(&mut tx, id).await.unwrap();
        assert_eq!(handle.read_to_vec(64 * 1024).await.unwrap(), pdf);
        let rows = tx
            .query(
                "SELECT mimetype, description FROM ir_attachment WHERE id = $1",
                &[&id],
            )
            .await
            .unwrap();
        let (mimetype, description): (String, String) = (rows[0].get(0), rows[0].get(1));
        assert_eq!(
            (mimetype.as_str(), description.as_str()),
            ("application/pdf", "Print job 7")
        );
        tx.commit().await.unwrap();
        drop(tx);
        // the refused attempt staged nothing; the kept upload no longer waits for a claim: it is
        // left to the staging cleaner
        let mut tx = h.begin().await.unwrap();
        let rows = tx
            .query(
                "SELECT count(*), count(*) FILTER (WHERE state IN (0, 1) AND expires_at > now()) \
                 FROM ir_blob_upload",
                &[],
            )
            .await
            .unwrap();
        assert_eq!((rows[0].get::<_, i64>(0), rows[0].get::<_, i64>(1)), (1, 0));
        tx.rollback().await.unwrap();
    }
}
