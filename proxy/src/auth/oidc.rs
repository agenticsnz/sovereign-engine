use std::sync::Arc;
use std::time::Instant;

use anyhow::{Context, Result};
use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Redirect, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use dashmap::DashMap;
use openidconnect::core::{
    CoreClient, CoreIdToken, CoreIdTokenClaims, CoreIdTokenVerifier, CoreProviderMetadata,
    CoreResponseType, CoreTokenResponse,
};
use openidconnect::{
    AuthenticationFlow, AuthorizationCode, ClientId, ClientSecret, CsrfToken, EndpointMaybeSet,
    EndpointNotSet, EndpointSet, IssuerUrl, Nonce, PkceCodeChallenge, PkceCodeVerifier,
    RedirectUrl, Scope, TokenResponse,
};
use tracing::{error, info, warn};

use crate::auth::{bootstrap, sessions};
use crate::db::models::IdpConfig;
use crate::db::Database;
use crate::AppState;

// ---------------------------------------------------------------------------
// Rate limiting for POST /auth/bootstrap-login
// ---------------------------------------------------------------------------

/// Per-IP attempt counter: (count, window_start).
/// Window is 60 seconds; max 5 attempts per window per IP.
static BOOTSTRAP_RATE_LIMIT: std::sync::LazyLock<DashMap<String, (u32, Instant)>> =
    std::sync::LazyLock::new(DashMap::new);

const RATE_LIMIT_WINDOW_SECS: u64 = 60;
const RATE_LIMIT_MAX_ATTEMPTS: u32 = 5;

/// Extract the client IP for rate-limiting purposes.
///
/// Tries `x-forwarded-for` first (the first IP in the list, which is the
/// original client when a trusted reverse proxy is in front). Falls back to
/// a single global bucket key `"global"` when no forwarded header is present,
/// since `ConnectInfo` is not wired in this codebase.
///
/// TODO: wire `axum::extract::ConnectInfo<SocketAddr>` into the `routes()`
/// function to get true per-IP limiting even without a reverse proxy in front.
fn extract_client_ip(headers: &axum::http::HeaderMap) -> String {
    headers
        .get("x-forwarded-for")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.split(',').next())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "global".to_string())
}

/// Check rate limit for a client key. Returns `true` if the request should be
/// allowed, `false` if the limit has been exceeded.
fn check_rate_limit(client_key: &str) -> bool {
    let now = Instant::now();
    let mut entry = BOOTSTRAP_RATE_LIMIT
        .entry(client_key.to_string())
        .or_insert((0, now));

    let (count, window_start) = entry.value_mut();
    let elapsed = now.duration_since(*window_start).as_secs();

    if elapsed >= RATE_LIMIT_WINDOW_SECS {
        // Reset window
        *count = 1;
        *window_start = now;
        true
    } else if *count < RATE_LIMIT_MAX_ATTEMPTS {
        *count += 1;
        true
    } else {
        false
    }
}

/// The concrete client type returned by `from_provider_metadata`:
/// - auth URL is always set (EndpointSet)
/// - token URL may or may not be present (EndpointMaybeSet)
/// - userinfo URL may or may not be present (EndpointMaybeSet)
/// - device auth, introspection, revocation are not set
type DiscoveredClient = CoreClient<
    EndpointSet,
    EndpointNotSet,
    EndpointNotSet,
    EndpointNotSet,
    EndpointMaybeSet,
    EndpointMaybeSet,
>;

/// Build OIDC + auth routes.
pub fn routes(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/login", get(login))
        .route("/callback", get(callback))
        .route("/providers", get(list_providers))
        .route("/me", get(me))
        .route("/logout", post(logout))
        .route("/bootstrap-login", post(bootstrap_login))
        .with_state(state)
}

