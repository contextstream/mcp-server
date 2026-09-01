//! Credential storage and retrieval.
//!
//! Handles reading and writing credentials to ~/.contextstream/credentials.json

use anyhow::{Context, Result};
use mcp_client::json::parse_value_without_duplicate_keys;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};

use super::safe_edit;

/// Stored credentials.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SavedCredentials {
    /// API key for authentication.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub api_key: Option<String>,

    /// JWT token (alternative to API key).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub jwt: Option<String>,

    /// Custom API URL.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub api_url: Option<String>,

    /// User email (for display).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub email: Option<String>,

    /// Timestamp when credentials were saved.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub saved_at: Option<String>,
}

/// Get the path to the credentials file.
pub fn credentials_file_path() -> PathBuf {
    contextstream_config_dir().join("credentials.json")
}

/// Get the ContextStream config directory.
pub fn contextstream_config_dir() -> PathBuf {
    if let Some(home) = dirs::home_dir() {
        home.join(".contextstream")
    } else {
        PathBuf::from(".contextstream")
    }
}

/// Read saved credentials from disk.
pub fn read_saved_credentials() -> Result<SavedCredentials> {
    let path = credentials_file_path();

    if !path.try_exists()? {
        return Ok(SavedCredentials::default());
    }

    let content = std::fs::read_to_string(&path)?;
    let value = parse_value_without_duplicate_keys(&content)?;
    let creds: SavedCredentials = serde_json::from_value(value)?;

    Ok(creds)
}

/// Write credentials to disk.
pub fn write_saved_credentials(api_key: &str, api_url: Option<&str>) -> Result<()> {
    let path = credentials_file_path();
    let loaded = safe_edit::read_for_edit(&path, safe_edit::JsonDialect::Strict)?;
    if loaded.nonstandard_syntax {
        anyhow::bail!(
            "Refusing to modify {}: credentials must be strict JSON without comments, a BOM, or trailing commas",
            path.display()
        );
    }
    let mut updated: Value = loaded.value.clone();
    let object = updated
        .as_object_mut()
        .ok_or_else(|| anyhow::anyhow!("Credentials root is not an object"))?;
    let credentials_changed = object.get("api_key").and_then(Value::as_str) != Some(api_key)
        || object.contains_key("jwt")
        || match api_url {
            Some(api_url) => object.get("api_url").and_then(Value::as_str) != Some(api_url),
            None => object.contains_key("api_url"),
        };
    object.insert("api_key".to_string(), json!(api_key));
    object.remove("jwt");
    match api_url {
        Some(api_url) => {
            object.insert("api_url".to_string(), json!(api_url));
        }
        None => {
            object.remove("api_url");
        }
    }
    if credentials_changed
        || object
            .get("saved_at")
            .and_then(Value::as_str)
            .is_none_or(str::is_empty)
    {
        object.insert(
            "saved_at".to_string(),
            json!(chrono::Utc::now().to_rfc3339()),
        );
    }

    // Remove any recovery sidecar left by an older release before publishing
    // the new secret. The private commit below never creates one, so even a
    // crash between atomic replacement and cleanup cannot strand the old key.
    remove_credentials_backup(&path)?;
    safe_edit::commit_private(&path, &loaded, &updated, &["api_url", "jwt"])?;

    Ok(())
}

