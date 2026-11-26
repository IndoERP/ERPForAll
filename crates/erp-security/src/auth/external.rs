// SPDX-License-Identifier: MIT OR Apache-2.0
//! OpenID Connect authorization-code transport for an explicitly configured issuer.

use std::fmt;
use std::time::Duration;

use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use erp_fields::{Error, Result};
use ring::signature::{
    ED25519, RSA_PKCS1_2048_8192_SHA256, RsaPublicKeyComponents, UnparsedPublicKey,
};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;
use url::Url;
use zeroize::Zeroizing;

const MAX_RESPONSE: usize = 65_536;
const MAX_TOKEN: usize = 16_384;

/// Startup configuration for one trusted OpenID Connect issuer.
#[derive(Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub struct ExternalProviderConfig {
    /// Exact issuer identifier the ID token must contain.
    pub issuer: String,
    /// The administrator-pinned authorization endpoint.
    pub authorization_endpoint: String,
    /// The administrator-pinned token endpoint.
    pub token_endpoint: String,
    /// The relying party registered with the issuer.
    pub client_id: String,
    /// The issuer name shown on the login screen.
    pub label: String,
    /// Signed ID tokens must carry this exact provider-specific MFA assurance level, if configured.
    pub required_acr: Option<String>,
    /// Optional enrollment in one company's portal. Manual linking is the default.
    pub provisioning: Option<Provisioning>,
}

/// Constrained, opt-in enrollment for new provider subjects.
#[derive(Debug, Clone, Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub struct Provisioning {
    /// The only company granted to new portal accounts.
    pub company_id: i64,
    /// The exact verified email domain eligible for account creation.
    pub email_domain: String,
}

/// Validated issuer and outbound transport, without any dynamic endpoint discovery.
pub struct ExternalProvider {
    cfg: ExternalProviderConfig,
    authorization: Url,
    token: Url,
    secret: Zeroizing<String>,
    keys: Vec<SigningKey>,
    http: reqwest::Client,
    fingerprint: [u8; 32],
}

impl fmt::Debug for ExternalProvider {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ExternalProvider")
            .field("issuer", &self.cfg.issuer)
            .finish_non_exhaustive()
    }
}

#[derive(Deserialize)]
struct KeySet {
    keys: Vec<JsonKey>,
}

#[derive(Deserialize)]
#[serde(tag = "kty")]
enum JsonKey {
    #[serde(rename = "RSA")]
    Rsa {
        kid: String,
        n: String,
        e: String,
        alg: Option<String>,
        #[serde(rename = "use")]
        purpose: Option<String>,
    },
    #[serde(rename = "OKP")]
    Ed {
        kid: String,
        crv: String,
        x: String,
        alg: Option<String>,
        #[serde(rename = "use")]
        purpose: Option<String>,
    },
}

enum SigningKey {
    Rsa(String, RsaPublicKeyComponents<Vec<u8>>),
    Ed(String, Vec<u8>),
}

impl SigningKey {
    fn id(&self) -> &str {
        match self {
            Self::Rsa(id, _) | Self::Ed(id, _) => id,
        }
    }

    fn verify(&self, alg: &str, signed: &[u8], signature: &[u8]) -> Result<()> {
        let verified = match (self, alg) {
            (Self::Rsa(_, key), "RS256") => {
                key.verify(&RSA_PKCS1_2048_8192_SHA256, signed, signature)
            }
            (Self::Ed(_, key), "EdDSA") => {
                UnparsedPublicKey::new(&ED25519, key).verify(signed, signature)
            }
            _ => return Err(denied()),
        };
        verified.map_err(|_| denied())
    }
}

fn invalid_config() -> Error {
    Error::validation("Invalid external identity provider configuration")
}
fn denied() -> Error {
    super::credential::access_denied()
}

fn endpoint(value: &str) -> Result<Url> {
    let url = Url::parse(value).map_err(|_| invalid_config())?;
    let loopback = url
        .host_str()
        .is_some_and(|host| matches!(host, "127.0.0.1" | "[::1]"));
    if (url.scheme() != "https" && !(url.scheme() == "http" && loopback))
        || !url.has_host()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return Err(invalid_config());
    }
    Ok(url)
}

fn valid_text(s: &str, max: usize) -> bool {
    !s.is_empty() && s.len() <= max && !s.chars().any(char::is_control)
}

