mod api;
mod auth;
mod config;
mod db;
mod docker;
mod metrics;
mod proxy;
mod scheduler;
mod tls;

#[cfg(test)]
mod admin_tests;
#[cfg(test)]
mod meta_token_tests;
#[cfg(test)]
mod reservation_tests;

use std::sync::{Arc, OnceLock};

use anyhow::Result;
use axum::extract::DefaultBodyLimit;
use axum::http::{HeaderValue, Method, StatusCode};
use axum::middleware;
use axum::response::{IntoResponse, Response};
use axum::Router;
use base64::Engine;
use sha2::{Digest, Sha256};
use tower::ServiceExt;
use tower_http::compression::CompressionLayer;
use tower_http::cors::CorsLayer;
use tower_http::trace::TraceLayer;
use tracing::{error, info, warn};

/// CSP header value computed at startup from the built index.html.
/// Falls back to a hardcoded hash when the UI bundle is absent (dev mode).
static CSP_HEADER: OnceLock<String> = OnceLock::new();

use crate::config::AppConfig;
use crate::db::Database;
use crate::docker::DockerManager;
use crate::metrics::MetricsBroadcaster;
use crate::scheduler::reservation::ReservationBroadcaster;
use crate::scheduler::Scheduler;

/// Shared application state available to all handlers.
pub struct AppState {
    pub config: AppConfig,
    pub db: Database,
    pub docker: DockerManager,
    pub scheduler: Scheduler,
    pub metrics: MetricsBroadcaster,
    pub reservations: ReservationBroadcaster,
}

#[tokio::main]
async fn main() -> Result<()> {
    // Load .env if present (not required)
    dotenvy::dotenv().ok();

    // Initialize tracing
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "sovereign_engine=info,tower_http=info".into()),
        )
        .init();

    info!("Starting Sovereign Engine v{}", env!("CARGO_PKG_VERSION"));

    // Load configuration
    let config = AppConfig::from_env()?;
    info!(listen_addr = %config.listen_addr, "Configuration loaded");

    // Initialize database
    let db = Database::connect(&config.database_url).await?;
    db.migrate().await?;
    info!("Database initialized");

    // Provision internal API token (for Open WebUI → proxy /v1 calls)
    auth::tokens::ensure_internal_token(&config, &db).await?;

    // Backfill GGUF metadata for models missing architecture info
    backfill_gguf_metadata(&db, &config).await;

    // Backfill mmproj (multimodal projector) filename for text-image models
    backfill_mmproj_filename(&db, &config).await;

    // Initialize Docker manager
    let docker = DockerManager::new(&config).await?;
    info!("Docker manager initialized");

    // Pull backend images in the background (non-blocking)
    docker.pull_backend_images().await;

    // Initialize scheduler and load settings from DB
    let scheduler = Scheduler::new();
    if let Err(e) = scheduler.reload_settings(&db).await {
        warn!("Failed to load scheduler settings from DB: {e}");
    }

    // NOTE: active reservation recovery happens after Arc<AppState> is built (below)

    // Initialize metrics broadcaster
    let metrics = MetricsBroadcaster::new();

    // Initialize reservation change broadcaster
    let reservations_broadcaster = ReservationBroadcaster::new();

    // Build shared state
    let state = Arc::new(AppState {
        config: config.clone(),
        db,
        docker,
        scheduler,
        metrics,
        reservations: reservations_broadcaster,
    });

    // Recover concurrency gate state from DB. Each loaded model is probed via
    // Docker — phantoms get reconciled (loaded=0, no gate), survivors get a
    // fresh gate registration. Must run after AppState is built so we can pass
    // the full Arc to reconcile_dead_backend().
    recover_gate_state(&state).await;

    // Start background metrics collection (broadcasts every 2s)
    state.metrics.spawn_collector(
        state.docker.clone(),
        state.scheduler.clone(),
        state.config.model_path.clone(),
    );

    // Recover active reservation from DB (if proxy restarted during a reservation)
    scheduler::reservation::recover_active_reservation(&state.db.pool, &state.scheduler).await;

    // Spawn reservation tick task (every 30s)
    {
        let pool = state.db.pool.clone();
        let sched = state.scheduler.clone();
        let res_broadcaster = state.reservations.clone();
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(std::time::Duration::from_secs(30));
            interval.tick().await; // first tick is immediate — skip it
            loop {
                interval.tick().await;
                scheduler::reservation::tick_reservations(&pool, &sched, &res_broadcaster).await;
            }
        });
    }

    // Warn about insecure bootstrap credential defaults
    if config.break_glass {
        if config.bootstrap_user.as_deref() == Some("admin")
            && config.bootstrap_password.as_deref() == Some("changeme")
        {
            warn!(
                "BREAK_GLASS is enabled with default credentials (admin/changeme). \
                   Change BOOTSTRAP_USER and BOOTSTRAP_PASSWORD for any non-local deployment."
            );
        } else {
            warn!(
                "BREAK_GLASS is enabled — bootstrap credentials are active. \
                   Disable after configuring an OIDC identity provider."
            );
        }
    }

    // Encrypt plaintext IdP secrets if encryption key is configured
    if let Some(ref key) = config.db_encryption_key {
        let old_key = config.db_encryption_key_old.as_deref();
        if let Err(e) = db::crypto::migrate_plaintext_secrets(&state.db, key, old_key).await {
            error!(error = %e, "Failed to migrate IdP secrets to encrypted form");
        }
    } else {
        warn!("DB_ENCRYPTION_KEY not set — IdP client secrets stored in plaintext");
    }

    // Spawn hourly session/state cleanup
    {
        let db = state.db.clone();
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(std::time::Duration::from_secs(3600));
            interval.tick().await; // first tick is immediate — skip it
            loop {
                interval.tick().await;
                if let Ok(n) = auth::sessions::cleanup_expired(&db).await {
                    if n > 0 {
                        info!(deleted = n, "Cleaned up expired sessions");
                    }
                }
                // Also clean expired OIDC auth state
                let _ =
                    sqlx::query("DELETE FROM oidc_auth_state WHERE expires_at < datetime('now')")
                        .execute(&db.pool)
                        .await;
            }
        });
    }

    // Compute CSP hashes from built index.html (or fall back to hardcoded)
    init_csp_header(&config.ui_path);

    // Build router
    let app = build_router(state.clone());

    // Start server
    let addr = config.listen_addr.parse::<std::net::SocketAddr>()?;

    if let Some(acme) = config.acme_config()? {
        if config.tls_cert_path.is_some() || config.tls_key_path.is_some() {
            warn!("ACME is enabled — ignoring TLS_CERT_PATH/TLS_KEY_PATH");
        }
        info!(
            "Starting HTTPS server on {} with ACME (domains: {:?})",
            addr, acme.domains
        );
        tls::serve_acme(app, addr, &acme.domains, &acme.contact, acme.staging).await?;
    } else if config.tls_cert_path.is_some() && config.tls_key_path.is_some() {
        info!("Starting HTTPS server on {} with manual TLS", addr);
        tls::serve_tls(app, addr, &config).await?;
    } else {
        info!("Starting HTTP server on {} (no TLS configured)", addr);
        let listener = tokio::net::TcpListener::bind(addr).await?;
        axum::serve(listener, app).await?;
    }

    Ok(())
}