/// Delete saved credentials.
pub fn delete_saved_credentials() -> Result<()> {
    let path = credentials_file_path();
    match std::fs::read_to_string(&path) {
        Ok(existing) => {
            safe_edit::remove_owned_file_if_unchanged(&path, &existing)?;
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    remove_credentials_backup(&path)?;

    Ok(())
}

fn remove_credentials_backup(path: &Path) -> Result<()> {
    let backup = safe_edit::backup_path(path)?;
    if let Some(existing) = safe_edit::read_recovery_file(&backup)? {
        safe_edit::remove_owned_file_if_unchanged(&backup, &existing)?;
    }
    Ok(())
}

/// Normalize an API URL (ensure trailing slash, etc.).
pub fn normalize_api_url(url: &str) -> String {
    let mut url = url.trim().to_string();

    // Remove trailing slash
    while url.ends_with('/') {
        url.pop();
    }

    // Ensure https
    if !url.starts_with("http://") && !url.starts_with("https://") {
        url = format!("https://{}", url);
    }

    url
}

fn is_local_dev_binary_path(path: &Path) -> bool {
    let Some(binary_dir) = path.parent() else {
        return false;
    };
    let Some(profile) = binary_dir.file_name().and_then(|name| name.to_str()) else {
        return false;
    };
    if !matches!(profile, "debug" | "release") {
        return false;
    }

    let Some(target_dir) = binary_dir.parent() else {
        return false;
    };
    if target_dir.file_name().and_then(|name| name.to_str()) != Some("target") {
        return false;
    }

    let Some(project_root) = target_dir.parent() else {
        return false;
    };
    project_root.join("Cargo.toml").is_file()
}

pub fn local_dev_api_url_override() -> Option<String> {
    let exe = std::env::current_exe().ok()?;
    if is_local_dev_binary_path(&exe) {
        Some("http://localhost:8080".to_string())
    } else {
        None
    }
}

/// Check if credentials are configured.
pub fn has_credentials() -> bool {
    if let Ok(creds) = read_saved_credentials() {
        creds.api_key.is_some() || creds.jwt.is_some()
    } else {
        false
    }
}

/// Get an API key from the environment or saved credentials.
///
/// Unlike [`get_api_key`], this preserves errors from an existing credentials
/// file. Setup and refresh commands must use this form so a malformed or
/// unreadable file cannot be mistaken for an absent key and followed by a
/// destructive editor-config rewrite.
pub fn get_api_key_result() -> Result<Option<String>> {
    // First check environment
    if let Ok(key) = std::env::var("CONTEXTSTREAM_API_KEY") {
        if !key.trim().is_empty() {
            return Ok(Some(key));
        }
    }

    // Then check saved credentials
    let path = credentials_file_path();
    let creds = read_saved_credentials().with_context(|| {
        format!(
            "Could not read saved credentials at {}; refusing to modify editor configuration",
            path.display()
        )
    })?;

    Ok(creds.api_key.filter(|key| !key.trim().is_empty()))
}

/// Best-effort API-key lookup for runtime paths that can operate unauthenticated.
///
/// Installer and refresh paths must use [`get_api_key_result`] instead.
pub fn get_api_key() -> Option<String> {
    get_api_key_result().ok().flatten()
}

/// Get JWT from environment or saved credentials.
pub fn get_jwt() -> Option<String> {
    // First check environment
    if let Ok(jwt) = std::env::var("CONTEXTSTREAM_JWT") {
        if !jwt.is_empty() {
            return Some(jwt);
        }
    }

    // Then check saved credentials
    if let Ok(creds) = read_saved_credentials() {
        if creds.jwt.is_some() {
            return creds.jwt;
        }
    }

    None
}

/// Get API URL from environment or saved credentials.
pub fn get_api_url() -> String {
    // First check environment
    if let Ok(url) = std::env::var("CONTEXTSTREAM_API_URL") {
        if !url.is_empty() {
            return normalize_api_url(&url);
        }
    }

    // Local cargo-built binaries default to the local development API.
    if let Some(url) = local_dev_api_url_override() {
        return url;
    }

    // Then check saved credentials
    if let Ok(creds) = read_saved_credentials() {
        if let Some(url) = creds.api_url {
            return normalize_api_url(&url);
        }
    }

    // Default
    "https://api.contextstream.io".to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::env_test_mutex;
    use std::ffi::OsString;
    use tempfile::tempdir;

    struct EnvironmentGuard {
        home: Option<OsString>,
        api_key: Option<OsString>,
    }

    impl EnvironmentGuard {
        fn capture() -> Self {
            Self {
                home: std::env::var_os("HOME"),
                api_key: std::env::var_os("CONTEXTSTREAM_API_KEY"),
            }
        }
    }

    impl Drop for EnvironmentGuard {
        fn drop(&mut self) {
            match self.home.take() {
                Some(value) => std::env::set_var("HOME", value),
                None => std::env::remove_var("HOME"),
            }
            match self.api_key.take() {
                Some(value) => std::env::set_var("CONTEXTSTREAM_API_KEY", value),
                None => std::env::remove_var("CONTEXTSTREAM_API_KEY"),
            }
        }
    }

    #[test]
    fn test_normalize_api_url() {
        assert_eq!(
            normalize_api_url("api.contextstream.io"),
            "https://api.contextstream.io"
        );
        assert_eq!(
            normalize_api_url("https://api.contextstream.io/"),
            "https://api.contextstream.io"
        );
        assert_eq!(
            normalize_api_url("http://localhost:8080/"),
            "http://localhost:8080"
        );
    }

    #[test]
    fn test_credentials_file_path() {
        let path = credentials_file_path();
        assert!(path.ends_with("credentials.json"));
    }

    #[test]
    fn test_is_local_dev_binary_path_detects_cargo_target_builds() {
        let workspace_root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .and_then(Path::parent)
            .expect("mcp-server crate should live under workspace/crates");

        assert!(is_local_dev_binary_path(
            &workspace_root.join("target/debug/contextstream-mcp")
        ));
        assert!(is_local_dev_binary_path(
            &workspace_root.join("target/release/contextstream-mcp")
        ));
        assert!(!is_local_dev_binary_path(Path::new(
            "/usr/local/bin/contextstream-mcp"
        )));
    }

    #[test]
    fn api_key_result_fails_closed_for_malformed_credentials() {
        let _lock = env_test_mutex().lock().unwrap_or_else(|e| e.into_inner());
        let _environment = EnvironmentGuard::capture();
        let home = tempdir().expect("temporary home");
        let config_dir = home.path().join(".contextstream");
        std::fs::create_dir_all(&config_dir).expect("create config directory");
        std::fs::write(config_dir.join("credentials.json"), "{not json")
            .expect("seed malformed credentials");
        std::env::set_var("HOME", home.path());
        std::env::remove_var("CONTEXTSTREAM_API_KEY");

        let error = get_api_key_result().expect_err("malformed credentials must be an error");
        assert!(
            error
                .to_string()
                .contains("refusing to modify editor configuration"),
            "unexpected error: {error:#}"
        );
    }

    #[test]
    fn api_key_result_fails_closed_for_duplicate_credential_keys() {
        let _lock = env_test_mutex().lock().unwrap_or_else(|e| e.into_inner());
        let _environment = EnvironmentGuard::capture();
        let home = tempdir().expect("temporary home");
        let config_dir = home.path().join(".contextstream");
        std::fs::create_dir_all(&config_dir).expect("create config directory");
        let path = config_dir.join("credentials.json");
        let original = "{\"api_key\":\"first\",\"api_key\":\"second\"}\n";
        std::fs::write(&path, original).expect("seed ambiguous credentials");
        std::env::set_var("HOME", home.path());
        std::env::remove_var("CONTEXTSTREAM_API_KEY");

        assert!(read_saved_credentials().is_err());
        assert!(get_api_key_result().is_err());
        assert_eq!(std::fs::read_to_string(path).unwrap(), original);
    }

    #[test]
    fn explicit_api_key_overrides_malformed_saved_credentials() {
        let _lock = env_test_mutex().lock().unwrap_or_else(|e| e.into_inner());
        let _environment = EnvironmentGuard::capture();
        let home = tempdir().expect("temporary home");
        let config_dir = home.path().join(".contextstream");
        std::fs::create_dir_all(&config_dir).expect("create config directory");
        std::fs::write(config_dir.join("credentials.json"), "{not json")
            .expect("seed malformed credentials");
        std::env::set_var("HOME", home.path());
        std::env::set_var("CONTEXTSTREAM_API_KEY", "explicit-test-key");

        assert_eq!(
            get_api_key_result().expect("explicit key should bypass saved credentials"),
            Some("explicit-test-key".to_string())
        );
    }

    #[test]
    fn credential_rotation_preserves_unknown_fields_without_secret_backup() {
        let _lock = env_test_mutex().lock().unwrap_or_else(|e| e.into_inner());
        let _environment = EnvironmentGuard::capture();
        let home = tempdir().expect("temporary home");
        std::env::set_var("HOME", home.path());
        let path = credentials_file_path();
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(
            &path,
            "{\n  \"api_key\": \"old-secret\",\n  \"jwt\": \"old-jwt\",\n  \"future_field\": {\"keep\": true},\n  \"email\": \"user@example.com\"\n}",
        )
        .unwrap();
        let stale_backup = safe_edit::backup_path(&path).unwrap();
        std::fs::write(&stale_backup, "{\"api_key\":\"even-older-secret\"}\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        }

        write_saved_credentials("new-secret", Some("https://api.example.com"))
            .expect("rotate credentials");

        let value: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(value["api_key"], "new-secret");
        assert_eq!(value["future_field"]["keep"], true);
        assert_eq!(value["email"], "user@example.com");
        assert!(value.get("jwt").is_none());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
        assert!(!stale_backup.exists());
        assert!(!std::fs::read_to_string(&path)
            .unwrap()
            .contains("old-secret"));
    }

    #[test]
    fn identical_credentials_write_is_byte_identical() {
        let _lock = env_test_mutex().lock().unwrap_or_else(|e| e.into_inner());
        let _environment = EnvironmentGuard::capture();
        let home = tempdir().expect("temporary home");
        std::env::set_var("HOME", home.path());
        let path = credentials_file_path();

        write_saved_credentials("same-secret", Some("https://api.example.com"))
            .expect("initial credential write");
        let before = std::fs::read(&path).unwrap();
        write_saved_credentials("same-secret", Some("https://api.example.com"))
            .expect("idempotent credential write");

        assert_eq!(std::fs::read(&path).unwrap(), before);
    }

    #[test]
    fn credential_rotation_refuses_nonstandard_json_without_touching_bytes() {
        let _lock = env_test_mutex().lock().unwrap_or_else(|e| e.into_inner());
        let _environment = EnvironmentGuard::capture();
        let home = tempdir().expect("temporary home");
        std::env::set_var("HOME", home.path());
        let path = credentials_file_path();
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let original =
            "{\n  // not accepted by the credentials reader\n  \"api_key\": \"old\",\n}\n";
        std::fs::write(&path, original).unwrap();

        let error = write_saved_credentials("new", None)
            .expect_err("credentials must remain parseable by the strict reader");

        assert!(error.to_string().contains("strict JSON"), "{error:#}");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), original);
        assert!(!safe_edit::backup_path(&path).unwrap().exists());
    }

    #[test]
    fn deleting_credentials_removes_existing_recovery_sidecar() {
        let _lock = env_test_mutex().lock().unwrap_or_else(|e| e.into_inner());
        let _environment = EnvironmentGuard::capture();
        let home = tempdir().expect("temporary home");
        std::env::set_var("HOME", home.path());
        let path = credentials_file_path();
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "{\"api_key\":\"live\"}").unwrap();
        let backup = safe_edit::backup_path(&path).unwrap();
        std::fs::write(&backup, "{\"api_key\":\"old\"}").unwrap();

        delete_saved_credentials().expect("delete credentials");

        assert!(!path.exists());
        assert!(!backup.exists());
    }
}
