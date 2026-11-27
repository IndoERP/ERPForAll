// SPDX-License-Identifier: MIT OR Apache-2.0
//! Browser list data exchange. Only the three upload routes accept a bounded binary file.
//! Each operation starts its own session transaction after the request has finished streaming.
use super::*;
use bytes::Bytes;
use erp_fields::Result as DomainResult;
use erp_orm::{BoxFuture, Context, Transaction};
use serde::Deserialize;
use serde_json::Value as Json;

const FILE_BYTES: u64 = 10 * 1024 * 1024;
const OPTIONS_BYTES: usize = 256 * 1024;
const FORM_OVERHEAD: u64 = 16 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImportMode {
    Preview,
    Test,
    Import,
}

#[derive(Debug, Clone, Copy)]
pub(super) enum Action {
    Upload(ImportMode),
    Template,
    Export,
}

impl Action {
    pub(super) fn from_path(path: &str) -> Option<Self> {
        Some(match path {
            routes::IMPEX_PREVIEW => Self::Upload(ImportMode::Preview),
            routes::IMPEX_TEST => Self::Upload(ImportMode::Test),
            routes::IMPEX_IMPORT => Self::Upload(ImportMode::Import),
            routes::IMPEX_TEMPLATE => Self::Template,
            routes::IMPEX_EXPORT => Self::Export,
            _ => return None,
        })
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExchangeUpload {
    pub model: String,
    pub format: String,
    pub sheet: Option<String>,
    pub separator: Option<char>,
    /// A null entry skips the file column at that position.
    pub fields: Option<Vec<Option<String>>>,
    #[serde(default)]
    context: serde_json::Map<String, Json>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExchangeColumn {
    pub name: String,
    pub label: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExchangeDownload {
    pub model: String,
    pub format: String,
    pub fields: Vec<ExchangeColumn>,
    pub ids: Option<Vec<i64>>,
    #[serde(default)]
    context: serde_json::Map<String, Json>,
}

/// A download produced as the current session user.
pub struct ExchangeFile {
    pub name: String,
    pub content_type: &'static str,
    pub bytes: Vec<u8>,
}

/// Adapter to the shared import reader, ORM loader/exporter, and spreadsheet writer.
pub trait WebDataExchange: Send + Sync {
    fn upload<'a>(
        &'a self,
        tx: &'a mut Transaction,
        mode: ImportMode,
        options: ExchangeUpload,
        file: Bytes,
    ) -> BoxFuture<'a, DomainResult<Json>>;

    fn download<'a>(
        &'a self,
        tx: &'a mut Transaction,
        template: bool,
        options: ExchangeDownload,
    ) -> BoxFuture<'a, DomainResult<ExchangeFile>>;
}

enum ExchangeRequest {
    Upload(ImportMode, ExchangeUpload, Bytes),
    Download {
        template: bool,
        options: ExchangeDownload,
    },
}

impl ExchangeRequest {
    fn context(&self) -> HttpResult<Context> {
        let map = match self {
            Self::Upload(_, options, _) => &options.context,
            Self::Download { options, .. } => &options.context,
        };
        let bytes = serde_json::to_vec(map).map_err(|_| internal())?;
        let value = erp_fields::json::parse(&bytes, 64).map_err(|e| bad(e.to_string()))?;
        erp_rpc::browser_context(&value).map_err(|e| bad(e.to_string()))
    }
}

fn form_error(error: multer::Error) -> Response {
    match error {
        multer::Error::FieldSizeExceeded { .. } | multer::Error::StreamSizeExceeded { .. } => {
            response(
                StatusCode::PAYLOAD_TOO_LARGE,
                "The import request exceeds its size limit. Files may contain up to 10 MiB and options up to 256 KiB.",
            )
        }
        _ => bad(format!("Invalid import form: {error}")),
    }
}

async fn read_upload(content_type: &str, body: Body) -> HttpResult<(ExchangeUpload, Bytes)> {
    let boundary = multer::parse_boundary(content_type).map_err(|_| {
        response(
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            "Expected multipart/form-data with a boundary.",
        )
    })?;
    let limits = multer::SizeLimit::new()
        .whole_stream(FILE_BYTES + OPTIONS_BYTES as u64 + FORM_OVERHEAD)
        .for_field("file", FILE_BYTES)
        .for_field("options", OPTIONS_BYTES as u64);
    let constraints = multer::Constraints::new()
        .allowed_fields(vec!["file", "options"])
        .size_limit(limits);
    let mut form =
        multer::Multipart::with_constraints(body.into_data_stream(), boundary, constraints);
    let (mut options, mut file) = (None, None);
    while let Some(field) = form.next_field().await.map_err(form_error)? {
        match field.name() {
            Some("file") if file.is_none() => file = Some(field.bytes().await.map_err(form_error)?),
            Some("options") if options.is_none() => {
                let bytes = field.bytes().await.map_err(form_error)?;
                options = Some(
                    serde_json::from_slice(&bytes)
                        .map_err(|e| bad(format!("Invalid import options: {e}")))?,
                );
            }
            _ => return Err(bad("Expected one options part and one file part.")),
        }
    }
    Ok((
        options.ok_or_else(|| bad("Missing import options."))?,
        file.ok_or_else(|| bad("Missing import file."))?,
    ))
}

impl WebApp {
    /// Supply the list data exchange adapter.
    pub fn with_data_exchange(mut self, adapter: Arc<dyn WebDataExchange>) -> Self {
        self.data_exchange = Some(adapter);
        self
    }

    pub(super) async fn data_exchange(
        &self,
        parts: &axum::http::request::Parts,
        body: Body,
        auth: &Arc<Auth>,
        action: Action,
    ) -> HttpResult<Response> {
        if parts.method != Method::POST {
            let mut err = response(StatusCode::METHOD_NOT_ALLOWED, "POST required");
            err.headers_mut()
                .insert(header::ALLOW, HeaderValue::from_static("POST"));
            return Err(err);
        }
        let headers = &parts.headers;
        boundary::same_origin_fetch(
            one(headers, "sec-fetch-site")?,
            one(headers, "origin")?,
            self.config.origin(),
        )
        .map_err(|e| response(StatusCode::FORBIDDEN, e.to_string()))?;
        if headers.contains_key(header::AUTHORIZATION) {
            return Err(response(
                StatusCode::UNAUTHORIZED,
                "Sign in with a browser session.",
            ));
        }
        let secret = self
            .session(headers)?
            .ok_or_else(|| response(StatusCode::UNAUTHORIZED, "User is not connected."))?;
        let token = one(headers, "x-csrf-token")?
            .ok_or_else(|| response(StatusCode::FORBIDDEN, "A CSRF token is required."))?;
        self.verify_csrf(headers, token).await?;
        let adapter = self.data_exchange.as_ref().ok_or_else(|| {
            response(
                StatusCode::SERVICE_UNAVAILABLE,
                "Data exchange composition is unavailable.",
            )
        })?;
        // Check the session before receiving files. Slow clients never hold a pooled connection.
        let mut initial = auth
            .begin_session(&secret, CredentialOptions::new())
            .await
            .map_err(auth_error)?;
        initial.tx.rollback().await.map_err(auth_error)?;
        drop(initial);
        let content_type = one(headers, "content-type")?.unwrap_or_default();
        let request = match action {
            Action::Upload(mode) => {
                let (options, file) = read_upload(content_type, body).await?;
                ExchangeRequest::Upload(mode, options, file)
            }
            Action::Export | Action::Template => {
                if content_type != "application/json" {
                    return Err(response(
                        StatusCode::UNSUPPORTED_MEDIA_TYPE,
                        "Expected application/json.",
                    ));
                }
                let bytes = to_bytes(body, OPTIONS_BYTES).await.map_err(|_| {
                    response(
                        StatusCode::PAYLOAD_TOO_LARGE,
                        "Download options exceed 256 KiB.",
                    )
                })?;
                let options = serde_json::from_slice(&bytes)
                    .map_err(|e| bad(format!("Invalid download options: {e}")))?;
                ExchangeRequest::Download {
                    template: matches!(action, Action::Template),
                    options,
                }
            }
        };
        let context = request.context()?;
        let mut begun = auth
            .begin_session(&secret, CredentialOptions::new().context(context))
            .await
            .map_err(auth_error)?;
        let env = begun.tx.env();
        if let Err(error) = env.companies() {
            return Err(self.rpc_error(error).await);
        }
        if let Some(lang) = env.lang().filter(|code| *code != "en_US")
            && let Err(error) = begun.tx.validate_lang_code(lang).await
        {
            return Err(self.rpc_error(error).await);
        }
        let tx = &mut begun.tx;
        let (mut reply, commit) = match request {
            ExchangeRequest::Upload(mode, options, file) => {
                let result = match adapter.upload(tx, mode, options, file).await {
                    Ok(result) => result,
                    Err(e) => return Err(self.rpc_error(e).await),
                };
                // Failed loads roll back. Test always rolls back the actual loader transaction.
                let commit = mode == ImportMode::Import && result["ids"].is_array();
                (json_response(StatusCode::OK, &result)?, commit)
            }
            ExchangeRequest::Download { template, options } => {
                let file = match adapter.download(tx, template, options).await {
                    Ok(file) => file,
                    Err(e) => return Err(self.rpc_error(e).await),
                };
                let mut reply = Response::new(Body::from(file.bytes));
                reply.headers_mut().insert(
                    header::CONTENT_TYPE,
                    HeaderValue::from_static(file.content_type),
                );
                reply.headers_mut().insert(
                    header::CONTENT_DISPOSITION,
                    HeaderValue::from_str(&content::disposition(false, &file.name))
                        .map_err(|_| internal())?,
                );
                // Export can create external IDs. Persist them only after generating the file.
                (reply, !template)
            }
        };
        if commit {
            tx.commit().await.map_err(auth_error)?;
        } else {
            tx.rollback().await.map_err(auth_error)?;
        }
        reply.headers_mut().insert(
            header::CACHE_CONTROL,
            HeaderValue::from_static("private, no-store"),
        );
        reply.headers_mut().insert(
            header::X_CONTENT_TYPE_OPTIONS,
            HeaderValue::from_static("nosniff"),
        );
        Ok(reply)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn multipart_file_limit_counts_file_bytes_separately_from_the_envelope() {
        fn form(size: usize) -> Body {
            let mut bytes = br#"--data
Content-Disposition: form-data; name="options"

{"model":"test.note","format":"csv"}
--data
Content-Disposition: form-data; name="file"; filename="file.csv"

"#
            .split(|b| *b == b'\n')
            .collect::<Vec<_>>()
            .join(&b"\r\n"[..]);
            bytes.resize(bytes.len() + size, b'x');
            bytes.extend_from_slice(b"\r\n--data--\r\n");
            Body::from(bytes)
        }
        const TYPE: &str = "multipart/form-data; boundary=data";
        let (_, bytes) = read_upload(TYPE, form(FILE_BYTES as usize)).await.unwrap();
        assert_eq!(bytes.len(), FILE_BYTES as usize);
        let err = read_upload(TYPE, form(FILE_BYTES as usize + 1))
            .await
            .unwrap_err();
        assert_eq!(err.status(), StatusCode::PAYLOAD_TOO_LARGE);
    }
}
