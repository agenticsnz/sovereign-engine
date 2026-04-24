use std::collections::HashMap;
use std::sync::Arc;

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::{delete, get, post};
use axum::{Json, Router};
use futures::StreamExt;
use serde::{Deserialize, Serialize};
use tokio::sync::RwLock;
use tracing::{error, info, warn};
use uuid::Uuid;

use crate::AppState;

// ---------------------------------------------------------------------------
// Download state — shared across handlers and background tasks
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize)]
pub struct DownloadState {
    pub id: String,
    pub hf_repo: String,
    pub progress_bytes: u64,
    pub total_bytes: u64,
    pub status: DownloadStatus,
    pub error: Option<String>,
    pub category_id: Option<String>,
    pub backend_type: String,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum DownloadStatus {
    Downloading,
    Complete,
    Failed,
    Cancelled,
}

pub type Downloads = Arc<RwLock<HashMap<String, DownloadState>>>;

// ---------------------------------------------------------------------------
// Shared state wrapper — holds Downloads + a handle to AppState
// ---------------------------------------------------------------------------

#[derive(Clone)]
pub struct HfState {
    pub app: Arc<AppState>,
    pub downloads: Downloads,
}

// ---------------------------------------------------------------------------
// Router
// ---------------------------------------------------------------------------

pub fn routes(state: Arc<AppState>) -> Router {
    let hf_state = HfState {
        app: state,
        downloads: Arc::new(RwLock::new(HashMap::new())),
    };

    Router::new()
        .route("/search", get(search_models))
        .route("/files", get(list_repo_files))
        .route("/download", post(start_download))
        .route("/downloads", get(list_downloads))
        .route("/downloads/{id}", delete(cancel_download))
        .with_state(hf_state)
}

// ---------------------------------------------------------------------------
// GET /search?q=<query>&task=<task>
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct SearchQuery {
    q: Option<String>,
    task: Option<String>,
    tags: Option<String>,
    offset: Option<usize>,
    limit: Option<usize>,
}

#[derive(Debug, Deserialize)]
struct HfModelResult {
    #[serde(rename = "modelId")]
    model_id: Option<String>,
    id: Option<String>,
    downloads: Option<u64>,
    likes: Option<u64>,
    #[serde(rename = "pipeline_tag")]
    pipeline_tag: Option<String>,
    tags: Option<Vec<String>>,
}

async fn search_models(
    State(_state): State<HfState>,
    Query(params): Query<SearchQuery>,
) -> impl IntoResponse {
    let mut query = params.q.unwrap_or_default();
    let task = params.task.unwrap_or_default();
    let offset = params.offset.unwrap_or(0);
    let limit = params.limit.unwrap_or(20).min(100);

    // When GGUF filter is active, add "GGUF" to the search so HF ranks GGUF repos first
    if params.tags.as_deref() == Some("gguf") && !query.to_lowercase().contains("gguf") {
        query = format!("{} GGUF", query);
    }

    // Request extra from HF to compensate for client-side GGUF name filtering
    let hf_limit = if params.tags.as_deref() == Some("gguf") {
        (limit + offset) * 3
    } else {
        limit + offset
    };

    let mut url = format!(
        "https://huggingface.co/api/models?search={}&sort=downloads&direction=-1&limit={}",
        urlencoded(&query),
        hf_limit,
    );

    // Only filter by pipeline_tag if a specific task was requested (not empty / "any")
    if !task.is_empty() && task != "any" {
        url.push_str(&format!("&pipeline_tag={}", urlencoded(&task)));
    }

    if let Some(ref tags) = params.tags {
        for tag in tags.split(',') {
            url.push_str(&format!("&tags={}", urlencoded(tag.trim())));
        }
    }

    let client = match reqwest::Client::builder()
        .user_agent("sovereign-engine/0.1")
        .build()
    {
        Ok(c) => c,
        Err(e) => {
            return super::error::internal_error("hf:build_http_client", e);
        }
    };

    let resp = match client.get(&url).send().await {
        Ok(r) => r,
        Err(e) => {
            return (
                StatusCode::BAD_GATEWAY,
                Json(
                    serde_json::json!({ "error": format!("HuggingFace API request failed: {e}") }),
                ),
            )
                .into_response();
        }
    };

    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        return (
            StatusCode::BAD_GATEWAY,
            Json(serde_json::json!({
                "error": format!("HuggingFace API returned {status}: {body}")
            })),
        )
            .into_response();
    }

    let hf_models: Vec<HfModelResult> = match resp.json().await {
        Ok(m) => m,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({ "error": format!("Failed to parse HuggingFace response: {e}") })),
            )
                .into_response();
        }
    };

    let require_gguf_name = params.tags.as_deref() == Some("gguf");

    let all_models: Vec<serde_json::Value> = hf_models
        .into_iter()
        .filter_map(|m| {
            let id = m.model_id.or(m.id).unwrap_or_default();
            // When GGUF filter is active, only show repos with GGUF in the name
            // (HF's tag filter is too loose — includes repos that merely contain GGUF files)
            if require_gguf_name && !id.to_lowercase().contains("gguf") {
                return None;
            }
            Some(serde_json::json!({
                "id": id,
                "downloads": m.downloads.unwrap_or(0),
                "likes": m.likes.unwrap_or(0),
                "pipeline_tag": m.pipeline_tag,
                "tags": m.tags.unwrap_or_default(),
            }))
        })
        .collect();

    let has_more = all_models.len() > offset + limit;
    let models: Vec<serde_json::Value> = all_models.into_iter().skip(offset).take(limit).collect();

    Json(serde_json::json!({ "models": models, "has_more": has_more })).into_response()
}

// ---------------------------------------------------------------------------
// GET /files?repo=<repo>
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct FilesQuery {
    repo: String,
}

async fn list_repo_files(
    State(_state): State<HfState>,
    Query(params): Query<FilesQuery>,
) -> impl IntoResponse {
    let client = match reqwest::Client::builder()
        .user_agent("sovereign-engine/0.1")
        .build()
    {
        Ok(c) => c,
        Err(e) => {
            return super::error::internal_error("hf:build_http_client", e);
        }
    };

    if let Some(r) = super::error::validate_len("repo", &params.repo, super::error::MAX_NAME) {
        return r;
    }
    if let Some(r) = super::error::validate_hf_repo(&params.repo) {
        return r;
    }

    let tree_url = format!(
        "https://huggingface.co/api/models/{}/tree/main",
        params.repo
    );

    let resp = match client.get(&tree_url).send().await {
        Ok(r) => r,
        Err(e) => {
            return (
                StatusCode::BAD_GATEWAY,
                Json(
                    serde_json::json!({ "error": format!("HuggingFace API request failed: {e}") }),
                ),
            )
                .into_response();
        }
    };

    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        return (
            StatusCode::BAD_GATEWAY,
            Json(serde_json::json!({ "error": format!("HuggingFace API returned {status}: {body}") })),
        )
            .into_response();
    }

    let files: Vec<HfFileEntry> = match resp.json().await {
        Ok(f) => f,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({ "error": format!("Failed to parse file listing: {e}") })),
            )
                .into_response();
        }
    };

    let skip_files = [".gitattributes", ".gitignore", ".git"];
    let file_list: Vec<serde_json::Value> = files
        .iter()
        .filter(|f| f.file_type == "file" && !skip_files.iter().any(|s| f.path.starts_with(s)))
        .map(|f| {
            serde_json::json!({
                "path": f.path,
                "size": f.size.unwrap_or(0),
            })
        })
        .collect();

    Json(serde_json::json!({ "files": file_list })).into_response()
}

// ---------------------------------------------------------------------------
// POST /download
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct DownloadRequest {
    hf_repo: String,
    /// Optional list of specific files to download (e.g. a single GGUF file)
    files: Option<Vec<String>>,
    category_id: Option<String>,
    backend_type: Option<String>,
}

async fn start_download(
    State(state): State<HfState>,
    Json(req): Json<DownloadRequest>,
) -> impl IntoResponse {
    if let Some(r) = super::error::validate_len("hf_repo", &req.hf_repo, super::error::MAX_NAME) {
        return r;
    }
    if let Some(r) = super::error::validate_hf_repo(&req.hf_repo) {
        return r;
    }
    // Check disk space before starting
    let model_path = &state.app.config.model_path;
    match get_disk_usage(model_path) {
        Ok(disk) => {
            let usage_pct = if disk.total_bytes > 0 {
                (disk.used_bytes as f64 / disk.total_bytes as f64) * 100.0
            } else {
                0.0
            };
            if usage_pct >= 95.0 {
                return (
                    StatusCode::INSUFFICIENT_STORAGE,
                    Json(serde_json::json!({
                        "error": format!(
                            "Disk usage at {:.1}% — downloads blocked above 95%",
                            usage_pct
                        )
                    })),
                )
                    .into_response();
            }
            if usage_pct >= 90.0 {
                warn!(
                    usage_pct = format!("{:.1}%", usage_pct),
                    "Disk usage above 90% warning threshold"
                );
            }
        }
        Err(e) => {
            warn!("Could not check disk usage: {e}");
            // Continue anyway — don't block downloads if df fails
        }
    }

    // Check if we're already downloading this repo
    {
        let downloads = state.downloads.read().await;
        for dl in downloads.values() {
            if dl.hf_repo == req.hf_repo && dl.status == DownloadStatus::Downloading {
                return (
                    StatusCode::CONFLICT,
                    Json(serde_json::json!({
                        "error": format!("Download already in progress for {}", req.hf_repo),
                        "download_id": dl.id,
                    })),
                )
                    .into_response();
            }
        }
    }

    let download_id = Uuid::new_v4().to_string();

    // Create initial download state
    let dl_state = DownloadState {
        id: download_id.clone(),
        hf_repo: req.hf_repo.clone(),
        progress_bytes: 0,
        total_bytes: 0,
        status: DownloadStatus::Downloading,
        error: None,
        category_id: req.category_id.clone(),
        backend_type: req
            .backend_type
            .clone()
            .unwrap_or_else(|| "llamacpp".to_string()),
    };

    {
        let mut downloads = state.downloads.write().await;
        downloads.insert(download_id.clone(), dl_state);
    }

    // Spawn background download task
    let downloads = state.downloads.clone();
    let app_state = state.app.clone();
    let hf_repo = req.hf_repo.clone();
    let file_filter = req.files.clone();
    let category_id = req.category_id.clone();
    let backend_type = req.backend_type.clone();
    let dl_id = download_id.clone();

    tokio::spawn(async move {
        run_download(
            app_state,
            downloads,
            dl_id,
            hf_repo,
            file_filter,
            category_id,
            backend_type,
        )
        .await;
    });

    (
        StatusCode::ACCEPTED,
        Json(serde_json::json!({
            "download_id": download_id,
            "status": "started",
        })),
    )
        .into_response()
}

