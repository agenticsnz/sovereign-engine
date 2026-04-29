use std::sync::Arc;

use axum::body::Body;
use axum::http::{Response, StatusCode};
use bytes::Bytes;
use futures::StreamExt;
use reqwest::Client;
use tokio::sync::mpsc;
use tracing::error;

use crate::supervisor::{ProbeKick, ProbeReason};
use crate::AppState;

/// Result of proxying to a backend. For non-streaming responses, includes
/// the raw body bytes so callers can extract usage data.
pub struct ProxyResult {
    pub response: Response<Body>,
    /// Raw response body bytes (only populated for non-streaming successful responses)
    pub body_bytes: Option<Bytes>,
}

/// Fire-and-forget kick to the supervisor. Uses `try_send` so a full channel
/// (which would imply many concurrent failures, all triggering kicks) cannot
/// block the hot path — the supervisor will still catch up on its next 10s
/// tick. A closed channel is also tolerated (the supervisor task may have
/// shut down on proxy exit).
fn kick_supervisor(probe_tx: &mpsc::Sender<ProbeKick>, model_id: &str) {
    let _ = probe_tx.try_send(ProbeKick {
        model_id: model_id.to_string(),
        reason: ProbeReason::OnFailure,
    });
}

/// Hot-path "worked-flag" recorder.
///
/// Called immediately after we have a `StatusCode` from the backend's first
/// response on the hot path. On a 2xx, compare-exchange the in-memory
/// atomic; on the false→true transition, fire-and-forget a single
/// `UPDATE models SET worked = 1` so the flag survives a proxy restart.
/// Cost on subsequent 2xx responses for the same backend instance: a single
/// atomic load + a failed compare_exchange, zero DB I/O.
///
/// Extracted from `proxy_to_backend` so it can be unit-tested without
/// spinning up an HTTP backend stub. The hot-path call is one line.
pub fn record_worked(state: &Arc<AppState>, model_id: &str, status: axum::http::StatusCode) {
    if !status.is_success() {
        return;
    }
    use std::sync::atomic::{AtomicBool, Ordering};
    let entry = state
        .worked_map
        .entry(model_id.to_string())
        .or_insert_with(|| AtomicBool::new(false));
    if entry
        .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
        .is_ok()
    {
        // Transition fired — fire-and-forget DB UPDATE.
        let pool = state.db.pool.clone();
        let model_id_owned = model_id.to_string();
        tokio::spawn(async move {
            if let Err(e) = sqlx::query("UPDATE models SET worked = 1 WHERE id = ?")
                .bind(&model_id_owned)
                .execute(&pool)
                .await
            {
                tracing::error!(model = %model_id_owned, error = %e, "Failed to persist worked=1");
            }
        });
    }
}

