pub mod bootstrap;
pub mod oidc;
pub mod sessions;
pub mod tokens;

use std::sync::Arc;

use axum::extract::{Request, State};
use axum::http::StatusCode;
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use axum::Json;

use crate::db::Database;
use crate::AppState;

/// Authenticated user context extracted from a valid Bearer token.
///
/// Fields are populated from the DB during token validation and consumed
/// by handlers via `Extension<AuthUser>`.
#[derive(Debug, Clone)]
pub struct AuthUser {
    pub user_id: String,
    pub token_id: String,
    pub category_id: Option<String>,
    pub specific_model_id: Option<String>,
    #[allow(dead_code)] // populated from DB; will be consumed by authorization middleware
    pub is_admin: bool,
    #[allow(dead_code)] // populated from DB; will be consumed by authorization middleware
    pub is_internal: bool,
}

/// Authenticated session user (from cookie).
#[derive(Debug, Clone)]
pub struct SessionAuth {
    pub user_id: String,
    pub is_admin: bool,
    pub email: Option<String>,
    pub display_name: Option<String>,
}

/// Middleware: validate Bearer token or x-api-key on /v1/* API requests.
pub async fn bearer_auth_middleware(
    State(state): State<Arc<AppState>>,
    mut req: Request,
    next: Next,
) -> Result<Response, StatusCode> {
    // Try Authorization: Bearer <token> first, then fall back to x-api-key header.
    let token = req
        .headers()
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .or_else(|| req.headers().get("x-api-key").and_then(|v| v.to_str().ok()))
        .ok_or(StatusCode::UNAUTHORIZED)?;

    let auth_user = tokens::validate_token(&state.db, token)
        .await
        .map_err(|_| StatusCode::UNAUTHORIZED)?;

    req.extensions_mut().insert(auth_user);
    Ok(next.run(req).await)
}

/// Extract all session tokens from the Cookie header.
///
/// Browsers may send multiple cookies with the same name (e.g. one from before
/// subdomain routing without Domain, plus a new one with Domain=.parent).
/// Returns an iterator over all matching token values.
pub(crate) fn extract_session_tokens(cookie_header: &str) -> impl Iterator<Item = &str> {
    let prefix = format!("{}=", sessions::cookie_name());
    cookie_header.split(';').filter_map(move |c| {
        let c = c.trim();
        c.strip_prefix(&prefix)
    })
}

/// Try to validate any session cookie from the Cookie header.
/// Tries each matching `se_session` cookie until one validates.
pub(crate) async fn validate_any_session(
    cookie_header: &str,
    db: &Database,
) -> Option<sessions::SessionUser> {
    for token in extract_session_tokens(cookie_header) {
        if let Ok(user) = sessions::validate_session(db, token).await {
            return Some(user);
        }
    }
    None
}

/// Middleware: validate session cookie on /api/* portal requests.
pub async fn session_auth_middleware(
    State(state): State<Arc<AppState>>,
    mut req: Request,
    next: Next,
) -> Result<Response, Response> {
    // Try session cookie(s)
    let cookie_header = req
        .headers()
        .get("cookie")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");

    let session_user = validate_any_session(cookie_header, &state.db)
        .await
        .ok_or_else(|| {
            (
                StatusCode::UNAUTHORIZED,
                Json(serde_json::json!({ "error": "Authentication required" })),
            )
                .into_response()
        })?;

    req.extensions_mut().insert(SessionAuth {
        user_id: session_user.user_id,
        is_admin: session_user.is_admin,
        email: session_user.email,
        display_name: session_user.display_name,
    });

    Ok(next.run(req).await)
}

/// Middleware: validate session for browser routes, redirecting to portal if unauthenticated.
///
/// Unlike `session_auth_middleware` which returns 401 JSON, this variant:
/// - Redirects unauthenticated browser requests to the API subdomain's portal
/// - Returns 401 JSON for API-style requests (XHR, fetch, etc.)
pub async fn session_auth_redirect_middleware(
    State(state): State<Arc<AppState>>,
    mut req: Request,
    next: Next,
) -> Result<Response, Response> {
    let portal_url = format!("{}/portal/", state.config.api_external_url());

    // Try session cookie(s)
    let cookie_header = req
        .headers()
        .get("cookie")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");

    let session_user = validate_any_session(cookie_header, &state.db)
        .await
        .ok_or_else(|| unauth_response(&req, &portal_url))?;

    req.extensions_mut().insert(SessionAuth {
        user_id: session_user.user_id,
        is_admin: session_user.is_admin,
        email: session_user.email,
        display_name: session_user.display_name,
    });

    Ok(next.run(req).await)
}