// ---------------------------------------------------------------------------
// GET /downloads
// ---------------------------------------------------------------------------

async fn list_downloads(State(state): State<HfState>) -> impl IntoResponse {
    let downloads = state.downloads.read().await;
    let data: Vec<serde_json::Value> = downloads
        .values()
        .map(|dl| {
            serde_json::json!({
                "id": dl.id,
                "hf_repo": dl.hf_repo,
                "progress_bytes": dl.progress_bytes,
                "total_bytes": dl.total_bytes,
                "status": dl.status,
                "error": dl.error,
            })
        })
        .collect();

    Json(serde_json::json!({ "downloads": data }))
}

// ---------------------------------------------------------------------------
// DELETE /downloads/:id
// ---------------------------------------------------------------------------

async fn cancel_download(
    State(state): State<HfState>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    let mut downloads = state.downloads.write().await;

    match downloads.get_mut(&id) {
        Some(dl) => {
            if dl.status == DownloadStatus::Downloading {
                dl.status = DownloadStatus::Cancelled;
                info!(download_id = %id, "Download cancelled");
                Json(serde_json::json!({ "status": "cancelled" })).into_response()
            } else {
                (
                    StatusCode::CONFLICT,
                    Json(serde_json::json!({
                        "error": format!("Download is not active (status: {:?})", dl.status)
                    })),
                )
                    .into_response()
            }
        }
        None => (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({ "error": "Download not found" })),
        )
            .into_response(),
    }
}

// ---------------------------------------------------------------------------
// Background download task — helper functions
// ---------------------------------------------------------------------------

/// Build an HTTP client with optional HuggingFace token authentication.
fn build_hf_client(hf_token: &Option<String>) -> Result<reqwest::Client, String> {
    let mut client_builder = reqwest::Client::builder().user_agent("sovereign-engine/0.1");

    if let Some(ref token) = hf_token {
        let mut headers = reqwest::header::HeaderMap::new();
        if let Ok(val) = reqwest::header::HeaderValue::from_str(&format!("Bearer {token}")) {
            headers.insert(reqwest::header::AUTHORIZATION, val);
        }
        client_builder = client_builder.default_headers(headers);
    }

    client_builder
        .build()
        .map_err(|e| format!("HTTP client error: {e}"))
}

/// Fetch the file tree from a HuggingFace repo and filter to downloadable files.
async fn list_downloadable_files(
    client: &reqwest::Client,
    hf_repo: &str,
    file_filter: &Option<Vec<String>>,
) -> Result<Vec<HfFileEntry>, String> {
    let tree_url = format!("https://huggingface.co/api/models/{}/tree/main", hf_repo);
    let tree_resp = client
        .get(&tree_url)
        .send()
        .await
        .map_err(|e| format!("Failed to list repo files: {e}"))?;

    if !tree_resp.status().is_success() {
        let status = tree_resp.status();
        let body = tree_resp.text().await.unwrap_or_default();
        return Err(format!("HuggingFace tree API returned {status}: {body}"));
    }

    let files: Vec<HfFileEntry> = tree_resp
        .json()
        .await
        .map_err(|e| format!("Failed to parse file listing: {e}"))?;

    let skip_files = [".gitattributes", ".gitignore", ".git"];
    let downloadable: Vec<HfFileEntry> = files
        .into_iter()
        .filter(|f| {
            if f.file_type != "file" {
                return false;
            }
            if skip_files.iter().any(|s| f.path.starts_with(s)) {
                return false;
            }
            should_include(&f.path, file_filter.as_deref())
        })
        .collect();

    if downloadable.is_empty() {
        return Err("No files found in repository".to_string());
    }

    Ok(downloadable)
}

/// Validate that the download (plus other in-flight downloads) will fit on disk.
/// Returns Err(()) if disk space is insufficient (error is reported via set_download_error).
async fn validate_disk_space(
    app_state: &AppState,
    downloads: &Downloads,
    download_id: &str,
    total_bytes: u64,
) -> Result<(), ()> {
    let disk = match get_disk_usage(&app_state.config.model_path) {
        Ok(d) => d,
        Err(_) => return Ok(()), // Can't check — don't block
    };

    let other_inflight: u64 = {
        let dls = downloads.read().await;
        dls.values()
            .filter(|dl| dl.id != download_id && dl.status == DownloadStatus::Downloading)
            .map(|dl| dl.total_bytes.saturating_sub(dl.progress_bytes))
            .sum()
    };

    let required = total_bytes + other_inflight;
    let projected_used = disk.used_bytes + required;
    let projected_pct = if disk.total_bytes > 0 {
        (projected_used as f64 / disk.total_bytes as f64) * 100.0
    } else {
        0.0
    };

    if required > disk.free_bytes {
        set_download_error(
            downloads,
            download_id,
            &format!(
                "Not enough disk space: need {} but only {} free",
                format_bytes(required),
                format_bytes(disk.free_bytes),
            ),
        )
        .await;
        return Err(());
    }

    if projected_pct >= 95.0 {
        set_download_error(
            downloads,
            download_id,
            &format!(
                "Download would push disk to {:.1}% (threshold 95%): need {}, {} free",
                projected_pct,
                format_bytes(required),
                format_bytes(disk.free_bytes),
            ),
        )
        .await;
        return Err(());
    }

    if projected_pct >= 90.0 {
        warn!(
            projected_pct = format!("{:.1}%", projected_pct),
            download_bytes = total_bytes,
            "Download will push disk above 90% warning threshold"
        );
    }

    Ok(())
}

/// Download all files to disk with progress tracking and cancellation support.
/// Creates the destination directory and streams each file.
/// Returns the total bytes downloaded on success, or Err(()) if the download
/// was cancelled or an error occurred (reported via set_download_error).
async fn download_files_to_disk(
    client: &reqwest::Client,
    downloads: &Downloads,
    download_id: &str,
    downloadable: &[HfFileEntry],
    dest_dir: &str,
    hf_repo: &str,
) -> Result<u64, ()> {
    if let Err(e) = tokio::fs::create_dir_all(dest_dir).await {
        set_download_error(
            downloads,
            download_id,
            &format!("Failed to create directory {dest_dir}: {e}"),
        )
        .await;
        return Err(());
    }

    let mut total_downloaded: u64 = 0;

    for file in downloadable {
        // Check for cancellation between files
        if is_download_cancelled(downloads, download_id).await {
            info!(download_id = %download_id, "Download was cancelled, stopping");
            return Err(());
        }

        total_downloaded += download_single_file(
            client,
            downloads,
            download_id,
            file,
            dest_dir,
            hf_repo,
            total_downloaded,
        )
        .await?;
    }

    Ok(total_downloaded)
}

/// Check whether a download has been cancelled.
async fn is_download_cancelled(downloads: &Downloads, download_id: &str) -> bool {
    let dls = downloads.read().await;
    dls.get(download_id)
        .is_some_and(|dl| dl.status == DownloadStatus::Cancelled)
}

/// Build a user-facing hint for HTTP errors from HuggingFace.
fn hf_http_error_hint(status: reqwest::StatusCode) -> &'static str {
    if status == reqwest::StatusCode::UNAUTHORIZED || status == reqwest::StatusCode::FORBIDDEN {
        if std::env::var("HF_TOKEN").is_ok() {
            " — HF_TOKEN may lack access to this gated model"
        } else {
            " — this may be a gated model; set HF_TOKEN env var to authenticate"
        }
    } else {
        ""
    }
}

/// Stream an HTTP response body to a file on disk with progress tracking and
/// cancellation support. Returns the number of bytes written.
async fn stream_response_to_file(
    resp: reqwest::Response,
    file_dest: &str,
    file_path: &str,
    downloads: &Downloads,
    download_id: &str,
    progress_offset: u64,
) -> Result<u64, ()> {
    let mut out_file = match tokio::fs::File::create(file_dest).await {
        Ok(f) => f,
        Err(e) => {
            set_download_error(
                downloads,
                download_id,
                &format!("Failed to create file {file_dest}: {e}"),
            )
            .await;
            return Err(());
        }
    };

    let mut file_downloaded: u64 = 0;
    let mut stream = resp.bytes_stream();
    while let Some(chunk_result) = stream.next().await {
        if is_download_cancelled(downloads, download_id).await {
            info!(download_id = %download_id, "Download cancelled during transfer");
            let _ = tokio::fs::remove_file(file_dest).await;
            return Err(());
        }

        match chunk_result {
            Ok(chunk) => {
                use tokio::io::AsyncWriteExt;
                if let Err(e) = out_file.write_all(&chunk).await {
                    set_download_error(
                        downloads,
                        download_id,
                        &format!("Write error for {file_path}: {e}"),
                    )
                    .await;
                    return Err(());
                }

                file_downloaded += chunk.len() as u64;

                let mut dls = downloads.write().await;
                if let Some(dl) = dls.get_mut(download_id) {
                    dl.progress_bytes = progress_offset + file_downloaded;
                }
            }
            Err(e) => {
                set_download_error(
                    downloads,
                    download_id,
                    &format!("Stream error for {file_path}: {e}"),
                )
                .await;
                return Err(());
            }
        }
    }

    Ok(file_downloaded)
}

/// Download a single file from HuggingFace to disk with progress tracking.
/// `progress_offset` is the cumulative bytes already downloaded (for progress reporting).
/// Returns the number of bytes downloaded for this file.
async fn download_single_file(
    client: &reqwest::Client,
    downloads: &Downloads,
    download_id: &str,
    file: &HfFileEntry,
    dest_dir: &str,
    hf_repo: &str,
    progress_offset: u64,
) -> Result<u64, ()> {
    // Reject path components that could escape the destination directory
    if file.path.contains("..") || file.path.starts_with('/') {
        set_download_error(
            downloads,
            download_id,
            &format!("Refusing file with unsafe path: {}", file.path),
        )
        .await;
        return Err(());
    }

    let file_url = format!(
        "https://huggingface.co/{}/resolve/main/{}",
        hf_repo, file.path
    );
    let file_dest = format!("{}/{}", dest_dir, file.path);

    // Create parent directory for nested files
    if let Some(parent) = std::path::Path::new(&file_dest).parent() {
        if let Err(e) = tokio::fs::create_dir_all(parent).await {
            set_download_error(
                downloads,
                download_id,
                &format!("Failed to create directory for {}: {e}", file.path),
            )
            .await;
            return Err(());
        }
    }

    info!(file = %file.path, url = %file_url, "Downloading file");

    let resp = match client.get(&file_url).send().await {
        Ok(r) => r,
        Err(e) => {
            set_download_error(
                downloads,
                download_id,
                &format!("Failed to download {}: {e}", file.path),
            )
            .await;
            return Err(());
        }
    };

    if !resp.status().is_success() {
        let status = resp.status();
        let hint = hf_http_error_hint(status);
        set_download_error(
            downloads,
            download_id,
            &format!("Download of {} returned HTTP {status}{hint}", file.path),
        )
        .await;
        return Err(());
    }

    stream_response_to_file(
        resp,
        &file_dest,
        &file.path,
        downloads,
        download_id,
        progress_offset,
    )
    .await
}