impl ExternalProvider {
    /// Admit bounded issuer configuration, a private client secret and public verification keys.
    /// Endpoints are fixed for the lifetime of this configuration and redirects are refused.
    ///
    /// # Errors
    /// Validation for malformed configuration or unsupported signing keys.
    pub fn new(
        cfg: ExternalProviderConfig,
        secret: Zeroizing<String>,
        jwks: &[u8],
    ) -> Result<Self> {
        endpoint(&cfg.issuer)?;
        let authorization = endpoint(&cfg.authorization_endpoint)?;
        let token = endpoint(&cfg.token_endpoint)?;
        // Never send secrets over HTTP except for an isolated loopback development provider.
        if cfg.issuer.starts_with("https:")
            && (authorization.scheme() != "https" || token.scheme() != "https")
            || !valid_text(&cfg.issuer, 512)
            || !valid_text(&cfg.client_id, 256)
            || !valid_text(&cfg.label, 80)
            || !valid_text(&secret, 512)
            || cfg
                .required_acr
                .as_ref()
                .is_some_and(|acr| !valid_text(acr, 256))
            || cfg.provisioning.as_ref().is_some_and(|rule| {
                rule.company_id <= 0
                    || !rule.email_domain.contains('.')
                    || rule.email_domain.len() > 253
                    || !rule
                        .email_domain
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b == b'.' || b == b'-')
            })
            || jwks.len() > MAX_RESPONSE
        {
            return Err(invalid_config());
        }
        let KeySet { keys } = serde_json::from_slice(jwks).map_err(|_| invalid_config())?;
        if keys.is_empty() || keys.len() > 16 {
            return Err(invalid_config());
        }
        let decode = |s: &str| URL_SAFE_NO_PAD.decode(s).map_err(|_| invalid_config());
        let mut selected: Vec<SigningKey> = Vec::with_capacity(keys.len());
        for key in keys {
            let key = match key {
                JsonKey::Rsa {
                    kid,
                    n,
                    e,
                    alg,
                    purpose,
                } if alg.as_deref().is_none_or(|s| s == "RS256")
                    && purpose.as_deref().is_none_or(|s| s == "sig") =>
                {
                    let (n, e) = (decode(&n)?, decode(&e)?);
                    if !(256..=1024).contains(&n.len()) || !(1..=8).contains(&e.len()) {
                        return Err(invalid_config());
                    }
                    SigningKey::Rsa(kid, RsaPublicKeyComponents { n, e })
                }
                JsonKey::Ed {
                    kid,
                    crv,
                    x,
                    alg,
                    purpose,
                } if crv == "Ed25519"
                    && alg.as_deref().is_none_or(|s| s == "EdDSA")
                    && purpose.as_deref().is_none_or(|s| s == "sig") =>
                {
                    let x = decode(&x)?;
                    if x.len() != 32 {
                        return Err(invalid_config());
                    }
                    SigningKey::Ed(kid, x)
                }
                _ => return Err(invalid_config()),
            };
            if !valid_text(key.id(), 128) || selected.iter().any(|k| k.id() == key.id()) {
                return Err(invalid_config());
            }
            selected.push(key);
        }
        let http = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .no_proxy()
            .timeout(Duration::from_secs(8))
            .build()
            .map_err(|_| invalid_config())?;
        let settings = serde_json::to_vec(&cfg).map_err(|_| invalid_config())?;
        let mut fingerprint = Sha256::new();
        for part in [settings.as_slice(), jwks, secret.as_bytes()] {
            fingerprint.update((part.len() as u64).to_be_bytes());
            fingerprint.update(part);
        }
        Ok(Self {
            cfg,
            authorization,
            token,
            secret,
            keys: selected,
            http,
            fingerprint: fingerprint.finalize().into(),
        })
    }

    /// Exact issuer used as the account-link namespace.
    pub fn issuer(&self) -> &str {
        &self.cfg.issuer
    }

    /// Name displayed in the browser.
    pub fn label(&self) -> &str {
        &self.cfg.label
    }

    /// Pass to shared settings as a secret fingerprint to bind all serving nodes to the same policy.
    pub fn configuration_fingerprint(&self) -> &[u8; 32] {
        &self.fingerprint
    }

    pub(super) fn provisioning(&self) -> Option<&Provisioning> {
        self.cfg.provisioning.as_ref()
    }

    pub(super) fn authorize(
        &self,
        redirect_uri: &str,
        state: &str,
        nonce: &str,
        verifier: &str,
    ) -> String {
        let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
        let mut url = self.authorization.clone();
        let mut params = url.query_pairs_mut();
        params.extend_pairs([
            ("response_type", "code"),
            ("scope", "openid email profile"),
            ("client_id", &self.cfg.client_id),
            ("redirect_uri", redirect_uri),
            ("state", state),
            ("nonce", nonce),
            ("code_challenge_method", "S256"),
            ("code_challenge", &challenge),
        ]);
        if let Some(acr) = &self.cfg.required_acr {
            params.append_pair("acr_values", acr);
        }
        drop(params);
        url.into()
    }

    pub(super) async fn exchange(
        &self,
        redirect_uri: &str,
        code: &str,
        nonce: &str,
        verifier: &str,
    ) -> Result<Claims> {
        if code.is_empty() || code.len() > 4096 || !code.bytes().all(|b| b.is_ascii_graphic()) {
            return Err(denied());
        }
        let body = url::form_urlencoded::Serializer::new(String::new())
            .extend_pairs([
                ("grant_type", "authorization_code"),
                ("code", code),
                ("redirect_uri", redirect_uri),
                ("code_verifier", verifier),
            ])
            .finish();
        let basic_part = |value: &str| {
            url::form_urlencoded::byte_serialize(value.as_bytes()).collect::<String>()
        };
        let mut response = self
            .http
            .post(self.token.clone())
            .basic_auth(
                basic_part(&self.cfg.client_id),
                Some(Zeroizing::new(basic_part(&self.secret)).as_str()),
            )
            .header("content-type", "application/x-www-form-urlencoded")
            .body(body)
            .send()
            .await
            .map_err(|_| denied())?;
        if !response.status().is_success()
            || response
                .content_length()
                .is_some_and(|len| len > MAX_RESPONSE as u64)
        {
            return Err(denied());
        }
        let mut bytes = Zeroizing::new(Vec::new());
        while let Some(chunk) = response.chunk().await.map_err(|_| denied())? {
            if bytes.len().saturating_add(chunk.len()) > MAX_RESPONSE {
                return Err(denied());
            }
            bytes.extend_from_slice(&chunk);
        }
        #[derive(Deserialize)]
        struct TokenReply {
            id_token: String,
        }
        let TokenReply { id_token } = serde_json::from_slice(&bytes).map_err(|_| denied())?;
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_err(|_| denied())?
            .as_secs();
        self.verify(&Zeroizing::new(id_token), nonce, now)
    }

    fn verify(&self, jwt: &str, nonce: &str, now: u64) -> Result<Claims> {
        if jwt.len() > MAX_TOKEN {
            return Err(denied());
        }
        let (signed, signature) = jwt.rsplit_once('.').ok_or_else(denied)?;
        let (header, body) = signed.split_once('.').ok_or_else(denied)?;
        let decode = |s: &str| URL_SAFE_NO_PAD.decode(s).map_err(|_| denied());
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Header {
            alg: String,
            kid: String,
            typ: Option<String>,
        }
        let header: Header = serde_json::from_slice(&decode(header)?).map_err(|_| denied())?;
        if header.typ.as_deref().is_some_and(|kind| kind != "JWT") {
            return Err(denied());
        }
        let key = self
            .keys
            .iter()
            .find(|key| key.id() == header.kid)
            .ok_or_else(denied)?;
        key.verify(&header.alg, signed.as_bytes(), &decode(signature)?)?;
        let claims: Claims = serde_json::from_slice(&decode(body)?).map_err(|_| denied())?;
        if claims.iss != self.cfg.issuer
            || !claims
                .aud
                .matches(&self.cfg.client_id, claims.azp.as_deref())
            || !valid_text(&claims.sub, 255)
            || !claims.sub.is_ascii()
            || claims.exp <= now
            || claims.exp <= claims.iat
            || claims.iat > now.saturating_add(30)
            || now.saturating_sub(claims.iat) > 600
            || claims.nbf.is_some_and(|nbf| nbf > now.saturating_add(30))
            || !bool::from(claims.nonce.as_bytes().ct_eq(nonce.as_bytes()))
            || self
                .cfg
                .required_acr
                .as_ref()
                .is_some_and(|acr| claims.acr.as_ref() != Some(acr))
        {
            return Err(denied());
        }
        Ok(claims)
    }
}