/// Return a redirect for browser requests, or 401 JSON for API requests.
fn unauth_response(req: &Request, portal_url: &str) -> Response {
    let accepts_html = req
        .headers()
        .get("accept")
        .and_then(|v| v.to_str().ok())
        .map(|v| v.contains("text/html"))
        .unwrap_or(false);

    if accepts_html {
        axum::response::Redirect::temporary(portal_url).into_response()
    } else {
        (
            StatusCode::UNAUTHORIZED,
            Json(serde_json::json!({ "error": "Authentication required" })),
        )
            .into_response()
    }
}

/// Middleware: require admin role (must be chained after session_auth_middleware).
pub async fn admin_only_middleware(req: Request, next: Next) -> Result<Response, Response> {
    let session = req.extensions().get::<SessionAuth>().ok_or_else(|| {
        (
            StatusCode::UNAUTHORIZED,
            Json(serde_json::json!({ "error": "Authentication required" })),
        )
            .into_response()
    })?;

    if !session.is_admin {
        return Err((
            StatusCode::FORBIDDEN,
            Json(serde_json::json!({ "error": "Admin access required" })),
        )
            .into_response());
    }

    Ok(next.run(req).await)
}

// ---------------------------------------------------------------------------
// Regression tests: Basic auth must NOT be accepted by session middleware
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use axum::middleware;
    use axum::routing::get;
    use axum::Router;
    use base64::Engine as _;
    use tower::ServiceExt;

    use crate::auth::bootstrap;
    use crate::auth::sessions;
    use crate::config::AppConfig;
    use crate::db::Database;
    use crate::docker::DockerManager;
    use crate::metrics::MetricsBroadcaster;
    use crate::scheduler::reservation::ReservationBroadcaster;
    use crate::scheduler::Scheduler;
    use crate::AppState;

    fn test_config_with_bootstrap() -> AppConfig {
        AppConfig {
            listen_addr: "127.0.0.1:0".to_string(),
            database_url: "sqlite::memory:".to_string(),
            tls_cert_path: None,
            tls_key_path: None,
            bootstrap_user: Some("admin".to_string()),
            bootstrap_password: Some("hunter2".to_string()),
            break_glass: true,
            docker_host: "unix:///var/run/docker.sock".to_string(),
            model_path: "/tmp/test-models-authmid".to_string(),
            model_host_path: "/tmp/test-models-authmid".to_string(),
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

    /// Build a minimal router that wraps a single route with session_auth_middleware.
    fn session_middleware_router(state: Arc<AppState>) -> Router {
        Router::new()
            .route("/api/ping", get(|| async { "ok" }))
            .layer(middleware::from_fn_with_state(
                state.clone(),
                super::session_auth_middleware,
            ))
            .with_state(state)
    }

    fn basic_auth_header(user: &str, pass: &str) -> String {
        let encoded = base64::engine::general_purpose::STANDARD.encode(format!("{user}:{pass}"));
        format!("Basic {encoded}")
    }

    /// Regression: Basic auth is no longer accepted by session_auth_middleware.
    /// A request with valid bootstrap Basic credentials and no cookie must get 401.
    #[tokio::test]
    async fn session_middleware_rejects_basic_auth() {
        let state = test_app_state(test_config_with_bootstrap()).await;
        let app = session_middleware_router(state);

        let req = Request::builder()
            .method("GET")
            .uri("/api/ping")
            .header("authorization", basic_auth_header("admin", "hunter2"))
            .body(Body::empty())
            .unwrap();

        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(
            resp.status(),
            StatusCode::UNAUTHORIZED,
            "Basic auth must not be accepted by session_auth_middleware"
        );
    }

    /// A request with a valid session cookie must still be accepted (existing behaviour preserved).
    #[tokio::test]
    async fn session_middleware_accepts_valid_cookie() {
        let state = test_app_state(test_config_with_bootstrap()).await;
        let db = state.db.clone();

        // Create a bootstrap user and a session for it.
        let user_id = bootstrap::ensure_bootstrap_user(&db, "admin")
            .await
            .expect("ensure_bootstrap_user");
        let token = sessions::create_session(&db, &user_id)
            .await
            .expect("create_session");

        let app = session_middleware_router(state);

        let cookie = format!("se_session={token}");
        let req = Request::builder()
            .method("GET")
            .uri("/api/ping")
            .header("cookie", &cookie)
            .body(Body::empty())
            .unwrap();

        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(
            resp.status(),
            StatusCode::OK,
            "Valid session cookie must be accepted"
        );
    }
}