/// Detect the primary model file from a list of downloaded files.
/// Prefers the largest .gguf file, then the largest .safetensors file.
fn detect_primary_file(downloadable: &[HfFileEntry]) -> Option<String> {
    let mut best_gguf: Option<(&str, u64)> = None;
    let mut best_safetensors: Option<(&str, u64)> = None;
    for file in downloadable {
        let sz = file.size.unwrap_or(0);
        if file.path.ends_with(".gguf") && best_gguf.is_none_or(|(_, prev)| sz > prev) {
            best_gguf = Some((&file.path, sz));
        } else if file.path.ends_with(".safetensors")
            && best_safetensors.is_none_or(|(_, prev)| sz > prev)
        {
            best_safetensors = Some((&file.path, sz));
        }
    }
    best_gguf
        .or(best_safetensors)
        .map(|(path, _)| path.to_string())
}

/// True iff `path` names a companion multimodal-projector GGUF sitting at the
/// repository root. Accepts both `mmproj-*.gguf` and `mmproj_*.gguf`; rejects
/// anything nested under a subdirectory (repo-root only).
fn is_mmproj_filename(path: &str) -> bool {
    if path.contains('/') {
        return false;
    }
    (path.starts_with("mmproj-") || path.starts_with("mmproj_")) && path.ends_with(".gguf")
}

/// Pure filter decision for [`list_downloadable_files`].
///
/// - If `filter` is `None`, every non-skip file passes.
/// - If `filter` is `Some(list)`, only paths in `list` pass — **except** that
///   mmproj sibling files are always included when present (7b: user intent
///   for this card is "grab the mmproj if it's there").
fn should_include(path: &str, filter: Option<&[String]>) -> bool {
    if is_mmproj_filename(path) {
        return true;
    }
    match filter {
        None => true,
        Some(list) => list.iter().any(|p| p == path),
    }
}

/// Detect the companion mmproj (multimodal projector) GGUF from a list of
/// downloadable files. Returns `None` if none are present.
///
/// When multiple mmproj candidates exist, prefers `f16 > bf16 > f32` with a
/// lexical fallback. The `f16` probe is guarded against matching `bf16` —
/// same safety rule as [`crate::pick_mmproj_variant`] in the startup backfill.
///
/// Logs at `info!` when multiple candidates are present so operators can see
/// which variant was picked and which were skipped.
fn detect_mmproj_file(entries: &[HfFileEntry]) -> Option<String> {
    let mut candidates: Vec<&str> = entries
        .iter()
        .filter(|f| is_mmproj_filename(&f.path))
        .map(|f| f.path.as_str())
        .collect();
    if candidates.is_empty() {
        return None;
    }
    candidates.sort();

    let picked = if let Some(&n) = candidates
        .iter()
        .find(|n| n.contains("f16") && !n.contains("bf16"))
    {
        n
    } else if let Some(&n) = candidates.iter().find(|n| n.contains("bf16")) {
        n
    } else if let Some(&n) = candidates.iter().find(|n| n.contains("f32")) {
        n
    } else {
        candidates[0]
    };

    if candidates.len() > 1 {
        let skipped: Vec<&&str> = candidates.iter().filter(|c| **c != picked).collect();
        info!(
            picked = %picked,
            skipped = ?skipped,
            "mmproj download: multiple candidates, picked preferred variant"
        );
    }

    Some(picked.to_string())
}

// ---------------------------------------------------------------------------
// Background download task — orchestrator
// ---------------------------------------------------------------------------

/// All fields needed to INSERT a freshly-downloaded model into the `models`
/// table. Extracted into a struct so the INSERT is testable in isolation
/// (see `insert_downloaded_model_*` tests) and so `run_download` stays a
/// straight-line orchestrator.
struct DownloadedModelRow {
    model_id: String,
    hf_repo: String,
    primary_filename: Option<String>,
    /// Companion multimodal-projector filename (7b). `None` for text-only
    /// models; persisted at INSERT time so the row is correct without
    /// relying on the startup-time backfill.
    mmproj_filename: Option<String>,
    size_bytes: i64,
    category_id: Option<String>,
    backend_type: String,
    model_metadata: Option<String>,
    gguf_meta: GgufMetadata,
    kv_bpt_global: Option<i64>,
    kv_bpt_swa: Option<i64>,
    runtime_overrides_json: String,
}