/// GET /auth/providers — List enabled OIDC providers for the login page.
///
/// Also includes `bootstrap_active: bool` so the UI knows whether to show
/// the break-glass login form. The field is additive and backwards-compatible.
async fn list_providers(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    let providers =
        sqlx::query_as::<_, (String, String)>("SELECT id, name FROM idp_configs WHERE enabled = 1")
            .fetch_all(&state.db.pool)
            .await;

    let bootstrap_active = bootstrap::is_bootstrap_active(&state.config);

    match providers {
        Ok(list) => {
            let data: Vec<serde_json::Value> = list
                .into_iter()
                .map(|(id, name)| serde_json::json!({ "id": id, "name": name }))
                .collect();
            Json(serde_json::json!({
                "providers": data,
                "bootstrap_active": bootstrap_active,
            }))
            .into_response()
        }
        Err(e) => {
            tracing::error!(context = "list_providers", error = %e, "Internal error");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({ "error": "Internal server error" })),
            )
                .into_response()
        }
    }
}

#[derive(serde::Deserialize)]
struct LoginQuery {
    idp: String,
}

/// GET /auth/login?idp=<id> — Initiate OIDC authorization redirect.
async fn login(State(state): State<Arc<AppState>>, Query(query): Query<LoginQuery>) -> Response {
    let idp = match load_idp(&state.db, &query.idp).await {
        Ok(idp) => idp,
        Err(e) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({ "error": e.to_string() })),
            )
                .into_response();
        }
    };

    let api_url = state.config.api_external_url();
    let client =
        match build_oidc_client(&idp, &api_url, state.config.db_encryption_key.as_deref()).await {
            Ok(c) => c,
            Err(e) => {
                error!(error = %e, idp = %query.idp, "Failed to build OIDC client");
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(serde_json::json!({ "error": "OIDC provider configuration error" })),
                )
                    .into_response();
            }
        };

    let scopes: Vec<Scope> = idp
        .scopes
        .split_whitespace()
        .map(|s| Scope::new(s.to_string()))
        .collect();

    let (pkce_challenge, pkce_verifier) = PkceCodeChallenge::new_random_sha256();

    let mut auth_request = client
        .authorize_url(
            AuthenticationFlow::<CoreResponseType>::AuthorizationCode,
            CsrfToken::new_random,
            Nonce::new_random,
        )
        .set_pkce_challenge(pkce_challenge);

    for scope in scopes {
        auth_request = auth_request.add_scope(scope);
    }

    let (auth_url, csrf_token, nonce) = auth_request.url();

    // Store CSRF, nonce, and PKCE verifier in DB for callback validation
    if let Err(e) = store_auth_state(
        &state.db,
        csrf_token.secret(),
        nonce.secret(),
        &query.idp,
        pkce_verifier.secret(),
    )
    .await
    {
        error!(error = %e, "Failed to store auth state");
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "error": "Failed to initiate login" })),
        )
            .into_response();
    }

    Redirect::temporary(auth_url.as_str()).into_response()
}

#[derive(serde::Deserialize)]
struct CallbackQuery {
    code: String,
    state: String,
}

