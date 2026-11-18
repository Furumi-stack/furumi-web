use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::sync::LazyLock;
use std::time::Instant;

use cot::db::Database;
use cot::request::extractors::UrlQuery;
use cot::session::Session;
use openidconnect::core::{CoreClient, CoreProviderMetadata};
use openidconnect::{
    AuthorizationCode, ClientId, ClientSecret, CsrfToken, EndpointMaybeSet, EndpointNotSet,
    EndpointSet, IssuerUrl, Nonce, PkceCodeChallenge, PkceCodeVerifier, RedirectUrl, Scope,
};

use cot::request::RequestHead;
use cot::request::extractors::FromRequestHead;

use crate::auth;
use crate::config::AppConfig;
use crate::i18n::I18n;
use crate::user::{OidcLink, User};

// ---------------------------------------------------------------------------
// Request origin extractor (scheme + host from headers)
// ---------------------------------------------------------------------------

/// Extracts the origin (e.g. "http://127.0.0.1:3001") from the request so we
/// can build the correct OIDC redirect URI.
pub struct RequestOrigin(pub String);

impl FromRequestHead for RequestOrigin {
    async fn from_request_head(head: &RequestHead) -> cot::Result<Self> {
        let scheme = head
            .headers
            .get("x-forwarded-proto")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("http");

        let host = head
            .headers
            .get(cot::http::header::HOST)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("localhost");

        Ok(RequestOrigin(format!("{scheme}://{host}")))
    }
}

// ---------------------------------------------------------------------------
// Session keys for OIDC flow state
// ---------------------------------------------------------------------------

const SESSION_CSRF_STATE: &str = "oidc_csrf_state";
const SESSION_NONCE: &str = "oidc_nonce";
const SESSION_PKCE_VERIFIER: &str = "oidc_pkce_verifier";
const SESSION_REDIRECT_URI: &str = "oidc_redirect_uri";

const SESSION_MOBILE_CSRF_STATE: &str = "mobile_oidc_csrf_state";
const SESSION_MOBILE_NONCE: &str = "mobile_oidc_nonce";
const SESSION_MOBILE_PKCE_VERIFIER: &str = "mobile_oidc_pkce_verifier";
const SESSION_MOBILE_PROVIDER_REDIRECT_URI: &str = "mobile_oidc_provider_redirect_uri";
const SESSION_MOBILE_APP_REDIRECT_URI: &str = "mobile_oidc_app_redirect_uri";
const DEFAULT_MOBILE_REDIRECT_URI: &str = "furumi://auth/callback";

// ---------------------------------------------------------------------------
// Provider cache
// ---------------------------------------------------------------------------

/// Concrete client type returned by `from_provider_metadata` + `set_redirect_uri`.
/// The provider metadata discovery sets auth URL to EndpointSet, and token/userinfo
/// endpoints to EndpointMaybeSet. The remaining endpoints stay EndpointNotSet.
type ConfiguredClient = CoreClient<
    EndpointSet,
    EndpointNotSet,
    EndpointNotSet,
    EndpointNotSet,
    EndpointMaybeSet,
    EndpointMaybeSet,
>;

struct CachedProvider {
    client: ConfiguredClient,
    fetched_at: Instant,
    config_hash: u64,
}

static PROVIDER_CACHE: LazyLock<tokio::sync::RwLock<Option<CachedProvider>>> =
    LazyLock::new(|| tokio::sync::RwLock::new(None));

/// TTL for cached provider metadata (1 hour).
const PROVIDER_TTL_SECS: u64 = 3600;

/// Compute a hash of the OIDC configuration values so we can detect changes.
fn config_hash(issuer: &str, client_id: &str, client_secret: &str) -> u64 {
    let mut hasher = DefaultHasher::new();
    issuer.hash(&mut hasher);
    client_id.hash(&mut hasher);
    client_secret.hash(&mut hasher);
    hasher.finish()
}

fn oidc_http_client() -> reqwest::Client {
    reqwest::ClientBuilder::new()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .expect("valid reqwest client")
}

/// Get or refresh the cached OIDC provider. Returns a cloned `ConfiguredClient`.
async fn get_or_refresh_provider(
    config: &AppConfig,
    http: &reqwest::Client,
) -> Result<ConfiguredClient, String> {
    let hash = config_hash(
        &config.oidc_issuer,
        &config.oidc_client_id,
        &config.oidc_client_secret,
    );

    // Fast path: check if we have a valid cached provider.
    {
        let cache = PROVIDER_CACHE.read().await;
        if let Some(ref cached) = *cache {
            if cached.config_hash == hash
                && cached.fetched_at.elapsed().as_secs() < PROVIDER_TTL_SECS
            {
                return Ok(cached.client.clone());
            }
        }
    }

    // Slow path: discover provider metadata + JWKS.
    // Strip /.well-known/openid-configuration suffix if the user pasted the
    // full discovery URL, so discover_async doesn't double-append it.
    let issuer = config
        .oidc_issuer
        .trim_end_matches('/')
        .strip_suffix("/.well-known/openid-configuration")
        .unwrap_or(config.oidc_issuer.trim_end_matches('/'))
        .to_owned();

    let issuer_url = IssuerUrl::new(issuer).map_err(|e| format!("invalid issuer URL: {e}"))?;

    let metadata = CoreProviderMetadata::discover_async(issuer_url, http)
        .await
        .map_err(|e| format!("OIDC discovery failed: {e}"))?;

    let client = CoreClient::from_provider_metadata(
        metadata,
        ClientId::new(config.oidc_client_id.clone()),
        Some(ClientSecret::new(config.oidc_client_secret.clone())),
    );

    let mut cache = PROVIDER_CACHE.write().await;
    *cache = Some(CachedProvider {
        client: client.clone(),
        fetched_at: Instant::now(),
        config_hash: hash,
    });

    Ok(client)
}

