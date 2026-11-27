// SPDX-License-Identifier: MIT OR Apache-2.0
use super::*;
use erp_security::auth::browser::SignedPrefs;
use erp_security::auth::external::ExternalProvider;

const CALLBACK_REFUSAL: &str = "/erp/login?external_error=1";

impl WebApp {
    fn external_provider(&self) -> HttpResult<&ExternalProvider> {
        self.config
            .external_identity()
            .ok_or_else(|| response(StatusCode::NOT_FOUND, "External sign-in is unavailable."))
    }

    fn external_callback_url(&self) -> String {
        format!("{}{}", self.config.origin(), routes::EXTERNAL_CALLBACK)
    }

    async fn external_prefs(&self, headers: &HeaderMap, auth: &Auth) -> HttpResult<SignedPrefs> {
        let signer = auth.browser_tokens().await.map_err(auth_error)?;
        let cookies = values(headers, "cookie")?;
        let cookie = boundary::cookie(&cookies, "prefs")
            .map_err(|_| bad("ambiguous or malformed authentication cookie"))?
            .ok_or_else(|| bad("Sign-in request expired."))?;
        signer
            .verify(cookie)
            .map_err(|_| bad("Sign-in request expired."))
    }

    pub(super) async fn external_login(
        &self,
        headers: &HeaderMap,
        body: Body,
        client: IpAddr,
        auth: &Auth,
    ) -> HttpResult<Response> {
        let provider = self.external_provider()?;
        self.form_origin(headers)?;
        let fields = read_form(headers, body, &["csrf_token", "redirect", "link"]).await?;
        self.verify_csrf(headers, required(&fields, "csrf_token")?)
            .await?;
        let destination = self.redirect(fields.get("redirect").map(String::as_str))?;
        let prefs = self.external_prefs(headers, auth).await?;
        let link = match fields.get("link").map(String::as_str) {
            None => None,
            Some("1") => Some(
                self.session(headers)?
                    .ok_or_else(|| bad("Sign in before linking an external identity."))?,
            ),
            _ => return Err(bad("Invalid external sign-in request.")),
        };
        match auth
            .begin_external(
                provider,
                &prefs,
                &self.external_callback_url(),
                &destination,
                link.as_ref(),
                ClientAddr::Known(client),
            )
            .await
        {
            Ok(location) if one(headers, "accept")? == Some("application/json") => {
                let mut result = response(
                    StatusCode::OK,
                    serde_json::json!({ "location": location }).to_string(),
                );
                result.headers_mut().insert(
                    header::CONTENT_TYPE,
                    HeaderValue::from_static("application/json"),
                );
                result
                    .headers_mut()
                    .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
                Ok(result)
            }
            Ok(location) => redirect(&location),
            Err(error) if error.kind() == erp_fields::ErrorKind::AccessDenied => {
                self.login_page(
                    headers,
                    destination,
                    Some("External sign-in is unavailable.".into()),
                )
                .await
            }
            Err(error) => Err(auth_error(error)),
        }
    }