/// Persist a freshly-downloaded model. Binds every column the download flow
/// knows about — including `mmproj_filename` (7b) — so a fresh row does not
/// depend on the startup backfill to become correct.
async fn insert_downloaded_model(
    pool: &sqlx::SqlitePool,
    row: &DownloadedModelRow,
) -> Result<sqlx::sqlite::SqliteQueryResult, sqlx::Error> {
    sqlx::query(
        "INSERT INTO models (id, hf_repo, filename, size_bytes, category_id, backend_type, model_metadata, context_length, n_layers, n_heads, n_kv_heads, embedding_length, key_length, value_length, sliding_window, kv_bytes_per_token_global, kv_bytes_per_token_swa, runtime_overrides, mmproj_filename) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(&row.model_id)
    .bind(&row.hf_repo)
    .bind(&row.primary_filename)
    .bind(row.size_bytes)
    .bind(&row.category_id)
    .bind(&row.backend_type)
    .bind(&row.model_metadata)
    .bind(row.gguf_meta.context_length.map(|v| v as i64))
    .bind(row.gguf_meta.block_count.map(|v| v as i64))
    .bind(row.gguf_meta.head_count.map(|v| v as i64))
    .bind(row.gguf_meta.head_count_kv.map(|v| v as i64))
    .bind(row.gguf_meta.embedding_length.map(|v| v as i64))
    .bind(row.gguf_meta.key_length.map(|v| v as i64))
    .bind(row.gguf_meta.value_length.map(|v| v as i64))
    .bind(row.gguf_meta.sliding_window.map(|v| v as i64))
    .bind(row.kv_bpt_global)
    .bind(row.kv_bpt_swa)
    .bind(&row.runtime_overrides_json)
    .bind(&row.mmproj_filename)
    .execute(pool)
    .await
}

async fn run_download(
    app_state: Arc<AppState>,
    downloads: Downloads,
    download_id: String,
    hf_repo: String,
    file_filter: Option<Vec<String>>,
    category_id: Option<String>,
    backend_type: Option<String>,
) {
    info!(hf_repo = %hf_repo, download_id = %download_id, "Starting model download");

    let hf_token = std::env::var("HF_TOKEN").ok();

    // Step 1: Build HTTP client with optional auth
    let client = match build_hf_client(&hf_token) {
        Ok(c) => c,
        Err(e) => {
            set_download_error(&downloads, &download_id, &e).await;
            return;
        }
    };

    // Step 2-3: List and filter downloadable files
    let downloadable = match list_downloadable_files(&client, &hf_repo, &file_filter).await {
        Ok(files) => files,
        Err(e) => {
            set_download_error(&downloads, &download_id, &e).await;
            return;
        }
    };

    // Step 4: Calculate total size and update download state
    let total_bytes: u64 = downloadable.iter().map(|f| f.size.unwrap_or(0)).sum();
    {
        let mut dls = downloads.write().await;
        if let Some(dl) = dls.get_mut(&download_id) {
            dl.total_bytes = total_bytes;
        }
    }

    // Step 5: Check disk space (including other in-flight downloads)
    if validate_disk_space(&app_state, &downloads, &download_id, total_bytes)
        .await
        .is_err()
    {
        return;
    }

    // Step 6-7: Create destination directory and download files
    let safe_repo = hf_repo.replace('/', "--");
    let dest_dir = format!("{}/{}", app_state.config.model_path, safe_repo);

    let total_downloaded = match download_files_to_disk(
        &client,
        &downloads,
        &download_id,
        &downloadable,
        &dest_dir,
        &hf_repo,
    )
    .await
    {
        Ok(bytes) => bytes,
        Err(()) => return,
    };

    // Step 8: Capture tokenizer metadata for future use (chat template detection, etc.)
    let model_metadata = fetch_tokenizer_config(&dest_dir, &hf_repo, &client).await;

    // Step 9: Extract architecture metadata from GGUF file
    let gguf_meta = {
        let mut meta: Option<GgufMetadata> = None;
        for file in &downloadable {
            if file.path.ends_with(".gguf") {
                let gguf_path = format!("{}/{}", dest_dir, file.path);
                match read_gguf_metadata(&gguf_path).await {
                    Ok(m) => {
                        info!(
                            file = %file.path,
                            context_length = ?m.context_length,
                            n_layers = ?m.block_count,
                            n_heads = ?m.head_count,
                            n_kv_heads = ?m.head_count_kv,
                            embedding_length = ?m.embedding_length,
                            "Extracted GGUF metadata"
                        );
                        meta = Some(m);
                        break;
                    }
                    Err(e) => {
                        warn!(file = %file.path, error = %e, "Failed to read GGUF metadata");
                    }
                }
            }
        }
        meta.unwrap_or_default()
    };

    // Step 10: Detect the primary model file and companion mmproj sibling
    let primary_filename = detect_primary_file(&downloadable);
    let mmproj_filename = detect_mmproj_file(&downloadable);
    if let Some(ref m) = mmproj_filename {
        info!(hf_repo = %hf_repo, mmproj = %m, "Detected companion mmproj file");
    }

    // Auto-mitigation for llama.cpp #21762: SWA-bearing dense models can crash
    // in the prompt-cache save path. Disable the cache by default for those.
    // Operator can override via PUT /api/admin/models/:id.
    let runtime_overrides_json = auto_runtime_overrides(&gguf_meta);
    if runtime_overrides_json != "{}" {
        info!(
            hf_repo = %hf_repo,
            sliding_window = ?gguf_meta.sliding_window,
            "Auto-setting runtime_overrides cache_ram_mib=0 (SWA + dense)"
        );
    }

    // Step 11: Register model in DB
    let model_id = Uuid::new_v4().to_string();
    let size_bytes = total_downloaded as i64;
    let bt = backend_type.as_deref().unwrap_or("llamacpp").to_string();
    let (kv_bpt_global, kv_bpt_swa) = compute_kv_aggregates(&gguf_meta);
    let row = DownloadedModelRow {
        model_id: model_id.clone(),
        hf_repo: hf_repo.clone(),
        primary_filename,
        mmproj_filename,
        size_bytes,
        category_id,
        backend_type: bt,
        model_metadata,
        gguf_meta,
        kv_bpt_global,
        kv_bpt_swa,
        runtime_overrides_json: runtime_overrides_json.to_string(),
    };
    match insert_downloaded_model(&app_state.db.pool, &row).await {
        Ok(_) => {
            info!(
                hf_repo = %hf_repo,
                model_id = %model_id,
                size_bytes = size_bytes,
                "Model downloaded and registered"
            );
        }
        Err(e) => {
            error!(hf_repo = %hf_repo, "Failed to register model in DB: {e}");
            set_download_error(
                &downloads,
                &download_id,
                &format!("Download complete but DB registration failed: {e}"),
            )
            .await;
            return;
        }
    }

    // Step 12: Mark download as complete
    let mut dls = downloads.write().await;
    if let Some(dl) = dls.get_mut(&download_id) {
        dl.status = DownloadStatus::Complete;
        dl.progress_bytes = total_downloaded;
    }
}

// ---------------------------------------------------------------------------
// Tokenizer metadata capture
// ---------------------------------------------------------------------------

/// Read tokenizer_config.json from a local directory, validating it as JSON.
async fn try_local_tokenizer(dest_dir: &str) -> Option<String> {
    let local_path = format!("{}/tokenizer_config.json", dest_dir);
    let contents = tokio::fs::read_to_string(&local_path).await.ok()?;
    serde_json::from_str::<serde_json::Value>(&contents).ok()?;
    Some(contents)
}

/// Fetch tokenizer_config.json from a remote URL, validating it as JSON.
async fn try_remote_tokenizer(client: &reqwest::Client, url: &str) -> Option<String> {
    let resp = client.get(url).send().await.ok()?;
    if !resp.status().is_success() {
        return None;
    }
    let text = resp.text().await.ok()?;
    serde_json::from_str::<serde_json::Value>(&text).ok()?;
    Some(text)
}

/// Look up the base_model for an HF repo via the API, then fetch its tokenizer.
async fn try_base_model_tokenizer(
    client: &reqwest::Client,
    hf_repo: &str,
) -> Option<(String, String)> {
    let api_url = format!("https://huggingface.co/api/models/{}", hf_repo);
    let resp = client.get(&api_url).send().await.ok()?;
    if !resp.status().is_success() {
        return None;
    }
    let model_info = resp.json::<serde_json::Value>().await.ok()?;

    // cardData.base_model can be a string or array of strings
    let base_model = model_info
        .get("cardData")
        .and_then(|cd| cd.get("base_model"))
        .and_then(|bm| {
            bm.as_str().map(String::from).or_else(|| {
                bm.as_array()
                    .and_then(|a| a.first())
                    .and_then(|v| v.as_str().map(String::from))
            })
        })?;

    let base_url = format!(
        "https://huggingface.co/{}/raw/main/tokenizer_config.json",
        base_model
    );
    let text = try_remote_tokenizer(client, &base_url).await?;
    Some((base_model, text))
}

/// Attempt to fetch tokenizer_config.json for a downloaded model.
///
/// Strategy:
/// 1. Read from local download directory (already downloaded)
/// 2. Fetch from HuggingFace repo directly
/// 3. Look up base_model in HF API and fetch from there
///
/// Returns the JSON string if found, None otherwise.
async fn fetch_tokenizer_config(
    dest_dir: &str,
    hf_repo: &str,
    client: &reqwest::Client,
) -> Option<String> {
    // 1. Try local file (may already be in the download)
    if let Some(contents) = try_local_tokenizer(dest_dir).await {
        info!(hf_repo = %hf_repo, source = "local", "Captured tokenizer_config.json");
        return Some(contents);
    }

    // 2. Fetch directly from the repo
    let url = format!(
        "https://huggingface.co/{}/raw/main/tokenizer_config.json",
        hf_repo
    );
    if let Some(text) = try_remote_tokenizer(client, &url).await {
        info!(hf_repo = %hf_repo, source = "repo", "Captured tokenizer_config.json");
        return Some(text);
    }

    // 3. Try to find base_model via HF API and fetch from there
    if let Some((base, text)) = try_base_model_tokenizer(client, hf_repo).await {
        info!(
            hf_repo = %hf_repo,
            base_model = %base,
            source = "base_model",
            "Captured tokenizer_config.json from base model"
        );
        return Some(text);
    }

    info!(hf_repo = %hf_repo, "No tokenizer_config.json found");
    None
}

#[derive(Debug, Deserialize)]
struct HfFileEntry {
    #[serde(rename = "type")]
    file_type: String,
    #[serde(rename = "rfilename", alias = "path")]
    path: String,
    size: Option<u64>,
}

async fn set_download_error(downloads: &Downloads, download_id: &str, error_msg: &str) {
    error!(download_id = %download_id, error = %error_msg, "Download failed");
    let mut dls = downloads.write().await;
    if let Some(dl) = dls.get_mut(download_id) {
        dl.status = DownloadStatus::Failed;
        dl.error = Some(error_msg.to_string());
    }
}

// ---------------------------------------------------------------------------
// Disk usage monitoring
// ---------------------------------------------------------------------------

pub struct DiskUsage {
    pub total_bytes: u64,
    pub used_bytes: u64,
    pub free_bytes: u64,
}

/// Get disk usage for the filesystem containing the given path.
/// Uses `df` command to avoid additional crate dependencies.
pub fn get_disk_usage(path: &str) -> Result<DiskUsage, String> {
    let output = std::process::Command::new("df")
        .args(["-B1", "--output=size,used,avail", path])
        .output()
        .map_err(|e| format!("Failed to run df: {e}"))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!("df command failed: {stderr}"));
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    // Output format:
    //      1B-blocks          Used         Avail
    //  1000204886016  537715044352  411439906816

    let lines: Vec<&str> = stdout.lines().collect();
    if lines.len() < 2 {
        return Err("Unexpected df output format".to_string());
    }

    let data_line = lines[1].trim();
    let parts: Vec<&str> = data_line.split_whitespace().collect();
    if parts.len() < 3 {
        return Err(format!("Unexpected df output columns: {data_line}"));
    }

    let total_bytes: u64 = parts[0]
        .parse()
        .map_err(|e| format!("Failed to parse total bytes: {e}"))?;
    let used_bytes: u64 = parts[1]
        .parse()
        .map_err(|e| format!("Failed to parse used bytes: {e}"))?;
    let free_bytes: u64 = parts[2]
        .parse()
        .map_err(|e| format!("Failed to parse free bytes: {e}"))?;

    Ok(DiskUsage {
        total_bytes,
        used_bytes,
        free_bytes,
    })
}

// ---------------------------------------------------------------------------
// Human-readable byte formatting
// ---------------------------------------------------------------------------

fn format_bytes(bytes: u64) -> String {
    const KB: u64 = 1024;
    const MB: u64 = 1024 * KB;
    const GB: u64 = 1024 * MB;
    const TB: u64 = 1024 * GB;

    if bytes >= TB {
        format!("{:.1} TB", bytes as f64 / TB as f64)
    } else if bytes >= GB {
        format!("{:.1} GB", bytes as f64 / GB as f64)
    } else if bytes >= MB {
        format!("{:.1} MB", bytes as f64 / MB as f64)
    } else if bytes >= KB {
        format!("{:.1} KB", bytes as f64 / KB as f64)
    } else {
        format!("{} B", bytes)
    }
}

// ---------------------------------------------------------------------------
// Simple URL encoding for query parameters
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// GGUF metadata reader — extracts architecture metadata from file header
// ---------------------------------------------------------------------------

/// Architecture metadata extracted from a GGUF file header.
#[derive(Debug, Clone, Default)]
pub struct GgufMetadata {
    pub context_length: Option<u32>,
    pub block_count: Option<u32>, // n_layers
    pub embedding_length: Option<u32>,
    pub head_count: Option<u32>,       // attention.head_count
    pub head_count_kv: Option<u32>,    // attention.head_count_kv (max, for fallback estimator)
    pub key_length: Option<u32>,       // attention.key_length (global layers)
    pub value_length: Option<u32>,     // attention.value_length (global layers)
    pub key_length_swa: Option<u32>,   // attention.key_length_swa (sliding-window layers)
    pub value_length_swa: Option<u32>, // attention.value_length_swa (sliding-window layers)
    pub sliding_window: Option<u32>,   // <arch>.attention.sliding_window
    pub expert_count: Option<u32>,     // <arch>.expert_count
    /// Full per-layer kv-head array (Gemma 3/4 style heterogeneous attention).
    /// When present, this lives alongside `head_count_kv` (which is the max).
    pub head_count_kv_per_layer: Option<Vec<u32>>,
    /// Per-layer SWA flags: `true` = sliding-window attention layer,
    /// `false` = full-context (global) layer. Gemma 3/4 uses the key
    /// `<arch>.attention.sliding_window_pattern` (GGUF type 9 array of bool).
    pub sliding_window_pattern: Option<Vec<bool>>,
}

/// Pre-compute per-token KV-cache bytes, split between global (full-context)
/// and SWA (sliding-window) layers. See migration
/// `20260423000001_swa_kv_aggregates.sql` for the formula and rationale.
///
/// Returns `(global_bytes_per_token, swa_bytes_per_token)`.
///
/// * When the GGUF has per-layer `head_count_kv` **and** a
///   `sliding_window_pattern` of the same length, the layers are partitioned
///   and each contributes `kv_heads_i × (key_len + val_len) × 2` bytes/token
///   (using the `_swa` dims for SWA layers when present).
/// * Otherwise, every layer is treated as global, and we fall back to
///   `n_layers × max(head_count_kv) × (key_len + val_len) × 2`. This matches
///   the pre-SWA estimator exactly for legacy models.
/// * If we can't determine `key_length` / `value_length` or `block_count`,
///   we return `(None, None)` and leave the DB columns NULL — the estimator
///   takes its legacy path.
pub fn compute_kv_aggregates(meta: &GgufMetadata) -> (Option<i64>, Option<i64>) {
    let Some(key_len) = meta.key_length else {
        return (None, None);
    };
    let Some(val_len) = meta.value_length else {
        return (None, None);
    };
    let key_len_swa = meta.key_length_swa.unwrap_or(key_len);
    let val_len_swa = meta.value_length_swa.unwrap_or(val_len);

    // Heterogeneous path: per-layer head counts + sliding-window pattern.
    if let (Some(heads), Some(pat)) = (
        meta.head_count_kv_per_layer.as_ref(),
        meta.sliding_window_pattern.as_ref(),
    ) {
        if heads.len() == pat.len() && !heads.is_empty() {
            let mut global_bytes: i64 = 0;
            let mut swa_bytes: i64 = 0;
            for (i, &h) in heads.iter().enumerate() {
                let is_swa = pat[i];
                if is_swa {
                    swa_bytes += (h as i64) * ((key_len_swa + val_len_swa) as i64) * 2;
                } else {
                    global_bytes += (h as i64) * ((key_len + val_len) as i64) * 2;
                }
            }
            return (Some(global_bytes), Some(swa_bytes));
        }
    }

    // Homogeneous / missing pattern: treat every layer as global, using the
    // max head_count_kv × n_layers — identical to the legacy estimator.
    let (Some(n_layers), Some(kv_heads)) = (meta.block_count, meta.head_count_kv) else {
        return (None, None);
    };
    let per_token = (n_layers as i64) * (kv_heads as i64) * ((key_len + val_len) as i64) * 2;
    (Some(per_token), None)
}