/// Backfill GGUF architecture metadata for models that have NULL metadata columns.
/// Scans GGUF files on disk and updates the DB.
async fn backfill_gguf_metadata(db: &Database, config: &AppConfig) {
    let rows: Vec<(String, String, Option<String>)> = match sqlx::query_as(
        "SELECT id, hf_repo, filename FROM models WHERE n_layers IS NULL AND filename IS NOT NULL",
    )
    .fetch_all(&db.pool)
    .await
    {
        Ok(r) => r,
        Err(e) => {
            warn!("Failed to query models for GGUF backfill: {e}");
            return;
        }
    };

    if rows.is_empty() {
        return;
    }

    info!(count = rows.len(), "Backfilling GGUF metadata for models");

    for (model_id, hf_repo, filename) in &rows {
        let filename = match filename {
            Some(f) if f.ends_with(".gguf") => f,
            _ => continue,
        };

        let safe_repo = hf_repo.replace('/', "--");
        let gguf_path = format!("{}/{}/{}", config.model_path, safe_repo, filename);

        match api::hf::read_gguf_metadata(&gguf_path).await {
            Ok(meta) => {
                let (kv_bpt_global, kv_bpt_swa) = api::hf::compute_kv_aggregates(&meta);
                if let Err(e) = sqlx::query(
                    "UPDATE models SET context_length = COALESCE(context_length, ?), n_layers = ?, n_heads = ?, n_kv_heads = ?, embedding_length = ?, key_length = COALESCE(key_length, ?), value_length = COALESCE(value_length, ?), sliding_window = COALESCE(sliding_window, ?), kv_bytes_per_token_global = COALESCE(kv_bytes_per_token_global, ?), kv_bytes_per_token_swa = COALESCE(kv_bytes_per_token_swa, ?) WHERE id = ?",
                )
                .bind(meta.context_length.map(|v| v as i64))
                .bind(meta.block_count.map(|v| v as i64))
                .bind(meta.head_count.map(|v| v as i64))
                .bind(meta.head_count_kv.map(|v| v as i64))
                .bind(meta.embedding_length.map(|v| v as i64))
                .bind(meta.key_length.map(|v| v as i64))
                .bind(meta.value_length.map(|v| v as i64))
                .bind(meta.sliding_window.map(|v| v as i64))
                .bind(kv_bpt_global)
                .bind(kv_bpt_swa)
                .bind(model_id)
                .execute(&db.pool)
                .await
                {
                    error!(model = %model_id, error = %e, "Failed to update GGUF metadata");
                } else {
                    info!(model = %model_id, "Backfilled GGUF metadata");
                }
            }
            Err(e) => {
                warn!(model = %model_id, path = %gguf_path, error = %e, "Failed to read GGUF for backfill");
            }
        }
    }
}

