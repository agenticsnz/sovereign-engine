//! Per-model runtime overrides for llama-server CLI args.
//!
//! Persisted as a JSON blob in `models.runtime_overrides` and folded into the
//! container start path via [`ModelRuntimeOverrides::to_cli_args`].
//!
//! Mitigates upstream issues like the Gemma3 dense + SWA cache-save crash
//! (`GGML_ASSERT(tensor->data != NULL)`, llama.cpp issue #21762) where we
//! need a per-model knob (`--cache-ram 0`) that defaults off everywhere else.

use serde::{Deserialize, Serialize};

/// CLI flags the proxy reserves for itself; users cannot pass them via `extra`.
const FORBIDDEN_EXTRA_PREFIXES: &[&str] = &[
    "--model",
    "--host",
    "--port",
    "--api-key",
    "-ngl",
    "-c",
    "-np",
];

const MAX_CACHE_RAM_MIB: u32 = 16384;
const MAX_CTX_CHECKPOINTS: u32 = 128;
const MAX_CACHE_REUSE: u32 = 8192;

/// User-controllable subset of llama-server CLI flags.
///
/// All fields are optional. An empty struct means "use llama.cpp defaults".
/// Unknown JSON keys are rejected so a typo doesn't silently no-op.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct ModelRuntimeOverrides {
    /// `--cache-ram <n>` — KV cache RAM budget in MiB. `Some(0)` disables the
    /// prompt-cache save path entirely (the Gemma3 SWA workaround).
    pub cache_ram_mib: Option<u32>,
    /// `--swa-full` — emitted only when `Some(true)`. `Some(false)` and `None`
    /// both leave the flag off (llama.cpp default).
    pub swa_full: Option<bool>,
    /// `-ctxcp <n>` — number of context checkpoints retained.
    pub ctx_checkpoints: Option<u32>,
    /// `--cache-reuse <n>` — minimum prefix reuse threshold.
    pub cache_reuse: Option<u32>,
    /// Free-form trailing args, appended last. Validated against
    /// [`FORBIDDEN_EXTRA_PREFIXES`] so users cannot override proxy-owned flags.
    pub extra: Vec<String>,
}

impl ModelRuntimeOverrides {
    /// Build the CLI fragment in the documented order:
    /// `--cache-ram`, `--swa-full`, `-ctxcp`, `--cache-reuse`, then `extra`.
    pub fn to_cli_args(&self) -> Vec<String> {
        let mut out = Vec::new();

        if let Some(n) = self.cache_ram_mib {
            out.push("--cache-ram".to_string());
            out.push(n.to_string());
        }

        if matches!(self.swa_full, Some(true)) {
            out.push("--swa-full".to_string());
        }

        if let Some(n) = self.ctx_checkpoints {
            out.push("-ctxcp".to_string());
            out.push(n.to_string());
        }

        if let Some(n) = self.cache_reuse {
            out.push("--cache-reuse".to_string());
            out.push(n.to_string());
        }

        out.extend(self.extra.iter().cloned());
        out
    }

    /// Clamp range checks and forbid `extra` entries that collide with
    /// proxy-owned flags. Returns `Err(reason)` on the first failure.
    pub fn validate(&self) -> Result<(), String> {
        if let Some(n) = self.cache_ram_mib {
            if n > MAX_CACHE_RAM_MIB {
                return Err(format!(
                    "cache_ram_mib {n} exceeds maximum {MAX_CACHE_RAM_MIB}"
                ));
            }
        }
        if let Some(n) = self.ctx_checkpoints {
            if n > MAX_CTX_CHECKPOINTS {
                return Err(format!(
                    "ctx_checkpoints {n} exceeds maximum {MAX_CTX_CHECKPOINTS}"
                ));
            }
        }
        if let Some(n) = self.cache_reuse {
            if n > MAX_CACHE_REUSE {
                return Err(format!("cache_reuse {n} exceeds maximum {MAX_CACHE_REUSE}"));
            }
        }

        for entry in &self.extra {
            for forbidden in FORBIDDEN_EXTRA_PREFIXES {
                // Case-sensitive match: exact flag or `--flag=value` form.
                if entry == *forbidden || entry.starts_with(&format!("{forbidden}=")) {
                    return Err(format!("extra argument '{entry}' is reserved by the proxy"));
                }
            }
        }

        Ok(())
    }
}

