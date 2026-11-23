// SPDX-License-Identifier: MIT OR Apache-2.0
//! `ids` are comma-separated record ids; `?download=1` asks for an attachment instead of
//! an inline answer. The route owns the transport: the credential (a browser session, or an API
//! key for PDF), the path syntax, the headers. Finding the report, the access checks, the
//! document and its layout belong to the composition behind [`WebReport`] (addons cannot depend
//! on this crate: layer check).
use super::*;

/// Most record ids one report address takes before the database is asked; the composition behind
/// the seam then refuses more than `base.report_sync_max_records` and repeated ids.
const MAX_IDS: usize = 10_000;
/// Longest report reference in the path, in bytes.
const MAX_REPORT_BYTES: usize = 128;

/// What the route answers with.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum ReportFormat {
    /// The laid-out PDF.
    Pdf,
    /// The HTML preview of the same document.
    Html,
}

/// A printed report.
#[derive(Debug)]
pub struct PrintedReport {
    /// The name offered to the user, already cleaned by the composition (with its extension).
    pub file_name: String,
    /// The finished file.
    pub bytes: Vec<u8>,
}

/// Trusted composition adapter of the report route.
pub trait WebReport: Send + Sync {
    /// Find `report` (technical name, id or external id) as the caller may see it, check that the
    /// caller may read every record of `ids`, and print them. Refusals are `Missing`, `Access`
    /// and `Validation` naming what was refused.
    fn print<'a>(
        &'a self,
        tx: &'a mut erp_orm::Transaction,
        format: ReportFormat,
        report: &'a str,
        ids: &'a [i64],
    ) -> erp_orm::BoxFuture<'a, erp_fields::Result<PrintedReport>>;
}

/// `<pdf|html>/<report>/<ids>`: the format, the reference and the ids, or why not.
fn parse(rest: &str) -> Result<(ReportFormat, &str, Vec<i64>), String> {
    let mut parts = rest.split('/');
    let (Some(format), Some(report), Some(ids), None) =
        (parts.next(), parts.next(), parts.next(), parts.next())
    else {
        return Err("A report address is /erp/report/<pdf|html>/<report>/<ids>.".to_owned());
    };
    let format = match format {
        "pdf" => ReportFormat::Pdf,
        "html" => ReportFormat::Html,
        other => return Err(format!("'{other}' is not a report format: pdf or html.")),
    };
    if report.is_empty()
        || report.len() > MAX_REPORT_BYTES
        || !report
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'_' | b'.'))
    {
        return Err(format!(
            "A report reference is 1 to {MAX_REPORT_BYTES} letters, digits, '_' and '.'."
        ));
    }
    let ids = ids
        .split(',')
        .map(|id| id.parse::<i64>().ok().filter(|id| *id > 0))
        .collect::<Option<Vec<i64>>>()
        .ok_or_else(|| {
            "The record ids of a report address are positive integers separated by commas."
                .to_owned()
        })?;
    if ids.len() > MAX_IDS {
        return Err(format!(
            "A report address takes at most {MAX_IDS} record ids."
        ));
    }
    Ok((format, report, ids))
}

impl WebApp {
    /// Supply the report route's composition. Without it the route answers `503` naming the
    /// missing composition.
    pub fn with_report(mut self, report: Arc<dyn WebReport>) -> Self {
        self.report = Some(report);
        self
    }

