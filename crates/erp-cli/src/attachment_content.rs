// SPDX-License-Identifier: MIT OR Apache-2.0
//! Composition of the attachment content route (`/erp/content/<id>`): the server's
//! [`WebAttachmentContent`] seam backed by the base addon's checked read
//! ([`erp_addon_base::attachments::content`]). Addons cannot depend on the server crate (
//! layer check), so the adapter lives here and holds no state: the attachment store comes from the
//! ORM generation of the transaction the route hands in.
use erp_addon_base::attachments::content::{
    self, ContentLookup as BaseLookup, ContentOpenError, RouteContent,
};
use erp_sdk::blob::ByteRange;
use erp_server::web::{
    ContentLookup, ContentOpenFailure, ContentOpener, ContentRange, ContentReader, FoundContent,
    WebAttachmentContent,
};

/// The base addon's content read behind the server's content route.
pub(crate) struct BaseAttachmentContent;

impl erp_server::web::WebAttachmentPolicy for BaseAttachmentContent {
    fn load<'a>(
        &'a self,
        tx: &'a mut erp_orm::Transaction,
    ) -> erp_orm::BoxFuture<'a, erp_fields::Result<erp_views::wire::AttachmentUploadLimits>> {
        Box::pin(async move {
            let policy = erp_addon_base::attachments::upload_policy(tx).await?;
            erp_views::wire::AttachmentUploadLimits::new(
                policy.max_file_bytes(),
                policy.inline_max_bytes(),
            )
            .map_err(erp_fields::Error::programming)
        })
    }
}

impl WebAttachmentContent for BaseAttachmentContent {
    fn lookup<'a>(
        &'a self,
        tx: &'a mut erp_orm::Transaction,
        id: i64,
    ) -> erp_orm::BoxFuture<'a, erp_fields::Result<ContentLookup>> {
        Box::pin(async move {
            match content::lookup_content(tx, id).await? {
                BaseLookup::Found(found) => Ok(ContentLookup::Found(FoundContent::new(
                    found.attachment_id(),
                    found.name().to_owned(),
                    found.mimetype().to_owned(),
                    found.size(),
                    found.digest().to_owned(),
                    Box::new(Opener(found)),
                ))),
                BaseLookup::NotFound => Ok(ContentLookup::NotFound),
                BaseLookup::Missing { attachment, digest } => {
                    Ok(ContentLookup::Missing { attachment, digest })
                }
                // a new outcome of the base read must be mapped here, never guessed
                other => Err(erp_fields::Error::programming(format!(
                    "the content route does not know the attachment lookup outcome {other:?}"
                ))),
            }
        })
    }
}

struct Opener(RouteContent);

impl ContentOpener for Opener {
    fn open(
        self: Box<Self>,
        range: Option<ContentRange>,
    ) -> erp_orm::BoxFuture<'static, Result<ContentReader, ContentOpenFailure>> {
        Box::pin(async move {
            let range = range.map(|range| ByteRange::new(range.start(), range.len()));
            self.0.open(range).await.map_err(|error| match error {
                ContentOpenError::Busy { .. } => ContentOpenFailure::Busy,
                ContentOpenError::Missing => ContentOpenFailure::Missing,
                ContentOpenError::Failed(error) => ContentOpenFailure::Failed(error),
                other => ContentOpenFailure::Failed(erp_fields::Error::internal(format!(
                    "unclassified attachment open failure: {other:?}"
                ))),
            })
        })
    }
}
