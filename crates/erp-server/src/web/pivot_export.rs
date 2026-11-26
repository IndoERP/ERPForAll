// SPDX-License-Identifier: MIT OR Apache-2.0
//! `POST /erp/pivot/export`: the pivot view's table, as the browser
//! drew it, answered as an `.xlsx` file. The route owns the transport: a browser session, the same
//! origin, a JSON body bounded by the declared setting, the download headers. The document, its
//! bounds, the caller's read access to its model and the workbook belong to the `web` addon behind
//! [`WebPivotExport`] (addons cannot depend on this crate: layer check).
use super::*;
use std::sync::atomic::{AtomicUsize, Ordering};

/// Holds an export slot across buffering, authorization and blocking workbook construction.
pub struct PivotExportPermit(Arc<AtomicUsize>);

impl PivotExportPermit {
    fn acquire(active: &Arc<AtomicUsize>, limit: usize) -> Option<Arc<Self>> {
        active
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |count| {
                (count < limit).then_some(count + 1)
            })
            .ok()?;
        Some(Arc::new(Self(Arc::clone(active))))
    }
}

impl Drop for PivotExportPermit {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::AcqRel);
    }
}

/// The workbook a download answers with.
#[derive(Debug)]
pub struct PivotWorkbook {
    /// The name offered to the user, already sanitized by the addon (ends in `.xlsx`).
    pub file_name: String,
    /// The finished file.
    pub bytes: Vec<u8>,
}

/// Trusted composition adapter of the pivot download (the `web` addon's checked export).
pub trait WebPivotExport: Send + Sync {
    /// The maximum request bytes and concurrent jobs, read in the
    /// caller's session transaction (the route rolls it back afterwards).
    fn request_limits<'a>(
        &'a self,
        tx: &'a mut erp_orm::Transaction,
    ) -> erp_orm::BoxFuture<'a, erp_fields::Result<(usize, usize)>>;
    /// Check the caller's read access to the document's model, bound and parse the document, write
    /// the workbook. Refusals are `Validation` (names the path or the setting) and `Access`.
    fn export<'a>(
        &'a self,
        tx: &'a mut erp_orm::Transaction,
        body: &'a [u8],
        permit: Arc<PivotExportPermit>,
    ) -> erp_orm::BoxFuture<'a, erp_fields::Result<PivotWorkbook>>;
}

const XLSX: &str = "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet";

impl WebApp {
    /// Supply the pivot download's composition. Without it the route answers `503` naming the
    /// missing composition.
    pub fn with_pivot_export(mut self, export: Arc<dyn WebPivotExport>) -> Self {
        self.pivot_export = Some(export);
        self
    }

    /// `POST /erp/pivot/export`.
    pub(super) async fn pivot_export(
        &self,
        parts: &axum::http::request::Parts,
        body: Body,
        auth: &Arc<Auth>,
    ) -> HttpResult<Response> {
        let headers = &parts.headers;
        if parts.method != Method::POST {
            let mut refused = response(StatusCode::METHOD_NOT_ALLOWED, "POST required");
            refused
                .headers_mut()
                .insert(header::ALLOW, HeaderValue::from_static("POST"));
            return Err(refused);
        }
        boundary::same_origin_fetch(
            one(headers, "sec-fetch-site")?,
            one(headers, "origin")?,
            self.config.origin(),
        )
        .map_err(|error| response(StatusCode::FORBIDDEN, error.to_string()))?;
        if one(headers, "content-type")? != Some("application/json") {
            return Err(response(
                StatusCode::UNSUPPORTED_MEDIA_TYPE,
                "expected application/json",
            ));
        }
        if headers.contains_key(header::AUTHORIZATION) {
            return Err(response(
                StatusCode::UNAUTHORIZED,
                "Bearer credentials are not accepted on the pivot download; sign in with a browser session.",
            ));
        }
        let exporter = self.pivot_export.as_ref().ok_or_else(|| {
            response(
                StatusCode::SERVICE_UNAVAILABLE,
                "pivot download composition is unavailable",
            )
        })?;
        let secret = self
            .session(headers)?
            .ok_or_else(|| response(StatusCode::UNAUTHORIZED, "user is not connected"))?;
        // The bound is a setting, read before the body is buffered; no connection is held while
        // the body arrives.
        let mut begun = auth
            .begin_session(&secret, CredentialOptions::new())
            .await
            .map_err(auth_error)?;
        let limits = exporter.request_limits(&mut begun.tx).await;
        let rolled_back = begun.tx.rollback().await;
        drop(begun);
        let (limit, concurrent) = match limits {
            Ok(limit) => limit,
            Err(error) => return Err(self.rpc_error(error).await),
        };
        rolled_back.map_err(auth_error)?;
        let permit = PivotExportPermit::acquire(&self.active_pivot_exports, concurrent).ok_or_else(|| {
            response(StatusCode::TOO_MANY_REQUESTS, "The simultaneous pivot download limit (web.pivot_export_max_concurrent) has been reached. Try again after a download finishes.")
        })?;
        let bytes = to_bytes(body, limit).await.map_err(|_| {
            response(
                StatusCode::PAYLOAD_TOO_LARGE,
                format!(
                    "The pivot download is larger than {limit} bytes, the limit set by web.pivot_export_max_request_bytes. Filter the table further or ask an administrator to raise the setting."
                ),
            )
        })?;
        let mut begun = auth
            .begin_session(&secret, CredentialOptions::new())
            .await
            .map_err(auth_error)?;
        let written = exporter.export(&mut begun.tx, &bytes, permit).await;
        let rolled_back = begun.tx.rollback().await;
        drop(begun);
        let workbook = match written {
            Ok(workbook) => workbook,
            Err(error) => return Err(self.rpc_error(error).await),
        };
        rolled_back.map_err(auth_error)?;
        let mut result = Response::new(Body::from(workbook.bytes));
        let out = result.headers_mut();
        out.insert(header::CONTENT_TYPE, HeaderValue::from_static(XLSX));
        out.insert(
            header::CONTENT_DISPOSITION,
            HeaderValue::from_str(&content::disposition(false, &workbook.file_name))
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
        Ok(result)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn export_slots_follow_the_live_limit_and_outlive_cancelled_waiters() {
        let active = Arc::new(AtomicUsize::new(0));
        let first = PivotExportPermit::acquire(&active, 1).unwrap();
        let worker = first.clone();
        drop(first);
        assert!(PivotExportPermit::acquire(&active, 1).is_none());
        let second = PivotExportPermit::acquire(&active, 2).unwrap();
        drop(worker);
        assert!(PivotExportPermit::acquire(&active, 1).is_none());
        drop(second);
        assert!(PivotExportPermit::acquire(&active, 1).is_some());
    }
}