/// Decide the initial `runtime_overrides` JSON value based on GGUF metadata.
///
/// Auto-mitigation for llama.cpp #21762: SWA-bearing dense models can crash
/// in the prompt-cache save path. Disable the cache by default for those.
/// MoE models (expert_count > 0) and non-SWA models get the empty-object
/// default. Operator can override via PUT /api/admin/models/:id.
pub fn auto_runtime_overrides(meta: &GgufMetadata) -> &'static str {
    if meta.sliding_window.is_some() && meta.expert_count.unwrap_or(0) == 0 {
        r#"{"cache_ram_mib":0}"#
    } else {
        "{}"
    }
}

/// Read architecture metadata from a GGUF file's header.
///
/// Extracts: context_length, block_count, embedding_length,
/// attention.head_count, attention.head_count_kv.
///
/// GGUF format: magic (4B) + version (u32) + n_tensors (u64) + n_kv (u64)
/// then n_kv key-value pairs, each: string key + type tag (u32) + value.
pub async fn read_gguf_metadata(path: &str) -> Result<GgufMetadata, String> {
    use tokio::io::AsyncReadExt;

    let mut f = tokio::fs::File::open(path)
        .await
        .map_err(|e| format!("open: {e}"))?;

    // Read and validate magic
    let mut magic = [0u8; 4];
    f.read_exact(&mut magic)
        .await
        .map_err(|e| format!("read magic: {e}"))?;
    if &magic != b"GGUF" {
        return Err("not a GGUF file".to_string());
    }

    // Version (u32 LE)
    let mut buf4 = [0u8; 4];
    f.read_exact(&mut buf4)
        .await
        .map_err(|e| format!("read version: {e}"))?;
    let _version = u32::from_le_bytes(buf4);

    // n_tensors (u64 LE)
    let mut buf8 = [0u8; 8];
    f.read_exact(&mut buf8)
        .await
        .map_err(|e| format!("read n_tensors: {e}"))?;

    // n_kv (u64 LE)
    f.read_exact(&mut buf8)
        .await
        .map_err(|e| format!("read n_kv: {e}"))?;
    let n_kv = u64::from_le_bytes(buf8);
    if n_kv > 10_000 {
        return Err(format!("GGUF n_kv too large: {n_kv} (max 10000)"));
    }

    // Helper: read a GGUF string (u64 length + bytes)
    async fn read_string(f: &mut tokio::fs::File) -> Result<String, String> {
        let mut buf = [0u8; 8];
        f.read_exact(&mut buf)
            .await
            .map_err(|e| format!("read string len: {e}"))?;
        let len = u64::from_le_bytes(buf) as usize;
        if len > 1_000_000 {
            return Err(format!("string too long: {len}"));
        }
        let mut data = vec![0u8; len];
        f.read_exact(&mut data)
            .await
            .map_err(|e| format!("read string data: {e}"))?;
        String::from_utf8(data).map_err(|e| format!("invalid utf8: {e}"))
    }

    // Helper: read a single scalar integer value from common GGUF types.
    // Returns None for non-integer types (caller should skip_value instead).
    async fn read_int_value(f: &mut tokio::fs::File, vtype: u32) -> Result<Option<u32>, String> {
        match vtype {
            4 => {
                let mut b = [0u8; 4];
                f.read_exact(&mut b).await.map_err(|e| e.to_string())?;
                Ok(Some(u32::from_le_bytes(b)))
            }
            5 => {
                let mut b = [0u8; 4];
                f.read_exact(&mut b).await.map_err(|e| e.to_string())?;
                Ok(Some(i32::from_le_bytes(b) as u32))
            }
            10 => {
                let mut b = [0u8; 8];
                f.read_exact(&mut b).await.map_err(|e| e.to_string())?;
                Ok(Some(u64::from_le_bytes(b) as u32))
            }
            11 => {
                let mut b = [0u8; 8];
                f.read_exact(&mut b).await.map_err(|e| e.to_string())?;
                Ok(Some(i64::from_le_bytes(b) as u32))
            }
            _ => Ok(None),
        }
    }

    // Helper: read an integer value, handling both scalars and arrays (type 9).
    // For arrays of integers, returns (max_element, full_array). The max is
    // what the legacy single-column estimator uses; the full array is what
    // the SWA-aware estimator needs (per-layer kv-head counts).
    // For scalar integers, returns (Some(value), None).
    // Returns (None, None) for non-integer types.
    async fn read_int_scalar_or_array(
        f: &mut tokio::fs::File,
        vtype: u32,
    ) -> Result<(Option<u32>, Option<Vec<u32>>), String> {
        if vtype == 9 {
            // Array: element type (u32) + count (u64) + elements
            let mut tb = [0u8; 4];
            f.read_exact(&mut tb).await.map_err(|e| e.to_string())?;
            let atype = u32::from_le_bytes(tb);
            let mut cb = [0u8; 8];
            f.read_exact(&mut cb).await.map_err(|e| e.to_string())?;
            let count = u64::from_le_bytes(cb);

            let mut values: Vec<u32> = Vec::with_capacity(count.min(4096) as usize);
            let mut max_val: Option<u32> = None;
            let mut all_integer = true;
            for _ in 0..count {
                if let Some(v) = read_int_value(f, atype).await? {
                    max_val = Some(max_val.map_or(v, |cur| cur.max(v)));
                    values.push(v);
                } else {
                    // Array of non-integer type — skip remaining and bail out
                    Box::pin(skip_value(f, atype)).await?;
                    all_integer = false;
                }
            }
            if all_integer {
                Ok((max_val, Some(values)))
            } else {
                Ok((max_val, None))
            }
        } else {
            Ok((read_int_value(f, vtype).await?, None))
        }
    }

    // Helper: read a GGUF type-9 array of bools (element type 7, 1 byte each;
    // 0 = false, anything else = true). Returns None if the value is not a
    // bool array (caller should skip_value separately in that case).
    async fn read_bool_array(
        f: &mut tokio::fs::File,
        vtype: u32,
    ) -> Result<Option<Vec<bool>>, String> {
        if vtype != 9 {
            return Ok(None);
        }
        let mut tb = [0u8; 4];
        f.read_exact(&mut tb).await.map_err(|e| e.to_string())?;
        let atype = u32::from_le_bytes(tb);
        let mut cb = [0u8; 8];
        f.read_exact(&mut cb).await.map_err(|e| e.to_string())?;
        let count = u64::from_le_bytes(cb);

        if atype != 7 {
            // Not a bool array — skip the remaining elements and signal "not bools"
            for _ in 0..count {
                Box::pin(skip_value(f, atype)).await?;
            }
            return Ok(None);
        }

        let mut values: Vec<bool> = Vec::with_capacity(count.min(4096) as usize);
        for _ in 0..count {
            let mut b = [0u8; 1];
            f.read_exact(&mut b).await.map_err(|e| e.to_string())?;
            values.push(b[0] != 0);
        }
        Ok(Some(values))
    }

    // Helper: skip a GGUF value by type tag
    async fn skip_value(f: &mut tokio::fs::File, vtype: u32) -> Result<(), String> {
        match vtype {
            0 | 1 | 7 => {
                let mut b = [0u8; 1];
                f.read_exact(&mut b).await.map_err(|e| e.to_string())?;
            }
            2 | 3 => {
                let mut b = [0u8; 2];
                f.read_exact(&mut b).await.map_err(|e| e.to_string())?;
            }
            4..=6 => {
                let mut b = [0u8; 4];
                f.read_exact(&mut b).await.map_err(|e| e.to_string())?;
            }
            8 => {
                read_string(f).await?;
            }
            9 => {
                // array: type (u32) + count (u64) + elements
                let mut tb = [0u8; 4];
                f.read_exact(&mut tb).await.map_err(|e| e.to_string())?;
                let atype = u32::from_le_bytes(tb);
                let mut cb = [0u8; 8];
                f.read_exact(&mut cb).await.map_err(|e| e.to_string())?;
                let count = u64::from_le_bytes(cb);
                for _ in 0..count {
                    Box::pin(skip_value(f, atype)).await?;
                }
            }
            10..=12 => {
                let mut b = [0u8; 8];
                f.read_exact(&mut b).await.map_err(|e| e.to_string())?;
            }
            _ => return Err(format!("unknown GGUF type: {vtype}")),
        }
        Ok(())
    }

    let mut meta = GgufMetadata::default();

    // Keys we're looking for (all suffixed with arch prefix, e.g. "llama.context_length").
    // Order matters for ends_with matching: longer/more-specific suffixes must be
    // checked before their shorter prefixes (e.g. KEY_LENGTH_SWA before KEY_LENGTH,
    // SLIDING_WINDOW_PATTERN before SLIDING_WINDOW).
    const CONTEXT_LENGTH: &str = ".context_length";
    const BLOCK_COUNT: &str = ".block_count";
    const EMBEDDING_LENGTH: &str = ".embedding_length";
    const HEAD_COUNT: &str = ".attention.head_count";
    const HEAD_COUNT_KV: &str = ".attention.head_count_kv";
    const KEY_LENGTH: &str = ".attention.key_length";
    const VALUE_LENGTH: &str = ".attention.value_length";
    const KEY_LENGTH_SWA: &str = ".attention.key_length_swa";
    const VALUE_LENGTH_SWA: &str = ".attention.value_length_swa";
    const SLIDING_WINDOW: &str = ".attention.sliding_window";
    const SLIDING_WINDOW_PATTERN: &str = ".attention.sliding_window_pattern";
    const EXPERT_COUNT: &str = ".expert_count";

    for _ in 0..n_kv {
        let key = read_string(&mut f).await?;
        let mut tb = [0u8; 4];
        f.read_exact(&mut tb)
            .await
            .map_err(|e| format!("read type: {e}"))?;
        let vtype = u32::from_le_bytes(tb);

        // head_count_kv may be an array (per-layer) in heterogeneous-attention
        // models like Gemma 4 — capture BOTH the max (for legacy estimator)
        // AND the full array (for SWA-aware estimator).
        if key.ends_with(HEAD_COUNT_KV) {
            let (max_val, arr) = read_int_scalar_or_array(&mut f, vtype).await?;
            if let Some(v) = max_val {
                meta.head_count_kv = Some(v);
            } else if arr.is_none() {
                // read_int_scalar_or_array didn't consume a value for non-int
                // scalar types — skip explicitly.
                skip_value(&mut f, vtype).await?;
            }
            if let Some(a) = arr {
                meta.head_count_kv_per_layer = Some(a);
            }
            continue;
        }

        // sliding_window_pattern is a bool array (GGUF type 9, element type 7).
        if key.ends_with(SLIDING_WINDOW_PATTERN) {
            match read_bool_array(&mut f, vtype).await? {
                Some(pat) => meta.sliding_window_pattern = Some(pat),
                None => {
                    // Not a bool array (or not an array at all). read_bool_array
                    // consumes arrays; for non-array types we still need to skip.
                    if vtype != 9 {
                        skip_value(&mut f, vtype).await?;
                    }
                }
            }
            continue;
        }

        let target = if key.ends_with(CONTEXT_LENGTH) {
            Some(&mut meta.context_length)
        } else if key.ends_with(BLOCK_COUNT) {
            Some(&mut meta.block_count)
        } else if key.ends_with(EMBEDDING_LENGTH) {
            Some(&mut meta.embedding_length)
        } else if key.ends_with(HEAD_COUNT) {
            Some(&mut meta.head_count)
        } else if key.ends_with(KEY_LENGTH_SWA) {
            Some(&mut meta.key_length_swa)
        } else if key.ends_with(VALUE_LENGTH_SWA) {
            Some(&mut meta.value_length_swa)
        } else if key.ends_with(KEY_LENGTH) {
            Some(&mut meta.key_length)
        } else if key.ends_with(VALUE_LENGTH) {
            Some(&mut meta.value_length)
        } else if key.ends_with(SLIDING_WINDOW) {
            Some(&mut meta.sliding_window)
        } else if key.ends_with(EXPERT_COUNT) {
            Some(&mut meta.expert_count)
        } else {
            None
        };

        if let Some(field) = target {
            if let Some(val) = read_int_value(&mut f, vtype).await? {
                *field = Some(val);
            } else {
                skip_value(&mut f, vtype).await?;
            }
        } else {
            skip_value(&mut f, vtype).await?;
        }
    }

    Ok(meta)
}

