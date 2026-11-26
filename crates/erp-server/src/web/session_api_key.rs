//! `POST /erp/session/api-key`: a native client exchanges a password for a device API key.
//!
//! The password goes through the same [`Auth::authenticate`] as the web login form, so the size
//! bounds, the pre-filter, the cooldowns and the dummy verify apply unchanged. The browser session
//! that login creates only opens the user's transaction for the issuance and is ended before
//! the answer, whatever the outcome; no cookie is set. The key expires at the longest lifetime
//! the user's groups allow, and its secret leaves only after the commit succeeded.
use super::*;
use erp_fields::{DateTime, ErrorKind};
use erp_orm::{CorrelationId, OperationIdentity, audit::AuditOrigin};
use erp_security::auth::{
    Login, Mfa, apikey_manage,
    apikey_store::{IssueRequest, IssuedSecret, KeyKind, issue},
};
use zeroize::{Zeroize, Zeroizing};

/// The largest accepted body, as for the login form.
const BODY_LIMIT: usize = 16 * 1024;
/// Maximum byte length of `device_name`, matching [`IssueRequest`].
const MAX_DEVICE_NAME_BYTES: usize = 128;
/// One answer for a wrong password, an unknown login and an archived user.
const DENIED: &str = "Wrong login/password";
/// The answer when the login needs a second factor (or left a session waiting for one).
pub(super) const SECOND_FACTOR: &str =
    "This account requires a second authentication step; API key sign-in is not available for it";
const INVALID_BODY: &str = "invalid JSON body: unknown, duplicate or missing field";
const INVALID_DEVICE_NAME: &str =
    "device_name must be nonempty after trimming and at most 128 bytes";
const CROSS_ORIGIN: &str = "cross-origin request refused";
const BUSY: &str = "Server busy; retry later.";

/// The request body. Neither `Debug` nor `Clone`; credentials are wiped when it is dropped.
#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct Exchange {
    #[serde(default)]
    audience: Audience,
    db: String,
    login: String,
    password: String,
    device_name: String,
}

#[derive(Clone, Copy, Default, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
enum Audience {
    #[default]
    Rpc,
    Mcp,
}

impl Drop for Exchange {
    fn drop(&mut self) {
        self.password.zeroize();
        self.login.zeroize();
    }
}

/// Borrowed response payload serialized directly to bytes without an intermediate `serde_json::Value`.
#[derive(serde::Serialize)]
struct IssuedBody<'a> {
    api_key: &'a str,
    expires_at: String,
    uid: i64,
}

/// JSON 503 response when login is temporarily unavailable on this server.
pub(super) fn unavailable() -> Response {
    let mut result = refuse(StatusCode::SERVICE_UNAVAILABLE, BUSY);
    result
        .headers_mut()
        .insert(header::RETRY_AFTER, HeaderValue::from_static("5"));
    result
}