    /// `GET /erp/report/<pdf|html>/<report>/<ids>`, with a browser session or, for PDF, an API
    /// key (`Authorization: Bearer`). Both open the caller's own request transaction and hand it
    /// to the same composition, so the report's groups, read access on every id and the id limits
    /// are checked the same way.
    pub(super) async fn report(
        &self,
        parts: &axum::http::request::Parts,
        rest: &str,
        client: IpAddr,
        auth: &Arc<Auth>,
    ) -> HttpResult<Response> {
        if parts.method != Method::GET {
            let mut refused = response(StatusCode::METHOD_NOT_ALLOWED, "GET required");
            refused
                .headers_mut()
                .insert(header::ALLOW, HeaderValue::from_static("GET"));
            return Err(refused);
        }
        let (format, report, ids) =
            parse(rest).map_err(|reason| response(StatusCode::NOT_FOUND, reason))?;
        let download = matches!(parts.uri.query(), Some("download=1" | "download=true"));
        let headers = &parts.headers;
        let bearer = headers.contains_key(header::AUTHORIZATION);
        if bearer && format != ReportFormat::Pdf {
            // the HTML preview is a page for a browser tab, which has the session
            return Err(response(
                StatusCode::FORBIDDEN,
                "API keys print PDF only; the HTML preview needs a browser session.",
            ));
        }
        let printer = self.report.as_ref().ok_or_else(|| {
            response(
                StatusCode::SERVICE_UNAVAILABLE,
                "report printing composition is unavailable",
            )
        })?;
        let request = PrintRequest {
            printer: printer.as_ref(),
            format,
            report,
            ids: &ids,
        };
        let (printed, drop_cookie) = if bearer {
            self.print_with_key(headers, client, auth, request).await?
        } else {
            (
                self.print_with_session(headers, auth, request).await?,
                false,
            )
        };
        let content_type = match format {
            ReportFormat::Pdf => "application/pdf",
            ReportFormat::Html => "text/html; charset=utf-8",
        };
        let mut result = Response::new(Body::from(printed.bytes));
        let out = result.headers_mut();
        out.insert(header::CONTENT_TYPE, HeaderValue::from_static(content_type));
        out.insert(
            header::CONTENT_DISPOSITION,
            HeaderValue::from_str(&content::disposition(!download, &printed.file_name))
                .map_err(|_| internal())?,
        );
        out.insert(
            header::CACHE_CONTROL,
            HeaderValue::from_static("private, no-store"),
        );
        out.insert(
            header::X_CONTENT_TYPE_OPTIONS,
            HeaderValue::from_static("nosniff"),
        );
        if format == ReportFormat::Html {
            // the preview is our own escaped markup with a fixed stylesheet: no script runs in it
            out.insert(
                header::CONTENT_SECURITY_POLICY,
                HeaderValue::from_static(
                    "default-src 'none'; img-src data:; style-src 'unsafe-inline'; \
                     base-uri 'none'; form-action 'none'; frame-ancestors 'self'",
                ),
            );
        }
        if drop_cookie {
            self.set_cookie(&mut result, "session_id", "", 0)?;
        }
        Ok(result)
    }

    /// Print in the transaction of the browser session.
    async fn print_with_session(
        &self,
        headers: &HeaderMap,
        auth: &Arc<Auth>,
        request: PrintRequest<'_>,
    ) -> HttpResult<PrintedReport> {
        let secret = self
            .session(headers)?
            .ok_or_else(|| response(StatusCode::UNAUTHORIZED, "user is not connected"))?;
        let mut begun = auth
            .begin_session(&secret, CredentialOptions::new())
            .await
            .map_err(auth_error)?;
        let printed = request.print(&mut begun.tx).await;
        // printing reads only: nothing of the transaction is kept
        let rolled_back = begun.tx.rollback().await;
        drop(begun);
        let printed = match printed {
            Ok(printed) => printed,
            Err(error) => return Err(self.rpc_error(error).await),
        };
        rolled_back.map_err(auth_error)?;
        Ok(printed)
    }