/// GET /auth/callback — Handle OIDC authorization callback.
async fn callback(
    State(state): State<Arc<AppState>>,
    Query(query): Query<CallbackQuery>,
) -> Response {
    // Look up the stored auth state
    let auth_state = match load_auth_state(&state.db, &query.state).await {
        Ok(s) => s,
        Err(e) => {
            error!(error = %e, "Invalid OIDC callback state");
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({ "error": "Invalid or expired login state" })),
            )
                .into_response();
        }
    };

    let idp = match load_idp(&state.db, &auth_state.idp_id).await {
        Ok(idp) => idp,
        Err(e) => {
            error!(error = %e, "Failed to load IdP for callback");
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({ "error": "OIDC configuration error" })),
            )
                .into_response();
        }
    };

    let api_url = state.config.api_external_url();
    let client =
        match build_oidc_client(&idp, &api_url, state.config.db_encryption_key.as_deref()).await {
            Ok(c) => c,
            Err(e) => {
                error!(error = %e, "Failed to build OIDC client for callback");
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(serde_json::json!({ "error": "OIDC configuration error" })),
                )
                    .into_response();
            }
        };

    let http_client = build_http_client();

    // Exchange code for tokens with PKCE verifier
    let pkce_verifier = PkceCodeVerifier::new(auth_state.pkce_verifier);
    let token_request = match client.exchange_code(AuthorizationCode::new(query.code)) {
        Ok(req) => req,
        Err(e) => {
            error!(error = %e, "OIDC token endpoint not configured");
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({ "error": "Token endpoint not configured" })),
            )
                .into_response();
        }
    };

    let token_response: CoreTokenResponse = match token_request
        .set_pkce_verifier(pkce_verifier)
        .request_async(&http_client)
        .await
    {
        Ok(t) => t,
        Err(e) => {
            error!(error = %e, "OIDC token exchange failed");
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({ "error": "Token exchange failed" })),
            )
                .into_response();
        }
    };

    // Extract ID token claims
    let id_token: &CoreIdToken = match token_response.id_token() {
        Some(t) => t,
        None => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({ "error": "No ID token in response" })),
            )
                .into_response();
        }
    };

    let nonce = Nonce::new(auth_state.nonce);
    let verifier: CoreIdTokenVerifier = client.id_token_verifier();
    let claims: &CoreIdTokenClaims = match id_token.claims(&verifier, &nonce) {
        Ok(c) => c,
        Err(e) => {
            error!(error = %e, "ID token verification failed");
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({ "error": "Token verification failed" })),
            )
                .into_response();
        }
    };

    let subject = claims.subject().to_string();
    let email = claims
        .email()
        .map(|e: &openidconnect::EndUserEmail| e.to_string());
    let display_name = claims
        .preferred_username()
        .map(|u: &openidconnect::EndUserUsername| u.to_string())
        .or_else(|| email.clone());

    // Upsert user
    let user_id = match upsert_user(
        &state.db,
        &auth_state.idp_id,
        &subject,
        email.as_deref(),
        display_name.as_deref(),
    )
    .await
    {
        Ok(id) => id,
        Err(e) => {
            error!(error = %e, "Failed to upsert user");
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({ "error": "Failed to create user record" })),
            )
                .into_response();
        }
    };

    info!(user_id = %user_id, subject = %subject, "OIDC login successful");

    // Create session
    let session_token = match sessions::create_session(&state.db, &user_id).await {
        Ok(t) => t,
        Err(e) => {
            error!(error = %e, "Failed to create session");
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({ "error": "Failed to create session" })),
            )
                .into_response();
        }
    };

    // Clean up auth state
    let _ = delete_auth_state(&state.db, &query.state).await;

    // Set cookie and redirect to portal
    let cookie = sessions::build_cookie(
        &session_token,
        86400,
        state.config.secure_cookies,
        state.config.cookie_domain.as_deref(),
    );

    let portal_url = format!("{}/portal/", state.config.api_external_url());
    (
        [("set-cookie", cookie), ("location", portal_url)],
        StatusCode::FOUND,
    )
        .into_response()
}

/// GET /auth/me — Return current session user info.
///
/// Accepts only a session cookie. Returns 200 with user info if a valid session
/// cookie is present, or 401 otherwise. Does not accept Basic auth or mint new
/// sessions.
async fn me(State(state): State<Arc<AppState>>, headers: axum::http::HeaderMap) -> Response {
    let cookie_header = headers
        .get("cookie")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");

    let session_user = match super::validate_any_session(cookie_header, &state.db).await {
        Some(u) => u,
        None => {
            return (
                StatusCode::UNAUTHORIZED,
                Json(serde_json::json!({ "error": "Authentication required" })),
            )
                .into_response();
        }
    };

    Json(serde_json::json!({
        "user_id": session_user.user_id,
        "email": session_user.email,
        "display_name": session_user.display_name,
        "is_admin": session_user.is_admin,
        "chat_url": state.config.chat_external_url(),
    }))
    .into_response()
}

/// POST /auth/logout — Clear session.
async fn logout(State(state): State<Arc<AppState>>, headers: axum::http::HeaderMap) -> Response {
    // Delete all matching session cookies (browser may send multiple)
    let cookie_header = headers
        .get("cookie")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    for token in super::extract_session_tokens(cookie_header) {
        let _ = sessions::delete_session(&state.db, token).await;
    }

    // Clear the cookie regardless
    let clear_cookie = sessions::clear_cookie(
        state.config.secure_cookies,
        state.config.cookie_domain.as_deref(),
    );

    (
        [("set-cookie", clear_cookie)],
        Json(serde_json::json!({ "status": "logged_out" })),
    )
        .into_response()
}