fn urlencoded(s: &str) -> String {
    let mut result = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            'A'..='Z' | 'a'..='z' | '0'..='9' | '-' | '_' | '.' | '~' => result.push(c),
            ' ' => result.push('+'),
            _ => {
                for b in c.to_string().as_bytes() {
                    result.push_str(&format!("%{:02X}", b));
                }
            }
        }
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    // -- hf_http_error_hint --------------------------------------------------
    // These tests mutate HF_TOKEN env var, so they must be serialized to avoid
    // races when cargo runs tests in parallel.

    use std::sync::Mutex;
    static HF_TOKEN_LOCK: Mutex<()> = Mutex::new(());

    #[test]
    fn hf_http_error_hint_unauthorized_without_token() {
        let _guard = HF_TOKEN_LOCK.lock().unwrap();
        std::env::remove_var("HF_TOKEN");
        let hint = hf_http_error_hint(reqwest::StatusCode::UNAUTHORIZED);
        assert!(
            hint.contains("set HF_TOKEN"),
            "Expected suggestion to set HF_TOKEN, got: {hint}"
        );
    }

    #[test]
    fn hf_http_error_hint_forbidden_without_token() {
        let _guard = HF_TOKEN_LOCK.lock().unwrap();
        std::env::remove_var("HF_TOKEN");
        let hint = hf_http_error_hint(reqwest::StatusCode::FORBIDDEN);
        assert!(
            hint.contains("set HF_TOKEN"),
            "Expected suggestion to set HF_TOKEN, got: {hint}"
        );
    }

    #[test]
    fn hf_http_error_hint_unauthorized_with_token() {
        let _guard = HF_TOKEN_LOCK.lock().unwrap();
        std::env::set_var("HF_TOKEN", "hf_test_token");
        let hint = hf_http_error_hint(reqwest::StatusCode::UNAUTHORIZED);
        assert!(
            hint.contains("lack access"),
            "Expected 'lack access' hint, got: {hint}"
        );
        std::env::remove_var("HF_TOKEN");
    }

    #[test]
    fn hf_http_error_hint_forbidden_with_token() {
        let _guard = HF_TOKEN_LOCK.lock().unwrap();
        std::env::set_var("HF_TOKEN", "hf_test_token");
        let hint = hf_http_error_hint(reqwest::StatusCode::FORBIDDEN);
        assert!(
            hint.contains("lack access"),
            "Expected 'lack access' hint, got: {hint}"
        );
        std::env::remove_var("HF_TOKEN");
    }

    #[test]
    fn hf_http_error_hint_other_status_returns_empty() {
        let hint = hf_http_error_hint(reqwest::StatusCode::NOT_FOUND);
        assert_eq!(hint, "");

        let hint = hf_http_error_hint(reqwest::StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(hint, "");

        let hint = hf_http_error_hint(reqwest::StatusCode::BAD_REQUEST);
        assert_eq!(hint, "");
    }

    // -- format_bytes --------------------------------------------------------

    #[test]
    fn format_bytes_zero() {
        assert_eq!(format_bytes(0), "0 B");
    }

    #[test]
    fn format_bytes_small() {
        assert_eq!(format_bytes(1), "1 B");
        assert_eq!(format_bytes(512), "512 B");
        assert_eq!(format_bytes(1023), "1023 B");
    }

    #[test]
    fn format_bytes_kb() {
        assert_eq!(format_bytes(1024), "1.0 KB");
        assert_eq!(format_bytes(1536), "1.5 KB");
        assert_eq!(format_bytes(10 * 1024), "10.0 KB");
    }

    #[test]
    fn format_bytes_mb() {
        assert_eq!(format_bytes(1024 * 1024), "1.0 MB");
        assert_eq!(format_bytes(5 * 1024 * 1024 + 512 * 1024), "5.5 MB");
    }

    #[test]
    fn format_bytes_gb() {
        assert_eq!(format_bytes(1024 * 1024 * 1024), "1.0 GB");
        assert_eq!(
            format_bytes(7 * 1024 * 1024 * 1024 + 512 * 1024 * 1024),
            "7.5 GB"
        );
    }

    #[test]
    fn format_bytes_tb() {
        assert_eq!(format_bytes(1024u64 * 1024 * 1024 * 1024), "1.0 TB");
        assert_eq!(
            format_bytes(2 * 1024u64 * 1024 * 1024 * 1024 + 512 * 1024 * 1024 * 1024),
            "2.5 TB"
        );
    }

    // -- detect_primary_file -------------------------------------------------

    fn make_file(path: &str, size: u64) -> HfFileEntry {
        HfFileEntry {
            file_type: "file".to_string(),
            path: path.to_string(),
            size: Some(size),
        }
    }

    #[test]
    fn detect_primary_file_empty_list() {
        assert_eq!(detect_primary_file(&[]), None);
    }

    #[test]
    fn detect_primary_file_no_matching_extensions() {
        let files = vec![make_file("README.md", 1000), make_file("config.json", 500)];
        assert_eq!(detect_primary_file(&files), None);
    }

    #[test]
    fn detect_primary_file_prefers_gguf() {
        let files = vec![
            make_file("model.safetensors", 10_000_000),
            make_file("model-q4.gguf", 5_000_000),
        ];
        assert_eq!(
            detect_primary_file(&files),
            Some("model-q4.gguf".to_string())
        );
    }

    #[test]
    fn detect_primary_file_largest_gguf() {
        let files = vec![
            make_file("model-q2.gguf", 2_000_000),
            make_file("model-q8.gguf", 8_000_000),
            make_file("model-q4.gguf", 4_000_000),
        ];
        assert_eq!(
            detect_primary_file(&files),
            Some("model-q8.gguf".to_string())
        );
    }

    #[test]
    fn detect_primary_file_falls_back_to_safetensors() {
        let files = vec![
            make_file("config.json", 500),
            make_file("model.safetensors", 10_000_000),
            make_file("model-2.safetensors", 20_000_000),
        ];
        assert_eq!(
            detect_primary_file(&files),
            Some("model-2.safetensors".to_string())
        );
    }

    #[test]
    fn detect_primary_file_none_size_treated_as_zero() {
        let files = vec![HfFileEntry {
            file_type: "file".to_string(),
            path: "model.gguf".to_string(),
            size: None,
        }];
        assert_eq!(detect_primary_file(&files), Some("model.gguf".to_string()));
    }

    // -- urlencoded ----------------------------------------------------------

    #[test]
    fn urlencoded_alphanumeric_preserved() {
        assert_eq!(urlencoded("Hello123"), "Hello123");
    }

    #[test]
    fn urlencoded_unreserved_chars_preserved() {
        assert_eq!(urlencoded("a-b_c.d~e"), "a-b_c.d~e");
    }

    #[test]
    fn urlencoded_space_becomes_plus() {
        assert_eq!(urlencoded("hello world"), "hello+world");
    }

    #[test]
    fn urlencoded_special_chars_encoded() {
        assert_eq!(urlencoded("a&b=c"), "a%26b%3Dc");
    }

    #[test]
    fn urlencoded_slash_encoded() {
        assert_eq!(urlencoded("path/to/thing"), "path%2Fto%2Fthing");
    }

    #[test]
    fn urlencoded_empty_string() {
        assert_eq!(urlencoded(""), "");
    }

    #[test]
    fn urlencoded_unicode_chars() {
        // Multi-byte UTF-8 character should be percent-encoded per byte
        let encoded = urlencoded("café");
        assert!(encoded.starts_with("caf"));
        assert!(encoded.contains('%'));
        // 'é' is U+00E9, encoded as %C3%A9 in UTF-8
        assert_eq!(encoded, "caf%C3%A9");
    }

    // -- read_gguf_metadata (binary blob tests) ------------------------------

    /// Build a minimal valid GGUF file with the given key-value pairs.
    /// Each entry is (key, vtype, raw_value_bytes).
    fn build_gguf(entries: &[(&str, u32, Vec<u8>)]) -> Vec<u8> {
        let mut buf = Vec::new();
        // Magic
        buf.extend_from_slice(b"GGUF");
        // Version (3)
        buf.extend_from_slice(&3u32.to_le_bytes());
        // n_tensors (0)
        buf.extend_from_slice(&0u64.to_le_bytes());
        // n_kv
        buf.extend_from_slice(&(entries.len() as u64).to_le_bytes());
        for (key, vtype, value_bytes) in entries {
            // String key: u64 len + bytes
            buf.extend_from_slice(&(key.len() as u64).to_le_bytes());
            buf.extend_from_slice(key.as_bytes());
            // Type tag
            buf.extend_from_slice(&vtype.to_le_bytes());
            // Raw value
            buf.extend_from_slice(value_bytes);
        }
        buf
    }

    /// Encode a GGUF array value: element_type (u32) + count (u64) + elements
    fn gguf_array_i32(values: &[i32]) -> Vec<u8> {
        let mut buf = Vec::new();
        buf.extend_from_slice(&5u32.to_le_bytes()); // element type = i32
        buf.extend_from_slice(&(values.len() as u64).to_le_bytes());
        for v in values {
            buf.extend_from_slice(&v.to_le_bytes());
        }
        buf
    }

    /// Encode a GGUF bool array (element type 7, 1 byte each).
    fn gguf_array_bool(values: &[bool]) -> Vec<u8> {
        let mut buf = Vec::new();
        buf.extend_from_slice(&7u32.to_le_bytes()); // element type = bool
        buf.extend_from_slice(&(values.len() as u64).to_le_bytes());
        for &v in values {
            buf.push(if v { 1u8 } else { 0u8 });
        }
        buf
    }

    async fn parse_gguf_bytes(data: &[u8]) -> GgufMetadata {
        use std::sync::atomic::{AtomicU64, Ordering};
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let id = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("gguf_test_{}_{id}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("test.gguf");
        std::fs::write(&path, data).unwrap();
        let result = read_gguf_metadata(path.to_str().unwrap()).await;
        std::fs::remove_dir_all(&dir).ok();
        result.expect("Failed to parse GGUF")
    }

    #[tokio::test]
    async fn gguf_scalar_head_count_kv() {
        let data = build_gguf(&[
            ("llama.block_count", 4, 32u32.to_le_bytes().to_vec()), // u32
            ("llama.embedding_length", 4, 4096u32.to_le_bytes().to_vec()),
            (
                "llama.attention.head_count",
                4,
                32u32.to_le_bytes().to_vec(),
            ),
            (
                "llama.attention.head_count_kv",
                5,
                8i32.to_le_bytes().to_vec(),
            ), // i32
            ("llama.context_length", 4, 2048u32.to_le_bytes().to_vec()),
        ]);
        let meta = parse_gguf_bytes(&data).await;
        assert_eq!(meta.head_count_kv, Some(8));
        assert_eq!(meta.head_count, Some(32));
        assert_eq!(meta.block_count, Some(32));
        assert_eq!(meta.embedding_length, Some(4096));
        assert_eq!(meta.context_length, Some(2048));
        assert_eq!(meta.key_length, None);
        assert_eq!(meta.value_length, None);
    }

    #[tokio::test]
    async fn gguf_array_head_count_kv_returns_max() {
        // Simulate Gemma 4: per-layer kv head counts as an i32 array
        // Mix of values — should return the max (8)
        let arr = gguf_array_i32(&[4, 8, 4, 8, 4, 8]);
        let data = build_gguf(&[
            ("gemma4.attention.head_count_kv", 9, arr),
            (
                "gemma4.attention.head_count",
                4,
                32u32.to_le_bytes().to_vec(),
            ),
        ]);
        let meta = parse_gguf_bytes(&data).await;
        assert_eq!(meta.head_count_kv, Some(8));
        assert_eq!(meta.head_count, Some(32));
    }

    #[tokio::test]
    async fn gguf_array_head_count_kv_uniform() {
        // All layers have same kv head count
        let arr = gguf_array_i32(&[8, 8, 8, 8]);
        let data = build_gguf(&[("llama.attention.head_count_kv", 9, arr)]);
        let meta = parse_gguf_bytes(&data).await;
        assert_eq!(meta.head_count_kv, Some(8));
    }

    #[tokio::test]
    async fn gguf_explicit_key_value_lengths() {
        let data = build_gguf(&[
            (
                "gemma4.attention.key_length",
                4,
                512u32.to_le_bytes().to_vec(),
            ),
            (
                "gemma4.attention.value_length",
                4,
                512u32.to_le_bytes().to_vec(),
            ),
            ("gemma4.embedding_length", 4, 3584u32.to_le_bytes().to_vec()),
        ]);
        let meta = parse_gguf_bytes(&data).await;
        assert_eq!(meta.key_length, Some(512));
        assert_eq!(meta.value_length, Some(512));
        assert_eq!(meta.embedding_length, Some(3584));
    }

    #[tokio::test]
    async fn gguf_no_key_value_lengths_stays_none() {
        // Standard model without explicit key/value lengths
        let data = build_gguf(&[
            ("llama.embedding_length", 4, 4096u32.to_le_bytes().to_vec()),
            (
                "llama.attention.head_count",
                4,
                32u32.to_le_bytes().to_vec(),
            ),
            (
                "llama.attention.head_count_kv",
                5,
                8i32.to_le_bytes().to_vec(),
            ),
        ]);
        let meta = parse_gguf_bytes(&data).await;
        assert_eq!(meta.key_length, None);
        assert_eq!(meta.value_length, None);
    }

    // -- sliding_window / expert_count parsing -------------------------------

    #[tokio::test]
    async fn gguf_sliding_window_parsed() {
        // Gemma3-style SWA model
        let data = build_gguf(&[(
            "gemma3.attention.sliding_window",
            4,
            1024u32.to_le_bytes().to_vec(),
        )]);
        let meta = parse_gguf_bytes(&data).await;
        assert_eq!(meta.sliding_window, Some(1024));
        assert_eq!(meta.expert_count, None);
    }

    #[tokio::test]
    async fn gguf_expert_count_parsed() {
        // Qwen-MoE-style model
        let data = build_gguf(&[("qwen.expert_count", 4, 128u32.to_le_bytes().to_vec())]);
        let meta = parse_gguf_bytes(&data).await;
        assert_eq!(meta.expert_count, Some(128));
        assert_eq!(meta.sliding_window, None);
    }

    #[tokio::test]
    async fn gguf_no_sliding_window_or_expert_count_stays_none() {
        // Standard dense, non-SWA model
        let data = build_gguf(&[
            ("llama.embedding_length", 4, 4096u32.to_le_bytes().to_vec()),
            (
                "llama.attention.head_count",
                4,
                32u32.to_le_bytes().to_vec(),
            ),
        ]);
        let meta = parse_gguf_bytes(&data).await;
        assert_eq!(meta.sliding_window, None);
        assert_eq!(meta.expert_count, None);
    }

    // -- SWA-aware metadata parsing ------------------------------------------

    #[tokio::test]
    async fn gguf_sliding_window_pattern_parsed() {
        // Gemma 4-style: first 5 layers SWA (True), last layer global (False).
        let pat = gguf_array_bool(&[true, true, true, true, true, false]);
        let data = build_gguf(&[("gemma4.attention.sliding_window_pattern", 9, pat)]);
        let meta = parse_gguf_bytes(&data).await;
        assert_eq!(
            meta.sliding_window_pattern,
            Some(vec![true, true, true, true, true, false])
        );
    }

    #[tokio::test]
    async fn gguf_head_count_kv_per_layer_parsed() {
        // Per-layer kv-head array should populate BOTH the max scalar field
        // (for the legacy estimator) and the full per-layer vector (for SWA).
        let arr = gguf_array_i32(&[16, 16, 16, 16, 16, 4]);
        let data = build_gguf(&[("gemma4.attention.head_count_kv", 9, arr)]);
        let meta = parse_gguf_bytes(&data).await;
        assert_eq!(meta.head_count_kv, Some(16));
        assert_eq!(
            meta.head_count_kv_per_layer,
            Some(vec![16, 16, 16, 16, 16, 4])
        );
    }

    #[tokio::test]
    async fn gguf_key_length_swa_parsed() {
        // Gemma 4 has distinct global and SWA kv-dim sizes.
        let data = build_gguf(&[
            (
                "gemma4.attention.key_length",
                4,
                512u32.to_le_bytes().to_vec(),
            ),
            (
                "gemma4.attention.value_length",
                4,
                512u32.to_le_bytes().to_vec(),
            ),
            (
                "gemma4.attention.key_length_swa",
                4,
                256u32.to_le_bytes().to_vec(),
            ),
            (
                "gemma4.attention.value_length_swa",
                4,
                256u32.to_le_bytes().to_vec(),
            ),
        ]);
        let meta = parse_gguf_bytes(&data).await;
        assert_eq!(meta.key_length, Some(512));
        assert_eq!(meta.value_length, Some(512));
        assert_eq!(meta.key_length_swa, Some(256));
        assert_eq!(meta.value_length_swa, Some(256));
    }

    // -- compute_kv_aggregates -----------------------------------------------

    #[test]
    fn compute_kv_aggregates_gemma4_style() {
        // 6 layers: [16, 16, 16, 16, 16, 4] kv-heads.
        // Pattern:   [T,  T,  T,  T,  T,  F] (True = SWA).
        // key_len = val_len = 512 (global); _swa = 256.
        //   global term (layer 5): 4 × (512 + 512) × 2 = 8_192
        //   swa    term (5 layers × 16): 5 × 16 × (256 + 256) × 2 = 81_920
        let meta = GgufMetadata {
            block_count: Some(6),
            head_count_kv: Some(16),
            head_count_kv_per_layer: Some(vec![16, 16, 16, 16, 16, 4]),
            sliding_window_pattern: Some(vec![true, true, true, true, true, false]),
            key_length: Some(512),
            value_length: Some(512),
            key_length_swa: Some(256),
            value_length_swa: Some(256),
            ..Default::default()
        };
        let (global_bpt, swa_bpt) = compute_kv_aggregates(&meta);
        assert_eq!(global_bpt, Some(8_192));
        assert_eq!(swa_bpt, Some(81_920));
    }

    #[test]
    fn compute_kv_aggregates_homogeneous() {
        // No per-layer arrays and no SWA pattern — every layer is treated
        // as global using max(head_count_kv) × n_layers.
        //   32 layers × 8 kv_heads × (128 + 128) × 2 = 131_072 bytes/token
        let meta = GgufMetadata {
            block_count: Some(32),
            head_count_kv: Some(8),
            key_length: Some(128),
            value_length: Some(128),
            ..Default::default()
        };
        let (global_bpt, swa_bpt) = compute_kv_aggregates(&meta);
        assert_eq!(global_bpt, Some(131_072));
        assert_eq!(swa_bpt, None);
    }

    #[test]
    fn compute_kv_aggregates_missing_metadata() {
        // No key/value length → no aggregates at all.
        let meta = GgufMetadata {
            block_count: Some(32),
            head_count_kv: Some(8),
            ..Default::default()
        };
        assert_eq!(compute_kv_aggregates(&meta), (None, None));

        // Has key_length but no block_count / no per-layer info → no aggregates.
        let meta = GgufMetadata {
            key_length: Some(128),
            value_length: Some(128),
            ..Default::default()
        };
        assert_eq!(compute_kv_aggregates(&meta), (None, None));
    }

    #[test]
    fn compute_kv_aggregates_swa_dims_fallback_to_global() {
        // Per-layer + pattern present, but no _swa dims → SWA layers reuse
        // the global key/value lengths.
        //   Pattern = [T, F], heads = [4, 4], key = val = 100.
        //   global_bpt = 4 × (100+100) × 2 = 1_600
        //   swa_bpt    = 4 × (100+100) × 2 = 1_600  (fallback)
        let meta = GgufMetadata {
            block_count: Some(2),
            head_count_kv: Some(4),
            head_count_kv_per_layer: Some(vec![4, 4]),
            sliding_window_pattern: Some(vec![true, false]),
            key_length: Some(100),
            value_length: Some(100),
            ..Default::default()
        };
        let (global_bpt, swa_bpt) = compute_kv_aggregates(&meta);
        assert_eq!(global_bpt, Some(1_600));
        assert_eq!(swa_bpt, Some(1_600));
    }

    #[test]
    fn compute_kv_aggregates_pattern_length_mismatch_falls_back() {
        // If the per-layer array and the pattern disagree on length, we
        // conservatively treat the whole model as homogeneous/global rather
        // than guessing the alignment.
        let meta = GgufMetadata {
            block_count: Some(6),
            head_count_kv: Some(16),
            head_count_kv_per_layer: Some(vec![16, 16, 16, 16, 16, 4]),
            sliding_window_pattern: Some(vec![true, false]), // wrong length
            key_length: Some(128),
            value_length: Some(128),
            ..Default::default()
        };
        let (global_bpt, swa_bpt) = compute_kv_aggregates(&meta);
        // 6 × 16 × (128+128) × 2 = 49_152
        assert_eq!(global_bpt, Some(49_152));
        assert_eq!(swa_bpt, None);
    }

    // -- auto_runtime_overrides ----------------------------------------------

    #[test]
    fn auto_overrides_swa_dense_disables_cache() {
        // SWA-bearing, dense (no MoE) → must disable the prompt cache.
        let meta = GgufMetadata {
            sliding_window: Some(1024),
            expert_count: None,
            ..Default::default()
        };
        assert_eq!(auto_runtime_overrides(&meta), r#"{"cache_ram_mib":0}"#);
    }

    #[test]
    fn auto_overrides_swa_dense_explicit_zero_experts() {
        // expert_count = Some(0) is also "dense" — same treatment.
        let meta = GgufMetadata {
            sliding_window: Some(2048),
            expert_count: Some(0),
            ..Default::default()
        };
        assert_eq!(auto_runtime_overrides(&meta), r#"{"cache_ram_mib":0}"#);
    }

    #[test]
    fn auto_overrides_swa_moe_keeps_default() {
        // SWA + MoE — bug doesn't apply, leave overrides empty.
        let meta = GgufMetadata {
            sliding_window: Some(1024),
            expert_count: Some(128),
            ..Default::default()
        };
        assert_eq!(auto_runtime_overrides(&meta), "{}");
    }

    #[test]
    fn auto_overrides_no_swa_dense_keeps_default() {
        // Standard dense model without SWA — leave overrides empty.
        let meta = GgufMetadata {
            sliding_window: None,
            expert_count: None,
            ..Default::default()
        };
        assert_eq!(auto_runtime_overrides(&meta), "{}");
    }

    #[test]
    fn auto_overrides_no_swa_moe_keeps_default() {
        // MoE without SWA — leave overrides empty.
        let meta = GgufMetadata {
            sliding_window: None,
            expert_count: Some(8),
            ..Default::default()
        };
        assert_eq!(auto_runtime_overrides(&meta), "{}");
    }

    // -- is_mmproj_filename --------------------------------------------------

    #[test]
    fn is_mmproj_filename_accepts_dash_prefix() {
        assert!(is_mmproj_filename("mmproj-foo-f16.gguf"));
    }

    #[test]
    fn is_mmproj_filename_accepts_underscore_prefix() {
        assert!(is_mmproj_filename("mmproj_foo_f16.gguf"));
    }

    #[test]
    fn is_mmproj_filename_rejects_non_gguf() {
        assert!(!is_mmproj_filename("mmproj-foo.bin"));
    }

    #[test]
    fn is_mmproj_filename_rejects_non_prefix() {
        assert!(!is_mmproj_filename("foo-mmproj.gguf"));
    }

    #[test]
    fn is_mmproj_filename_rejects_non_root_path() {
        assert!(!is_mmproj_filename("subdir/mmproj-foo.gguf"));
    }

    // -- detect_mmproj_file --------------------------------------------------

    #[test]
    fn detect_mmproj_file_none_when_absent() {
        let files = vec![make_file("model.gguf", 1_000)];
        assert_eq!(detect_mmproj_file(&files), None);
    }

    #[test]
    fn detect_mmproj_file_single_match() {
        let files = vec![
            make_file("model.gguf", 1_000),
            make_file("mmproj-foo-f16.gguf", 500),
        ];
        assert_eq!(
            detect_mmproj_file(&files),
            Some("mmproj-foo-f16.gguf".to_string())
        );
    }

    #[test]
    fn detect_mmproj_file_prefers_f16_over_bf16_and_f32() {
        let files = vec![
            make_file("model.gguf", 1_000),
            make_file("mmproj-foo-bf16.gguf", 400),
            make_file("mmproj-foo-f32.gguf", 600),
            make_file("mmproj-foo-f16.gguf", 500),
        ];
        assert_eq!(
            detect_mmproj_file(&files),
            Some("mmproj-foo-f16.gguf".to_string())
        );
    }

    #[test]
    fn detect_mmproj_file_prefers_bf16_over_f32() {
        let files = vec![
            make_file("model.gguf", 1_000),
            make_file("mmproj-foo-f32.gguf", 600),
            make_file("mmproj-foo-bf16.gguf", 400),
        ];
        assert_eq!(
            detect_mmproj_file(&files),
            Some("mmproj-foo-bf16.gguf".to_string())
        );
    }

    #[test]
    fn detect_mmproj_file_f16_not_confused_by_bf16() {
        // Only bf16 present — must pick the bf16 entry, NOT misclassify it as f16.
        let files = vec![make_file("mmproj-foo-bf16.gguf", 400)];
        assert_eq!(
            detect_mmproj_file(&files),
            Some("mmproj-foo-bf16.gguf".to_string())
        );
    }

    #[test]
    fn detect_mmproj_file_ignores_subdir_matches() {
        let files = vec![
            make_file("model.gguf", 1_000),
            make_file("subdir/mmproj-x.gguf", 500),
        ];
        // mmproj under a subdir must not be picked.
        assert_eq!(detect_mmproj_file(&files), None);
    }

    // -- should_include (filter helper) --------------------------------------

    #[test]
    fn should_include_explicit_filter_accepts_listed_file() {
        let filter = vec!["main.gguf".to_string()];
        assert!(should_include("main.gguf", Some(&filter)));
    }

    #[test]
    fn should_include_explicit_filter_always_includes_mmproj() {
        let filter = vec!["main.gguf".to_string()];
        assert!(should_include("mmproj-foo-f16.gguf", Some(&filter)));
    }

    #[test]
    fn should_include_explicit_filter_rejects_unlisted_non_mmproj() {
        let filter = vec!["main.gguf".to_string()];
        assert!(!should_include("extra.gguf", Some(&filter)));
    }

    #[test]
    fn should_include_no_filter_accepts_any_file() {
        assert!(should_include("main.gguf", None));
    }

    #[test]
    fn should_include_no_filter_accepts_mmproj() {
        assert!(should_include("mmproj-foo-f16.gguf", None));
    }

    // -- insert_downloaded_model (INSERT binding test) -----------------------

    #[tokio::test]
    async fn insert_downloaded_model_persists_mmproj_filename() {
        // Drive the INSERT helper with a known mmproj filename and assert
        // the row is persisted with the expected value. Proves the download
        // flow populates mmproj_filename at INSERT time (no restart needed).
        let db = crate::db::Database::test_db().await;

        let row = DownloadedModelRow {
            model_id: "model-with-mmproj".to_string(),
            hf_repo: "owner/repo".to_string(),
            primary_filename: Some("model-q4.gguf".to_string()),
            mmproj_filename: Some("mmproj-foo-f16.gguf".to_string()),
            size_bytes: 1_234,
            category_id: None,
            backend_type: "llamacpp".to_string(),
            model_metadata: None,
            gguf_meta: GgufMetadata::default(),
            kv_bpt_global: None,
            kv_bpt_swa: None,
            runtime_overrides_json: "{}".to_string(),
        };

        insert_downloaded_model(&db.pool, &row)
            .await
            .expect("insert succeeds");

        let (fname, mmproj): (Option<String>, Option<String>) =
            sqlx::query_as("SELECT filename, mmproj_filename FROM models WHERE id = ?")
                .bind(&row.model_id)
                .fetch_one(&db.pool)
                .await
                .expect("row exists");
        assert_eq!(fname.as_deref(), Some("model-q4.gguf"));
        assert_eq!(mmproj.as_deref(), Some("mmproj-foo-f16.gguf"));
    }

    #[tokio::test]
    async fn insert_downloaded_model_leaves_mmproj_null_for_text_only() {
        // No mmproj → column persists as NULL (text-only model path).
        let db = crate::db::Database::test_db().await;

        let row = DownloadedModelRow {
            model_id: "text-only-model".to_string(),
            hf_repo: "owner/text-repo".to_string(),
            primary_filename: Some("model-q4.gguf".to_string()),
            mmproj_filename: None,
            size_bytes: 1_000,
            category_id: None,
            backend_type: "llamacpp".to_string(),
            model_metadata: None,
            gguf_meta: GgufMetadata::default(),
            kv_bpt_global: None,
            kv_bpt_swa: None,
            runtime_overrides_json: "{}".to_string(),
        };

        insert_downloaded_model(&db.pool, &row)
            .await
            .expect("insert succeeds");

        let (mmproj,): (Option<String>,) =
            sqlx::query_as("SELECT mmproj_filename FROM models WHERE id = ?")
                .bind(&row.model_id)
                .fetch_one(&db.pool)
                .await
                .expect("row exists");
        assert_eq!(mmproj, None);
    }
}