/// Persistence-only sibling of [`ModelRuntimeOverrides`] holding the
/// container-launch knobs the supervisor needs to rebuild a `LlamacppConfig`
/// after a crash.
///
/// These fields are inert at CLI-build time — they intentionally do NOT flow
/// through `to_cli_args()`. They reproduce values already passed via
/// `StartContainerParams` so the supervisor can reconstruct the start call
/// without re-prompting the operator. Stored in the same
/// `models.runtime_overrides` JSON column under the `launch` top-level key.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct PersistedLaunchConfig {
    pub gpu_type: Option<String>,
    pub gpu_layers: Option<u32>,
    pub parallel: Option<u32>,
}

/// Wrapper for the on-disk shape of `models.runtime_overrides`.
///
/// New shape: `{"cli": {...}, "launch": {...}}`. Legacy shape (pre-supervisor)
/// is the bare `ModelRuntimeOverrides` JSON; [`WrappedRuntimeJson::parse`]
/// transparently accepts both — legacy blobs are surfaced as
/// `{cli: <legacy>, launch: <default>}`. The Phase 3 supervisor logs and
/// skips models whose launch sub-struct is still default.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct WrappedRuntimeJson {
    pub cli: ModelRuntimeOverrides,
    pub launch: PersistedLaunchConfig,
}

impl WrappedRuntimeJson {
    /// Parse `models.runtime_overrides` JSON. Accepts both the new wrapped
    /// shape and the legacy bare [`ModelRuntimeOverrides`] shape (treated as
    /// `cli` only, `launch` defaulted).
    ///
    /// Note: a legacy blob like `{"cache_ram_mib": 0}` has `cache_ram_mib` at
    /// the top level, which fails wrapped parse (`deny_unknown_fields`). We
    /// then fall through to bare parse. An empty `{}` parses successfully as
    /// wrapped (both sub-structs default).
    pub fn parse(blob: &str) -> Result<Self, serde_json::Error> {
        if let Ok(wrapped) = serde_json::from_str::<Self>(blob) {
            return Ok(wrapped);
        }
        let cli: ModelRuntimeOverrides = serde_json::from_str(blob)?;
        Ok(Self {
            cli,
            launch: PersistedLaunchConfig::default(),
        })
    }

