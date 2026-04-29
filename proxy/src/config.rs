use anyhow::{Context, Result};
use subtle::ConstantTimeEq;

#[derive(Debug, Clone)]
pub struct AppConfig {
    /// Bind address (e.g. "0.0.0.0:443")
    pub listen_addr: String,

    /// SQLite database URL
    pub database_url: String,

    /// Path to TLS certificate PEM file
    pub tls_cert_path: Option<String>,

    /// Path to TLS private key PEM file
    pub tls_key_path: Option<String>,

    /// Bootstrap admin username (first-run only)
    pub bootstrap_user: Option<String>,

    /// Bootstrap admin password (first-run only)
    pub bootstrap_password: Option<String>,

    /// Re-enable bootstrap creds alongside OIDC
    pub break_glass: bool,

    /// Docker socket path
    pub docker_host: String,

    /// Path to models directory (local to this process, e.g. /models when Dockerized)
    pub model_path: String,

    /// Host-side models path for child container bind mounts.
    /// Defaults to MODEL_PATH (correct when proxy runs directly on host).
    pub model_host_path: String,

    /// Static UI files path
    pub ui_path: String,

    /// API subdomain hostname (e.g. "api.example.com", env: API_HOSTNAME)
    pub api_hostname: String,

    /// Chat subdomain hostname (e.g. "chat.example.com", env: CHAT_HOSTNAME)
    pub chat_hostname: String,

    /// Cookie domain for cross-subdomain sharing (e.g. ".example.com", env: COOKIE_DOMAIN)
    pub cookie_domain: Option<String>,

    /// Docker network name for backend containers (internal, isolated)
    pub backend_network: String,

    /// ACME contact email — enables automatic Let's Encrypt cert provisioning when set
    pub acme_contact: Option<String>,

    /// Use Let's Encrypt staging environment (default false)
    pub acme_staging: bool,

    /// Open WebUI backend URL (internal, no external access)
    pub webui_backend_url: String,

    /// Pre-shared API key for Open WebUI → proxy /v1 calls (env: WEBUI_API_KEY)
    pub webui_api_key: Option<String>,

    /// Max seconds to hold a queued request before returning 429 (env: QUEUE_TIMEOUT_SECS)
    pub queue_timeout_secs: u64,

    /// Set Secure flag on session cookies (env: SECURE_COOKIES, default: true).
    /// Set to false for HTTP-only dev instances.
    pub secure_cookies: bool,

    /// Encryption key for IdP client secrets at rest (env: DB_ENCRYPTION_KEY).
    /// When set, client_secret_enc is AES-256-GCM encrypted. When absent, stored plaintext.
    pub db_encryption_key: Option<String>,

    /// Previous encryption key for key rotation (env: DB_ENCRYPTION_KEY_OLD).
    /// Set this to the old key when rotating to a new DB_ENCRYPTION_KEY.
    /// The migration will re-encrypt secrets from old key to new key on startup.
    /// Remove after one successful startup cycle.
    pub db_encryption_key_old: Option<String>,

    /// Filesystem root for proxy-managed runtime artefacts (crash log dumps,
    /// future per-feature subdirs). Defaults to `/config` to match the
    /// `./data:/config` Docker bind-mount in `docker-compose.runtime.yml`,
    /// which is also where the SQLite DB lives. Phase 5 of the supervisor
    /// feature writes `<data_path>/crash_logs/<model_id>-<unix_ts>.log`
    /// here; `gc_crash_logs` keeps that subdir under 1 GiB.
    pub data_path: String,
}

/// ACME configuration derived from hostnames and contact email.
#[derive(Debug, Clone)]
pub struct AcmeSettings {
    pub domains: Vec<String>,
    pub contact: String,
    pub staging: bool,
}