// ---------------------------------------------------------------------------
// GET /auth/oidc/start
// ---------------------------------------------------------------------------

pub async fn oidc_start_handler(
    origin: RequestOrigin,
    i18n: I18n,
    db: Database,
    session: Session,
) -> cot::Result<cot::response::Response> {
    let (config, _) = AppConfig::load_with_db(&db).await;

    // Validate SSO is enabled and configured.
    if !config.auth_sso_enabled
        || config.oidc_issuer.is_empty()
        || config.oidc_client_id.is_empty()
        || config.oidc_client_secret.is_empty()
    {
        tracing::warn!("OIDC start requested but SSO is not configured");
        crate::metrics::record_auth_attempt("oidc", "failure", "not_configured");
        return redirect_login_with_error(i18n.t.login_sso_disabled);
    }

    let http = oidc_http_client();
    let client = match get_or_refresh_provider(&config, &http).await {
        Ok(c) => c,
        Err(e) => {
            tracing::error!("OIDC provider error: {e}");
            crate::metrics::record_auth_attempt("oidc", "failure", "provider_error");
            return redirect_login_with_error(i18n.t.login_oidc_error);
        }
    };

    // Build redirect URI from the actual request origin.
    let redirect_uri_str = format!("{}/auth/oidc/callback", origin.0);
    let redirect_url = RedirectUrl::new(redirect_uri_str.clone())
        .map_err(|e| cot::Error::internal(format!("bad redirect URI: {e}")))?;
    let client = client.set_redirect_uri(redirect_url);
    tracing::info!(
        redirect_uri = %redirect_uri_str,
        oidc_issuer = %config.oidc_issuer,
        "OIDC start: building authorization request",
    );

    // Build PKCE challenge.
    let (pkce_challenge, pkce_verifier) = PkceCodeChallenge::new_random_sha256();

    // Build authorization URL.
    // The openid scope is added automatically by the crate; only add email + profile.
    let (auth_url, csrf_state, nonce) = client
        .authorize_url(
            openidconnect::AuthenticationFlow::<openidconnect::core::CoreResponseType>::AuthorizationCode,
            CsrfToken::new_random,
            Nonce::new_random,
        )
        .add_scope(Scope::new("email".to_string()))
        .add_scope(Scope::new("profile".to_string()))
        .set_pkce_challenge(pkce_challenge)
        .url();
    tracing::info!(auth_url = %auth_url, "OIDC start: redirecting to provider");

    // Store OIDC flow state in the session.
    session
        .insert(SESSION_CSRF_STATE, csrf_state.secret().clone())
        .await
        .map_err(|e| cot::Error::internal(e.to_string()))?;
    session
        .insert(SESSION_NONCE, nonce.secret().clone())
        .await
        .map_err(|e| cot::Error::internal(e.to_string()))?;
    session
        .insert(SESSION_PKCE_VERIFIER, pkce_verifier.secret().clone())
        .await
        .map_err(|e| cot::Error::internal(e.to_string()))?;
    session
        .insert(SESSION_REDIRECT_URI, redirect_uri_str)
        .await
        .map_err(|e| cot::Error::internal(e.to_string()))?;

    Ok(auth::redirect(auth_url.as_str()))
}

// ---------------------------------------------------------------------------
// GET /auth/oidc/callback
// ---------------------------------------------------------------------------

use serde::Deserialize;

#[derive(Deserialize)]
pub struct OidcCallbackQuery {
    code: String,
    state: String,
}

#[derive(Deserialize)]
pub struct MobileOidcStartQuery {
    redirect_uri: Option<String>,
}

#[derive(Deserialize)]
pub struct MobileOidcCallbackQuery {
    code: Option<String>,
    state: Option<String>,
    error: Option<String>,
}