/// Serve the exchange: refusals are JSON `{"error": …}`, every answer is `no-store`.
pub(super) async fn exchange(
    config: &ServerConfig,
    headers: &HeaderMap,
    body: Body,
    client: IpAddr,
    auth: &Arc<Auth>,
    request_id: Option<&erp_rpc::CorrelationId>,
) -> HttpResult<Response> {
    same_origin(config, headers)?;
    let mut request = read(headers, body).await?;
    if request.db != config.database() {
        return Err(refuse(StatusCode::BAD_REQUEST, "Database not found."));
    }
    validate_device_name(&request.device_name)?;
    let agent = header(headers, "user-agent")?.map(str::to_owned);
    let correlation = request_id.map(|id| id.as_str().to_owned());
    // Authentication, key issuance and ending the temporary browser session all run inside one
    // spawned task: if the client disconnects or times out, we never leave an orphaned session or
    // commit an undeliverable key that consumes the user's quota.
    let owner = Arc::clone(auth);
    let (tx_reply, rx_reply) = tokio::sync::oneshot::channel();
    tokio::spawn(async move {
        let mut attempt = LoginRequest::new(
            request.login.as_str(),
            request.password.as_str(),
            ClientAddr::Known(client),
        )
        .interactive(false);
        if let Some(ref ua) = agent {
            attempt = attempt.user_agent(ua.as_str());
        }
        let authenticated = owner.authenticate(attempt).await;
        let device_name = std::mem::take(&mut request.device_name);
        let audience = request.audience;
        drop(request);
        let login = match authenticated {
            Ok(login) => login,
            Err(error) => {
                let _ = tx_reply.send(Err(login_refusal(error)));
                return;
            }
        };
        let uid = login.uid().get();
        let issued = if tx_reply.is_closed() {
            Err(refuse(StatusCode::REQUEST_TIMEOUT, "Request cancelled"))
        } else {
            issue_key(
                &owner,
                &login,
                &device_name,
                audience,
                correlation.as_deref(),
            )
            .await
        };
        // The browser session only opened the issuing transaction: it ends here on every outcome.
        let ended = owner.logout(login.session().id()).await;
        if tx_reply.is_closed() {
            if let Err(error) = ended {
                tracing::error!(
                    kind = ?error.kind(),
                    sqlstate = error.sqlstate(),
                    reason = ?error.message(),
                    request_id = correlation.as_deref(),
                    "failed to end temporary session after api-key client disconnected"
                );
            }
            return;
        }
        let outcome = match ended {
            Ok(_) => issued.map(|(secret, expiry)| (secret, expiry, uid)),
            Err(error) => Err(failure(error)),
        };
        let _ = tx_reply.send(outcome);
    });
    let (secret, expiry, uid) = rx_reply
        .await
        .map_err(|_| refuse(StatusCode::INTERNAL_SERVER_ERROR, "Internal server error"))??;
    let payload = IssuedBody {
        api_key: secret.expose(),
        expires_at: iso_utc(expiry),
        uid,
    };
    let body = serde_json::to_vec(&payload)
        .map_err(|_| refuse(StatusCode::INTERNAL_SERVER_ERROR, "Internal server error"))?;
    let mut result = Response::new(Body::from(body));
    let headers = result.headers_mut();
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/json"),
    );
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    Ok(result)
}

/// Issue the key in the user's own transaction and commit it.
async fn issue_key(
    auth: &Auth,
    login: &Login,
    device_name: &str,
    audience: Audience,
    request_id: Option<&str>,
) -> HttpResult<(IssuedSecret, DateTime)> {
    require_single_factor(login.auth().mfa())?;
    let (kind, scope) = match audience {
        Audience::Rpc => (KeyKind::User, super::KeyScope::Rpc),
        Audience::Mcp => (KeyKind::Agent, super::KeyScope::Mcp),
    };
    let request = IssueRequest::at_policy_maximum(login.uid(), device_name, kind, scope)
        .map_err(issue_refusal)?;
    let identity = match request_id.and_then(|raw| CorrelationId::parse(raw).ok()) {
        Some(corr) => OperationIdentity::new(corr).map_err(failure)?,
        None => OperationIdentity::generate().map_err(failure)?,
    };
    let mut begun = auth
        .begin_session(
            login.session().secret(),
            CredentialOptions::new().operation_identity(identity),
        )
        .await
        .map_err(session_refusal)?;
    if let Err(error) = begun.tx.audit_origin(AuditOrigin::Auth) {
        let _ = begun.tx.rollback().await;
        return Err(failure(error));
    }
    let hasher = match apikey_manage::hasher(&mut begun.tx).await {
        Ok(hasher) => hasher,
        Err(error) => {
            let _ = begun.tx.rollback().await;
            return Err(failure(error));
        }
    };
    let issued = match issue(&mut begun.tx, &hasher, &request).await {
        Ok(issued) => issued,
        Err(error) => {
            begun.tx.rollback().await.map_err(failure)?;
            return Err(issue_refusal(error));
        }
    };
    begun.tx.commit().await.map_err(failure)?;
    let expiry = issued.expiration_date().ok_or_else(|| {
        failure(erp_fields::Error::programming(
            "a policy-maximum key was issued without an expiry",
        ))
    })?;
    Ok((issued.into_secret(), expiry))
}

