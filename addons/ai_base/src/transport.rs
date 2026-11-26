// SPDX-License-Identifier: MIT OR Apache-2.0
//! Bounded, non-streaming HTTP exchanges for a caller's explicitly configured AI route.

use crate::{config::Credentials, encoding::RequestEncoder, routing::ConfiguredRoute};
use erp_sdk::ai::{ProviderError, ToolTurnRequest, ToolTurnResponse};
use reqwest::{Client, Response, StatusCode, dns, header};
use std::{error::Error as _, io, net::IpAddr, sync::Arc, time::Duration};
use url::{Host, Url};
use zeroize::Zeroizing;

pub(crate) fn trusted_base(input: &str) -> Option<Url> {
    let mut url = Url::parse(input).ok()?;
    let allowed = match (url.scheme(), url.host()?) {
        // Local providers must be configured explicitly using a literal loopback address.
        ("http", Host::Ipv4(ip)) => ip.is_loopback(),
        ("http", Host::Ipv6(ip)) => ip.is_loopback(),
        ("https", Host::Ipv4(ip)) => public_ip(ip.into()),
        ("https", Host::Ipv6(ip)) => public_ip(ip.into()),
        ("https", Host::Domain(_)) => true,
        _ => false,
    };
    if !allowed
        || url.port() == Some(0)
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return None;
    }
    url.path_segments_mut().ok()?.pop_if_empty().push("");
    Some(url)
}

fn public_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(ip) => {
            let [a, b, c, _] = ip.octets();
            !(a == 0
                || a >= 224
                || ip.is_private()
                || ip.is_loopback()
                || ip.is_link_local()
                || ip.is_documentation()
                || (a == 100 && (64..=127).contains(&b))
                || (a == 192 && (b, c) == (0, 0))
                || (a == 192 && (b, c) == (88, 99))
                || (a == 198 && (18..=19).contains(&b)))
        }
        IpAddr::V6(ip) => {
            let [a, b, ..] = ip.segments();
            (0x2000..0x4000).contains(&a)
                && !matches!(a, 0x2002 | 0x3fff)
                && !(a == 0x2001 && (b <= 0x1ff || b == 0xdb8))
        }
    }
}

struct CheckedDns;

impl dns::Resolve for CheckedDns {
    fn resolve(&self, name: dns::Name) -> dns::Resolving {
        Box::pin(async move {
            const MAX_ADDRESSES: usize = 32;
            let addresses: Vec<_> = tokio::net::lookup_host((name.as_str(), 0))
                .await?
                .take(MAX_ADDRESSES + 1)
                .collect();
            if addresses.is_empty()
                || addresses.len() > MAX_ADDRESSES
                || addresses.iter().any(|address| !public_ip(address.ip()))
            {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "provider DNS is outside the authorized address policy",
                )
                .into());
            }
            // The HTTP connector connects to these same addresses. TLS still verifies the
            // configured hostname, and any later connection repeats the address checks.
            Ok(Box::new(addresses.into_iter()) as dns::Addrs)
        })
    }
}

/// Sends a single assistant turn or its continuations to the same configured provider.
/// Reuse this value for calls in a turn to preserve labelled context delimiters.
#[non_exhaustive]
pub struct ProviderTransport<'route> {
    provider: &'route str,
    encoder: RequestEncoder<'route>,
    client: Client,
    endpoint: Url,
    timeout: Duration,
    response_bytes: usize,
}

impl ConfiguredRoute {
    /// Prepare the provider configured for this unprivileged, non-impersonated user.
    /// The operator must bind the credential to this exact API base in node configuration.
    ///
    /// # Errors
    /// Returns a sanitized provider error when the caller, endpoint or credential is inadmissible,
    /// or the client or request encoder cannot be initialized.
    pub fn transport(
        &self,
        credentials: &Credentials,
    ) -> Result<ProviderTransport<'_>, ProviderError> {
        let invalid = |message: &str| ProviderError::InvalidRequest {
            provider: self.provider().to_owned(),
            message: message.to_owned(),
        };
        if !self.interactive {
            return Err(invalid(
                "provider transport requires an unprivileged interactive user",
            ));
        }
        let base = trusted_base(self.base_url())
            .ok_or_else(|| invalid("unsupported provider endpoint"))?;
        let key = credentials
            .bound_key(self.provider(), &base)
            .ok_or_else(|| {
                invalid("node credential is not bound to the selected provider endpoint")
            })?;
        let mut headers = header::HeaderMap::new();
        let (suffix, name, value) = match self.family() {
            "anthropic" => {
                headers.insert(
                    "anthropic-version",
                    header::HeaderValue::from_static("2023-06-01"),
                );
                (
                    "messages",
                    header::HeaderName::from_static("x-api-key"),
                    header::HeaderValue::from_bytes(key),
                )
            }
            "openai_compatible" => {
                let mut token = Zeroizing::new(b"Bearer ".to_vec());
                token.extend_from_slice(key);
                (
                    "chat/completions",
                    header::AUTHORIZATION,
                    header::HeaderValue::from_bytes(&token),
                )
            }
            _ => return Err(invalid("unsupported configured provider protocol family")),
        };
        let mut value = value.map_err(|_| invalid("invalid provider credential"))?;
        value.set_sensitive(true);
        headers.insert(name, value);
        headers.insert(
            header::CONTENT_TYPE,
            header::HeaderValue::from_static("application/json"),
        );
        let endpoint = base
            .join(suffix)
            .map_err(|_| invalid("unsupported provider endpoint"))?;
        let client = Client::builder()
            .default_headers(headers)
            .dns_resolver(Arc::new(CheckedDns))
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .retry(reqwest::retry::never())
            .build()
            .map_err(|_| ProviderError::Unavailable {
                provider: self.provider().to_owned(),
                message: "could not initialize the provider HTTP client".into(),
            })?;
        Ok(ProviderTransport {
            provider: self.provider(),
            encoder: self.request_encoder()?,
            client,
            endpoint,
            timeout: credentials.timeout,
            response_bytes: credentials.response_bytes,
        })
    }
}