// --- Bootstrap login ---

#[derive(serde::Deserialize)]
struct BootstrapLoginRequest {
    user: String,
    pass: String,
}

/// POST /auth/bootstrap-login — Exchange break-glass credentials for a session cookie.
///
/// Returns 404 when bootstrap is disabled (so the endpoint doesn't reveal its
/// existence to attackers scanning for it).  Returns 401 on wrong credentials.
/// On success mints a session and returns 204 with a `Set-Cookie` header.
///
/// Rate-limited to 5 attempts per 60-second window per source IP.
async fn bootstrap_login(
    State(state): State<Arc<AppState>>,
    headers: axum::http::HeaderMap,
    Json(body): Json<BootstrapLoginRequest>,
) -> Response {
    // 404 when bootstrap is not active — don't hint the endpoint exists.
    if !bootstrap::is_bootstrap_active(&state.config) {
        return StatusCode::NOT_FOUND.into_response();
    }

    // Rate limit: ~5 attempts per minute per source IP.
    let client_key = extract_client_ip(&headers);
    if !check_rate_limit(&client_key) {
        warn!(
            client = %client_key,
            "Bootstrap login rate limit exceeded"
        );
        return (
            StatusCode::TOO_MANY_REQUESTS,
            Json(serde_json::json!({ "error": "Too many login attempts, please wait" })),
        )
            .into_response();
    }

    // Validate credentials.
    let user_id =
        match bootstrap::validate_bootstrap(&state.config, &state.db, &body.user, &body.pass).await
        {
            Ok(id) => id,
            Err(_) => {
                return (
                    StatusCode::UNAUTHORIZED,
                    Json(serde_json::json!({ "error": "Invalid credentials" })),
                )
                    .into_response();
            }
        };

    // Mint a session.
    let session_token = match sessions::create_session(&state.db, &user_id).await {
        Ok(t) => t,
        Err(e) => {
            error!(error = %e, "Failed to create session for bootstrap user");
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({ "error": "Failed to create session" })),
            )
                .into_response();
        }
    };

    // Build and set the session cookie.
    let cookie = sessions::build_cookie(
        &session_token,
        86400,
        state.config.secure_cookies,
        state.config.cookie_domain.as_deref(),
    );

    info!(user_id = %user_id, "Bootstrap login successful");

    (
        StatusCode::NO_CONTENT,
        [(axum::http::header::SET_COOKIE, cookie)],
    )
        .into_response()
}

// --- Helper functions ---

/// Build a reqwest HTTP client suitable for OIDC operations.
fn build_http_client() -> openidconnect::reqwest::Client {
    openidconnect::reqwest::ClientBuilder::new()
        // Disable redirects to prevent SSRF
        .redirect(openidconnect::reqwest::redirect::Policy::none())
        .build()
        .expect("Failed to build HTTP client")
}

async fn load_idp(db: &Database, idp_id: &str) -> Result<IdpConfig> {
    sqlx::query_as::<_, IdpConfig>(
        "SELECT id, name, issuer, client_id, client_secret_enc, scopes, enabled, created_at FROM idp_configs WHERE id = ? AND enabled = 1",
    )
    .bind(idp_id)
    .fetch_optional(&db.pool)
    .await?
    .context("IdP not found or disabled")
}

async fn build_oidc_client(
    idp: &IdpConfig,
    api_external_url: &str,
    encryption_key: Option<&str>,
) -> Result<DiscoveredClient> {
    let issuer_url = IssuerUrl::new(idp.issuer.clone()).context("Invalid issuer URL")?;
    let http_client = build_http_client();

    let provider_metadata = CoreProviderMetadata::discover_async(issuer_url, &http_client)
        .await
        .context("OIDC discovery failed")?;

    let redirect_url = RedirectUrl::new(format!("{}/auth/callback", api_external_url))
        .context("Invalid redirect URL")?;

    // Decrypt client secret if encryption key is configured
    let client_secret = match encryption_key {
        Some(key) => crate::db::crypto::decrypt(&idp.client_secret_enc, key)
            .context("Failed to decrypt IdP client secret")?,
        None => idp.client_secret_enc.clone(),
    };

    let client = CoreClient::from_provider_metadata(
        provider_metadata,
        ClientId::new(idp.client_id.clone()),
        Some(ClientSecret::new(client_secret)),
    )
    .set_redirect_uri(redirect_url);

    Ok(client)
}

