//! Persists the tado° refresh token so the exporter survives restarts without
//! a new browser authorization.

use std::fs;
use std::io;
use std::path::PathBuf;

use serde_derive::{Deserialize, Serialize};

use crate::persist::write_private_atomic;

#[derive(Serialize, Deserialize)]
struct StoredToken {
    refresh_token: String,
}

pub struct TokenStore {
    path: PathBuf,
}

impl TokenStore {
    pub fn new(path: impl Into<PathBuf>) -> TokenStore {
        TokenStore { path: path.into() }
    }

    /// Returns the stored refresh token, or `None` if nothing has been stored yet.
    pub fn load(&self) -> io::Result<Option<String>> {
        let contents = match fs::read_to_string(&self.path) {
            Ok(contents) => contents,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e),
        };

        let stored: StoredToken = serde_json::from_str(&contents)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;

        Ok(Some(stored.refresh_token))
    }

    /// Atomically replaces the stored refresh token. The file is readable by its owner only.
    pub fn save(&self, refresh_token: &str) -> io::Result<()> {
        let stored = StoredToken {
            refresh_token: refresh_token.to_string(),
        };
        let contents = serde_json::to_vec(&stored)?;

        write_private_atomic(&self.path, &contents)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use tempfile::TempDir;

    #[test]
    fn load_returns_none_when_file_is_missing() {
        // Arrange
        let dir = TempDir::new().unwrap();
        let store = TokenStore::new(dir.path().join("missing.json"));

        // Act
        let loaded = store.load().unwrap();

        // Assert
        assert_eq!(loaded, None);
    }

    #[test]
    fn save_then_load_round_trips_and_overwrites() {
        // Arrange
        let dir = TempDir::new().unwrap();
        let store = TokenStore::new(dir.path().join("nested/token.json"));

        // Act
        store.save("first").unwrap();
        store.save("second").unwrap();

        // Assert
        assert_eq!(store.load().unwrap().as_deref(), Some("second"));
        assert!(!dir.path().join("nested/token.tmp").exists());
    }

    #[test]
    fn load_fails_on_corrupt_file() {
        // Arrange
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("token.json");
        fs::write(&path, "not json").unwrap();
        let store = TokenStore::new(path);

        // Act
        let result = store.load();

        // Assert
        assert_eq!(result.unwrap_err().kind(), io::ErrorKind::InvalidData);
    }

    #[cfg(unix)]
    #[test]
    fn stale_temp_file_does_not_leak_permissions() {
        use std::os::unix::fs::PermissionsExt;

        // Arrange
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("token.json");
        let tmp_path = dir.path().join("token.tmp");
        fs::write(&tmp_path, "stale").unwrap();
        fs::set_permissions(&tmp_path, fs::Permissions::from_mode(0o644)).unwrap();
        let store = TokenStore::new(&path);

        // Act
        store.save("secret").unwrap();

        // Assert
        let mode = fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
    }

    #[cfg(unix)]
    #[test]
    fn saved_file_is_owner_only() {
        use std::os::unix::fs::PermissionsExt;

        // Arrange
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("token.json");
        let store = TokenStore::new(&path);

        // Act
        store.save("secret").unwrap();

        // Assert
        let mode = fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
    }
}