impl AppConfig {
    pub fn from_env() -> Result<Self> {
        Ok(Self {
            listen_addr: std::env::var("LISTEN_ADDR").unwrap_or_else(|_| "0.0.0.0:443".into()),
            database_url: std::env::var("DATABASE_URL")
                .unwrap_or_else(|_| "sqlite:///config/sovereign.db".into()),
            tls_cert_path: std::env::var("TLS_CERT_PATH").ok(),
            tls_key_path: std::env::var("TLS_KEY_PATH").ok(),
            bootstrap_user: std::env::var("BOOTSTRAP_USER").ok(),
            bootstrap_password: std::env::var("BOOTSTRAP_PASSWORD").ok(),
            break_glass: std::env::var("BREAK_GLASS")
                .map(|v| v.eq_ignore_ascii_case("true"))
                .unwrap_or(false),
            docker_host: std::env::var("DOCKER_HOST")
                .unwrap_or_else(|_| "unix:///var/run/docker.sock".into()),
            model_path: std::env::var("MODEL_PATH").unwrap_or_else(|_| "/models".into()),
            model_host_path: std::env::var("MODEL_HOST_PATH").unwrap_or_else(|_| {
                std::env::var("MODEL_PATH").unwrap_or_else(|_| "/models".into())
            }),
            ui_path: std::env::var("UI_PATH").unwrap_or_else(|_| "/app/ui".into()),
            api_hostname: std::env::var("API_HOSTNAME").unwrap_or_else(|_| "localhost".into()),
            chat_hostname: std::env::var("CHAT_HOSTNAME").unwrap_or_else(|_| "localhost".into()),
            cookie_domain: std::env::var("COOKIE_DOMAIN").ok(),
            backend_network: std::env::var("BACKEND_NETWORK")
                .unwrap_or_else(|_| "sovereign-internal".into()),
            acme_contact: std::env::var("ACME_CONTACT").ok(),
            acme_staging: std::env::var("ACME_STAGING")
                .map(|v| v.eq_ignore_ascii_case("true"))
                .unwrap_or(false),
            webui_backend_url: std::env::var("WEBUI_BACKEND_URL")
                .unwrap_or_else(|_| "http://open-webui:8080".into()),
            webui_api_key: std::env::var("WEBUI_API_KEY").ok(),
            queue_timeout_secs: std::env::var("QUEUE_TIMEOUT_SECS")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(30),
            secure_cookies: std::env::var("SECURE_COOKIES")
                .map(|v| !v.eq_ignore_ascii_case("false"))
                .unwrap_or(true),
            db_encryption_key: std::env::var("DB_ENCRYPTION_KEY")
                .ok()
                .filter(|s| !s.is_empty()),
            db_encryption_key_old: std::env::var("DB_ENCRYPTION_KEY_OLD")
                .ok()
                .filter(|s| !s.is_empty()),
            data_path: std::env::var("DATA_PATH").unwrap_or_else(|_| "/config".into()),
        })
    }

    /// Check if bootstrap credentials are configured.
    pub fn has_bootstrap_creds(&self) -> bool {
        self.bootstrap_user.is_some() && self.bootstrap_password.is_some()
    }

    /// Validate bootstrap credentials against the provided values.
    /// Uses constant-time comparison to prevent timing side-channel attacks.
    pub fn validate_bootstrap_creds(&self, user: &str, password: &str) -> bool {
        match (&self.bootstrap_user, &self.bootstrap_password) {
            (Some(u), Some(p)) => {
                let user_match = u.as_bytes().ct_eq(user.as_bytes());
                let pass_match = p.as_bytes().ct_eq(password.as_bytes());
                (user_match & pass_match).into()
            }
            _ => false,
        }
    }

    /// Load TLS certificate and key paths, returning an error if either is missing.
    pub fn tls_paths(&self) -> Result<(&str, &str)> {
        let cert = self
            .tls_cert_path
            .as_deref()
            .context("TLS_CERT_PATH not set")?;
        let key = self
            .tls_key_path
            .as_deref()
            .context("TLS_KEY_PATH not set")?;
        Ok((cert, key))
    }