/// Forward a request to an inference backend and stream the response back.
/// Handles both streaming (SSE) and non-streaming responses transparently.
/// If `api_key` is provided, sends `Authorization: Bearer <key>` to the backend.
///
/// On any non-2xx outcome — connect-failure or 5xx response — fires a
/// [`ProbeKick`] at the supervisor so it diagnoses the backend immediately
/// rather than waiting for the next 10s tick (Phase-3 design decision 2).
///
/// On a 2xx response, flips the per-model `worked_map` atomic via
/// [`record_worked`] (Phase 4) — the earliest signal that this backend has
/// served a real answer since its current container start.
pub async fn proxy_to_backend(
    client: &Client,
    backend_url: &str,
    body: Bytes,
    is_streaming: bool,
    api_key: Option<&str>,
    model_id: &str,
    state: &Arc<AppState>,
) -> ProxyResult {
    let mut request = client
        .post(backend_url)
        .header("content-type", "application/json");

    if let Some(key) = api_key {
        request = request.header("authorization", format!("Bearer {}", key));
    }

    let response = match request.body(body).send().await {
        Ok(resp) => resp,
        Err(e) => {
            error!(error = %e, "Failed to connect to backend");
            // Connect-failure kick — supervisor probes this model immediately.
            kick_supervisor(&state.probe_tx, model_id);
            return ProxyResult {
                response: Response::builder()
                    .status(StatusCode::BAD_GATEWAY)
                    .header("content-type", "application/json")
                    .body(Body::from(
                        serde_json::json!({
                            "error": {
                                "message": "Backend unavailable",
                                "type": "server_error",
                                "code": "backend_unavailable"
                            }
                        })
                        .to_string(),
                    ))
                    .unwrap(),
                body_bytes: None,
            };
        }
    };

    let status = response.status();
    let headers = response.headers().clone();

    // Phase 4 hot-path worked-flag flip — runs before the streaming/non-streaming
    // split so both response shapes flip on the same earliest signal.
    record_worked(
        state,
        model_id,
        axum::http::StatusCode::from_u16(status.as_u16()).unwrap_or(StatusCode::OK),
    );

    // 5xx response kick — supervisor probes immediately.
    if status.is_server_error() {
        kick_supervisor(&state.probe_tx, model_id);
    }

    if is_streaming {
        // Stream SSE events back to the client
        let stream = response.bytes_stream().map(|chunk| {
            chunk.map_err(|e| {
                error!(error = %e, "Error streaming from backend");
                std::io::Error::other(e)
            })
        });

        let mut builder = Response::builder()
            .status(status.as_u16())
            .header("content-type", "text/event-stream")
            .header("cache-control", "no-cache")
            .header("connection", "keep-alive");

        // Preserve transfer-encoding if present
        if let Some(te) = headers.get("transfer-encoding") {
            builder = builder.header("transfer-encoding", te);
        }

        ProxyResult {
            response: builder.body(Body::from_stream(stream)).unwrap(),
            body_bytes: None,
        }
    } else {
        // Non-streaming: collect full response and forward
        match response.bytes().await {
            Ok(body_bytes) => {
                let mut builder = Response::builder().status(status.as_u16());

                if let Some(ct) = headers.get("content-type") {
                    builder = builder.header("content-type", ct);
                } else {
                    builder = builder.header("content-type", "application/json");
                }

                ProxyResult {
                    response: builder.body(Body::from(body_bytes.clone())).unwrap(),
                    body_bytes: Some(body_bytes),
                }
            }
            Err(e) => {
                error!(error = %e, "Failed to read backend response body");
                ProxyResult {
                    response: Response::builder()
                        .status(StatusCode::BAD_GATEWAY)
                        .header("content-type", "application/json")
                        .body(Body::from(
                            serde_json::json!({
                                "error": {
                                    "message": "Failed to read backend response",
                                    "type": "server_error",
                                    "code": "backend_error"
                                }
                            })
                            .to_string(),
                        ))
                        .unwrap(),
                    body_bytes: None,
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};

    /// Build a minimal AppState for hot-path tests. Mirrors the helper in
    /// supervisor::tests::build_test_state but lives here so streaming.rs
    /// owns its own test fixture.
    async fn build_test_state() -> Arc<AppState> {
        use crate::config::AppConfig;
        use crate::db::Database;
        use crate::docker::DockerManager;
        use crate::metrics::MetricsBroadcaster;
        use crate::scheduler::reservation::ReservationBroadcaster;
        use crate::scheduler::Scheduler;

        let db = Database::test_db().await;
        let (probe_tx, _probe_rx) = crate::supervisor::channel();
        Arc::new(AppState {
            config: AppConfig {
                listen_addr: "127.0.0.1:0".to_string(),
                database_url: "sqlite::memory:".to_string(),
                tls_cert_path: None,
                tls_key_path: None,
                bootstrap_user: None,
                bootstrap_password: None,
                break_glass: false,
                docker_host: "unix:///var/run/docker.sock".to_string(),
                model_path: "/tmp/test-models-streaming".to_string(),
                model_host_path: "/tmp/test-models-streaming".to_string(),
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
            },
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

    /// Insert a `models` row with `worked = 0` so the post-flip UPDATE has a
    /// row to target.
    async fn seed_model_row(state: &Arc<AppState>, model_id: &str) {
        sqlx::query(
            "INSERT INTO models (id, hf_repo, filename, backend_type, loaded, worked, runtime_overrides) \
             VALUES (?, ?, ?, 'llamacpp', 0, 0, '{}')",
        )
        .bind(model_id)
        .bind("owner/repo")
        .bind("model.gguf")
        .execute(&state.db.pool)
        .await
        .expect("insert model");
    }

    async fn read_db_worked(state: &Arc<AppState>, model_id: &str) -> i64 {
        sqlx::query_scalar("SELECT worked FROM models WHERE id = ?")
            .bind(model_id)
            .fetch_one(&state.db.pool)
            .await
            .expect("query worked")
    }

    /// 1. First 2xx flips the in-memory atomic to true.
    #[tokio::test]
    async fn record_worked_2xx_flips_atomic_to_true() {
        let state = build_test_state().await;
        let model_id = "m-flip";
        seed_model_row(&state, model_id).await;

        record_worked(&state, model_id, StatusCode::OK);

        let flag = state
            .worked_map
            .get(model_id)
            .expect("entry inserted")
            .load(Ordering::SeqCst);
        assert!(flag, "first 2xx must flip atomic to true");
    }

    /// 1b. The flip schedules a DB UPDATE; `models.worked = 1` after spawn drains.
    #[tokio::test]
    async fn record_worked_2xx_persists_db_row() {
        let state = build_test_state().await;
        let model_id = "m-db-flip";
        seed_model_row(&state, model_id).await;

        record_worked(&state, model_id, StatusCode::OK);

        // Yield long enough for the spawned UPDATE to drain. The test pool is
        // in-memory so the round-trip is trivial; 100ms is comfortable.
        for _ in 0..20 {
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
            if read_db_worked(&state, model_id).await == 1 {
                return;
            }
        }
        panic!("expected models.worked = 1 within 100ms of the flip");
    }

    /// 2. Subsequent 2xxs are no-ops — the atomic stays true and
    ///    `compare_exchange` returns Err on every call after the first.
    #[tokio::test]
    async fn record_worked_repeated_2xx_only_first_flips() {
        let state = build_test_state().await;
        let model_id = "m-repeat";
        seed_model_row(&state, model_id).await;

        // First call flips.
        record_worked(&state, model_id, StatusCode::OK);
        // Subsequent calls hit the entry's atomic but compare_exchange fails.
        for _ in 0..5 {
            record_worked(&state, model_id, StatusCode::OK);
        }

        let entry = state.worked_map.get(model_id).expect("entry present");
        // The atomic stays true.
        assert!(entry.load(Ordering::SeqCst));
        // And calling compare_exchange ourselves returns Err — proving the
        // hot path's compare_exchange would also have returned Err on every
        // call after the first, hence no DB write.
        assert!(entry
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .is_err());
    }

    /// 3. Non-2xx (5xx) responses do NOT flip the atomic.
    #[tokio::test]
    async fn record_worked_5xx_does_not_flip() {
        let state = build_test_state().await;
        let model_id = "m-5xx";
        seed_model_row(&state, model_id).await;

        record_worked(&state, model_id, StatusCode::INTERNAL_SERVER_ERROR);
        record_worked(&state, model_id, StatusCode::BAD_GATEWAY);
        record_worked(&state, model_id, StatusCode::SERVICE_UNAVAILABLE);

        // No entry was inserted — non-2xx returns before the entry() call.
        assert!(state.worked_map.get(model_id).is_none());

        // And the DB column stays 0.
        assert_eq!(read_db_worked(&state, model_id).await, 0);
    }

    /// 4. 4xx responses also do NOT flip — only 2xx counts as "worked".
    #[tokio::test]
    async fn record_worked_4xx_does_not_flip() {
        let state = build_test_state().await;
        let model_id = "m-4xx";
        seed_model_row(&state, model_id).await;

        record_worked(&state, model_id, StatusCode::BAD_REQUEST);
        record_worked(&state, model_id, StatusCode::TOO_MANY_REQUESTS);

        assert!(state.worked_map.get(model_id).is_none());
        assert_eq!(read_db_worked(&state, model_id).await, 0);
    }

    /// 5. Multiple 2xx codes (200, 201, 204) all count.
    #[tokio::test]
    async fn record_worked_various_2xx_codes_all_count() {
        for code in [
            StatusCode::OK,
            StatusCode::CREATED,
            StatusCode::ACCEPTED,
            StatusCode::NO_CONTENT,
        ] {
            let state = build_test_state().await;
            let model_id = "m-various";
            seed_model_row(&state, model_id).await;

            record_worked(&state, model_id, code);
            let entry = state
                .worked_map
                .get(model_id)
                .unwrap_or_else(|| panic!("entry should exist for {}", code));
            assert!(
                entry.load(Ordering::SeqCst),
                "status {} should flip the atomic",
                code
            );
        }
    }

    /// 6. read_worked: in-memory `true` is returned without touching DB.
    #[tokio::test]
    async fn read_worked_memory_true_wins_over_db_zero() {
        let state = build_test_state().await;
        let model_id = "m-mem-wins";
        // DB says 0; in-memory says true. Memory must win.
        seed_model_row(&state, model_id).await;
        state
            .worked_map
            .insert(model_id.to_string(), AtomicBool::new(true));

        let got = crate::supervisor::read_worked(&state, model_id).await;
        assert!(got, "in-memory true must win even when DB column is 0");
    }

    /// 7. read_worked: in-memory `false` returns false (memory still wins).
    #[tokio::test]
    async fn read_worked_memory_false_returned() {
        let state = build_test_state().await;
        let model_id = "m-mem-false";
        seed_model_row(&state, model_id).await;
        state
            .worked_map
            .insert(model_id.to_string(), AtomicBool::new(false));

        let got = crate::supervisor::read_worked(&state, model_id).await;
        assert!(!got);
    }

    /// 8. read_worked: cold-start fallback — empty map, DB has worked = 1.
    #[tokio::test]
    async fn read_worked_cold_start_falls_back_to_db_true() {
        let state = build_test_state().await;
        let model_id = "m-cold-start";
        sqlx::query(
            "INSERT INTO models (id, hf_repo, filename, backend_type, loaded, worked, runtime_overrides) \
             VALUES (?, ?, ?, 'llamacpp', 0, 1, '{}')",
        )
        .bind(model_id)
        .bind("owner/repo")
        .bind("model.gguf")
        .execute(&state.db.pool)
        .await
        .expect("insert model");

        // worked_map is empty — read_worked must consult the DB.
        assert!(state.worked_map.get(model_id).is_none());

        let got = crate::supervisor::read_worked(&state, model_id).await;
        assert!(got, "DB worked=1 must be returned when no in-memory entry");
    }

    /// 9. read_worked: cold-start fallback — DB has worked = 0.
    #[tokio::test]
    async fn read_worked_cold_start_falls_back_to_db_false() {
        let state = build_test_state().await;
        let model_id = "m-cold-zero";
        seed_model_row(&state, model_id).await;

        let got = crate::supervisor::read_worked(&state, model_id).await;
        assert!(!got);
    }

    /// 10. read_worked: model row missing → false (defensive default).
    #[tokio::test]
    async fn read_worked_missing_model_returns_false() {
        let state = build_test_state().await;
        let got = crate::supervisor::read_worked(&state, "no-such-model").await;
        assert!(!got);
    }

    /// 11. reconcile_dead_backend drops the in-memory worked entry.
    #[tokio::test]
    async fn reconcile_drops_worked_map_entry() {
        let state = build_test_state().await;
        let model_id = "m-reconcile-worked";
        seed_model_row(&state, model_id).await;

        // Pre-populate the worked entry (as if a 2xx had flipped it).
        state
            .worked_map
            .insert(model_id.to_string(), AtomicBool::new(true));
        assert!(state.worked_map.get(model_id).is_some());

        crate::api::common::reconcile_dead_backend(&state, model_id, None, "test_drop_worked")
            .await;

        assert!(
            state.worked_map.get(model_id).is_none(),
            "reconcile must remove the worked_map entry"
        );
    }

    /// 12. End-to-end via proxy_to_backend: a 200 from a real (in-test) HTTP
    ///     stub flips the worked flag. Covers the integration that
    ///     record_worked is actually wired to the hot path.
    #[tokio::test]
    async fn proxy_to_backend_2xx_flips_worked_via_hot_path() {
        use axum::routing::post;
        use axum::Router;

        let state = build_test_state().await;
        let model_id = "m-e2e";
        seed_model_row(&state, model_id).await;

        // Spin up a one-shot stub backend on a random port.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind stub");
        let addr = listener.local_addr().expect("addr");
        let app = Router::new().route(
            "/v1/chat/completions",
            post(|| async {
                axum::http::Response::builder()
                    .status(200)
                    .header("content-type", "application/json")
                    .body(axum::body::Body::from(r#"{"ok":true}"#))
                    .unwrap()
            }),
        );
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        let url = format!("http://{}/v1/chat/completions", addr);
        let client = reqwest::Client::new();
        let result = proxy_to_backend(
            &client,
            &url,
            Bytes::from_static(b"{}"),
            false,
            None,
            model_id,
            &state,
        )
        .await;

        assert_eq!(result.response.status().as_u16(), 200);

        // Confirm the in-memory atomic flipped.
        let flag = state
            .worked_map
            .get(model_id)
            .expect("entry inserted by hot path")
            .load(Ordering::SeqCst);
        assert!(flag, "hot path must flip worked atomic on 2xx");

        server.abort();
    }

    /// 13. End-to-end via proxy_to_backend with a 5xx response — the worked
    ///     flag stays absent, the supervisor kick fires (visible by checking
    ///     that no entry was added to worked_map; the kick channel itself
    ///     is exercised by Phase 3 tests).
    #[tokio::test]
    async fn proxy_to_backend_5xx_does_not_flip_worked() {
        use axum::routing::post;
        use axum::Router;

        let state = build_test_state().await;
        let model_id = "m-e2e-5xx";
        seed_model_row(&state, model_id).await;

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind stub");
        let addr = listener.local_addr().expect("addr");
        let app = Router::new().route(
            "/v1/chat/completions",
            post(|| async {
                axum::http::Response::builder()
                    .status(500)
                    .header("content-type", "application/json")
                    .body(axum::body::Body::from(r#"{"error":"boom"}"#))
                    .unwrap()
            }),
        );
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        let url = format!("http://{}/v1/chat/completions", addr);
        let client = reqwest::Client::new();
        let _ = proxy_to_backend(
            &client,
            &url,
            Bytes::from_static(b"{}"),
            false,
            None,
            model_id,
            &state,
        )
        .await;

        assert!(
            state.worked_map.get(model_id).is_none(),
            "5xx response must NOT flip worked"
        );
        assert_eq!(read_db_worked(&state, model_id).await, 0);

        server.abort();
    }
}