/// Startup wrapper: look up the primary models dir from config and delegate
/// to [`backfill_mmproj_filename_inner`] so the inner function stays
/// injectable from unit tests.
async fn backfill_mmproj_filename(db: &Database, config: &AppConfig) {
    backfill_mmproj_filename_inner(&db.pool, std::path::Path::new(&config.model_path)).await;
}

/// For every model row with `mmproj_filename IS NULL`, scan the on-disk
/// `<base_path>/<hf_repo-with-slashes-replaced>/` directory for a file whose
/// name starts with `mmproj-` or `mmproj_` and ends with `.gguf`. If exactly
/// one candidate exists, pick it. If multiple exist, prefer `f16` > `bf16` >
/// `f32`, falling back to the first lexically. Populate the column on
/// success; leave it NULL when the directory is missing, unreadable, or
/// has no candidates.
///
/// Never aborts — all errors are logged and skipped. Matches the tolerance
/// of [`backfill_gguf_metadata`].
async fn backfill_mmproj_filename_inner(pool: &sqlx::SqlitePool, base_path: &std::path::Path) {
    let rows: Vec<(String, String)> =
        match sqlx::query_as("SELECT id, hf_repo FROM models WHERE mmproj_filename IS NULL")
            .fetch_all(pool)
            .await
        {
            Ok(r) => r,
            Err(e) => {
                warn!("Failed to query models for mmproj backfill: {e}");
                return;
            }
        };

    if rows.is_empty() {
        return;
    }

    for (model_id, hf_repo) in &rows {
        let safe_repo = hf_repo.replace('/', "--");
        let dir = base_path.join(&safe_repo);

        let read = match std::fs::read_dir(&dir) {
            Ok(r) => r,
            Err(e) => {
                // Missing / unreadable dir is the common case for models
                // whose files have not been provisioned yet, or text-only
                // models hosted elsewhere. Log at `warn!` and carry on —
                // mirrors backfill_gguf_metadata's tolerance.
                warn!(
                    model = %model_id,
                    dir = %dir.display(),
                    error = %e,
                    "mmproj backfill: failed to read model dir; leaving NULL"
                );
                continue;
            }
        };

        let mut candidates: Vec<String> = Vec::new();
        for entry in read.flatten() {
            // Skip subdirectories — only repo-root files count.
            match entry.file_type() {
                Ok(ft) if ft.is_file() => {}
                _ => continue,
            }
            let name = match entry.file_name().into_string() {
                Ok(n) => n,
                Err(_) => continue, // non-UTF-8 filename → skip
            };
            if (name.starts_with("mmproj-") || name.starts_with("mmproj_"))
                && name.ends_with(".gguf")
            {
                candidates.push(name);
            }
        }

        if candidates.is_empty() {
            // Text-only model — no log, as documented.
            continue;
        }

        candidates.sort();
        let picked = pick_mmproj_variant(&candidates);

        if candidates.len() > 1 {
            let skipped: Vec<&String> = candidates.iter().filter(|c| *c != &picked).collect();
            info!(
                model = %model_id,
                picked = %picked,
                skipped = ?skipped,
                "mmproj backfill: multiple candidates, picked preferred variant"
            );
        } else {
            info!(
                model = %model_id,
                picked = %picked,
                "mmproj backfill: single candidate"
            );
        }

        if let Err(e) = sqlx::query("UPDATE models SET mmproj_filename = ? WHERE id = ?")
            .bind(&picked)
            .bind(model_id)
            .execute(pool)
            .await
        {
            error!(model = %model_id, error = %e, "Failed to update mmproj_filename");
        }
    }
}

