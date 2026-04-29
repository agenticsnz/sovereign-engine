use anyhow::{bail, Result};
use uuid::Uuid;

use crate::config::AppConfig;
use crate::db::Database;

/// Check if bootstrap credentials should be active.
/// Bootstrap creds are only active when BREAK_GLASS is true and credentials are configured.
pub fn is_bootstrap_active(config: &AppConfig) -> bool {
    config.break_glass && config.has_bootstrap_creds()
}

/// Validate bootstrap credentials and return a bootstrap admin user ID.
/// Creates the bootstrap user record if it doesn't exist.
pub async fn validate_bootstrap(
    config: &AppConfig,
    db: &Database,
    username: &str,
    password: &str,
) -> Result<String> {
    if !is_bootstrap_active(config) {
        bail!("Bootstrap authentication is not active");
    }

    if !config.validate_bootstrap_creds(username, password) {
        bail!("Invalid bootstrap credentials");
    }

    // Ensure bootstrap user exists in the database
    let user_id = ensure_bootstrap_user(db, username).await?;

    Ok(user_id)
}

/// Ensure a bootstrap admin user record exists.
pub async fn ensure_bootstrap_user(db: &Database, username: &str) -> Result<String> {
    let existing: Option<(String,)> =
        sqlx::query_as("SELECT id FROM users WHERE subject = ? AND idp_id = 'bootstrap'")
            .bind(username)
            .fetch_optional(&db.pool)
            .await?;

    if let Some((id,)) = existing {
        return Ok(id);
    }

    // Create the bootstrap IdP config if it doesn't exist
    sqlx::query(
        "INSERT OR IGNORE INTO idp_configs (id, name, issuer, client_id, client_secret_enc, scopes, enabled) VALUES ('bootstrap', 'Bootstrap', 'local', 'bootstrap', '', '', 0)",
    )
    .execute(&db.pool)
    .await?;

    let id = Uuid::new_v4().to_string();
    sqlx::query(
        "INSERT INTO users (id, idp_id, subject, display_name, is_admin) VALUES (?, 'bootstrap', ?, ?, 1)",
    )
    .bind(&id)
    .bind(username)
    .bind(username)
    .execute(&db.pool)
    .await?;

    Ok(id)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a minimal AppConfig for testing bootstrap logic.
    fn test_config(break_glass: bool, user: Option<&str>, password: Option<&str>) -> AppConfig {
        AppConfig {
            listen_addr: "0.0.0.0:443".into(),
            database_url: "sqlite::memory:".into(),
            tls_cert_path: None,
            tls_key_path: None,
            bootstrap_user: user.map(String::from),
            bootstrap_password: password.map(String::from),
            break_glass,
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

    #[test]
    fn bootstrap_active_when_break_glass_and_creds() {
        let config = test_config(true, Some("admin"), Some("password"));
        assert!(is_bootstrap_active(&config));
    }

    #[test]
    fn bootstrap_inactive_when_break_glass_false() {
        let config = test_config(false, Some("admin"), Some("password"));
        assert!(!is_bootstrap_active(&config));
    }

    #[test]
    fn bootstrap_inactive_when_no_user() {
        let config = test_config(true, None, Some("password"));
        assert!(!is_bootstrap_active(&config));
    }

    #[test]
    fn bootstrap_inactive_when_no_password() {
        let config = test_config(true, Some("admin"), None);
        assert!(!is_bootstrap_active(&config));
    }

    #[test]
    fn bootstrap_inactive_when_no_creds_at_all() {
        let config = test_config(true, None, None);
        assert!(!is_bootstrap_active(&config));
    }
}