/// Second factors are not implemented in the core yet: a login that asks for one is refused,
/// so this route never becomes a way around it once they are.
fn require_single_factor(mfa: Mfa) -> HttpResult<()> {
    match mfa {
        Mfa::Default | Mfa::Skip => Ok(()),
        _ => Err(refuse(StatusCode::FORBIDDEN, SECOND_FACTOR)),
    }
}

/// The fresh session refused right after its login: it waits for a second factor (`pending`),
/// or it was ended in between. Either way no key is issued.
fn session_refusal(error: erp_fields::Error) -> Response {
    if error.kind() == ErrorKind::SessionExpired {
        return refuse(StatusCode::FORBIDDEN, SECOND_FACTOR);
    }
    failure(error)
}

fn login_refusal(error: LoginError) -> Response {
    match error {
        LoginError::Denied => refuse(StatusCode::UNAUTHORIZED, DENIED),
        LoginError::Cooldown => refuse(StatusCode::TOO_MANY_REQUESTS, LOGIN_COOLDOWN),
        LoginError::Overloaded { .. } => {
            let mut result = refuse(StatusCode::SERVICE_UNAVAILABLE, BUSY);
            result
                .headers_mut()
                .insert(header::RETRY_AFTER, HeaderValue::from_static("5"));
            result
        }
        LoginError::Failed(error) => failure(error),
        _ => refuse(StatusCode::INTERNAL_SERVER_ERROR, "Internal server error"),
    }
}

/// A policy refusal of the issuance (programmatic keys off, the per-user limit, the expiry
/// policy, an inactive subject) is the client's 403 with its text; anything else is a failure.
fn issue_refusal(error: erp_fields::Error) -> Response {
    if matches!(
        error.kind(),
        ErrorKind::UserError | ErrorKind::Validation | ErrorKind::Access | ErrorKind::AccessDenied
    ) {
        return refuse(StatusCode::FORBIDDEN, error.message());
    }
    failure(error)
}

/// A server-side failure: always logged and answered with a fixed 500 (or 503 when superseded).
fn failure(error: erp_fields::Error) -> Response {
    if error.is_superseded() {
        return refuse(
            StatusCode::SERVICE_UNAVAILABLE,
            "The application is updating; retry shortly.",
        );
    }
    tracing::error!(
        kind = ?error.kind(),
        sqlstate = error.sqlstate(),
        reason = ?error.message(),
        request_id = erp_orm::trace::current_request_id().as_deref(),
        "api-key session exchange failed"
    );
    refuse(StatusCode::INTERNAL_SERVER_ERROR, "Internal server error")
}

fn validate_device_name(name: &str) -> HttpResult<()> {
    if name.len() > MAX_DEVICE_NAME_BYTES || name.trim().is_empty() {
        return Err(refuse(StatusCode::BAD_REQUEST, INVALID_DEVICE_NAME));
    }
    Ok(())
}

fn refuse(status: StatusCode, error: &str) -> Response {
    let mut result = match json_response(status, &serde_json::json!({ "error": error })) {
        Ok(result) | Err(result) => result,
    };
    result
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    result
}

/// As the login form: a browser `Origin` other than the server's, or a cross-site fetch, is
/// refused. A native client sends neither.
fn same_origin(config: &ServerConfig, headers: &HeaderMap) -> HttpResult<()> {
    if header(headers, "sec-fetch-site")?
        .is_some_and(|site| !matches!(site, "same-origin" | "none"))
        || header(headers, "origin")?.is_some_and(|origin| origin != config.origin())
    {
        return Err(refuse(StatusCode::FORBIDDEN, CROSS_ORIGIN));
    }
    Ok(())
}