#[derive(Debug, sqlx::FromRow)]
struct AuthState {
    idp_id: String,
    nonce: String,
    pkce_verifier: String,
}

async fn store_auth_state(
    db: &Database,
    csrf_token: &str,
    nonce: &str,
    idp_id: &str,
    pkce_verifier: &str,
) -> Result<()> {
    sqlx::query(
        "INSERT INTO oidc_auth_state (csrf_token, nonce, idp_id, pkce_verifier, expires_at) VALUES (?, ?, ?, ?, datetime('now', '+10 minutes'))",
    )
    .bind(csrf_token)
    .bind(nonce)
    .bind(idp_id)
    .bind(pkce_verifier)
    .execute(&db.pool)
    .await?;
    Ok(())
}

async fn load_auth_state(db: &Database, csrf_token: &str) -> Result<AuthState> {
    sqlx::query_as::<_, AuthState>(
        "SELECT idp_id, nonce, pkce_verifier FROM oidc_auth_state WHERE csrf_token = ? AND expires_at > datetime('now')",
    )
    .bind(csrf_token)
    .fetch_optional(&db.pool)
    .await?
    .context("Invalid or expired auth state")
}

async fn delete_auth_state(db: &Database, csrf_token: &str) -> Result<()> {
    sqlx::query("DELETE FROM oidc_auth_state WHERE csrf_token = ?")
        .bind(csrf_token)
        .execute(&db.pool)
        .await?;
    Ok(())
}

async fn upsert_user(
    db: &Database,
    idp_id: &str,
    subject: &str,
    email: Option<&str>,
    display_name: Option<&str>,
) -> Result<String> {
    // Check if user exists
    let existing: Option<(String,)> =
        sqlx::query_as("SELECT id FROM users WHERE idp_id = ? AND subject = ?")
            .bind(idp_id)
            .bind(subject)
            .fetch_optional(&db.pool)
            .await?;

    if let Some((id,)) = existing {
        // Update email/display_name
        sqlx::query("UPDATE users SET email = COALESCE(?, email), display_name = COALESCE(?, display_name) WHERE id = ?")
            .bind(email)
            .bind(display_name)
            .bind(&id)
            .execute(&db.pool)
            .await?;
        return Ok(id);
    }

    // Create new user
    let id = uuid::Uuid::new_v4().to_string();
    // First OIDC user for an IdP gets admin
    let count: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM users WHERE idp_id != 'bootstrap'")
        .fetch_one(&db.pool)
        .await?;
    let is_admin = count.0 == 0;

    sqlx::query(
        "INSERT INTO users (id, idp_id, subject, email, display_name, is_admin) VALUES (?, ?, ?, ?, ?, ?)",
    )
    .bind(&id)
    .bind(idp_id)
    .bind(subject)
    .bind(email)
    .bind(display_name)
    .bind(is_admin)
    .execute(&db.pool)
    .await?;

    if is_admin {
        info!(user_id = %id, "First OIDC user promoted to admin");
    }

    Ok(id)
}