    /// Print in the transaction of the presented API key, checked as on JSON-2: the address
    /// throttle, a foreign `Origin` refused, the `rpc` audience, a cookie sent alongside must be
    /// the key's user. Answers whether an invalid cookie sent alongside is to be cleared.
    async fn print_with_key(
        &self,
        headers: &HeaderMap,
        client: IpAddr,
        auth: &Arc<Auth>,
        request: PrintRequest<'_>,
    ) -> HttpResult<(PrintedReport, bool)> {
        let credential = self.bearer_credential(headers, client, auth).await?;
        let mut tx = self.open_bearer(&credential).await?;
        let printed = if printable_with(tx.env().principal().scope()) {
            request.print(&mut tx).await
        } else {
            Err(erp_fields::Error::access(
                "This API key is limited to some models or methods; it cannot print reports.",
            ))
        };
        let rolled_back = tx.rollback().await;
        // The transaction has ended: dropping the credential below may now start the key's
        // last-use write without contending with this request.
        drop(tx);
        let drop_cookie = credential.drop_cookie.load(Ordering::Relaxed);
        drop(credential);
        let printed = match printed {
            Ok(printed) => printed,
            Err(error) => {
                let mut refused = self.rpc_error(error).await;
                if drop_cookie {
                    self.set_cookie(&mut refused, "session_id", "", 0)?;
                }
                return Err(refused);
            }
        };
        rolled_back.map_err(auth_error)?;
        Ok((printed, drop_cookie))
    }
}

/// What one report request prints, handed to the transaction the credential opened.
#[derive(Clone, Copy)]
struct PrintRequest<'a> {
    printer: &'a dyn WebReport,
    format: ReportFormat,
    report: &'a str,
    ids: &'a [i64],
}
impl PrintRequest<'_> {
    async fn print(self, tx: &mut erp_orm::Transaction) -> erp_fields::Result<PrintedReport> {
        // shortcut: the request transaction stays open while the document is laid out; upgrade
        // to a prepare step and a render step when long layouts hold too many connections.
        self.printer
            .print(tx, self.format, self.report, self.ids)
            .await
    }
}

/// Whether a key of `scope` may print. A report reads records outside the RPC dispatch, where a
/// key's model and method allowlists are enforced, so a key limited by either is refused rather
/// than allowed past its list. Unrestricted keys (every user key today) and sessions print.
fn printable_with(scope: Option<&erp_orm::Scope>) -> bool {
    scope.is_none_or(|scope| {
        matches!(scope.models(), erp_orm::ModelAllow::All)
            && matches!(scope.methods(), erp_orm::MethodAllow::All)
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "tests")]
mod tests {
    use super::*;

    #[test]
    fn report_addresses_parse_or_name_the_refusal() {
        let (format, report, ids) = parse("pdf/sample_library.report_book/3,4").unwrap();
        assert_eq!(format, ReportFormat::Pdf);
        assert_eq!(report, "sample_library.report_book");
        assert_eq!(ids, vec![3, 4]);
        assert_eq!(parse("html/12/1").unwrap().0, ReportFormat::Html);
        assert!(
            parse("xlsx/a/1")
                .unwrap_err()
                .contains("not a report format")
        );
        assert!(parse("pdf/a/0").unwrap_err().contains("positive integers"));
        assert!(
            parse("pdf/a/1,x")
                .unwrap_err()
                .contains("positive integers")
        );
        assert!(parse("pdf/a b/1").unwrap_err().contains("report reference"));
        assert!(parse("pdf/a").unwrap_err().contains("/erp/report/"));
        assert!(parse("pdf/a/1/2").unwrap_err().contains("/erp/report/"));
    }

    #[test]
    fn only_unrestricted_keys_print() {
        let scope = |models, methods| {
            erp_orm::Scope::builder(erp_orm::ApiKeyId::new(1).unwrap(), "device")
                .models(models)
                .methods(methods)
                .build()
                .unwrap()
        };
        let all = scope(erp_orm::ModelAllow::All, erp_orm::MethodAllow::All);
        assert!(printable_with(None));
        assert!(printable_with(Some(&all)));
        let models = scope(
            erp_orm::ModelAllow::named(["res.partner"]).unwrap(),
            erp_orm::MethodAllow::All,
        );
        assert!(!printable_with(Some(&models)));
        let methods = scope(
            erp_orm::ModelAllow::All,
            erp_orm::MethodAllow::named(["read"]).unwrap(),
        );
        assert!(!printable_with(Some(&methods)));
    }
}