pub async fn oidc_callback_handler(
    i18n: I18n,
    db: Database,
    session: Session,
    UrlQuery(query): UrlQuery<OidcCallbackQuery>,
) -> cot::Result<cot::response::Response> {
    let (config, _) = AppConfig::load_with_db(&db).await;

    // Retrieve OIDC flow state from the session.
    let saved_csrf: Option<String> = session
        .get(SESSION_CSRF_STATE)
        .await
        .map_err(|e| cot::Error::internal(e.to_string()))?;
    let saved_nonce: Option<String> = session
        .get(SESSION_NONCE)
        .await
        .map_err(|e| cot::Error::internal(e.to_string()))?;
    let saved_pkce: Option<String> = session
        .get(SESSION_PKCE_VERIFIER)
        .await
        .map_err(|e| cot::Error::internal(e.to_string()))?;
    let saved_redirect_uri: Option<String> = session
        .get(SESSION_REDIRECT_URI)
        .await
        .map_err(|e| cot::Error::internal(e.to_string()))?;

    // Validate CSRF state.
    let Some(saved_csrf) = saved_csrf else {
        tracing::warn!("OIDC callback: no CSRF state in session");
        crate::metrics::record_auth_attempt("oidc", "failure", "missing_state");
        return redirect_login_with_error(i18n.t.login_oidc_error);
    };
    if query.state != saved_csrf {
        tracing::warn!("OIDC callback: CSRF state mismatch");
        crate::metrics::record_auth_attempt("oidc", "failure", "csrf");
        return redirect_login_with_error(i18n.t.login_oidc_error);
    }

    let Some(nonce_str) = saved_nonce else {
        tracing::warn!("OIDC callback: no nonce in session");
        crate::metrics::record_auth_attempt("oidc", "failure", "missing_nonce");
        return redirect_login_with_error(i18n.t.login_oidc_error);
    };
    let Some(pkce_str) = saved_pkce else {
        tracing::warn!("OIDC callback: no PKCE verifier in session");
        crate::metrics::record_auth_attempt("oidc", "failure", "missing_pkce");
        return redirect_login_with_error(i18n.t.login_oidc_error);
    };

    let nonce = Nonce::new(nonce_str);
    let pkce_verifier = PkceCodeVerifier::new(pkce_str);

    let http = oidc_http_client();
    let client = match get_or_refresh_provider(&config, &http).await {
        Ok(c) => c,
        Err(e) => {
            tracing::error!("OIDC provider error during callback: {e}");
            crate::metrics::record_auth_attempt("oidc", "failure", "provider_error");
            return redirect_login_with_error(i18n.t.login_oidc_error);
        }
    };

    // Restore the redirect URI that was used in the authorization request.
    let client = if let Some(ref uri) = saved_redirect_uri {
        let redirect_url = RedirectUrl::new(uri.clone())
            .map_err(|e| cot::Error::internal(format!("bad redirect URI from session: {e}")))?;
        client.set_redirect_uri(redirect_url)
    } else {
        client
    };

    // Exchange code for tokens.
    let token_request = match client.exchange_code(AuthorizationCode::new(query.code.clone())) {
        Ok(req) => req,
        Err(e) => {
            tracing::error!("OIDC token endpoint not configured: {e}");
            crate::metrics::record_auth_attempt("oidc", "failure", "token_config");
            return redirect_login_with_error(i18n.t.login_oidc_error);
        }
    };
    let token_response = token_request
        .set_pkce_verifier(pkce_verifier)
        .request_async(&http)
        .await;

    let token_response = match token_response {
        Ok(t) => t,
        Err(e) => {
            tracing::error!("OIDC token exchange failed: {e}");
            crate::metrics::record_auth_attempt("oidc", "failure", "token_exchange");
            return redirect_login_with_error(i18n.t.login_oidc_error);
        }
    };

    // Verify and extract ID token claims.
    use openidconnect::TokenResponse;
    let id_token = match token_response.id_token() {
        Some(t) => t,
        None => {
            tracing::error!("OIDC response missing ID token");
            crate::metrics::record_auth_attempt("oidc", "failure", "missing_id_token");
            return redirect_login_with_error(i18n.t.login_oidc_error);
        }
    };

    let claims = match id_token.claims(&client.id_token_verifier(), &nonce) {
        Ok(c) => c,
        Err(e) => {
            tracing::error!("OIDC ID token verification failed: {e}");
            crate::metrics::record_auth_attempt("oidc", "failure", "id_token_verify");
            return redirect_login_with_error(i18n.t.login_oidc_error);
        }
    };

    let sub = claims.subject().to_string();
    let issuer = claims.issuer().to_string();
    let email = claims.email().map(|e| e.to_string());
    let name = claims
        .name()
        .and_then(|n| n.get(None))
        .map(|n| n.to_string());

    // Extract groups from the raw JWT payload (second dot-separated segment).
    // The token is already signature-verified above, so we only need to decode
    // the payload to read the non-standard `groups` claim.
    let groups: Vec<String> = (|| {
        use base64::Engine;
        let raw = id_token.to_string();
        let payload_b64 = raw.split('.').nth(1)?;
        // JWT payloads use URL-safe base64; try without padding first, then
        // fall back to the padded variant (some providers add trailing '=').
        let payload_bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .decode(payload_b64)
            .or_else(|_| base64::engine::general_purpose::URL_SAFE.decode(payload_b64))
            .ok()?;
        let value: serde_json::Value = serde_json::from_slice(&payload_bytes).ok()?;
        let arr = value.get("groups")?.as_array()?;
        Some(
            arr.iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect(),
        )
    })()
    .unwrap_or_default();

    tracing::info!(
        "OIDC login: sub={sub}, groups={groups:?}, admin_groups={:?}, user_groups={:?}",
        config.oidc_admin_groups,
        config.oidc_user_groups,
    );

    // User provisioning logic.
    let user = match provision_user(
        &db,
        &issuer,
        &sub,
        email.as_deref(),
        name.as_deref(),
        &groups,
        &config.oidc_admin_groups,
        &config.oidc_user_groups,
    )
    .await
    {
        Ok(u) => u,
        Err(e) => {
            tracing::error!("OIDC user provisioning failed: {e}");
            crate::metrics::record_auth_attempt("oidc", "failure", "provisioning");
            return redirect_login_with_error(i18n.t.login_oidc_error);
        }
    };

    let redirect_to = auth::get_post_login_redirect(&session)
        .await?
        .unwrap_or_else(|| "/".to_string());

    // Log the user in.
    auth::login(&session, user.id_val()).await?;
    auth::clear_post_login_redirect(&session).await?;
    crate::metrics::record_auth_attempt("oidc", "success", "ok");
    crate::metrics::record_session_created("oidc");

    // Clear OIDC session keys.
    let _: Option<String> = session
        .remove(SESSION_CSRF_STATE)
        .await
        .map_err(|e| cot::Error::internal(e.to_string()))?;
    let _: Option<String> = session
        .remove(SESSION_NONCE)
        .await
        .map_err(|e| cot::Error::internal(e.to_string()))?;
    let _: Option<String> = session
        .remove(SESSION_PKCE_VERIFIER)
        .await
        .map_err(|e| cot::Error::internal(e.to_string()))?;
    let _: Option<String> = session
        .remove(SESSION_REDIRECT_URI)
        .await
        .map_err(|e| cot::Error::internal(e.to_string()))?;

    Ok(auth::redirect(&redirect_to))
}

// ---------------------------------------------------------------------------
// Mobile OIDC flow
// ---------------------------------------------------------------------------