fn header<'a>(headers: &'a HeaderMap, name: &str) -> HttpResult<Option<&'a str>> {
    one(headers, name).map_err(|_| refuse(StatusCode::BAD_REQUEST, "ambiguous or invalid header"))
}

fn check_content_type(raw: Option<&str>) -> HttpResult<()> {
    let unsupported = || {
        refuse(
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            "expected application/json",
        )
    };
    let Some(value) = raw else {
        return Err(unsupported());
    };
    let mut parts = value.split(';');
    let essence = parts.next().unwrap_or("").trim();
    if !essence.eq_ignore_ascii_case("application/json") {
        return Err(unsupported());
    }
    for param in parts {
        let param = param.trim();
        if param.is_empty() {
            continue;
        }
        let Some((key, val)) = param.split_once('=') else {
            return Err(unsupported());
        };
        let val = val.trim().trim_matches('"');
        if !key.trim().eq_ignore_ascii_case("charset") || !val.eq_ignore_ascii_case("utf-8") {
            return Err(unsupported());
        }
    }
    Ok(())
}

async fn read(headers: &HeaderMap, body: Body) -> HttpResult<Exchange> {
    check_content_type(header(headers, "content-type")?)?;
    let bytes = to_bytes(body, BODY_LIMIT).await.map_err(|_| {
        refuse(
            StatusCode::PAYLOAD_TOO_LARGE,
            "authentication body exceeds 16384 bytes",
        )
    })?;
    parse(&Zeroizing::new(Vec::from(bytes)))
}

/// Only a JSON object: serde would also accept the fields as an array.
fn parse(raw: &[u8]) -> HttpResult<Exchange> {
    let invalid = || refuse(StatusCode::BAD_REQUEST, INVALID_BODY);
    if raw.iter().find(|byte| !byte.is_ascii_whitespace()) != Some(&b'{') {
        return Err(invalid());
    }
    serde_json::from_slice(raw).map_err(|_| invalid())
}

