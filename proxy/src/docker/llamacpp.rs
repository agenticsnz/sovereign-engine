use std::collections::HashMap;

use anyhow::{Context, Result};
use bollard::models::{
    ContainerCreateBody, DeviceMapping, EndpointSettings, HostConfig, Mount, MountTypeEnum,
    NetworkingConfig,
};
use bollard::query_parameters::{
    CreateContainerOptions, RemoveContainerOptions, StartContainerOptions, StopContainerOptions,
};
use tracing::{error, info, warn};

use super::{DockerManager, LABEL_BACKEND, LABEL_MANAGED_BY, LABEL_MANAGED_VALUE, LABEL_MODEL_ID};

pub(crate) const LLAMACPP_IMAGE_CPU: &str = "ghcr.io/ggml-org/llama.cpp:server";
pub(crate) const LLAMACPP_IMAGE_VULKAN: &str = "ghcr.io/ggml-org/llama.cpp:server-vulkan";

// NOTE: CUDA and ROCm backend support has been removed.
// - ROCm requires seccomp=unconfined (security concern) and showed poor
//   performance in testing (0.6–2.4 t/s vs 4.5 t/s from Vulkan).
// - CUDA support is untested and currently unsupported.
// Vulkan provides GPU acceleration on both AMD and NVIDIA hardware without
// these issues. See ADR 001 for details.
const LLAMACPP_INTERNAL_PORT: u16 = 8080;

/// GPU type for llama.cpp containers.
#[derive(Debug, Clone, Default)]
pub enum GpuType {
    #[default]
    None,
    Vulkan,
}

impl GpuType {
    pub fn from_str(s: &str) -> Self {
        match s.to_lowercase().as_str() {
            "vulkan" => GpuType::Vulkan,
            _ => GpuType::None,
        }
    }
}

/// Configuration for launching a llama.cpp container.
#[derive(Debug, Clone)]
pub struct LlamacppConfig {
    pub model_id: String,
    /// Path to the GGUF file relative to the model directory (e.g. "models--TheBloke--Llama-2-7B-GGUF/llama-2-7b.Q4_K_M.gguf")
    pub gguf_path: String,
    /// Optional multimodal projector path, relative to the model directory
    /// (e.g. "repo--x/mmproj-foo.gguf"). When `Some` and the file exists on
    /// disk under `model_path`, `--mmproj /models/<path>` is emitted BEFORE
    /// any user-supplied `extra_args` so the proxy's choice wins.
    pub mmproj_path: Option<String>,
    pub gpu_type: GpuType,
    /// Number of layers to offload to GPU (default 99 = all)
    pub gpu_layers: u32,
    /// Context size (default 4096)
    pub context_size: u32,
    /// Number of parallel sequences / slots (default 1)
    pub parallel: u32,
    pub extra_args: Vec<String>,
    /// Container UID — allocated by DockerManager::allocate_uid()
    pub uid: u32,
    /// API key for backend authentication — passed as --api-key to llama-server
    pub api_key: String,
}

impl Default for LlamacppConfig {
    fn default() -> Self {
        Self {
            model_id: String::new(),
            gguf_path: String::new(),
            mmproj_path: None,
            gpu_type: GpuType::None,
            gpu_layers: 99,
            context_size: 4096,
            parallel: 1,
            extra_args: Vec::new(),
            uid: 10000,
            api_key: String::new(),
        }
    }
}