pub async fn oidc_mobile_start_handler(
    origin: RequestOrigin,
    db: Database,
    session: Session,
    UrlQuery(query): UrlQuery<MobileOidcStartQuery>,
) -> cot::Result<cot::response::Response> {
    let Some(app_redirect_uri) = safe_mobile_redirect_uri(query.redirect_uri.as_deref()) else {
        crate::metrics::record_auth_attempt("mobile_oidc", "failure", "bad_redirect_uri");
        return Ok(text_response(
            cot::http::StatusCode::BAD_REQUEST,
            "invalid mobile redirect_uri",
        ));
    };

    let (config, _) = AppConfig::load_with_db(&db).await;

    if !config.auth_sso_enabled
        || config.oidc_issuer.is_empty()
        || config.oidc_client_id.is_empty()
        || config.oidc_client_secret.is_empty()
    {
        tracing::warn!("Mobile OIDC start requested but SSO is not configured");
        crate::metrics::record_auth_attempt("mobile_oidc", "failure", "not_configured");
        return Ok(mobile_redirect_error(
            &app_redirect_uri,
            "sso_not_configured",
        ));
    }

    let http = oidc_http_client();
    let client = match get_or_refresh_provider(&config, &http).await {
        Ok(c) => c,
        Err(e) => {
            tracing::error!("Mobile OIDC provider error: {e}");
            crate::metrics::record_auth_attempt("mobile_oidc", "failure", "provider_error");
            return Ok(mobile_redirect_error(&app_redirect_uri, "provider_error"));
        }
    };

    let provider_redirect_uri = format!("{}/auth/mobile/oidc/callback", origin.0);
    let redirect_url = RedirectUrl::new(provider_redirect_uri.clone())
        .map_err(|e| cot::Error::internal(format!("bad mobile redirect URI: {e}")))?;
    let client = client.set_redirect_uri(redirect_url);

    let (pkce_challenge, pkce_verifier) = PkceCodeChallenge::new_random_sha256();
    let (auth_url, csrf_state, nonce) = client
        .authorize_url(
            openidconnect::AuthenticationFlow::<openidconnect::core::CoreResponseType>::AuthorizationCode,
            CsrfToken::new_random,
            Nonce::new_random,
        )
        .add_scope(Scope::new("email".to_string()))
        .add_scope(Scope::new("profile".to_string()))
        .set_pkce_challenge(pkce_challenge)
        .url();

    session
        .insert(SESSION_MOBILE_CSRF_STATE, csrf_state.secret().clone())
        .await
        .map_err(|e| cot::Error::internal(e.to_string()))?;
    session
        .insert(SESSION_MOBILE_NONCE, nonce.secret().clone())
        .await
        .map_err(|e| cot::Error::internal(e.to_string()))?;
    session
        .insert(SESSION_MOBILE_PKCE_VERIFIER, pkce_verifier.secret().clone())
        .await
        .map_err(|e| cot::Error::internal(e.to_string()))?;
    session
        .insert(
            SESSION_MOBILE_PROVIDER_REDIRECT_URI,
            provider_redirect_uri.clone(),
        )
        .await
        .map_err(|e| cot::Error::internal(e.to_string()))?;
    session
        .insert(SESSION_MOBILE_APP_REDIRECT_URI, app_redirect_uri)
        .await
        .map_err(|e| cot::Error::internal(e.to_string()))?;

    tracing::info!(
        auth_url = %auth_url,
        provider_redirect_uri = %provider_redirect_uri,
        "Mobile OIDC start: redirecting to provider",
    );

    Ok(auth::redirect(auth_url.as_str()))
}