#[derive(Deserialize)]
#[serde(untagged)]
enum Audience {
    One(String),
    Many(Vec<String>),
}
impl Audience {
    fn matches(&self, expected: &str, azp: Option<&str>) -> bool {
        // This relying party trusts only the configured client ID as an audience. An azp
        // naming that client cannot make additional audiences trustworthy.
        let aud = match self {
            Self::One(aud) => aud,
            Self::Many(aud) => match aud.as_slice() {
                [only] => only,
                _ => return false,
            },
        };
        aud == expected && azp.is_none_or(|azp| azp == expected)
    }
}

#[derive(Deserialize)]
pub(super) struct Claims {
    iss: String,
    pub(super) sub: String,
    aud: Audience,
    azp: Option<String>,
    nonce: String,
    exp: u64,
    iat: u64,
    nbf: Option<u64>,
    acr: Option<String>,
    pub(super) email: Option<String>,
    pub(super) email_verified: Option<bool>,
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, reason = "ephemeral OIDC fixture")]
    use super::*;
    use ring::{
        rand::SystemRandom,
        signature::{Ed25519KeyPair, KeyPair},
    };
    use serde_json::json;

    #[test]
    fn verifies_signature_and_issuer_audience_nonce_and_mfa() {
        let generated = Ed25519KeyPair::generate_pkcs8(&SystemRandom::new()).unwrap();
        let signer = Ed25519KeyPair::from_pkcs8(generated.as_ref()).unwrap();
        let keys = json!({"keys":[{"kty":"OKP","crv":"Ed25519","alg":"EdDSA","kid":"fixture","x":URL_SAFE_NO_PAD.encode(signer.public_key().as_ref())}]});
        let provider = ExternalProvider::new(
            ExternalProviderConfig {
                issuer: "https://issuer.example.test".into(),
                authorization_endpoint: "https://issuer.example.test/authorize".into(),
                token_endpoint: "https://issuer.example.test/token".into(),
                client_id: "fixture".into(),
                label: "Example".into(),
                required_acr: Some("mfa".into()),
                provisioning: None,
            },
            Zeroizing::new("ephemeral-test-client".into()),
            keys.to_string().as_bytes(),
        )
        .unwrap();
        let claims = json!({"iss":provider.issuer(),"sub":"person","aud":"fixture","nonce":"issued-once","exp":1100,"iat":1000,"acr":"mfa"});
        let signed = |body: &serde_json::Value| {
            let prefix = format!(
                "{}.{}",
                URL_SAFE_NO_PAD.encode(br#"{"alg":"EdDSA","kid":"fixture"}"#),
                URL_SAFE_NO_PAD.encode(body.to_string())
            );
            format!(
                "{prefix}.{}",
                URL_SAFE_NO_PAD.encode(signer.sign(prefix.as_bytes()).as_ref())
            )
        };
        assert_eq!(
            provider
                .verify(&signed(&claims), "issued-once", 1005)
                .unwrap()
                .sub,
            "person"
        );
        assert!(
            provider
                .verify(&signed(&claims), "another-browser", 1005)
                .is_err()
        );
        assert!(
            provider
                .verify(&signed(&claims), "issued-once", 1100)
                .is_err()
        );
        for (field, value) in [
            ("iss", "https://another.example.test"),
            ("aud", "other-client"),
            ("acr", "password"),
        ] {
            let mut wrong = claims.clone();
            wrong[field] = json!(value);
            assert!(
                provider
                    .verify(&signed(&wrong), "issued-once", 1005)
                    .is_err()
            );
        }
        let mut array_audience = claims.clone();
        array_audience["aud"] = json!(["fixture"]);
        array_audience["azp"] = json!("fixture");
        assert!(
            provider
                .verify(&signed(&array_audience), "issued-once", 1005)
                .is_ok()
        );
        array_audience["aud"] = json!(["fixture", "untrusted-client"]);
        assert!(
            provider
                .verify(&signed(&array_audience), "issued-once", 1005)
                .is_err()
        );
        let token = signed(&claims);
        let mut tampered = token.as_bytes().to_vec();
        let sig = token.rfind('.').unwrap() + 1;
        tampered[sig] = if tampered[sig] == b'A' { b'B' } else { b'A' };
        assert!(
            provider
                .verify(std::str::from_utf8(&tampered).unwrap(), "issued-once", 1005)
                .is_err()
        );
    }
}