    /// Serialise into the wrapped shape for write-back.
    pub fn to_json(&self) -> Result<String, serde_json::Error> {
        serde_json::to_string(self)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_deserializes_to_all_none() {
        let parsed: ModelRuntimeOverrides = serde_json::from_str("{}").expect("parse");
        assert_eq!(parsed, ModelRuntimeOverrides::default());
        assert!(parsed.cache_ram_mib.is_none());
        assert!(parsed.swa_full.is_none());
        assert!(parsed.ctx_checkpoints.is_none());
        assert!(parsed.cache_reuse.is_none());
        assert!(parsed.extra.is_empty());
    }

    #[test]
    fn deny_unknown_fields_rejects_bad_json() {
        let err =
            serde_json::from_str::<ModelRuntimeOverrides>(r#"{"cache_ram_mb": 0}"#).unwrap_err();
        assert!(
            err.to_string().contains("cache_ram_mb"),
            "expected unknown-field error, got: {err}"
        );
    }

    #[test]
    fn cache_ram_emits_flag_and_value() {
        let o = ModelRuntimeOverrides {
            cache_ram_mib: Some(0),
            ..Default::default()
        };
        assert_eq!(o.to_cli_args(), vec!["--cache-ram", "0"]);
    }

    #[test]
    fn swa_full_true_emits_flag() {
        let o = ModelRuntimeOverrides {
            swa_full: Some(true),
            ..Default::default()
        };
        assert_eq!(o.to_cli_args(), vec!["--swa-full"]);
    }

    #[test]
    fn swa_full_false_is_omitted() {
        let o = ModelRuntimeOverrides {
            swa_full: Some(false),
            ..Default::default()
        };
        assert!(o.to_cli_args().is_empty());
    }

    #[test]
    fn ctx_checkpoints_emits_short_flag() {
        let o = ModelRuntimeOverrides {
            ctx_checkpoints: Some(32),
            ..Default::default()
        };
        assert_eq!(o.to_cli_args(), vec!["-ctxcp", "32"]);
    }

    #[test]
    fn cache_reuse_emits_flag_and_value() {
        let o = ModelRuntimeOverrides {
            cache_reuse: Some(256),
            ..Default::default()
        };
        assert_eq!(o.to_cli_args(), vec!["--cache-reuse", "256"]);
    }

    #[test]
    fn extra_appended_last_in_documented_order() {
        let o = ModelRuntimeOverrides {
            cache_ram_mib: Some(0),
            swa_full: Some(true),
            ctx_checkpoints: Some(32),
            cache_reuse: Some(256),
            extra: vec!["--threads".into(), "8".into()],
        };
        assert_eq!(
            o.to_cli_args(),
            vec![
                "--cache-ram",
                "0",
                "--swa-full",
                "-ctxcp",
                "32",
                "--cache-reuse",
                "256",
                "--threads",
                "8",
            ]
        );
    }

    #[test]
    fn validate_accepts_defaults() {
        assert!(ModelRuntimeOverrides::default().validate().is_ok());
    }

    #[test]
    fn validate_accepts_max_boundary() {
        let o = ModelRuntimeOverrides {
            cache_ram_mib: Some(MAX_CACHE_RAM_MIB),
            ctx_checkpoints: Some(MAX_CTX_CHECKPOINTS),
            cache_reuse: Some(MAX_CACHE_REUSE),
            ..Default::default()
        };
        assert!(o.validate().is_ok());
    }

    #[test]
    fn validate_rejects_cache_ram_over_max() {
        let o = ModelRuntimeOverrides {
            cache_ram_mib: Some(MAX_CACHE_RAM_MIB + 1),
            ..Default::default()
        };
        let err = o.validate().unwrap_err();
        assert!(err.contains("cache_ram_mib"));
    }

    #[test]
    fn validate_rejects_ctx_checkpoints_over_max() {
        let o = ModelRuntimeOverrides {
            ctx_checkpoints: Some(MAX_CTX_CHECKPOINTS + 1),
            ..Default::default()
        };
        let err = o.validate().unwrap_err();
        assert!(err.contains("ctx_checkpoints"));
    }

    #[test]
    fn validate_rejects_cache_reuse_over_max() {
        let o = ModelRuntimeOverrides {
            cache_reuse: Some(MAX_CACHE_REUSE + 1),
            ..Default::default()
        };
        let err = o.validate().unwrap_err();
        assert!(err.contains("cache_reuse"));
    }

    #[test]
    fn validate_rejects_forbidden_extra_exact() {
        for forbidden in FORBIDDEN_EXTRA_PREFIXES {
            let o = ModelRuntimeOverrides {
                extra: vec![(*forbidden).to_string()],
                ..Default::default()
            };
            let err = o.validate().unwrap_err();
            assert!(
                err.contains(forbidden),
                "expected error to mention {forbidden}, got: {err}"
            );
        }
    }

    #[test]
    fn validate_rejects_forbidden_extra_with_equals_value() {
        let o = ModelRuntimeOverrides {
            extra: vec!["--port=9999".to_string()],
            ..Default::default()
        };
        assert!(o.validate().is_err());
    }

    #[test]
    fn validate_allows_unrelated_extra() {
        let o = ModelRuntimeOverrides {
            extra: vec!["--threads".into(), "8".into(), "--mlock".into()],
            ..Default::default()
        };
        assert!(o.validate().is_ok());
    }

    #[test]
    fn validate_is_case_sensitive() {
        // `--MODEL` shouldn't be treated as `--model`.
        let o = ModelRuntimeOverrides {
            extra: vec!["--MODEL".into()],
            ..Default::default()
        };
        assert!(o.validate().is_ok());
    }

    #[test]
    fn roundtrip_full_struct_is_identical() {
        let original = ModelRuntimeOverrides {
            cache_ram_mib: Some(0),
            swa_full: Some(true),
            ctx_checkpoints: Some(32),
            cache_reuse: Some(256),
            extra: vec!["--threads".into(), "8".into()],
        };
        let json = serde_json::to_string(&original).expect("serialize");
        let back: ModelRuntimeOverrides = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(original, back);
    }

    #[test]
    fn roundtrip_default_struct_is_identical() {
        let original = ModelRuntimeOverrides::default();
        let json = serde_json::to_string(&original).expect("serialize");
        let back: ModelRuntimeOverrides = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(original, back);
    }
}

#[cfg(test)]
mod wrapped_tests {
    use super::*;

    #[test]
    fn roundtrip_wrapped_with_both_substructs_populated() {
        let original = WrappedRuntimeJson {
            cli: ModelRuntimeOverrides {
                cache_ram_mib: Some(0),
                swa_full: Some(true),
                ctx_checkpoints: Some(32),
                cache_reuse: Some(256),
                extra: vec!["--threads".into(), "8".into()],
            },
            launch: PersistedLaunchConfig {
                gpu_type: Some("cuda".into()),
                gpu_layers: Some(99),
                parallel: Some(4),
            },
        };
        let json = original.to_json().expect("serialize");
        let back = WrappedRuntimeJson::parse(&json).expect("parse");
        assert_eq!(original, back);
    }

    #[test]
    fn roundtrip_empty_wrapped_object_parses_as_default() {
        let parsed = WrappedRuntimeJson::parse("{}").expect("parse");
        assert_eq!(parsed, WrappedRuntimeJson::default());
        assert_eq!(parsed.cli, ModelRuntimeOverrides::default());
        assert_eq!(parsed.launch, PersistedLaunchConfig::default());
    }

    #[test]
    fn backward_compat_legacy_empty_bare_shape_parses_as_default() {
        // Empty `{}` parses successfully via the wrapped path (both
        // sub-structs default). Result is the same as
        // `WrappedRuntimeJson::default()`.
        let parsed = WrappedRuntimeJson::parse("{}").expect("parse");
        assert_eq!(parsed.cli, ModelRuntimeOverrides::default());
        assert_eq!(parsed.launch, PersistedLaunchConfig::default());
    }

    #[test]
    fn backward_compat_legacy_bare_shape_with_cache_ram_mib() {
        // Legacy on-disk blob: bare ModelRuntimeOverrides shape.
        // Wrapped parse fails (unknown field "cache_ram_mib" at top level),
        // we fall through to bare parse.
        let parsed =
            WrappedRuntimeJson::parse(r#"{"cache_ram_mib": 0}"#).expect("parse");
        assert_eq!(parsed.cli.cache_ram_mib, Some(0));
        assert_eq!(parsed.launch, PersistedLaunchConfig::default());
    }

    #[test]
    fn rejects_unknown_field_in_launch_substruct() {
        let blob = r#"{"cli": {}, "launch": {"gpu_typo": "cuda"}}"#;
        let err = WrappedRuntimeJson::parse(blob).unwrap_err();
        // The wrapped parse fails on the unknown launch field; the bare-parse
        // fallback also fails (top-level `cli`/`launch` aren't legacy fields).
        // We surface the bare-parse error, which mentions one of the unknown
        // top-level keys.
        let msg = err.to_string();
        assert!(
            msg.contains("cli") || msg.contains("launch"),
            "expected unknown-field error, got: {msg}"
        );
    }

    #[test]
    fn rejects_unknown_top_level_key() {
        // `garbage_field` is neither `cli`/`launch` nor a legacy override field,
        // so it fails wrapped parse and fails bare parse.
        let blob = r#"{"garbage_field": 42}"#;
        let err = WrappedRuntimeJson::parse(blob).unwrap_err();
        assert!(
            err.to_string().contains("garbage_field"),
            "expected unknown-field error mentioning garbage_field, got: {err}"
        );
    }
}