pub async fn oidc_mobile_callback_handler(
    db: Database,
    session: Session,
    UrlQuery(query): UrlQuery<MobileOidcCallbackQuery>,
) -> cot::Result<cot::response::Response> {
    let app_redirect_uri = mobile_app_redirect_uri_from_session(&session).await?;

    if query.error.is_some() {
        tracing::warn!("Mobile OIDC callback returned provider error");
        crate::metrics::record_auth_attempt("mobile_oidc", "failure", "provider_denied");
        clear_mobile_oidc_session(&session).await?;
        return Ok(mobile_redirect_error(&app_redirect_uri, "provider_denied"));
    }

    let Some(code) = query.code else {
        crate::metrics::record_auth_attempt("mobile_oidc", "failure", "missing_code");
        clear_mobile_oidc_session(&session).await?;
        return Ok(mobile_redirect_error(&app_redirect_uri, "missing_code"));
    };
    let Some(state) = query.state else {
        crate::metrics::record_auth_attempt("mobile_oidc", "failure", "missing_state");
        clear_mobile_oidc_session(&session).await?;
        return Ok(mobile_redirect_error(&app_redirect_uri, "missing_state"));
    };

    let saved_csrf: Option<String> = session
        .get(SESSION_MOBILE_CSRF_STATE)
        .await
        .map_err(|e| cot::Error::internal(e.to_string()))?;
    let saved_nonce: Option<String> = session
        .get(SESSION_MOBILE_NONCE)
        .await
        .map_err(|e| cot::Error::internal(e.to_string()))?;
    let saved_pkce: Option<String> = session
        .get(SESSION_MOBILE_PKCE_VERIFIER)
        .await
        .map_err(|e| cot::Error::internal(e.to_string()))?;
    let provider_redirect_uri: Option<String> = session
        .get(SESSION_MOBILE_PROVIDER_REDIRECT_URI)
        .await
        .map_err(|e| cot::Error::internal(e.to_string()))?;

    let Some(saved_csrf) = saved_csrf else {
        tracing::warn!("Mobile OIDC callback: no CSRF state in session");
        crate::metrics::record_auth_attempt("mobile_oidc", "failure", "missing_state");
        clear_mobile_oidc_session(&session).await?;
        return Ok(mobile_redirect_error(&app_redirect_uri, "missing_state"));
    };
    if state != saved_csrf {
        tracing::warn!("Mobile OIDC callback: CSRF state mismatch");
        crate::metrics::record_auth_attempt("mobile_oidc", "failure", "csrf");
        clear_mobile_oidc_session(&session).await?;
        return Ok(mobile_redirect_error(&app_redirect_uri, "csrf"));
    }

    let Some(nonce_str) = saved_nonce else {
        crate::metrics::record_auth_attempt("mobile_oidc", "failure", "missing_nonce");
        clear_mobile_oidc_session(&session).await?;
        return Ok(mobile_redirect_error(&app_redirect_uri, "missing_nonce"));
    };
    let Some(pkce_str) = saved_pkce else {
        crate::metrics::record_auth_attempt("mobile_oidc", "failure", "missing_pkce");
        clear_mobile_oidc_session(&session).await?;
        return Ok(mobile_redirect_error(&app_redirect_uri, "missing_pkce"));
    };
    let Some(provider_redirect_uri) = provider_redirect_uri else {
        crate::metrics::record_auth_attempt("mobile_oidc", "failure", "missing_redirect_uri");
        clear_mobile_oidc_session(&session).await?;
        return Ok(mobile_redirect_error(
            &app_redirect_uri,
            "missing_redirect_uri",
        ));
    };

    let (config, _) = AppConfig::load_with_db(&db).await;
    if !config.auth_sso_enabled
        || config.oidc_issuer.is_empty()
        || config.oidc_client_id.is_empty()
        || config.oidc_client_secret.is_empty()
    {
        crate::metrics::record_auth_attempt("mobile_oidc", "failure", "not_configured");
        clear_mobile_oidc_session(&session).await?;
        return Ok(mobile_redirect_error(
            &app_redirect_uri,
            "sso_not_configured",
        ));
    }

    let http = oidc_http_client();
    let client = match get_or_refresh_provider(&config, &http).await {
        Ok(c) => c,
        Err(e) => {
            tracing::error!("Mobile OIDC provider error during callback: {e}");
            crate::metrics::record_auth_attempt("mobile_oidc", "failure", "provider_error");
            clear_mobile_oidc_session(&session).await?;
            return Ok(mobile_redirect_error(&app_redirect_uri, "provider_error"));
        }
    };
    let redirect_url = RedirectUrl::new(provider_redirect_uri)
        .map_err(|e| cot::Error::internal(format!("bad mobile redirect URI from session: {e}")))?;
    let client = client.set_redirect_uri(redirect_url);

    let token_request = match client.exchange_code(AuthorizationCode::new(code)) {
        Ok(req) => req,
        Err(e) => {
            tracing::error!("Mobile OIDC token endpoint not configured: {e}");
            crate::metrics::record_auth_attempt("mobile_oidc", "failure", "token_config");
            clear_mobile_oidc_session(&session).await?;
            return Ok(mobile_redirect_error(&app_redirect_uri, "oidc_error"));
        }
    };
    let token_response = token_request
        .set_pkce_verifier(PkceCodeVerifier::new(pkce_str))
        .request_async(&http)
        .await;
    let token_response = match token_response {
        Ok(t) => t,
        Err(e) => {
            tracing::error!("Mobile OIDC token exchange failed: {e}");
            crate::metrics::record_auth_attempt("mobile_oidc", "failure", "token_exchange");
            clear_mobile_oidc_session(&session).await?;
            return Ok(mobile_redirect_error(&app_redirect_uri, "oidc_error"));
        }
    };

    use openidconnect::TokenResponse;
    let id_token = match token_response.id_token() {
        Some(t) => t,
        None => {
            tracing::error!("Mobile OIDC response missing ID token");
            crate::metrics::record_auth_attempt("mobile_oidc", "failure", "missing_id_token");
            clear_mobile_oidc_session(&session).await?;
            return Ok(mobile_redirect_error(&app_redirect_uri, "oidc_error"));
        }
    };

    let nonce = Nonce::new(nonce_str);
    let claims = match id_token.claims(&client.id_token_verifier(), &nonce) {
        Ok(c) => c,
        Err(e) => {
            tracing::error!("Mobile OIDC ID token verification failed: {e}");
            crate::metrics::record_auth_attempt("mobile_oidc", "failure", "id_token_verify");
            clear_mobile_oidc_session(&session).await?;
            return Ok(mobile_redirect_error(&app_redirect_uri, "oidc_error"));
        }
    };

    let sub = claims.subject().to_string();
    let issuer = claims.issuer().to_string();
    let email = claims.email().map(|e| e.to_string());
    let name = claims
        .name()
        .and_then(|n| n.get(None))
        .map(|n| n.to_string());
    let groups = extract_groups_from_jwt(&id_token.to_string());

    let user = match provision_user(
        &db,
        &issuer,
        &sub,
        email.as_deref(),
        name.as_deref(),
        &groups,
        &config.oidc_admin_groups,
        &config.oidc_user_groups,
    )
    .await
    {
        Ok(u) => u,
        Err(e) => {
            tracing::error!("Mobile OIDC user provisioning failed: {e}");
            crate::metrics::record_auth_attempt("mobile_oidc", "failure", "provisioning");
            clear_mobile_oidc_session(&session).await?;
            return Ok(mobile_redirect_error(&app_redirect_uri, "oidc_error"));
        }
    };

    if !user.is_active() {
        clear_mobile_oidc_session(&session).await?;
        return Ok(mobile_redirect_error(&app_redirect_uri, "approval_pending"));
    }

    let exchange_code = auth::create_mobile_exchange_code(&db, user.id_val())
        .await
        .map_err(|e| cot::Error::internal(e.to_string()))?;
    clear_mobile_oidc_session(&session).await?;

    crate::metrics::record_auth_attempt("mobile_oidc", "success", "ok");
    crate::metrics::record_session_created("mobile_oidc");
    Ok(mobile_redirect_success(&app_redirect_uri, &exchange_code))
}

// ---------------------------------------------------------------------------
// User provisioning
// ---------------------------------------------------------------------------

/// Group-derived access always takes precedence over local approval.
fn resolve_sso_role(
    groups: &[String],
    admin_groups: &str,
    user_groups: &str,
) -> Option<&'static str> {
    let matches = |configured: &str| {
        configured
            .split(',')
            .map(str::trim)
            .filter(|group| !group.is_empty())
            .any(|group| groups.iter().any(|g| g == group))
    };
    if matches(admin_groups) {
        Some("admin")
    } else if matches(user_groups) {
        Some("user")
    } else {
        None
    }
}