/// From a non-empty, lex-sorted list of mmproj candidate filenames, pick the
/// preferred variant: `f16` > `bf16` > `f32`, otherwise the first (lex-smallest).
///
/// Matching is **case-insensitive** — bartowski ships lowercase
/// (`mmproj-...-f16.gguf`) but unsloth ships uppercase
/// (`mmproj-F16.gguf`, `mmproj-BF16.gguf`, `mmproj-F32.gguf`). Without the
/// `.to_lowercase()` normalisation, all three branches miss for unsloth
/// repos and we fall through to lex-first, which is `BF16` (because
/// `'B' < 'F'`) rather than the documented `F16` preference.
///
/// `bf16` is checked as a distinct token — "contains f16 and not bf16" —
/// so a file named `mmproj-bf16.gguf` is never misclassified as f16.
fn pick_mmproj_variant(sorted: &[String]) -> String {
    if let Some(n) = sorted.iter().find(|n| {
        let lc = n.to_lowercase();
        lc.contains("f16") && !lc.contains("bf16")
    }) {
        return n.clone();
    }
    if let Some(n) = sorted.iter().find(|n| n.to_lowercase().contains("bf16")) {
        return n.clone();
    }
    if let Some(n) = sorted.iter().find(|n| n.to_lowercase().contains("f32")) {
        return n.clone();
    }
    sorted[0].clone()
}

/// Re-register concurrency gates for containers that survived a proxy restart.
///
/// Probes Docker for each model marked `loaded=1`: surviving containers get a
/// fresh gate registration; phantoms (Docker reports the container gone or
/// not running) are reconciled via [`api::common::reconcile_dead_backend`] —
/// `loaded` is cleared, no gate is registered, and a `backend_crash_log` row
/// is written with `signal = "discovered_at_proxy_startup"`.
///
/// This fixes the pre-Phase-2 bug where the proxy would re-register slots for
/// dead containers after a host reboot, causing requests to hang on a phantom
/// gate.
async fn recover_gate_state(state: &Arc<AppState>) {
    let rows: Vec<(String, i64)> = match sqlx::query_as(
        "SELECT cs.model_id, cs.parallel_slots FROM container_secrets cs JOIN models m ON m.id = cs.model_id WHERE m.loaded = 1",
    )
    .fetch_all(&state.db.pool)
    .await
    {
        Ok(r) => r,
        Err(e) => {
            warn!("Failed to query container_secrets for gate recovery: {e}");
            return;
        }
    };

    if rows.is_empty() {
        return;
    }

    let mut recovered = 0usize;
    let mut reconciled = 0usize;

    for (model_id, parallel_slots) in &rows {
        let container_name = format!("sovereign-llamacpp-{}", model_id);

        // Probe Docker. Treat both an Err result and an Ok-but-not-running
        // state as "container gone" — either way, the gate must NOT be
        // re-registered for a phantom backend.
        let inspect_result = state
            .docker
            .docker
            .inspect_container(&container_name, None)
            .await;

        let (alive, container_id) = match &inspect_result {
            Ok(info) => {
                let running = info
                    .state
                    .as_ref()
                    .and_then(|s| s.running)
                    .unwrap_or(false);
                (running, info.id.clone())
            }
            Err(_) => (false, None),
        };

        if !alive {
            info!(
                model = %model_id,
                container = %container_name,
                "Container gone for loaded model — reconciling",
            );
            api::common::reconcile_dead_backend(
                state,
                model_id,
                container_id.as_deref(),
                "discovered_at_proxy_startup",
            )
            .await;
            reconciled += 1;
            continue;
        }

        let slots = (*parallel_slots).max(1) as u32;
        state.scheduler.gate().register(model_id, slots).await;
        info!(model = %model_id, slots, "Recovered gate state");
        recovered += 1;
    }

    info!(
        total = rows.len(),
        recovered,
        reconciled,
        "Gate state recovery complete",
    );
}