/// `YYYY-MM-DDTHH:MM:SSZ`.
fn iso_utc(at: DateTime) -> String {
    let (hour, minute, second, _) = at.time();
    format!("{}T{hour:02}:{minute:02}:{second:02}Z", at.date())
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn error_of(result: Response) -> (StatusCode, serde_json::Value) {
        let status = result.status();
        assert_eq!(result.headers()[header::CACHE_CONTROL], "no-store");
        let body = to_bytes(result.into_body(), 1 << 16).await.unwrap();
        (status, serde_json::from_slice(&body).unwrap())
    }

    #[tokio::test]
    async fn a_login_that_asks_for_a_second_factor_is_refused() {
        assert!(require_single_factor(Mfa::Default).is_ok());
        assert!(require_single_factor(Mfa::Skip).is_ok());
        let refused = require_single_factor(Mfa::Enforce).unwrap_err();
        assert_eq!(
            error_of(refused).await,
            (
                StatusCode::FORBIDDEN,
                serde_json::json!({ "error": SECOND_FACTOR })
            )
        );
    }

    #[tokio::test]
    async fn a_pending_session_is_refused_like_a_second_factor() {
        // `Auth::begin_session` refuses a pending session as expired
        let pending =
            erp_security::auth::session::session_expired(erp_security::auth::Refusal::Pending);
        assert_eq!(
            error_of(session_refusal(pending)).await,
            (
                StatusCode::FORBIDDEN,
                serde_json::json!({ "error": SECOND_FACTOR })
            )
        );
        let other = session_refusal(erp_fields::Error::db("connection lost"));
        assert_eq!(other.status(), StatusCode::INTERNAL_SERVER_ERROR);
    }

    #[tokio::test]
    async fn the_body_is_closed_and_unambiguous() {
        let good = br#"{"db":"d","login":"l","password":"p","device_name":"n"}"#;
        let parsed = parse(good).ok().unwrap();
        assert_eq!(
            (
                parsed.db.as_str(),
                parsed.login.as_str(),
                parsed.device_name.as_str()
            ),
            ("d", "l", "n")
        );
        for bad in [
            &br#"{"db":"d","login":"l","password":"p","device_name":"n","x":1}"#[..],
            br#"{"db":"d","login":"l","login":"m","password":"p","device_name":"n"}"#,
            br#"{"db":"d","login":"l","password":"p"}"#,
            br#"["d","l","p","n"]"#,
            b"not json",
        ] {
            let refused = parse(bad).err().unwrap();
            assert_eq!(
                error_of(refused).await,
                (
                    StatusCode::BAD_REQUEST,
                    serde_json::json!({ "error": INVALID_BODY })
                )
            );
        }
    }

    #[tokio::test]
    async fn policy_refusals_keep_their_text_and_failures_do_not() {
        let refused = issue_refusal(erp_fields::Error::user(
            "API key issuance is disabled by tenant policy",
        ));
        assert_eq!(
            error_of(refused).await,
            (
                StatusCode::FORBIDDEN,
                serde_json::json!({ "error": "API key issuance is disabled by tenant policy" })
            )
        );
        let failed = issue_refusal(erp_fields::Error::db("secret detail"));
        assert_eq!(
            error_of(failed).await,
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                serde_json::json!({ "error": "Internal server error" })
            )
        );
    }

    #[test]
    fn expiry_is_written_in_utc_iso_8601() {
        // 2023-11-14 22:13:20.5 UTC
        assert_eq!(
            iso_utc(DateTime::from_micros(1_700_000_000_500_000)),
            "2023-11-14T22:13:20Z"
        );
    }

    #[tokio::test]
    async fn server_failure_never_maps_internal_validation_to_4xx() {
        for err in [
            erp_fields::Error::validation("database.secret is missing or invalid"),
            erp_fields::Error::access("internal policy check"),
            erp_fields::Error::db("connection reset"),
        ] {
            let resp = failure(err);
            assert_eq!(
                error_of(resp).await,
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    serde_json::json!({ "error": "Internal server error" })
                )
            );
        }
    }

    #[tokio::test]
    async fn content_type_accepts_json_and_utf8_charset_only() {
        for ok in [
            "application/json",
            "APPLICATION/JSON",
            "application/json; charset=utf-8",
            "application/json; charset=\"UTF-8\"",
        ] {
            assert!(check_content_type(Some(ok)).is_ok(), "{ok}");
        }
        for bad in [
            None,
            Some("text/plain"),
            Some("application/x-www-form-urlencoded"),
            Some("application/json; charset=latin1"),
            Some("application/json; bogus"),
        ] {
            let err = check_content_type(bad).unwrap_err();
            assert_eq!(err.status(), StatusCode::UNSUPPORTED_MEDIA_TYPE);
        }
    }

    #[tokio::test]
    async fn device_name_is_validated_before_authentication() {
        assert!(validate_device_name("Field tablet").is_ok());
        assert!(validate_device_name(&"a".repeat(128)).is_ok());
        for bad in ["", "   ", &"a".repeat(129)] {
            let err = validate_device_name(bad).unwrap_err();
            assert_eq!(
                error_of(err).await,
                (
                    StatusCode::BAD_REQUEST,
                    serde_json::json!({ "error": INVALID_DEVICE_NAME })
                )
            );
        }
    }

    #[tokio::test]
    async fn unavailable_returns_json_503_with_retry_after() {
        let resp = unavailable();
        assert_eq!(resp.headers()[header::RETRY_AFTER], "5");
        assert_eq!(
            error_of(resp).await,
            (
                StatusCode::SERVICE_UNAVAILABLE,
                serde_json::json!({ "error": BUSY })
            )
        );
    }
}