    pub(super) async fn external_callback(
        &self,
        headers: &HeaderMap,
        query: Option<&str>,
        client: IpAddr,
        auth: &Auth,
    ) -> HttpResult<Response> {
        let provider = self.external_provider()?;
        let prefs = self.external_prefs(headers, auth).await?;
        let fields = form(
            query.unwrap_or(""),
            &[
                "state",
                "code",
                // OIDC Session Management providers can include this alongside the authorization code.
                "session_state",
                "iss",
                "error",
                "error_description",
                "error_uri",
            ],
        )?;
        if fields.contains_key("error")
            || fields
                .get("iss")
                .is_some_and(|issuer| issuer != provider.issuer())
        {
            return redirect(CALLBACK_REFUSAL);
        }
        let native = self.session(headers)?;
        let result = auth
            .complete_external(
                provider,
                &prefs,
                &self.external_callback_url(),
                required(&fields, "state")?,
                required(&fields, "code")?,
                native.as_ref(),
                ClientAddr::Known(client),
                one(headers, "user-agent")?,
            )
            .await;
        let (login, destination) = match result {
            Ok(value) => value,
            Err(error)
                if matches!(
                    error.kind(),
                    erp_fields::ErrorKind::AccessDenied | erp_fields::ErrorKind::SessionExpired
                ) =>
            {
                return redirect(CALLBACK_REFUSAL);
            }
            Err(error) => return Err(auth_error(error)),
        };
        let mut begun = auth
            .begin_session(login.session().secret(), CredentialOptions::new())
            .await
            .map_err(auth_error)?;
        let env = begun.tx.env();
        let internal_user = begun
            .tx
            .has_group(&env, "base.group_user")
            .await
            .map_err(auth_error)?;
        begun.tx.rollback().await.map_err(auth_error)?;
        let target = self.redirect(Some(&destination))?;
        let mut result = redirect(if internal_user {
            &target
        } else {
            routes::LOGIN_SUCCESS
        })?;
        let value = format!(
            "{}.{}",
            self.config.database(),
            login.session().secret().to_cookie().as_str()
        );
        self.set_cookie(
            &mut result,
            "session_id",
            &value,
            login.session().lifetime().as_secs(),
        )?;
        Ok(result)
    }

    pub(super) async fn external_revoke(
        &self,
        headers: &HeaderMap,
        body: Body,
        auth: &Auth,
    ) -> HttpResult<Response> {
        let provider = self.external_provider()?;
        self.form_origin(headers)?;
        let fields = read_form(headers, body, &["csrf_token"]).await?;
        self.verify_csrf(headers, required(&fields, "csrf_token")?)
            .await?;
        let session = self
            .session(headers)?
            .ok_or_else(|| bad("Sign in before unlinking an external identity."))?;
        auth.revoke_external(provider, &session)
            .await
            .map_err(auth_error)?;
        redirect(routes::LOGIN)
    }

    pub(super) async fn external_provider_status(
        &self,
        headers: &HeaderMap,
        auth: &Auth,
    ) -> HttpResult<Response> {
        let session = self.session(headers)?.ok_or_else(|| {
            response(StatusCode::UNAUTHORIZED, "Sign in to view provider status.")
        })?;
        let provider = self.config.external_identity();
        let (enabled, neutralized) = auth
            .external_admin_status(&session, provider.map(ExternalProvider::issuer))
            .await
            .map_err(auth_error)?;
        let data = match provider {
            None => serde_json::json!({"configured":false, "neutralized":neutralized}),
            Some(p) => serde_json::json!({
                "configured":true, "enabled":enabled, "neutralized":neutralized, "issuer":p.issuer(),
                "label":p.label(), "callback_url":self.external_callback_url(), "keys":p.key_diagnostics().await
            }),
        };
        let mut result = json_response(StatusCode::OK, &data)?;
        result
            .headers_mut()
            .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
        Ok(result)
    }

    pub(super) async fn external_provider_policy(
        &self,
        headers: &HeaderMap,
        body: Body,
        auth: &Auth,
    ) -> HttpResult<Response> {
        let provider = self.external_provider()?;
        self.form_origin(headers)?;
        let fields = read_form(headers, body, &["csrf_token", "enabled"]).await?;
        self.verify_csrf(headers, required(&fields, "csrf_token")?)
            .await?;
        let enabled = match required(&fields, "enabled")? {
            "true" => true,
            "false" => false,
            _ => return Err(bad("Invalid external provider status.")),
        };
        let session = self
            .session(headers)?
            .ok_or_else(|| bad("Sign in before changing provider status."))?;
        auth.set_external_enabled(provider, &session, enabled)
            .await
            .map_err(auth_error)?;
        Ok(response(StatusCode::NO_CONTENT, ""))
    }
}