impl ProviderTransport<'_> {
    fn unavailable(&self, message: impl Into<String>) -> ProviderError {
        ProviderError::Unavailable {
            provider: self.provider.to_owned(),
            message: message.into(),
        }
    }

    /// Send a typed turn and decode a complete reply. The deadline covers both attempts and reading
    /// the response. Only a transient connection failure known to precede sending may be retried once.
    ///
    /// # Errors
    /// Returns typed request, transport, authentication, rate-limit or response errors. Neither
    /// credential bytes, conversation data, URLs nor upstream diagnostic text appear in errors.
    pub async fn complete_turn(
        &self,
        request: &ToolTurnRequest,
    ) -> Result<ToolTurnResponse, ProviderError> {
        let body = self.encoder.encode(request)?;
        let bytes = serde_json::to_vec(&body).map_err(|_| ProviderError::InvalidRequest {
            provider: self.provider.to_owned(),
            message: "could not serialize provider request".into(),
        })?;
        tokio::time::timeout(self.timeout, self.exchange(bytes))
            .await
            .map_err(|_| self.unavailable("request deadline expired"))?
    }

    async fn exchange(&self, bytes: Vec<u8>) -> Result<ToolTurnResponse, ProviderError> {
        let body = reqwest::Body::from(bytes);
        let template = self
            .client
            .post(self.endpoint.clone())
            .body(body)
            .build()
            .map_err(|_| self.unavailable("could not construct the HTTP request"))?;
        let replay = template
            .try_clone()
            .ok_or_else(|| self.unavailable("could not prepare bounded request replay"))?;
        let response = send_with_retry(
            || self.client.execute(replay),
            || self.client.execute(template),
        )
        .await
        .map_err(|_| self.unavailable("could not complete the HTTP exchange"))?;
        let status = response.status();
        if !status.is_success() {
            return Err(self.status_error(status));
        }
        let body = self.read_body(response).await?;
        self.encoder.decode_response(&body)
    }

    fn status_error(&self, status: StatusCode) -> ProviderError {
        let provider = self.provider.to_owned();
        let message = format!("HTTP {}", status.as_u16());
        match status {
            StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN => {
                ProviderError::AuthenticationFailed { provider, message }
            }
            StatusCode::TOO_MANY_REQUESTS => ProviderError::RateLimitExceeded { provider, message },
            _ if status.is_redirection() => self.unavailable("unexpected HTTP redirection"),
            _ if status.is_server_error() => self.unavailable(message),
            _ => ProviderError::InvalidRequest { provider, message },
        }
    }

    async fn read_body(&self, mut response: Response) -> Result<Vec<u8>, ProviderError> {
        let oversized = || ProviderError::InvalidResponse {
            provider: self.provider.to_owned(),
            message: "response exceeds configured byte limit".into(),
        };
        if response
            .content_length()
            .is_some_and(|size| size > self.response_bytes as u64)
        {
            return Err(oversized());
        }
        let mut body = Vec::new();
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(|_| self.unavailable("incomplete response body"))?
        {
            if chunk.len() > self.response_bytes - body.len() {
                return Err(oversized());
            }
            body.extend_from_slice(&chunk);
        }
        Ok(body)
    }
}

// A second identical POST is safe only when the first attempt never established a connection.
async fn send_with_retry<F: Future<Output = Result<Response, reqwest::Error>>>(
    first: impl FnOnce() -> F,
    retry: impl FnOnce() -> F,
) -> Result<Response, reqwest::Error> {
    match first().await {
        Err(ref error) if retryable_connect(error) => retry().await,
        result => result,
    }
}

fn retryable_connect(error: &reqwest::Error) -> bool {
    if !error.is_connect() {
        return false;
    }
    let mut cause = error.source();
    while let Some(err) = cause {
        if let Some(io) = err.downcast_ref::<io::Error>() {
            return matches!(
                io.kind(),
                io::ErrorKind::ConnectionRefused
                    | io::ErrorKind::ConnectionReset
                    | io::ErrorKind::ConnectionAborted
            );
        }
        cause = err.source();
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        io::{Read, Write},
        net::TcpListener,
        thread,
    };

    #[tokio::test]
    #[allow(clippy::unwrap_used, reason = "loopback test fixture")]
    async fn refused_connection_is_retried_once_and_the_same_post_reaches_the_server() {
        // Release a real loopback port before the first request. Only the retry opens the listener.
        let port = TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap();
        let client = Client::builder()
            .no_proxy()
            .retry(reqwest::retry::never())
            .build()
            .unwrap();
        let send = || {
            client
                .post(format!("http://{port}/v1/chat/completions"))
                .body("fixture-body")
                .send()
        };
        let mut server = None;
        let response = send_with_retry(send, || {
            let listener = TcpListener::bind(port).unwrap();
            server = Some(thread::spawn(move || {
                let (mut stream, _) = listener.accept().unwrap();
                let mut received = Vec::new();
                let mut buf = [0; 512];
                while !received.ends_with(b"fixture-body") {
                    let n = stream.read(&mut buf).unwrap();
                    assert!(n > 0);
                    received.extend_from_slice(&buf[..n]);
                }
                stream
                    .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
                    .unwrap();
                received
            }));
            send()
        })
        .await
        .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let sent = server.unwrap().join().unwrap();
        assert!(sent.starts_with(b"POST /v1/chat/completions HTTP/1.1\r\n"));
    }
}