    /// Construct the external URL for the API subdomain.
    /// Scheme derived from `secure_cookies` (true → https, false → http).
    pub fn api_external_url(&self) -> String {
        let scheme = if self.secure_cookies { "https" } else { "http" };
        format!("{scheme}://{}", self.api_hostname)
    }

    /// Construct the external URL for the chat subdomain.
    pub fn chat_external_url(&self) -> String {
        let scheme = if self.secure_cookies { "https" } else { "http" };
        format!("{scheme}://{}", self.chat_hostname)
    }

    /// Return ACME config if ACME_CONTACT is set.
    /// Derives domains from api_hostname + chat_hostname.
    pub fn acme_config(&self) -> Result<Option<AcmeSettings>> {
        match &self.acme_contact {
            Some(contact) => Ok(Some(AcmeSettings {
                domains: vec![self.api_hostname.clone(), self.chat_hostname.clone()],
                contact: contact.clone(),
                staging: self.acme_staging,
            })),
            None => Ok(None),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a minimal `AppConfig` with all fields defaulted. Override specific
    /// fields in each test via struct update syntax.
    fn base_config() -> AppConfig {
        AppConfig {
            listen_addr: "0.0.0.0:443".into(),
            database_url: "sqlite://:memory:".into(),
            tls_cert_path: None,
            tls_key_path: None,
            bootstrap_user: None,
            bootstrap_password: None,
            break_glass: false,
            docker_host: "unix:///var/run/docker.sock".into(),
            model_path: "/models".into(),
            model_host_path: "/models".into(),
            ui_path: "/app/ui".into(),
            api_hostname: "localhost".into(),
            chat_hostname: "localhost".into(),
            cookie_domain: None,
            backend_network: "sovereign-internal".into(),
            acme_contact: None,
            acme_staging: false,
            webui_backend_url: "http://open-webui:8080".into(),
            webui_api_key: None,
            queue_timeout_secs: 30,
            secure_cookies: true,
            db_encryption_key: None,
            db_encryption_key_old: None,
            data_path: "/tmp/test-data-path".to_string(),
        }
    }

    // -----------------------------------------------------------------------
    // has_bootstrap_creds
    // -----------------------------------------------------------------------

    #[test]
    fn has_bootstrap_creds_both_present() {
        let cfg = AppConfig {
            bootstrap_user: Some("admin".into()),
            bootstrap_password: Some("secret".into()),
            ..base_config()
        };
        assert!(cfg.has_bootstrap_creds());
    }

    #[test]
    fn has_bootstrap_creds_user_only() {
        let cfg = AppConfig {
            bootstrap_user: Some("admin".into()),
            bootstrap_password: None,
            ..base_config()
        };
        assert!(!cfg.has_bootstrap_creds());
    }

    #[test]
    fn has_bootstrap_creds_password_only() {
        let cfg = AppConfig {
            bootstrap_user: None,
            bootstrap_password: Some("secret".into()),
            ..base_config()
        };
        assert!(!cfg.has_bootstrap_creds());
    }

    #[test]
    fn has_bootstrap_creds_neither() {
        let cfg = base_config();
        assert!(!cfg.has_bootstrap_creds());
    }

    // -----------------------------------------------------------------------
    // validate_bootstrap_creds
    // -----------------------------------------------------------------------

    #[test]
    fn validate_bootstrap_creds_correct() {
        let cfg = AppConfig {
            bootstrap_user: Some("admin".into()),
            bootstrap_password: Some("hunter2".into()),
            ..base_config()
        };
        assert!(cfg.validate_bootstrap_creds("admin", "hunter2"));
    }

    #[test]
    fn validate_bootstrap_creds_wrong_password() {
        let cfg = AppConfig {
            bootstrap_user: Some("admin".into()),
            bootstrap_password: Some("hunter2".into()),
            ..base_config()
        };
        assert!(!cfg.validate_bootstrap_creds("admin", "wrong"));
    }

    #[test]
    fn validate_bootstrap_creds_wrong_username() {
        let cfg = AppConfig {
            bootstrap_user: Some("admin".into()),
            bootstrap_password: Some("hunter2".into()),
            ..base_config()
        };
        assert!(!cfg.validate_bootstrap_creds("root", "hunter2"));
    }

    #[test]
    fn validate_bootstrap_creds_empty_strings() {
        let cfg = AppConfig {
            bootstrap_user: Some("admin".into()),
            bootstrap_password: Some("hunter2".into()),
            ..base_config()
        };
        assert!(!cfg.validate_bootstrap_creds("", ""));
    }

    #[test]
    fn validate_bootstrap_creds_no_creds_configured() {
        let cfg = base_config();
        assert!(!cfg.validate_bootstrap_creds("admin", "hunter2"));
    }

    #[test]
    fn validate_bootstrap_creds_empty_configured_and_provided() {
        let cfg = AppConfig {
            bootstrap_user: Some("".into()),
            bootstrap_password: Some("".into()),
            ..base_config()
        };
        assert!(cfg.validate_bootstrap_creds("", ""));
    }

    // -----------------------------------------------------------------------
    // tls_paths
    // -----------------------------------------------------------------------

    #[test]
    fn tls_paths_both_present() {
        let cfg = AppConfig {
            tls_cert_path: Some("/cert.pem".into()),
            tls_key_path: Some("/key.pem".into()),
            ..base_config()
        };
        let (cert, key) = cfg.tls_paths().unwrap();
        assert_eq!(cert, "/cert.pem");
        assert_eq!(key, "/key.pem");
    }

    #[test]
    fn tls_paths_missing_cert() {
        let cfg = AppConfig {
            tls_cert_path: None,
            tls_key_path: Some("/key.pem".into()),
            ..base_config()
        };
        let err = cfg.tls_paths().unwrap_err();
        assert!(err.to_string().contains("TLS_CERT_PATH"));
    }

    #[test]
    fn tls_paths_missing_key() {
        let cfg = AppConfig {
            tls_cert_path: Some("/cert.pem".into()),
            tls_key_path: None,
            ..base_config()
        };
        let err = cfg.tls_paths().unwrap_err();
        assert!(err.to_string().contains("TLS_KEY_PATH"));
    }

    // -----------------------------------------------------------------------
    // acme_config
    // -----------------------------------------------------------------------

    #[test]
    fn acme_config_contact_present() {
        let cfg = AppConfig {
            api_hostname: "api.example.com".into(),
            chat_hostname: "chat.example.com".into(),
            acme_contact: Some("admin@example.com".into()),
            acme_staging: false,
            ..base_config()
        };
        let result = cfg.acme_config().unwrap().unwrap();
        assert_eq!(result.domains, vec!["api.example.com", "chat.example.com"]);
        assert_eq!(result.contact, "admin@example.com");
        assert!(!result.staging);
    }

    #[test]
    fn acme_config_staging() {
        let cfg = AppConfig {
            api_hostname: "api.example.com".into(),
            chat_hostname: "chat.example.com".into(),
            acme_contact: Some("admin@example.com".into()),
            acme_staging: true,
            ..base_config()
        };
        let result = cfg.acme_config().unwrap().unwrap();
        assert!(result.staging);
    }

    #[test]
    fn acme_config_no_contact_returns_none() {
        let cfg = base_config();
        let result = cfg.acme_config().unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn api_external_url_secure() {
        let cfg = AppConfig {
            api_hostname: "api.example.com".into(),
            secure_cookies: true,
            ..base_config()
        };
        assert_eq!(cfg.api_external_url(), "https://api.example.com");
    }

    #[test]
    fn api_external_url_insecure() {
        let cfg = AppConfig {
            api_hostname: "api.example.com".into(),
            secure_cookies: false,
            ..base_config()
        };
        assert_eq!(cfg.api_external_url(), "http://api.example.com");
    }

    #[test]
    fn chat_external_url_secure() {
        let cfg = AppConfig {
            chat_hostname: "chat.example.com".into(),
            secure_cookies: true,
            ..base_config()
        };
        assert_eq!(cfg.chat_external_url(), "https://chat.example.com");
    }
}