async fn provision_user(
    db: &Database,
    issuer: &str,
    sub: &str,
    email: Option<&str>,
    name: Option<&str>,
    groups: &[String],
    admin_groups: &str,
    user_groups: &str,
) -> Result<User, String> {
    let role = resolve_sso_role(groups, admin_groups, user_groups);

    // 1. Check for existing OIDC link.
    if let Some(mut link) = OidcLink::find_by_issuer_sub(db, issuer, sub)
        .await
        .map_err(|e| format!("DB error finding OIDC link: {e}"))?
    {
        // Fetch the linked user.
        match User::get_by_id(db, link.user_id()).await {
            Ok(Some(mut user)) => {
                // Update cached claims.
                link.update_claims(db, email, name)
                    .await
                    .map_err(|e| format!("DB error updating OIDC link: {e}"))?;

                // Refresh only the SSO grant; preserve independent local approval.
                user.sync_sso_role(db, role)
                    .await
                    .map_err(|e| format!("DB error updating user role: {e}"))?;

                return Ok(user);
            }
            Ok(None) => {
                // User was deleted but the OIDC link is stale — remove it
                // and fall through to re-create the user below.
                tracing::warn!(
                    "OIDC link points to deleted user {}; removing stale link",
                    link.user_id(),
                );
                link.delete(db)
                    .await
                    .map_err(|e| format!("DB error deleting stale OIDC link: {e}"))?;
            }
            Err(e) => return Err(format!("DB error fetching user: {e}")),
        }
    }

    // 3. Create a brand-new user + OIDC link.
    // Generate a unique username from the sub or email.
    let username = if let Some(email_str) = email {
        email_str.split('@').next().unwrap_or(sub).to_owned()
    } else {
        sub.to_owned()
    };

    // Ensure username uniqueness by appending a suffix if needed.
    let mut candidate = username.clone();
    let mut suffix = 0u32;
    loop {
        match User::get_by_username(db, &candidate).await {
            Ok(None) => break,
            Ok(Some(_)) => {
                suffix += 1;
                candidate = format!("{username}_{suffix}");
            }
            Err(e) => return Err(format!("DB error checking username: {e}")),
        }
    }

    let user = User::create_oidc(db, &candidate, email, name, role)
        .await
        .map_err(|e| format!("DB error creating user: {e}"))?;

    OidcLink::create_link(db, user.id_val(), issuer, sub, email, name)
        .await
        .map_err(|e| format!("DB error creating OIDC link: {e}"))?;

    Ok(user)
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn redirect_login_with_error(message: &str) -> cot::Result<cot::response::Response> {
    let encoded = urlencoded(message);
    Ok(auth::redirect(&format!("/login?error={encoded}")))
}

fn text_response(status: cot::http::StatusCode, message: &str) -> cot::response::Response {
    cot::http::Response::builder()
        .status(status)
        .body(cot::Body::fixed(message.to_owned()))
        .expect("valid response")
}

async fn mobile_app_redirect_uri_from_session(session: &Session) -> cot::Result<String> {
    let saved: Option<String> = session
        .get(SESSION_MOBILE_APP_REDIRECT_URI)
        .await
        .map_err(|e| cot::Error::internal(e.to_string()))?;
    Ok(safe_mobile_redirect_uri(saved.as_deref())
        .unwrap_or_else(|| DEFAULT_MOBILE_REDIRECT_URI.to_owned()))
}

async fn clear_mobile_oidc_session(session: &Session) -> cot::Result<()> {
    let _: Option<String> = session
        .remove(SESSION_MOBILE_CSRF_STATE)
        .await
        .map_err(|e| cot::Error::internal(e.to_string()))?;
    let _: Option<String> = session
        .remove(SESSION_MOBILE_NONCE)
        .await
        .map_err(|e| cot::Error::internal(e.to_string()))?;
    let _: Option<String> = session
        .remove(SESSION_MOBILE_PKCE_VERIFIER)
        .await
        .map_err(|e| cot::Error::internal(e.to_string()))?;
    let _: Option<String> = session
        .remove(SESSION_MOBILE_PROVIDER_REDIRECT_URI)
        .await
        .map_err(|e| cot::Error::internal(e.to_string()))?;
    let _: Option<String> = session
        .remove(SESSION_MOBILE_APP_REDIRECT_URI)
        .await
        .map_err(|e| cot::Error::internal(e.to_string()))?;
    Ok(())
}

fn safe_mobile_redirect_uri(raw: Option<&str>) -> Option<String> {
    let value = raw
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or(DEFAULT_MOBILE_REDIRECT_URI);
    if value.len() > 2048 || value.bytes().any(|b| matches!(b, b'\r' | b'\n')) {
        return None;
    }

    let lower = value.to_ascii_lowercase();
    if lower.starts_with("furumi://") || lower.starts_with("furumusic://") {
        return Some(value.to_owned());
    }
    if is_loopback_http_redirect(&lower) {
        return Some(value.to_owned());
    }
    None
}

/// RFC 8252 §7.3: native apps without a custom URL scheme (the CLI client)
/// receive the callback on a loopback listener with an ephemeral port.
fn is_loopback_http_redirect(lower: &str) -> bool {
    let Some(rest) = lower.strip_prefix("http://") else {
        return false;
    };
    let host_port = rest.split(['/', '?', '#']).next().unwrap_or("");
    let Some((host, port)) = host_port.rsplit_once(':') else {
        return false;
    };
    matches!(host, "127.0.0.1" | "localhost" | "[::1]")
        && !port.is_empty()
        && port.len() <= 5
        && port.bytes().all(|b| b.is_ascii_digit())
}

fn mobile_redirect_success(app_redirect_uri: &str, code: &str) -> cot::response::Response {
    let deep_link = append_query_param(app_redirect_uri, "code", code);
    if is_loopback_http_redirect(&app_redirect_uri.to_ascii_lowercase()) {
        return auth::redirect(&deep_link);
    }
    mobile_deep_link_page(
        "success",
        "Sign-in complete",
        "Furumi should open automatically. If it doesn't, use the button or copy the code below.",
        None,
        Some(code),
        &deep_link,
    )
}

fn mobile_redirect_error(app_redirect_uri: &str, error: &str) -> cot::response::Response {
    let deep_link = append_query_param(app_redirect_uri, "error", error);
    if is_loopback_http_redirect(&app_redirect_uri.to_ascii_lowercase()) {
        return auth::redirect(&deep_link);
    }
    mobile_deep_link_page(
        "error",
        "Sign-in failed",
        "Furumi should open automatically and show the sign-in error.",
        Some(error),
        None,
        &deep_link,
    )
}

fn mobile_deep_link_page(
    state: &str,
    title: &str,
    message: &str,
    detail: Option<&str>,
    code: Option<&str>,
    deep_link: &str,
) -> cot::response::Response {
    let state_class = html_escape(state);
    let title_html = html_escape(title);
    let message_html = html_escape(message);
    let detail_html = detail
        .map(|value| format!(r#"<p class="detail">Reason: {}</p>"#, html_escape(value)))
        .unwrap_or_default();
    let code_html = code
        .map(|value| {
            format!(
                r#"<p class="hint">Signing in from a terminal? Paste this code there:</p>
    <input class="code" readonly value="{}" onclick="this.select()">"#,
                html_escape(value)
            )
        })
        .unwrap_or_default();
    let deep_link_html = html_escape(deep_link);
    let deep_link_js =
        serde_json::to_string(deep_link).expect("serializing URL string cannot fail");

    let html = format!(
        r#"<!doctype html>
<html lang="en">
<head>
  <meta charset="utf-8">
  <meta name="viewport" content="width=device-width, initial-scale=1">
  <title>{title_html}</title>
  <style>
    :root {{
      color-scheme: light dark;
      font-family: -apple-system, BlinkMacSystemFont, "Segoe UI", sans-serif;
      background: #101114;
      color: #f5f2ea;
    }}
    body {{
      min-height: 100vh;
      margin: 0;
      display: grid;
      place-items: center;
      padding: 24px;
      box-sizing: border-box;
    }}
    main {{
      width: min(420px, 100%);
      text-align: center;
    }}
    .mark {{
      width: 54px;
      height: 54px;
      margin: 0 auto 18px;
      border-radius: 999px;
      display: grid;
      place-items: center;
      font-size: 18px;
      font-weight: 700;
      background: #2f7d52;
      color: white;
    }}
    .mark.error {{
      background: #9d3d42;
    }}
    h1 {{
      margin: 0 0 10px;
      font-size: 26px;
      line-height: 1.15;
      letter-spacing: 0;
    }}
    p {{
      margin: 0;
      color: #c9c2b7;
      font-size: 15px;
      line-height: 1.55;
    }}
    .detail {{
      margin-top: 12px;
      color: #f1b3b7;
      overflow-wrap: anywhere;
    }}
    a {{
      display: inline-flex;
      align-items: center;
      justify-content: center;
      min-height: 44px;
      margin-top: 24px;
      padding: 0 18px;
      border-radius: 8px;
      background: #e8d8a8;
      color: #17150f;
      font-weight: 700;
      text-decoration: none;
    }}
    .hint {{
      margin-top: 14px;
      font-size: 13px;
      color: #89847c;
    }}
    .code {{
      width: 100%;
      margin-top: 8px;
      padding: 10px 12px;
      box-sizing: border-box;
      border: 1px solid #3a3c42;
      border-radius: 8px;
      background: #1a1c20;
      color: #e8d8a8;
      font-family: ui-monospace, SFMono-Regular, Menlo, Consolas, monospace;
      font-size: 13px;
      text-align: center;
    }}
  </style>
</head>
<body>
  <main>
    <div class="mark {state_class}" aria-hidden="true">{mark}</div>
    <h1>{title_html}</h1>
    <p>{message_html}</p>
    {detail_html}
    <a href="{deep_link_html}">Open Furumi</a>
    <p class="hint">If nothing happens, use the button above.</p>
    {code_html}
  </main>
  <script>
    const deepLink = {deep_link_js};
    window.setTimeout(() => {{
      window.location.href = deepLink;
    }}, 100);
  </script>
</body>
</html>"#,
        mark = if state == "error" { "!" } else { "OK" }
    );

    cot::http::Response::builder()
        .status(cot::http::StatusCode::OK)
        .header(cot::http::header::CONTENT_TYPE, "text/html; charset=utf-8")
        .header(cot::http::header::CACHE_CONTROL, "no-store")
        .body(cot::Body::fixed(html))
        .expect("valid response")
}

fn append_query_param(uri: &str, key: &str, value: &str) -> String {
    let (base, fragment) = uri.split_once('#').unwrap_or((uri, ""));
    let separator = if base.contains('?') { '&' } else { '?' };
    let mut out = format!("{base}{separator}{key}={}", urlencoded(value));
    if !fragment.is_empty() {
        out.push('#');
        out.push_str(fragment);
    }
    out
}

fn html_escape(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for ch in value.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            _ => out.push(ch),
        }
    }
    out
}

fn extract_groups_from_jwt(token: &str) -> Vec<String> {
    use base64::Engine;

    let Some(payload_b64) = token.split('.').nth(1) else {
        return Vec::new();
    };
    let Ok(payload_bytes) = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(payload_b64)
        .or_else(|_| base64::engine::general_purpose::URL_SAFE.decode(payload_b64))
    else {
        return Vec::new();
    };
    let Ok(value) = serde_json::from_slice::<serde_json::Value>(&payload_bytes) else {
        return Vec::new();
    };
    let Some(arr) = value.get("groups").and_then(|value| value.as_array()) else {
        return Vec::new();
    };
    arr.iter()
        .filter_map(|value| value.as_str().map(String::from))
        .collect()
}

/// Minimal percent-encoding for query parameter values.
fn urlencoded(s: &str) -> String {
    let mut out = String::with_capacity(s.len() * 2);
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char);
            }
            _ => {
                out.push('%');
                out.push_str(&format!("{b:02X}"));
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mobile_oidc_append_query_param_preserves_fragment() {
        assert_eq!(
            append_query_param("furumi://auth/callback#done", "code", "a b"),
            "furumi://auth/callback?code=a%20b#done"
        );
        assert_eq!(
            append_query_param("furumi://auth/callback?desktop=1", "error", "oidc_error"),
            "furumi://auth/callback?desktop=1&error=oidc_error"
        );
    }

    #[test]
    fn mobile_oidc_html_escape_escapes_page_values() {
        assert_eq!(
            html_escape(r#"<tag attr="x&y">'text'</tag>"#),
            "&lt;tag attr=&quot;x&amp;y&quot;&gt;&#39;text&#39;&lt;/tag&gt;"
        );
    }

    #[test]
    fn mobile_oidc_redirect_uri_allows_only_furumi_schemes() {
        assert_eq!(
            safe_mobile_redirect_uri(Some("furumi://auth/callback")).as_deref(),
            Some("furumi://auth/callback")
        );
        assert_eq!(
            safe_mobile_redirect_uri(Some("furumusic://auth/callback")).as_deref(),
            Some("furumusic://auth/callback")
        );
        assert!(safe_mobile_redirect_uri(Some("https://example.com/callback")).is_none());
    }

    #[test]
    fn mobile_oidc_redirect_uri_allows_loopback_http() {
        assert_eq!(
            safe_mobile_redirect_uri(Some("http://127.0.0.1:8753/callback")).as_deref(),
            Some("http://127.0.0.1:8753/callback")
        );
        assert_eq!(
            safe_mobile_redirect_uri(Some("http://localhost:1234/callback")).as_deref(),
            Some("http://localhost:1234/callback")
        );
        assert_eq!(
            safe_mobile_redirect_uri(Some("http://[::1]:1234/callback")).as_deref(),
            Some("http://[::1]:1234/callback")
        );
        // Non-loopback hosts, missing ports and https stay rejected.
        assert!(safe_mobile_redirect_uri(Some("http://127.0.0.1/callback")).is_none());
        assert!(safe_mobile_redirect_uri(Some("http://evil.com:80/callback")).is_none());
        assert!(safe_mobile_redirect_uri(Some("https://127.0.0.1:80/callback")).is_none());
        assert!(safe_mobile_redirect_uri(Some("http://127.0.0.1:notaport/x")).is_none());
    }
}

#[cfg(test)]
mod approval_tests {
    use super::*;

    #[test]
    fn matching_groups_are_required_and_admin_wins() {
        let groups = vec!["listeners".into(), "admins".into()];
        assert_eq!(
            resolve_sso_role(&groups, " admins ", "listeners"),
            Some("admin")
        );
        assert_eq!(
            resolve_sso_role(&groups, "other", "listeners"),
            Some("user")
        );
        assert_eq!(resolve_sso_role(&groups, "other", ""), None);
        assert_eq!(resolve_sso_role(&[], "", ""), None);
    }

    #[tokio::test]
    #[ignore = "requires a migrated disposable database in FURU_ACCESS_TEST_DATABASE_URL"]
    async fn postgres_approval_survives_sso_changes_without_implicit_grants() {
        let db = Database::new(std::env::var("FURU_ACCESS_TEST_DATABASE_URL").unwrap())
            .await
            .unwrap();
        let subject = format!("approval-test-{}", uuid::Uuid::new_v4());
        let provision = |groups: Vec<String>| {
            let db = &db;
            let subject = &subject;
            async move {
                provision_user(
                    db,
                    "https://test.example",
                    subject,
                    None,
                    Some("Test pending user"),
                    &groups,
                    "admins",
                    "listeners",
                )
                .await
                .unwrap()
            }
        };
        let first = provision(vec![]).await;
        assert!(!first.is_active());
        let mut again = provision(vec![]).await;
        assert_eq!(again.id_val(), first.id_val());
        again.approve(&db, "admin", 1).await.unwrap();
        assert_eq!(provision(vec![]).await.role(), auth::Role::Admin);
        assert_eq!(
            provision(vec!["listeners".into()]).await.role(),
            auth::Role::User
        );
        assert_eq!(
            provision(vec!["admins".into()]).await.role(),
            auth::Role::Admin
        );
        assert_eq!(provision(vec![]).await.role(), auth::Role::Admin);

        let auto_sub = format!("auto-{subject}");
        let automatic = provision_user(
            &db,
            "https://test.example",
            &auto_sub,
            None,
            None,
            &["listeners".into()],
            "admins",
            "listeners",
        )
        .await
        .unwrap();
        assert!(automatic.is_active());
        let removed = provision_user(
            &db,
            "https://test.example",
            &auto_sub,
            None,
            None,
            &[],
            "admins",
            "listeners",
        )
        .await
        .unwrap();
        assert!(!removed.is_active());
        assert_eq!(automatic.id_val(), removed.id_val());
        // A stale User held by an OIDC callback must retain a newer local grant.
        let mut stale = removed.clone();
        let mut approving = removed.clone();
        approving.approve(&db, "admin", 1).await.unwrap();
        stale.sync_sso_role(&db, Some("user")).await.unwrap();
        stale.sync_sso_role(&db, None).await.unwrap();
        assert!(stale.is_active());
        assert_eq!(stale.role(), auth::Role::Admin);
        let username = stale.username_str().to_owned();
        stale
            .update_fields(&db, &username, None, Some("Renamed"), None, "")
            .await
            .unwrap();
        assert!(!stale.is_active());
    }
}
