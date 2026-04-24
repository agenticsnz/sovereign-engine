use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize, Serializer};
use sqlx::FromRow;

#[derive(Debug, Clone, Serialize, Deserialize, FromRow)]
pub struct IdpConfig {
    pub id: String,
    pub name: String,
    pub issuer: String,
    pub client_id: String,
    pub client_secret_enc: String,
    pub scopes: String,
    pub enabled: bool,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize, FromRow)]
pub struct ModelCategory {
    pub id: String,
    pub name: String,
    pub description: String,
    pub preferred_model_id: Option<String>,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize, FromRow)]
pub struct Model {
    pub id: String,
    pub hf_repo: String,
    pub filename: Option<String>,
    #[sqlx(default)]
    pub size_bytes: i64,
    pub category_id: Option<String>,
    pub loaded: bool,
    pub backend_port: Option<i32>,
    pub backend_type: String,
    pub last_used_at: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
    pub context_length: Option<i64>,
    pub n_layers: Option<i64>,
    pub n_heads: Option<i64>,
    pub n_kv_heads: Option<i64>,
    pub embedding_length: Option<i64>,
    pub key_length: Option<i64>,
    pub value_length: Option<i64>,
    pub sliding_window: Option<i64>,
    pub kv_bytes_per_token_global: Option<i64>,
    pub kv_bytes_per_token_swa: Option<i64>,
    /// Companion mmproj (multimodal projector) GGUF filename — sibling of the
    /// primary `filename` in the same `hf_repo` directory. `None` for
    /// text-only models.
    pub mmproj_filename: Option<String>,
    /// Per-model llama-server CLI overrides. Stored as a JSON TEXT column
    /// (defaults to `{}` per migration). Serialized to the API as a nested
    /// object, not a string — see [`serialize_runtime_overrides`].
    #[serde(serialize_with = "serialize_runtime_overrides")]
    pub runtime_overrides: String,
}

/// Serialize the `runtime_overrides` JSON column as a nested object so the
/// wire shape matches the typed `RuntimeOverrides | null` the UI expects.
/// Falls back to `{}` if the stored text fails to parse — keeps the API
/// contract stable even if a row somehow holds invalid JSON.
fn serialize_runtime_overrides<S: Serializer>(s: &str, ser: S) -> Result<S::Ok, S::Error> {
    let value: serde_json::Value =
        serde_json::from_str(s).unwrap_or_else(|_| serde_json::json!({}));
    value.serialize(ser)
}

#[derive(Debug, Clone, Serialize, Deserialize, FromRow)]
pub struct User {
    pub id: String,
    pub idp_id: String,
    pub subject: String,
    pub email: Option<String>,
    pub display_name: Option<String>,
    pub is_admin: bool,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize, FromRow)]
pub struct IdpConfigPublic {
    pub id: String,
    pub name: String,
    pub issuer: String,
    pub client_id: String,
    pub scopes: String,
    pub enabled: bool,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize, FromRow)]
pub struct TokenListItem {
    pub id: String,
    pub name: String,
    pub category_id: Option<String>,
    pub category_name: Option<String>,
    pub specific_model_id: Option<String>,
    pub expires_at: Option<DateTime<Utc>>,
    pub revoked: bool,
    pub created_at: DateTime<Utc>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_model(runtime_overrides: &str) -> Model {
        Model {
            id: "m".into(),
            hf_repo: "r".into(),
            filename: None,
            size_bytes: 0,
            category_id: None,
            loaded: false,
            backend_port: None,
            backend_type: "llamacpp".into(),
            last_used_at: None,
            created_at: Utc::now(),
            context_length: None,
            n_layers: None,
            n_heads: None,
            n_kv_heads: None,
            embedding_length: None,
            key_length: None,
            value_length: None,
            sliding_window: None,
            kv_bytes_per_token_global: None,
            kv_bytes_per_token_swa: None,
            mmproj_filename: None,
            runtime_overrides: runtime_overrides.into(),
        }
    }

    #[test]
    fn runtime_overrides_serializes_as_nested_object_not_string() {
        let m = sample_model(r#"{"cache_ram_mib":0}"#);
        let json = serde_json::to_value(&m).expect("serialize");
        assert_eq!(
            json["runtime_overrides"],
            serde_json::json!({"cache_ram_mib": 0})
        );
        assert!(!json["runtime_overrides"].is_string());
    }

    #[test]
    fn runtime_overrides_default_serializes_as_empty_object() {
        let m = sample_model("{}");
        let json = serde_json::to_value(&m).expect("serialize");
        assert_eq!(json["runtime_overrides"], serde_json::json!({}));
    }

    #[test]
    fn runtime_overrides_invalid_json_falls_back_to_empty_object() {
        let m = sample_model("not json");
        let json = serde_json::to_value(&m).expect("serialize");
        assert_eq!(json["runtime_overrides"], serde_json::json!({}));
    }
}