/// Pure helper: build the llama-server CLI argv for a given config.
///
/// Extracted so it can be unit-tested without a live Docker daemon.
/// `host_model_path` is the host-side directory that will be bind-mounted
/// at `/models` in the container — used for file-existence checks on
/// optional assets (e.g. mmproj).
pub(crate) fn build_llamacpp_cmd(
    config: &LlamacppConfig,
    host_model_path: &str,
    internal_port: u16,
) -> Vec<String> {
    let mut cmd = vec![
        "--model".to_string(),
        format!("/models/{}", config.gguf_path),
        "--host".to_string(),
        "0.0.0.0".to_string(),
        "--port".to_string(),
        internal_port.to_string(),
        "-ngl".to_string(),
        config.gpu_layers.to_string(),
        "-c".to_string(),
        (config.context_size as u64 * config.parallel as u64).to_string(),
        // --jinja activates llama-server's full Jinja2 chat-template path,
        // which parses model-native tool-call markup (qwen Hermes-style,
        // gemma `<|tool_call>...`, mistral `[TOOL_CALLS]`, ...) back into
        // structured OpenAI `tool_calls` JSON. Without this the markup
        // leaks into `choices[0].message.content` and any OpenAI-SDK
        // consumer sees the model's tool call as plain text.
        "--jinja".to_string(),
    ];

    // Parallel sequences (concurrency slots)
    if config.parallel > 1 {
        cmd.push("-np".to_string());
        cmd.push(config.parallel.to_string());
    }

    // Add API key for backend authentication
    cmd.push("--api-key".to_string());
    cmd.push(config.api_key.clone());

    // Multimodal projector: only emit when the file actually exists on
    // disk under the host model path. This runs BEFORE extra_args so a
    // user-supplied runtime_override cannot override the proxy's choice.
    // If the column is set but the file is missing, warn and continue —
    // text still works, images will 500, operator fixes forward.
    if let Some(rel) = &config.mmproj_path {
        let host_full = std::path::Path::new(host_model_path).join(rel);
        if host_full.exists() {
            cmd.push("--mmproj".to_string());
            cmd.push(format!("/models/{rel}"));
        } else {
            warn!(
                model = %config.model_id,
                expected = %host_full.display(),
                "mmproj_path set but file not found on disk; starting without --mmproj (text-only)"
            );
        }
    }

    cmd.extend(config.extra_args.clone());
    cmd
}

impl DockerManager {
    /// Start a llama.cpp container for the given model.
    pub async fn start_llamacpp(&self, config: &LlamacppConfig) -> Result<String> {
        let container_name = format!("sovereign-llamacpp-{}", config.model_id);

        // Check if container already exists
        if let Ok(info) = self.docker.inspect_container(&container_name, None).await {
            if let Some(state) = &info.state {
                if state.running.unwrap_or(false) {
                    info!(model = %config.model_id, "llama.cpp container already running");
                    return Ok(container_name);
                }
            }
            // Container exists but not running — remove and recreate
            warn!(model = %config.model_id, "Removing stopped llama.cpp container");
            self.docker
                .remove_container(
                    &container_name,
                    Some(RemoveContainerOptions {
                        force: true,
                        ..Default::default()
                    }),
                )
                .await
                .context("Failed to remove existing container")?;
        }

        // Select image based on GPU type
        let image = match config.gpu_type {
            GpuType::Vulkan => LLAMACPP_IMAGE_VULKAN,
            GpuType::None => LLAMACPP_IMAGE_CPU,
        };

        // Build llama-server command arguments via the pure helper so the
        // argv construction is unit-testable without a live Docker daemon.
        let cmd = build_llamacpp_cmd(config, &self.model_path, LLAMACPP_INTERNAL_PORT);

        let uid = config.uid;
        let user_str = format!("{}:{}", uid, uid);

        // Labels
        let mut labels = HashMap::new();
        labels.insert(
            LABEL_MANAGED_BY.to_string(),
            LABEL_MANAGED_VALUE.to_string(),
        );
        labels.insert(LABEL_MODEL_ID.to_string(), config.model_id.clone());
        labels.insert(LABEL_BACKEND.to_string(), "llamacpp".to_string());

        let mut host_config = HostConfig {
            // No port bindings — llama.cpp is only reachable via the internal network
            mounts: Some(vec![Mount {
                target: Some("/models".to_string()),
                source: Some(self.model_path.clone()),
                typ: Some(MountTypeEnum::BIND),
                read_only: Some(true),
                ..Default::default()
            }]),
            ..Default::default()
        };

        // GPU configuration
        match config.gpu_type {
            GpuType::Vulkan => {
                // Vulkan: expose /dev/dri (and /dev/kfd if present for AMD).
                let mut devices = vec![DeviceMapping {
                    path_on_host: Some("/dev/dri".to_string()),
                    path_in_container: Some("/dev/dri".to_string()),
                    cgroup_permissions: Some("rw".to_string()),
                }];
                if std::path::Path::new("/dev/kfd").exists() {
                    devices.push(DeviceMapping {
                        path_on_host: Some("/dev/kfd".to_string()),
                        path_in_container: Some("/dev/kfd".to_string()),
                        cgroup_permissions: Some("rw".to_string()),
                    });
                }
                host_config.devices = Some(devices);
                // Discover the GIDs that own the GPU device files and forward
                // them to the backend container so its non-root user can access
                // /dev/dri and /dev/kfd.
                let groups = gpu_device_gids();
                if !groups.is_empty() {
                    host_config.group_add = Some(groups);
                }
            }
            GpuType::None => {
                // CPU-only: no GPU config needed
            }
        }

        // Attach to the internal network so the proxy can reach this container by name
        let mut endpoints_config = HashMap::new();
        endpoints_config.insert(self.backend_network.clone(), EndpointSettings::default());

        let networking_config = NetworkingConfig {
            endpoints_config: Some(endpoints_config),
        };

        let container_config = ContainerCreateBody {
            image: Some(image.to_string()),
            cmd: Some(cmd),
            labels: Some(labels),
            user: Some(user_str.clone()),
            host_config: Some(host_config),
            networking_config: Some(networking_config),
            ..Default::default()
        };

        info!(
            model = %config.model_id,
            container = %container_name,
            image = %image,
            uid = uid,
            gpu = ?config.gpu_type,
            "Creating llama.cpp container"
        );

        self.docker
            .create_container(
                Some(CreateContainerOptions {
                    name: Some(container_name.clone()),
                    ..Default::default()
                }),
                container_config,
            )
            .await
            .context("Failed to create llama.cpp container")?;

        if let Err(e) = self
            .docker
            .start_container(&container_name, None::<StartContainerOptions>)
            .await
        {
            error!(
                model = %config.model_id,
                container = %container_name,
                error = %e,
                "Failed to start llama.cpp container — cleaning up"
            );
            // Clean up the created-but-not-started container
            let _ = self
                .docker
                .remove_container(
                    &container_name,
                    Some(RemoveContainerOptions {
                        force: true,
                        ..Default::default()
                    }),
                )
                .await;
            return Err(e).context("Failed to start llama.cpp container");
        }

        info!(
            model = %config.model_id,
            container = %container_name,
            network = %self.backend_network,
            uid = uid,
            gpu = ?config.gpu_type,
            "llama.cpp container started on internal network"
        );

        Ok(container_name)
    }