// ---------------------------------------------------------------------------
// Tests for bootstrap-login and providers endpoints
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use axum::Router;
    use serde_json::Value;
    use tower::ServiceExt;

    use crate::auth::sessions;
    use crate::config::AppConfig;
    use crate::db::Database;
    use crate::docker::DockerManager;
    use crate::metrics::MetricsBroadcaster;
    use crate::scheduler::reservation::ReservationBroadcaster;
    use crate::scheduler::Scheduler;
    use crate::AppState;

    // -----------------------------------------------------------------------
    // Helpers
    // -----------------------------------------------------------------------

    fn test_config_no_bootstrap() -> AppConfig {
        AppConfig {
            listen_addr: "127.0.0.1:0".to_string(),
            database_url: "sqlite::memory:".to_string(),
            tls_cert_path: None,
            tls_key_path: None,
            bootstrap_user: None,
            bootstrap_password: None,
            break_glass: false,
            docker_host: "unix:///var/run/docker.sock".to_string(),
            model_path: "/tmp/test-models-oidc".to_string(),
            model_host_path: "/tmp/test-models-oidc".to_string(),
            ui_path: "/tmp/test-ui".to_string(),
            api_hostname: "localhost".to_string(),
            chat_hostname: "localhost".to_string(),
            cookie_domain: None,
            backend_network: "test-network".to_string(),
            acme_contact: None,
            acme_staging: false,
            webui_backend_url: "http://localhost:8080".to_string(),
            webui_api_key: None,
            queue_timeout_secs: 30,
            secure_cookies: false,
            db_encryption_key: None,
            db_encryption_key_old: None,
            data_path: "/tmp/test-data-path".to_string(),
        }
    }

    fn test_config_with_bootstrap() -> AppConfig {
        AppConfig {
            bootstrap_user: Some("admin".to_string()),
            bootstrap_password: Some("hunter2".to_string()),
            break_glass: true,
            ..test_config_no_bootstrap()
        }
    }

    async fn test_app_state(config: AppConfig) -> Arc<AppState> {
        let db = Database::test_db().await;
        let (probe_tx, _probe_rx) = crate::supervisor::channel();
        Arc::new(AppState {
            config,
            db,
            docker: DockerManager::test_dummy(),
            scheduler: Scheduler::new(),
            metrics: MetricsBroadcaster::new(),
            reservations: ReservationBroadcaster::new(),
            supervisor_map: std::sync::Arc::new(dashmap::DashMap::new()),
            probe_tx,
            worked_map: std::sync::Arc::new(dashmap::DashMap::new()),
        })
    }

    fn auth_router(state: Arc<AppState>) -> Router {
        Router::new().nest("/auth", super::super::oidc::routes(state))
    }

    /// Return a monotonically-increasing last-octet so each call gets its own
    /// rate-limit bucket and tests don't bleed into each other.
    fn unique_test_ip() -> String {
        use std::sync::atomic::{AtomicU16, Ordering};
        static COUNTER: AtomicU16 = AtomicU16::new(1);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        // Spread across two octets to stay in valid IPv4 space.
        format!("10.{}.{}.1", (n >> 8) & 0xff, n & 0xff)
    }

    fn post_bootstrap_login_req(user: &str, pass: &str, ip: &str) -> Request<Body> {
        let body = serde_json::json!({ "user": user, "pass": pass });
        Request::builder()
            .method("POST")
            .uri("/auth/bootstrap-login")
            .header("content-type", "application/json")
            .header("x-forwarded-for", ip)
            .body(Body::from(serde_json::to_vec(&body).unwrap()))
            .unwrap()
    }

    // -----------------------------------------------------------------------
    // POST /auth/bootstrap-login tests
    // -----------------------------------------------------------------------

    /// When bootstrap is disabled the endpoint returns 404.
    #[tokio::test]
    async fn bootstrap_login_404_when_bootstrap_inactive() {
        let state = test_app_state(test_config_no_bootstrap()).await;
        let app = auth_router(state);

        let ip = unique_test_ip();
        let req = post_bootstrap_login_req("admin", "hunter2", &ip);
        let resp = app.oneshot(req).await.unwrap();

        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    /// Wrong credentials return 401.
    #[tokio::test]
    async fn bootstrap_login_401_on_wrong_credentials() {
        let state = test_app_state(test_config_with_bootstrap()).await;
        let app = auth_router(state);

        let ip = unique_test_ip();
        let req = post_bootstrap_login_req("admin", "wrongpassword", &ip);
        let resp = app.oneshot(req).await.unwrap();

        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    /// Correct credentials yield 204 with a valid Set-Cookie header.
    /// The issued token must validate via `sessions::validate_session`.
    #[tokio::test]
    async fn bootstrap_login_204_with_set_cookie_on_success() {
        let state = test_app_state(test_config_with_bootstrap()).await;
        let db = state.db.clone();
        let app = auth_router(Arc::clone(&state));

        let ip = unique_test_ip();
        let req = post_bootstrap_login_req("admin", "hunter2", &ip);
        let resp = app.oneshot(req).await.unwrap();

        assert_eq!(resp.status(), StatusCode::NO_CONTENT);

        // Extract the Set-Cookie header.
        let set_cookie = resp
            .headers()
            .get("set-cookie")
            .expect("Set-Cookie header must be present")
            .to_str()
            .unwrap()
            .to_string();

        assert!(
            set_cookie.contains("se_session="),
            "cookie must contain se_session=, got: {set_cookie}"
        );

        // Parse the token value from `se_session=<token>; ...`.
        let token = set_cookie
            .split(';')
            .next()
            .unwrap()
            .trim()
            .strip_prefix("se_session=")
            .expect("should start with se_session=")
            .to_string();

        // The token must be valid in the DB.
        sessions::validate_session(&db, &token)
            .await
            .expect("session created by bootstrap-login must be valid");
    }

    /// After the rate limit is exhausted for an IP, subsequent requests return 429.
    #[tokio::test]
    async fn bootstrap_login_429_after_rate_limit_exhausted() {
        let ip = "192.0.2.77"; // fixed, deterministic IP for this test
                               // Clean up any prior state for this IP.
        super::BOOTSTRAP_RATE_LIMIT.remove(ip);

        let state = test_app_state(test_config_with_bootstrap()).await;

        // Exhaust the bucket.
        for i in 0..super::RATE_LIMIT_MAX_ATTEMPTS {
            let app = auth_router(Arc::clone(&state));
            let req = post_bootstrap_login_req("admin", "hunter2", ip);
            let resp = app.oneshot(req).await.unwrap();
            let status = resp.status();
            assert!(
                status == StatusCode::NO_CONTENT || status == StatusCode::TOO_MANY_REQUESTS,
                "attempt {i}: expected 204 or 429, got {status}"
            );
        }

        // One more request must be 429.
        let app = auth_router(Arc::clone(&state));
        let req = post_bootstrap_login_req("admin", "hunter2", ip);
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(
            resp.status(),
            StatusCode::TOO_MANY_REQUESTS,
            "request beyond rate limit must return 429"
        );

        // Cleanup.
        super::BOOTSTRAP_RATE_LIMIT.remove(ip);
    }

    // -----------------------------------------------------------------------
    // GET /auth/providers — bootstrap_active field
    // -----------------------------------------------------------------------

    /// When bootstrap is inactive, providers response includes `bootstrap_active: false`.
    #[tokio::test]
    async fn providers_bootstrap_active_false_when_inactive() {
        let state = test_app_state(test_config_no_bootstrap()).await;
        let app = auth_router(state);

        let req = Request::builder()
            .method("GET")
            .uri("/auth/providers")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        let bytes = axum::body::to_bytes(resp.into_body(), 1024 * 64)
            .await
            .unwrap();
        let json: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(
            json["bootstrap_active"],
            Value::Bool(false),
            "bootstrap_active should be false when break_glass=false"
        );
    }

    /// When bootstrap is active, providers response includes `bootstrap_active: true`.
    #[tokio::test]
    async fn providers_bootstrap_active_true_when_active() {
        let state = test_app_state(test_config_with_bootstrap()).await;
        let app = auth_router(state);

        let req = Request::builder()
            .method("GET")
            .uri("/auth/providers")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        let bytes = axum::body::to_bytes(resp.into_body(), 1024 * 64)
            .await
            .unwrap();
        let json: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(
            json["bootstrap_active"],
            Value::Bool(true),
            "bootstrap_active should be true when break_glass=true and creds are set"
        );
    }

    /// providers response always includes the `providers` array (backwards compatibility).
    #[tokio::test]
    async fn providers_always_includes_providers_array() {
        let state = test_app_state(test_config_no_bootstrap()).await;
        let app = auth_router(state);

        let req = Request::builder()
            .method("GET")
            .uri("/auth/providers")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        let bytes = axum::body::to_bytes(resp.into_body(), 1024 * 64)
            .await
            .unwrap();
        let json: Value = serde_json::from_slice(&bytes).unwrap();
        assert!(
            json["providers"].is_array(),
            "providers field must always be an array"
        );
    }

    // -----------------------------------------------------------------------
    // check_rate_limit unit tests
    // -----------------------------------------------------------------------

    #[test]
    fn rate_limit_allows_up_to_max_attempts() {
        let key = format!("unit-test-allow-{}", uuid::Uuid::new_v4());
        super::BOOTSTRAP_RATE_LIMIT.remove(key.as_str());

        for i in 0..super::RATE_LIMIT_MAX_ATTEMPTS {
            assert!(
                super::check_rate_limit(&key),
                "attempt {} should be allowed",
                i + 1
            );
        }

        super::BOOTSTRAP_RATE_LIMIT.remove(key.as_str());
    }

    #[test]
    fn rate_limit_blocks_after_max_attempts() {
        let key = format!("unit-test-block-{}", uuid::Uuid::new_v4());
        super::BOOTSTRAP_RATE_LIMIT.remove(key.as_str());

        for _ in 0..super::RATE_LIMIT_MAX_ATTEMPTS {
            super::check_rate_limit(&key);
        }

        assert!(
            !super::check_rate_limit(&key),
            "attempt beyond max should be blocked"
        );

        super::BOOTSTRAP_RATE_LIMIT.remove(key.as_str());
    }

    // -----------------------------------------------------------------------
    // GET /auth/me — regression tests
    // -----------------------------------------------------------------------

    fn basic_auth_header(user: &str, pass: &str) -> String {
        use base64::Engine as _;
        let encoded = base64::engine::general_purpose::STANDARD.encode(format!("{user}:{pass}"));
        format!("Basic {encoded}")
    }

    /// Regression: GET /auth/me with Basic bootstrap credentials and no session cookie
    /// must return 401 — it no longer mints a session from Basic creds.
    #[tokio::test]
    async fn me_rejects_basic_auth_with_no_cookie() {
        let state = test_app_state(test_config_with_bootstrap()).await;
        let app = auth_router(state);

        let req = Request::builder()
            .method("GET")
            .uri("/auth/me")
            .header("authorization", basic_auth_header("admin", "hunter2"))
            .body(Body::empty())
            .unwrap();

        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(
            resp.status(),
            StatusCode::UNAUTHORIZED,
            "GET /auth/me must not accept Basic auth"
        );
        // Must NOT set a session cookie.
        assert!(
            resp.headers().get("set-cookie").is_none(),
            "GET /auth/me must not emit Set-Cookie when rejecting Basic auth"
        );
    }

    /// Regression: GET /auth/me with a valid session cookie returns 200 with user info.
    #[tokio::test]
    async fn me_returns_200_with_valid_session_cookie() {
        use crate::auth::bootstrap;

        let state = test_app_state(test_config_with_bootstrap()).await;
        let db = state.db.clone();

        // Create a bootstrap user and a session for it.
        let user_id = bootstrap::ensure_bootstrap_user(&db, "admin")
            .await
            .expect("ensure_bootstrap_user");
        let token = sessions::create_session(&db, &user_id)
            .await
            .expect("create_session");

        let app = auth_router(state);

        let cookie_hdr = format!("se_session={token}");
        let req = Request::builder()
            .method("GET")
            .uri("/auth/me")
            .header("cookie", &cookie_hdr)
            .body(Body::empty())
            .unwrap();

        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(
            resp.status(),
            StatusCode::OK,
            "Valid session cookie must yield 200 from /auth/me"
        );

        let bytes = axum::body::to_bytes(resp.into_body(), 1024 * 64)
            .await
            .unwrap();
        let json: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(
            json["user_id"], user_id,
            "/auth/me should return the correct user_id"
        );
        assert!(
            json.get("is_admin").is_some(),
            "/auth/me response must include is_admin"
        );
    }

    /// GET /auth/me with no credentials at all returns 401.
    #[tokio::test]
    async fn me_returns_401_with_no_credentials() {
        let state = test_app_state(test_config_with_bootstrap()).await;
        let app = auth_router(state);

        let req = Request::builder()
            .method("GET")
            .uri("/auth/me")
            .body(Body::empty())
            .unwrap();

        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(
            resp.status(),
            StatusCode::UNAUTHORIZED,
            "GET /auth/me with no credentials must return 401"
        );
    }
}