fn build_router(state: Arc<AppState>) -> Router {
    // OIDC auth routes (no auth required)
    let auth_routes = auth::oidc::routes(state.clone());

    // Portal API routes (session auth required)
    let api_routes = api::routes(state.clone()).layer(middleware::from_fn_with_state(
        state.clone(),
        auth::session_auth_middleware,
    ));

    // OpenAI-compatible routes (bearer token auth required)
    let openai_routes = api::openai::routes(state.clone()).layer(middleware::from_fn_with_state(
        state.clone(),
        auth::bearer_auth_middleware,
    ));

    // Anthropic-compatible routes (bearer token auth required)
    let anthropic_routes = api::anthropic::routes(state.clone()).layer(
        middleware::from_fn_with_state(state.clone(), auth::bearer_auth_middleware),
    );

    let ui_path = state.config.ui_path.clone();

    // Open WebUI reverse proxy (session auth with redirect for browsers).
    let webui_fallback = Router::new()
        .fallback(proxy::webui::webui_proxy_handler)
        .layer(middleware::from_fn_with_state(
            state.clone(),
            auth::session_auth_redirect_middleware,
        ))
        .with_state(state.clone());

    let shared_layers = |router: Router| -> Router {
        router
            .layer(DefaultBodyLimit::max(10 * 1024 * 1024)) // 10 MB
            .layer(middleware::from_fn(security_headers))
            .layer(TraceLayer::new_for_http())
            .layer(CompressionLayer::new())
            .layer(build_cors_layer(&state.config))
    };

    // When both hostnames are the same (dev mode / unconfigured), build a combined
    // router that preserves the pre-subdomain layout: API routes + Open WebUI fallback.
    if state.config.api_hostname == state.config.chat_hostname {
        return shared_layers(
            Router::new()
                .nest("/auth", auth_routes)
                .nest("/api", api_routes)
                .nest("/v1", openai_routes)
                .nest("/v1", anthropic_routes)
                .nest_service(
                    "/portal",
                    tower_http::services::ServeDir::new(&ui_path).fallback(
                        tower_http::services::ServeFile::new(format!("{}/index.html", ui_path)),
                    ),
                )
                .fallback_service(webui_fallback),
        );
    }

    // Subdomain mode: separate API and Chat routers dispatched by Host header.
    let api_router = Router::new()
        .route(
            "/",
            axum::routing::get(|| async { axum::response::Redirect::permanent("/portal/") }),
        )
        .nest("/auth", auth_routes)
        .nest("/api", api_routes)
        .nest("/v1", openai_routes)
        .nest("/v1", anthropic_routes)
        .nest_service(
            "/portal",
            tower_http::services::ServeDir::new(&ui_path).fallback(
                tower_http::services::ServeFile::new(format!("{}/index.html", ui_path)),
            ),
        );

    let api_hostname = state.config.api_hostname.clone();
    let chat_hostname = state.config.chat_hostname.clone();

    shared_layers(
        Router::new()
            .fallback(move |req: axum::extract::Request| {
                let api_router = api_router.clone();
                let chat_router = webui_fallback.clone();
                let api_host = api_hostname.clone();
                let chat_host = chat_hostname.clone();
                async move {
                    let host = req
                        .headers()
                        .get("host")
                        .and_then(|v| v.to_str().ok())
                        .unwrap_or("")
                        .split(':')
                        .next()
                        .unwrap_or("");

                    if host == chat_host {
                        chat_router.oneshot(req).await.into_response()
                    } else if host == api_host {
                        api_router.oneshot(req).await.into_response()
                    } else {
                        (StatusCode::MISDIRECTED_REQUEST, "421 Misdirected Request").into_response()
                    }
                }
            })
            .with_state(state.clone()),
    )
}

fn build_cors_layer(config: &AppConfig) -> CorsLayer {
    let api_origin = config
        .api_external_url()
        .parse::<HeaderValue>()
        .unwrap_or_else(|_| HeaderValue::from_static("http://localhost:3000"));

    let chat_origin = config
        .chat_external_url()
        .parse::<HeaderValue>()
        .unwrap_or_else(|_| HeaderValue::from_static("http://localhost:3000"));

    use tower_http::cors::AllowOrigin;

    CorsLayer::new()
        .allow_origin(AllowOrigin::list([api_origin, chat_origin]))
        .allow_methods([
            Method::GET,
            Method::POST,
            Method::PUT,
            Method::DELETE,
            Method::OPTIONS,
        ])
        .allow_headers([
            axum::http::header::CONTENT_TYPE,
            axum::http::header::AUTHORIZATION,
            axum::http::header::ACCEPT,
            axum::http::HeaderName::from_static("x-api-key"),
        ])
        .allow_credentials(true)
}

/// Extract SHA-256 hashes of inline `<script>` blocks from the built index.html
/// and construct the full CSP header value. Falls back to a hardcoded hash if
/// the file is missing (e.g. dev mode without a built UI).
fn init_csp_header(ui_path: &str) {
    let index_path = format!("{}/index.html", ui_path);

    let hashes = match std::fs::read_to_string(&index_path) {
        Ok(html) => extract_inline_script_hashes(&html),
        Err(_) => {
            warn!(
                path = %index_path,
                "index.html not found — using hardcoded CSP hash (dev mode)"
            );
            vec!["sha256-CNK91oXKaUIpki3MXfrcGislo8qcATLtfVWO7y4j0rM=".to_string()]
        }
    };

    let hash_directives: Vec<String> = hashes.iter().map(|h| format!("'{h}'")).collect();
    let csp = format!(
        "default-src 'self'; script-src 'self' {}; style-src 'self' 'unsafe-inline'; img-src 'self' data:; connect-src 'self'",
        hash_directives.join(" ")
    );

    info!(hashes = ?hashes, "CSP script-src hashes computed");
    CSP_HEADER.set(csp).ok();
}