    /// Stop a llama.cpp container by model ID.
    pub async fn stop_llamacpp(&self, model_id: &str) -> Result<()> {
        let container_name = format!("sovereign-llamacpp-{}", model_id);

        // Check container state first — only attempt stop if actually running
        let is_running = match self.docker.inspect_container(&container_name, None).await {
            Ok(info) => {
                let status = info.state.as_ref().and_then(|s| s.status);
                info!(model = %model_id, container = %container_name, state = ?status, "Inspected container for stop");
                info.state.as_ref().and_then(|s| s.running).unwrap_or(false)
            }
            Err(e) => {
                warn!(model = %model_id, container = %container_name, error = %e, "Container not found during stop");
                return Ok(()); // Container doesn't exist — nothing to stop
            }
        };

        if is_running {
            self.docker
                .stop_container(
                    &container_name,
                    Some(StopContainerOptions {
                        t: Some(30),
                        ..Default::default()
                    }),
                )
                .await
                .context("Failed to stop llama.cpp container")?;
        }

        self.docker
            .remove_container(
                &container_name,
                Some(RemoveContainerOptions {
                    force: true,
                    ..Default::default()
                }),
            )
            .await
            .context("Failed to remove llama.cpp container")?;

        info!(model = %model_id, container = %container_name, "llama.cpp container stopped and removed");
        Ok(())
    }

    /// Check if a llama.cpp container is healthy and responding.
    pub async fn check_llamacpp_health(&self, model_id: &str) -> Result<bool> {
        let container_name = format!("sovereign-llamacpp-{}", model_id);
        let url = format!(
            "http://{}:{}/health",
            container_name, LLAMACPP_INTERNAL_PORT
        );
        match reqwest::get(&url).await {
            Ok(resp) => Ok(resp.status().is_success()),
            Err(_) => Ok(false),
        }
    }

