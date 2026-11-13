// SPDX-License-Identifier: MIT OR Apache-2.0
//! Composition of the pivot download (`POST /erp/pivot/export`): the server's
//! [`WebPivotExport`] seam backed by the `web` addon's checked export. Addons cannot depend on the
//! server crate (layer check), so the adapter lives here and holds no state.
use erp_server::web::{PivotWorkbook, WebPivotExport};

/// The `web` addon's pivot export behind the server's route.
pub(crate) struct WebAddonPivotExport;

impl WebPivotExport for WebAddonPivotExport {
    fn request_limit<'a>(
        &'a self,
        tx: &'a mut erp_orm::Transaction,
    ) -> erp_orm::BoxFuture<'a, erp_fields::Result<usize>> {
        Box::pin(async move { Ok(erp_addon_web::pivot_export::limits(tx).await?.request_bytes) })
    }

    fn export<'a>(
        &'a self,
        tx: &'a mut erp_orm::Transaction,
        body: &'a [u8],
    ) -> erp_orm::BoxFuture<'a, erp_fields::Result<PivotWorkbook>> {
        use erp_addon_web::pivot_export as web;
        Box::pin(async move {
            web::require_internal_user(tx).await?;
            let limits = web::limits(tx).await?;
            // JSON parsing and the workbook are CPU-bound and sized by the caller's document: they run on
            // the blocking pool, never on the async executor. The request's own deadline still bounds the
            // whole call (the route runs inside it); a finished blocking job nobody awaits is dropped.
            let body = body.to_vec();
            let posted = blocking(move || web::read_posted(&body, &limits)).await?;
            web::authorize(tx, &posted).await?;
            let workbook = blocking(move || web::build(&posted, &limits)).await?;
            Ok(PivotWorkbook {
                file_name: workbook.file_name,
                bytes: workbook.bytes,
            })
        })
    }
}

async fn blocking<T: Send + 'static>(
    job: impl FnOnce() -> erp_fields::Result<T> + Send + 'static,
) -> erp_fields::Result<T> {
    tokio::task::spawn_blocking(job).await.map_err(|error| {
        erp_fields::Error::internal(format!("the pivot export task failed: {error}"))
    })?
}