/// Parse HTML and return base64-encoded SHA-256 hashes for each inline script block.
fn extract_inline_script_hashes(html: &str) -> Vec<String> {
    let mut hashes = Vec::new();
    let engine = base64::engine::general_purpose::STANDARD;

    // Simple non-greedy extraction of <script>...</script> blocks (no src attribute).
    // This is intentionally simple — we only need to handle the known Vite output.
    let mut search_from = 0;
    while let Some(open_start) = html[search_from..].find("<script>") {
        let abs_open = search_from + open_start;
        let content_start = abs_open + "<script>".len();
        if let Some(close_offset) = html[content_start..].find("</script>") {
            let content = &html[content_start..content_start + close_offset];
            let digest = Sha256::digest(content.as_bytes());
            let b64 = engine.encode(digest);
            hashes.push(format!("sha256-{b64}"));
            search_from = content_start + close_offset + "</script>".len();
        } else {
            break;
        }
    }

    if hashes.is_empty() {
        warn!("No inline <script> blocks found in index.html — CSP may block scripts");
    }

    hashes
}

async fn security_headers(req: axum::extract::Request, next: axum::middleware::Next) -> Response {
    let is_portal = req.uri().path().starts_with("/portal");
    let mut response = next.run(req).await;
    let headers = response.headers_mut();
    headers.insert(
        "x-content-type-options",
        HeaderValue::from_static("nosniff"),
    );
    headers.insert("x-frame-options", HeaderValue::from_static("DENY"));
    headers.insert(
        "strict-transport-security",
        HeaderValue::from_static("max-age=31536000; includeSubDomains"),
    );
    headers.insert(
        "referrer-policy",
        HeaderValue::from_static("strict-origin-when-cross-origin"),
    );
    headers.insert(
        "permissions-policy",
        HeaderValue::from_static("camera=(), microphone=(), geolocation=(), payment=()"),
    );
    // Only apply restrictive CSP to our own portal UI; proxied apps (e.g.
    // Open WebUI) set their own CSP and need inline scripts to function.
    if is_portal {
        if let Some(csp) = CSP_HEADER.get() {
            if let Ok(val) = HeaderValue::from_str(csp) {
                headers.insert("content-security-policy", val);
            }
        }
    }
    response
}

#[cfg(test)]
mod csp_tests {
    use super::*;

    #[test]
    fn no_scripts_returns_empty_vec() {
        let html = "<html><body><p>Hello</p></body></html>";
        let hashes = extract_inline_script_hashes(html);
        assert!(hashes.is_empty());
    }

    #[test]
    fn single_inline_script_returns_one_hash() {
        let html = r#"<html><head><script>console.log("hi")</script></head></html>"#;
        let hashes = extract_inline_script_hashes(html);
        assert_eq!(hashes.len(), 1);
        assert!(hashes[0].starts_with("sha256-"));
    }

    #[test]
    fn single_inline_script_hash_is_deterministic() {
        let html = "<script>var x = 1;</script>";
        let h1 = extract_inline_script_hashes(html);
        let h2 = extract_inline_script_hashes(html);
        assert_eq!(h1, h2);
    }

    #[test]
    fn multiple_inline_scripts_returns_all_hashes() {
        let html = r#"
            <script>var a = 1;</script>
            <script>var b = 2;</script>
            <script>var c = 3;</script>
        "#;
        let hashes = extract_inline_script_hashes(html);
        assert_eq!(hashes.len(), 3);
        // All hashes should be distinct (different content)
        assert_ne!(hashes[0], hashes[1]);
        assert_ne!(hashes[1], hashes[2]);
        assert_ne!(hashes[0], hashes[2]);
    }

    #[test]
    fn script_with_src_attribute_is_not_matched() {
        // The parser looks for "<script>" exactly — a <script src="..."> tag won't match
        let html = r#"<script src="app.js"></script><script>inline()</script>"#;
        let hashes = extract_inline_script_hashes(html);
        assert_eq!(hashes.len(), 1);
        // The hash should be for "inline()" not the src tag
    }

    #[test]
    fn empty_inline_script_still_hashed() {
        let html = "<script></script>";
        let hashes = extract_inline_script_hashes(html);
        assert_eq!(hashes.len(), 1);
        assert!(hashes[0].starts_with("sha256-"));
    }

    #[test]
    fn hash_matches_known_value() {
        // SHA-256 of empty string = 47DEQpj8HBSa+/TImW+5JCeuQeRkm5NMpJWZG3hSuFU=
        let html = "<script></script>";
        let hashes = extract_inline_script_hashes(html);
        assert_eq!(
            hashes[0],
            "sha256-47DEQpj8HBSa+/TImW+5JCeuQeRkm5NMpJWZG3hSuFU="
        );
    }

    #[test]
    fn unclosed_script_tag_stops_parsing() {
        let html = "<script>var x = 1;";
        let hashes = extract_inline_script_hashes(html);
        assert!(hashes.is_empty());
    }
}

#[cfg(test)]
mod mmproj_backfill_tests {
    //! Unit tests for [`backfill_mmproj_filename_inner`].
    //!
    //! Each test: spin up an in-memory SQLite DB (with all migrations), a
    //! tempdir to stand in for `config.model_path`, optionally create files
    //! under `<tempdir>/<hf_repo-safe>/`, insert a `models` row, run the
    //! backfill, and assert on the resulting `mmproj_filename` value.

