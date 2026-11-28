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
    ContentHandoff, ContentHandoffRequest, ContentLookup, ContentOpenFailure, ContentOpener,
    ContentRange, ContentReader, FoundContent, StagedUploadSummary, UploadAdmission, UploadReader,
    UploadSpec, WebAttachmentContent, WebAttachmentUpload,
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

impl WebAttachmentUpload for BaseAttachmentContent {
    fn admit<'a>(
        &'a self,
        tx: &'a mut erp_orm::Transaction,
    ) -> erp_orm::BoxFuture<'a, erp_fields::Result<UploadAdmission>> {
        Box::pin(async move {
            let env = tx.env();
            env.records("ir.attachment")?
                .check_access(tx, erp_orm::Operation::Create)
                .await?;
            let policy = erp_addon_base::attachments::upload_policy(tx).await?;
            Ok(UploadAdmission {
                max_file_bytes: policy.max_file_bytes(),
            })
        })
    }

    fn stage<'a>(
        &'a self,
        orm: &'a erp_orm::Orm,
        env: &'a erp_orm::Env,
        body: UploadReader<'a>,
        spec: UploadSpec,
    ) -> erp_orm::BoxFuture<'a, erp_fields::Result<StagedUploadSummary>> {
        Box::pin(async move {
            let mut request = erp_addon_base::attachments::StageRequest::named(spec.name)
                .with_file_limit(spec.file_limit);
            if let Some(mimetype) = spec.declared_mimetype {
                request = request.declared(mimetype);
            }
            let staged = erp_addon_base::attachments::Uploader::new(orm)
                .stage(env, body, request)
                .await?;
            Ok(StagedUploadSummary {
                upload_id: staged.id.as_str().to_owned(),
                size: staged.size,
                checksum: staged.checksum,
                mimetype: staged.mimetype,
            })
        })
    }

    fn discard<'a>(
        &'a self,
        orm: &'a erp_orm::Orm,
        env: &'a erp_orm::Env,
        upload_id: &'a str,
    ) -> erp_orm::BoxFuture<'a, ()> {
        Box::pin(async move {
            if let Ok(id) = erp_addon_base::attachments::UploadId::parse(upload_id) {
                erp_addon_base::attachments::Uploader::new(orm)
                    .discard(
                        env,
                        &id,
                        "HTTP upload rejected after staging; fenced cleanup pending.",
                    )
                    .await;
            }
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
            match content::lookup_route(tx, id).await? {
                BaseLookup::Url(url) => Ok(ContentLookup::Url(url)),
                BaseLookup::Found(found) => {
                    let sha256 = *found.sha256();
                    Ok(ContentLookup::Found(
                        FoundContent::new(
                            found.attachment_id(),
                            found.name().to_owned(),
                            found.mimetype().to_owned(),
                            found.size(),
                            found.digest().to_owned(),
                            Box::new(Opener(found)),
                        )
                        .with_sha256(sha256),
                    ))
                }
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

    fn lookup_field<'a>(
        &'a self,
        tx: &'a mut erp_orm::Transaction,
        model: &'a str,
        id: i64,
        field: &'a str,
        headers_only: bool,
    ) -> erp_orm::BoxFuture<'a, erp_fields::Result<ContentLookup>> {
        Box::pin(async move {
            if !content::field_model_allowed(tx, model).await? {
                return Ok(ContentLookup::NotFound);
            }
            let env = tx.env();
            let Some(spec) = env
                .registry()
                .model(model)
                .and_then(|model| model.field(field))
            else {
                return Ok(ContentLookup::NotFound);
            };
            if !matches!(spec.ty, erp_fields::FieldType::Binary { .. }) || !spec.store {
                return Ok(ContentLookup::NotFound);
            }
            let records = env.records(model)?.browse([id])?;
            let rows = match records
                .read_content_binary(tx, field, FIELD_BYTE_LIMIT, headers_only)
                .await
            {
                Ok(rows) => rows,
                Err(error)
                    if matches!(
                        error.kind(),
                        erp_fields::ErrorKind::Access | erp_fields::ErrorKind::Missing
                    ) =>
                {
                    return Ok(ContentLookup::NotFound);
                }
                Err(error) => return Err(error),
            };
            let Some(row) = rows else {
                return Ok(ContentLookup::NotFound);
            };
            if row.size > FIELD_BYTE_LIMIT {
                return Ok(ContentLookup::TooLarge);
            }
            let Some(bytes) = row.bytes else {
                return Ok(ContentLookup::NotFound);
            };
            let digest = row
                .digest
                .unwrap_or_else(|| erp_sdk::blob::digests(&bytes).0);
            let head = &bytes[..bytes.len().min(erp_sdk::blob::DETECT_BYTES)];
            let mimetype = erp_sdk::blob::detect(head, None).mimetype;
            let found = FoundContent::new(
                id,
                format!("{model}-{id}-{field}"),
                mimetype,
                row.size as u64,
                erp_sdk::blob::to_hex(&digest),
                Box::new(FieldBytes(bytes)),
            )
            .with_sha256(digest);
            Ok(ContentLookup::Found(found))
        })
    }
    fn lookup_shared<'a>(
        &'a self,
        orm: &'a erp_orm::Orm,
        id: i64,
        token: Option<&'a str>,
    ) -> erp_orm::BoxFuture<'a, erp_fields::Result<Option<(i64, ContentLookup)>>> {
        use erp_addon_base::attachments::grants;
        use erp_orm::{BeginOptions, Context, SUPERUSER_ID, UserId};
        Box::pin(async move {
            // This bootstrap transaction may resolve only the publication's subject and
            // company. All record and field reads take place in the caller transaction below.
            let mut meta = orm
                .begin_opts(
                    BeginOptions::new(SUPERUSER_ID)
                        .on_primary()
                        .read_only("attachment grant metadata"),
                )
                .await?;
            let principal = grants::resolve(&mut meta, id, token).await;
            meta.rollback().await?;
            drop(meta);
            let Some((uid, company)) = principal? else {
                return Ok(None);
            };
            let context = Context::from_pairs([(
                "allowed_company_ids",
                erp_fields::Value::list([erp_fields::Value::Int(company)]),
            )]);
            let options = BeginOptions::caller(UserId::new(uid)?)?
                .context(context)
                .on_primary()
                .read_only("shared attachment content");
            let mut tx = match orm.begin_opts(options).await {
                Ok(tx) => tx,
                Err(error)
                    if matches!(
                        error.kind(),
                        erp_fields::ErrorKind::Access
                            | erp_fields::ErrorKind::AccessDenied
                            | erp_fields::ErrorKind::Missing
                    ) =>
                {
                    return Ok(None);
                }
                Err(error) => return Err(error),
            };
            let result = async {
                if grants::resolve(&mut tx, id, token).await? != Some((uid, company))
                    || tx.env().company()?.map(|c| c.get()) != Some(company)
                    || !grants::can_redeem(&mut tx, id).await?
                {
                    return Ok(None);
                }
                self.lookup(&mut tx, id)
                    .await
                    .map(|found| Some((uid, found)))
            }
            .await;
            tx.rollback().await?;
            match result {
                Err(error)
                    if matches!(
                        error.kind(),
                        erp_fields::ErrorKind::Access
                            | erp_fields::ErrorKind::AccessDenied
                            | erp_fields::ErrorKind::Missing
                    ) =>
                {
                    Ok(None)
                }
                other => other,
            }
        })
    }
}