    /// Get the internal URL for a llama.cpp container on the isolated network.
    pub fn llamacpp_base_url(&self, model_id: &str) -> String {
        let container_name = format!("sovereign-llamacpp-{}", model_id);
        format!("http://{}:{}", container_name, LLAMACPP_INTERNAL_PORT)
    }
}

/// Discover the GIDs that own GPU device files (/dev/dri/*, /dev/kfd).
///
/// These GIDs are forwarded to backend containers via group_add so the
/// non-root container user can access the GPU devices. By stat-ing the
/// actual device files we pick up the correct host GIDs regardless of
/// group naming or GID changes across reboots.
fn gpu_device_gids() -> Vec<String> {
    use std::collections::BTreeSet;
    use std::os::unix::fs::MetadataExt;

    let mut gids = BTreeSet::new();

    // Collect owning GIDs from all device files in /dev/dri/
    if let Ok(entries) = std::fs::read_dir("/dev/dri") {
        for entry in entries.flatten() {
            if let Ok(meta) = entry.metadata() {
                let gid = meta.gid();
                if gid != 0 {
                    gids.insert(gid);
                }
            }
        }
    }

    // /dev/kfd (AMD KFD) may have a different owning group
    if let Ok(meta) = std::fs::metadata("/dev/kfd") {
        let gid = meta.gid();
        if gid != 0 {
            gids.insert(gid);
        }
    }

    gids.into_iter().map(|g| g.to_string()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    /// Helper: construct a minimal viable config with the given mmproj_path.
    fn config_with_mmproj(mmproj_path: Option<String>) -> LlamacppConfig {
        LlamacppConfig {
            model_id: "test-model".to_string(),
            gguf_path: "repo--x/model.gguf".to_string(),
            api_key: "test-key".to_string(),
            mmproj_path,
            ..Default::default()
        }
    }

    // -- build_llamacpp_cmd: mmproj handling ---------------------------------

    #[test]
    fn build_cmd_omits_mmproj_when_none() {
        let cfg = config_with_mmproj(None);
        let cmd = build_llamacpp_cmd(&cfg, "/tmp/does-not-matter", 8080);
        assert!(
            !cmd.iter().any(|a| a == "--mmproj"),
            "cmd should not contain --mmproj when mmproj_path is None: {cmd:?}"
        );
    }

    #[test]
    fn build_cmd_emits_mmproj_when_file_exists() {
        let tmp = TempDir::new().expect("tempdir");
        let repo_dir = tmp.path().join("repo--x");
        std::fs::create_dir_all(&repo_dir).expect("mkdir repo");
        std::fs::write(repo_dir.join("mmproj-foo.gguf"), b"stub").expect("write");

        let cfg = config_with_mmproj(Some("repo--x/mmproj-foo.gguf".to_string()));
        let cmd = build_llamacpp_cmd(&cfg, tmp.path().to_str().unwrap(), 8080);

        let idx = cmd
            .iter()
            .position(|a| a == "--mmproj")
            .expect("cmd should contain --mmproj");
        assert_eq!(
            cmd.get(idx + 1).map(String::as_str),
            Some("/models/repo--x/mmproj-foo.gguf"),
            "arg after --mmproj should be container path: {cmd:?}"
        );
    }

    #[test]
    fn build_cmd_omits_mmproj_when_file_missing() {
        // Same config shape as _emits_ test, but the file is not on disk.
        let tmp = TempDir::new().expect("tempdir");
        std::fs::create_dir_all(tmp.path().join("repo--x")).expect("mkdir");
        // NOTE: mmproj-foo.gguf deliberately NOT created.

        let cfg = config_with_mmproj(Some("repo--x/mmproj-foo.gguf".to_string()));
        let cmd = build_llamacpp_cmd(&cfg, tmp.path().to_str().unwrap(), 8080);

        assert!(
            !cmd.iter().any(|a| a == "--mmproj"),
            "cmd should not contain --mmproj when file is missing: {cmd:?}"
        );
    }

    #[test]
    fn build_cmd_mmproj_precedes_extra_args() {
        // The proxy owns --mmproj; a user-supplied extra_args entry with the
        // same flag must not win. The proxy-emitted flag appears FIRST.
        let tmp = TempDir::new().expect("tempdir");
        let repo_dir = tmp.path().join("repo--x");
        std::fs::create_dir_all(&repo_dir).expect("mkdir");
        std::fs::write(repo_dir.join("mmproj-foo.gguf"), b"stub").expect("write");

        let mut cfg = config_with_mmproj(Some("repo--x/mmproj-foo.gguf".to_string()));
        cfg.extra_args = vec!["--mmproj".to_string(), "/evil.gguf".to_string()];

        let cmd = build_llamacpp_cmd(&cfg, tmp.path().to_str().unwrap(), 8080);

        let indices: Vec<usize> = cmd
            .iter()
            .enumerate()
            .filter_map(|(i, a)| if a == "--mmproj" { Some(i) } else { None })
            .collect();
        assert_eq!(
            indices.len(),
            2,
            "expected two --mmproj entries (proxy + user): {cmd:?}"
        );

        let first = indices[0];
        assert_eq!(
            cmd.get(first + 1).map(String::as_str),
            Some("/models/repo--x/mmproj-foo.gguf"),
            "proxy's --mmproj must come first: {cmd:?}"
        );
    }

    #[test]
    fn build_cmd_mmproj_uses_container_path_prefix() {
        let tmp = TempDir::new().expect("tempdir");
        let repo_dir = tmp.path().join("repo--x");
        std::fs::create_dir_all(&repo_dir).expect("mkdir");
        std::fs::write(repo_dir.join("mmproj-foo.gguf"), b"stub").expect("write");

        let cfg = config_with_mmproj(Some("repo--x/mmproj-foo.gguf".to_string()));
        let cmd = build_llamacpp_cmd(&cfg, tmp.path().to_str().unwrap(), 8080);

        let idx = cmd.iter().position(|a| a == "--mmproj").expect("--mmproj");
        let path_arg = cmd.get(idx + 1).expect("arg after --mmproj");
        assert!(
            path_arg.starts_with("/models/"),
            "container path must start with /models/, got: {path_arg}"
        );
        let host_prefix = tmp.path().to_str().unwrap();
        assert!(
            !path_arg.contains(host_prefix),
            "container path must not leak host prefix {host_prefix}: {path_arg}"
        );
    }

    #[test]
    fn build_cmd_mmproj_correct_position_relative_to_context_size() {
        let tmp = TempDir::new().expect("tempdir");
        let repo_dir = tmp.path().join("repo--x");
        std::fs::create_dir_all(&repo_dir).expect("mkdir");
        std::fs::write(repo_dir.join("mmproj-foo.gguf"), b"stub").expect("write");

        let cfg = config_with_mmproj(Some("repo--x/mmproj-foo.gguf".to_string()));
        let cmd = build_llamacpp_cmd(&cfg, tmp.path().to_str().unwrap(), 8080);

        let c_idx = cmd.iter().position(|a| a == "-c").expect("-c present");
        let mmproj_idx = cmd.iter().position(|a| a == "--mmproj").expect("--mmproj");
        assert!(
            mmproj_idx > c_idx,
            "--mmproj (idx {mmproj_idx}) should come AFTER -c (idx {c_idx}): {cmd:?}"
        );
    }

    // -- build_llamacpp_cmd: --jinja for OpenAI tool_calls translation ------

    #[test]
    fn build_cmd_includes_jinja_by_default() {
        // Without --jinja, llama-server uses a minimal built-in chat-template
        // parser that doesn't recognise modern model-native tool-call markup
        // (qwen Hermes-style, gemma `<|tool_call>...`, mistral `[TOOL_CALLS]`).
        // The markup leaks into `content` instead of being parsed back into
        // structured `tool_calls` JSON. --jinja activates the full Jinja2
        // template path which handles all of these.
        let cfg = LlamacppConfig {
            model_id: "m".to_string(),
            gguf_path: "r--r/m.gguf".to_string(),
            api_key: "k".to_string(),
            ..Default::default()
        };
        let cmd = build_llamacpp_cmd(&cfg, "/tmp/unused", 8080);
        assert!(
            cmd.iter().any(|a| a == "--jinja"),
            "--jinja flag must be present so model-native tool-call markup \
             is translated back into OpenAI tool_calls JSON; got: {cmd:?}"
        );
    }

    // -- build_llamacpp_cmd: regression guard on baseline arg order ----------

    #[test]
    fn build_cmd_existing_args_still_present() {
        // Regression guard: the default baseline args must remain in the
        // exact pre-refactor order (--model, --host, --port, -ngl, -c).
        let cfg = LlamacppConfig {
            model_id: "m".to_string(),
            gguf_path: "r--r/m.gguf".to_string(),
            api_key: "k".to_string(),
            ..Default::default()
        };
        let cmd = build_llamacpp_cmd(&cfg, "/tmp/unused", 8080);

        let expected_prefix = vec![
            "--model".to_string(),
            "/models/r--r/m.gguf".to_string(),
            "--host".to_string(),
            "0.0.0.0".to_string(),
            "--port".to_string(),
            "8080".to_string(),
            "-ngl".to_string(),
            "99".to_string(),
            "-c".to_string(),
            "4096".to_string(),
        ];
        assert_eq!(
            &cmd[..expected_prefix.len()],
            expected_prefix.as_slice(),
            "baseline arg order changed: {cmd:?}"
        );
        // API key block must still be present.
        let api_idx = cmd
            .iter()
            .position(|a| a == "--api-key")
            .expect("--api-key present");
        assert_eq!(cmd.get(api_idx + 1).map(String::as_str), Some("k"));
    }

    // -- GpuType::from_str ---------------------------------------------------

    #[test]
    fn gpu_type_vulkan_lowercase() {
        assert!(matches!(GpuType::from_str("vulkan"), GpuType::Vulkan));
    }

    #[test]
    fn gpu_type_vulkan_uppercase() {
        assert!(matches!(GpuType::from_str("VULKAN"), GpuType::Vulkan));
    }

    #[test]
    fn gpu_type_vulkan_mixed_case() {
        assert!(matches!(GpuType::from_str("Vulkan"), GpuType::Vulkan));
    }

    #[test]
    fn gpu_type_none_explicit() {
        assert!(matches!(GpuType::from_str("none"), GpuType::None));
    }

    #[test]
    fn gpu_type_unknown_string() {
        assert!(matches!(GpuType::from_str("cuda"), GpuType::None));
    }

    #[test]
    fn gpu_type_empty_string() {
        assert!(matches!(GpuType::from_str(""), GpuType::None));
    }

    #[test]
    fn gpu_type_garbage() {
        assert!(matches!(GpuType::from_str("foobar"), GpuType::None));
    }

    // -- GpuType default -----------------------------------------------------

    #[test]
    fn gpu_type_default_is_none() {
        assert!(matches!(GpuType::default(), GpuType::None));
    }

    // -- LlamacppConfig defaults ---------------------------------------------

    #[test]
    fn llamacpp_config_defaults() {
        let cfg = LlamacppConfig::default();
        assert_eq!(cfg.gpu_layers, 99);
        assert_eq!(cfg.context_size, 4096);
        assert_eq!(cfg.parallel, 1);
        assert_eq!(cfg.uid, 10000);
        assert!(cfg.model_id.is_empty());
        assert!(cfg.gguf_path.is_empty());
        assert!(cfg.api_key.is_empty());
        assert!(cfg.extra_args.is_empty());
        assert!(cfg.mmproj_path.is_none());
        assert!(matches!(cfg.gpu_type, GpuType::None));
    }

    // -- DockerManager::llamacpp_base_url ------------------------------------
    // This is a pure function; we can test its output format without Docker.

    #[test]
    fn llamacpp_base_url_format() {
        let dm = DockerManager::test_dummy();
        let url = dm.llamacpp_base_url("my-model-123");
        assert_eq!(url, "http://sovereign-llamacpp-my-model-123:8080");
    }

    // -- Image selection constants -------------------------------------------

    #[test]
    fn image_constants_are_distinct() {
        assert_ne!(LLAMACPP_IMAGE_CPU, LLAMACPP_IMAGE_VULKAN);
        assert!(LLAMACPP_IMAGE_CPU.contains("llama.cpp"));
        assert!(LLAMACPP_IMAGE_VULKAN.contains("vulkan"));
    }
}