    use super::*;
    use crate::db::Database;
    use std::fs;

    /// Insert a minimal models row. Caller decides whether `mmproj_filename`
    /// starts NULL (the usual backfill input) or pre-set.
    async fn insert_model_row(
        pool: &sqlx::SqlitePool,
        id: &str,
        hf_repo: &str,
        filename: &str,
        mmproj: Option<&str>,
    ) {
        sqlx::query(
            "INSERT INTO models (id, hf_repo, filename, backend_type, loaded, mmproj_filename) \
             VALUES (?, ?, ?, 'llamacpp', 0, ?)",
        )
        .bind(id)
        .bind(hf_repo)
        .bind(filename)
        .bind(mmproj)
        .execute(pool)
        .await
        .expect("insert model");
    }

    async fn get_mmproj(pool: &sqlx::SqlitePool, id: &str) -> Option<String> {
        let (val,): (Option<String>,) =
            sqlx::query_as("SELECT mmproj_filename FROM models WHERE id = ?")
                .bind(id)
                .fetch_one(pool)
                .await
                .expect("fetch mmproj_filename");
        val
    }

    fn repo_dir(root: &std::path::Path, hf_repo: &str) -> std::path::PathBuf {
        root.join(hf_repo.replace('/', "--"))
    }

    #[tokio::test]
    async fn backfill_mmproj_picks_single_match() {
        let db = Database::test_db().await;
        let tmp = tempfile::tempdir().unwrap();
        let dir = repo_dir(tmp.path(), "owner/repo");
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("model.gguf"), b"").unwrap();
        fs::write(dir.join("mmproj-owner_repo-f16.gguf"), b"").unwrap();

        insert_model_row(&db.pool, "m1", "owner/repo", "model.gguf", None).await;

        backfill_mmproj_filename_inner(&db.pool, tmp.path()).await;

        assert_eq!(
            get_mmproj(&db.pool, "m1").await.as_deref(),
            Some("mmproj-owner_repo-f16.gguf")
        );
    }

    #[tokio::test]
    async fn backfill_mmproj_no_match_leaves_null() {
        let db = Database::test_db().await;
        let tmp = tempfile::tempdir().unwrap();
        let dir = repo_dir(tmp.path(), "owner/repo");
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("model.gguf"), b"").unwrap();

        insert_model_row(&db.pool, "m1", "owner/repo", "model.gguf", None).await;

        backfill_mmproj_filename_inner(&db.pool, tmp.path()).await;

        assert_eq!(get_mmproj(&db.pool, "m1").await, None);
    }

    #[tokio::test]
    async fn backfill_mmproj_prefers_f16_over_bf16_and_f32() {
        let db = Database::test_db().await;
        let tmp = tempfile::tempdir().unwrap();
        let dir = repo_dir(tmp.path(), "owner/repo");
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("mmproj-f16.gguf"), b"").unwrap();
        fs::write(dir.join("mmproj-bf16.gguf"), b"").unwrap();
        fs::write(dir.join("mmproj-f32.gguf"), b"").unwrap();

        insert_model_row(&db.pool, "m1", "owner/repo", "model.gguf", None).await;

        backfill_mmproj_filename_inner(&db.pool, tmp.path()).await;

        assert_eq!(
            get_mmproj(&db.pool, "m1").await.as_deref(),
            Some("mmproj-f16.gguf")
        );
    }

    #[tokio::test]
    async fn backfill_mmproj_prefers_bf16_over_f32() {
        let db = Database::test_db().await;
        let tmp = tempfile::tempdir().unwrap();
        let dir = repo_dir(tmp.path(), "owner/repo");
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("mmproj-bf16.gguf"), b"").unwrap();
        fs::write(dir.join("mmproj-f32.gguf"), b"").unwrap();

        insert_model_row(&db.pool, "m1", "owner/repo", "model.gguf", None).await;

        backfill_mmproj_filename_inner(&db.pool, tmp.path()).await;

        assert_eq!(
            get_mmproj(&db.pool, "m1").await.as_deref(),
            Some("mmproj-bf16.gguf")
        );
    }

    #[tokio::test]
    async fn backfill_mmproj_accepts_underscore_prefix() {
        let db = Database::test_db().await;
        let tmp = tempfile::tempdir().unwrap();
        let dir = repo_dir(tmp.path(), "owner/repo");
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("mmproj_foo.gguf"), b"").unwrap();

        insert_model_row(&db.pool, "m1", "owner/repo", "model.gguf", None).await;

        backfill_mmproj_filename_inner(&db.pool, tmp.path()).await;

        assert_eq!(
            get_mmproj(&db.pool, "m1").await.as_deref(),
            Some("mmproj_foo.gguf")
        );
    }

    #[tokio::test]
    async fn backfill_mmproj_ignores_non_root_mmproj() {
        let db = Database::test_db().await;
        let tmp = tempfile::tempdir().unwrap();
        let dir = repo_dir(tmp.path(), "owner/repo");
        let subdir = dir.join("subdir");
        fs::create_dir_all(&subdir).unwrap();
        fs::write(subdir.join("mmproj-x.gguf"), b"").unwrap();

        insert_model_row(&db.pool, "m1", "owner/repo", "model.gguf", None).await;

        backfill_mmproj_filename_inner(&db.pool, tmp.path()).await;

        assert_eq!(get_mmproj(&db.pool, "m1").await, None);
    }

    #[tokio::test]
    async fn backfill_mmproj_skips_non_null_rows() {
        let db = Database::test_db().await;
        let tmp = tempfile::tempdir().unwrap();
        let dir = repo_dir(tmp.path(), "owner/repo");
        fs::create_dir_all(&dir).unwrap();
        // A real mmproj file exists on disk — but the DB already has a
        // preset value, so the backfill must NOT overwrite.
        fs::write(dir.join("mmproj-f16.gguf"), b"").unwrap();

        insert_model_row(
            &db.pool,
            "m1",
            "owner/repo",
            "model.gguf",
            Some("preset.gguf"),
        )
        .await;

        backfill_mmproj_filename_inner(&db.pool, tmp.path()).await;

        assert_eq!(
            get_mmproj(&db.pool, "m1").await.as_deref(),
            Some("preset.gguf")
        );
    }

    #[tokio::test]
    async fn backfill_mmproj_tolerates_missing_directory() {
        let db = Database::test_db().await;
        let tmp = tempfile::tempdir().unwrap();
        // Note: do NOT create `tmp.path()/owner--repo/` — the dir is absent.

        insert_model_row(&db.pool, "m1", "owner/repo", "model.gguf", None).await;

        // Must not panic, must not abort.
        backfill_mmproj_filename_inner(&db.pool, tmp.path()).await;

        assert_eq!(get_mmproj(&db.pool, "m1").await, None);
    }
}