struct Opener(RouteContent);

fn open_failure(error: ContentOpenError) -> ContentOpenFailure {
    match error {
        ContentOpenError::Busy { .. } => ContentOpenFailure::Busy,
        ContentOpenError::Missing => ContentOpenFailure::Missing,
        ContentOpenError::Failed(error) => ContentOpenFailure::Failed(error),
        other => ContentOpenFailure::Failed(erp_fields::Error::internal(format!(
            "unclassified attachment open failure: {other:?}"
        ))),
    }
}

impl ContentOpener for Opener {
    fn handoff<'a>(
        &'a self,
        request: ContentHandoffRequest<'a>,
    ) -> erp_orm::BoxFuture<'a, Result<Option<ContentHandoff>, ContentOpenFailure>> {
        use erp_sdk::blob::DownloadHandoff;
        Box::pin(async move {
            let handoff = self
                .0
                .download_handoff(
                    request.media_type,
                    request.attachment_disposition,
                    request.allow_presign,
                )
                .await
                .map_err(open_failure)?;
            match handoff {
                None => Ok(None),
                Some(DownloadHandoff::InternalProxy(path)) => {
                    Ok(Some(ContentHandoff::InternalProxy(path)))
                }
                Some(DownloadHandoff::Presigned(url)) => Ok(Some(ContentHandoff::Presigned(url))),
                Some(_) => Err(ContentOpenFailure::Failed(erp_fields::Error::invariant(
                    "unknown attachment handoff",
                ))),
            }
        })
    }

    fn open(
        self: Box<Self>,
        range: Option<ContentRange>,
    ) -> erp_orm::BoxFuture<'static, Result<ContentReader, ContentOpenFailure>> {
        Box::pin(async move {
            let range = range.map(|range| ByteRange::new(range.start(), range.len()));
            self.0.open(range).await.map_err(open_failure)
        })
    }
}

// Direct bytea fields are materialized in the authorized snapshot. Larger files use the
// attachment store and its streaming opener.
const FIELD_BYTE_LIMIT: usize = 8 * 1024 * 1024;

struct FieldBytes(std::sync::Arc<[u8]>);

impl ContentOpener for FieldBytes {
    fn open(
        self: Box<Self>,
        range: Option<ContentRange>,
    ) -> erp_orm::BoxFuture<'static, Result<ContentReader, ContentOpenFailure>> {
        let mut reader = std::io::Cursor::new(self.0);
        if let Some(range) = range {
            reader.set_position(range.start());
        }
        Box::pin(async move { Ok(Box::pin(reader) as ContentReader) })
    }
}