#[cfg(test)]
mod pick_mmproj_variant_tests {
    //! Unit tests for [`pick_mmproj_variant`].
    //!
    //! Mirrors the case-coverage in `proxy/src/api/hf.rs::detect_mmproj_file`
    //! tests so the two functions stay in lock-step.

    use super::pick_mmproj_variant;

    fn names(items: &[&str]) -> Vec<String> {
        let mut v: Vec<String> = items.iter().map(|s| s.to_string()).collect();
        v.sort();
        v
    }

    #[test]
    fn prefers_lowercase_f16_over_bf16_and_f32() {
        let v = names(&[
            "mmproj-foo-bf16.gguf",
            "mmproj-foo-f16.gguf",
            "mmproj-foo-f32.gguf",
        ]);
        assert_eq!(pick_mmproj_variant(&v), "mmproj-foo-f16.gguf");
    }

    #[test]
    fn lowercase_bf16_not_misclassified_as_f16() {
        let v = names(&["mmproj-foo-bf16.gguf"]);
        assert_eq!(pick_mmproj_variant(&v), "mmproj-foo-bf16.gguf");
    }

    #[test]
    fn prefers_uppercase_f16_over_bf16_and_f32() {
        // unsloth-style UPPERCASE quant tags (e.g. unsloth/Qwen3.6-A3B-GGUF).
        // Without case-insensitive matching, all three branches miss and we
        // fall through to lex-first → BF16 (because 'B' < 'F').
        let v = names(&["mmproj-BF16.gguf", "mmproj-F16.gguf", "mmproj-F32.gguf"]);
        assert_eq!(pick_mmproj_variant(&v), "mmproj-F16.gguf");
    }

    #[test]
    fn prefers_uppercase_bf16_over_f32() {
        let v = names(&["mmproj-BF16.gguf", "mmproj-F32.gguf"]);
        assert_eq!(pick_mmproj_variant(&v), "mmproj-BF16.gguf");
    }

    #[test]
    fn uppercase_bf16_not_confused_by_f16() {
        // Only BF16 present, uppercase — must pick the BF16 entry, NOT
        // misclassify it as F16 once we lowercase the haystack.
        let v = names(&["mmproj-BF16.gguf"]);
        assert_eq!(pick_mmproj_variant(&v), "mmproj-BF16.gguf");
    }

    #[test]
    fn mixed_case_quant_tag_resolves() {
        let v = names(&["mmproj-Bf16.gguf", "mmproj-f16.gguf"]);
        assert_eq!(pick_mmproj_variant(&v), "mmproj-f16.gguf");
    }
}
