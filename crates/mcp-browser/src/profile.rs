//! Saved browser session profiles: named snapshots of cookies, localStorage,
//! and sessionStorage. Useful for testing multi-user flows or swapping between
//! authenticated states instantly without re-logging in.
//!
//! File-backed JSON, keyed by profile name. On Unix platforms, the backing file
//! permissions are restricted to 0600 (owner read/write only) because it holds
//! session credentials.

use std::collections::BTreeMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// One saved profile snapshot.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Profile {
    pub name: String,
    pub cookies: Vec<Value>,
    pub local_storage: Value,
    pub session_storage: Value,
    pub url: Option<String>,
    pub updated_ms: u128,
}

/// Metadata summary of a saved profile for listings.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ProfileSummary {
    pub name: String,
    pub cookies_count: usize,
    pub local_storage_count: usize,
    pub url: Option<String>,
    pub updated_ms: u128,
}

#[derive(Debug)]
pub enum ProfileError {
    Invalid(String),
    Io(String),
}

/// File-backed profile store, keyed by name.
pub struct ProfileStore {
    path: PathBuf,
    max_profiles: usize,
}

impl ProfileStore {
    pub fn new(path: PathBuf, max_profiles: usize) -> Self {
        ProfileStore { path, max_profiles }
    }

    fn load(&self) -> Result<BTreeMap<String, Profile>, ProfileError> {
        match std::fs::read_to_string(&self.path) {
            Ok(t) => serde_json::from_str(&t)
                .map_err(|e| ProfileError::Io(format!("{}: {e}", self.path.display()))),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(BTreeMap::new()),
            Err(e) => Err(ProfileError::Io(format!("{}: {e}", self.path.display()))),
        }
    }

    fn persist(&self, map: &BTreeMap<String, Profile>) -> Result<(), ProfileError> {
        if let Some(dir) = self.path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let json = serde_json::to_string_pretty(map)
            .map_err(|e| ProfileError::Io(format!("serialize: {e}")))?;
        std::fs::write(&self.path, json)
            .map_err(|e| ProfileError::Io(format!("{}: {e}", self.path.display())))?;

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&self.path, std::fs::Permissions::from_mode(0o600));
        }

        Ok(())
    }

    pub fn get(&self, name: &str) -> Result<Option<Profile>, ProfileError> {
        Ok(self.load()?.remove(name.trim()))
    }

    pub fn save(
        &self,
        name: &str,
        cookies: Vec<Value>,
        local_storage: Value,
        session_storage: Value,
        url: Option<String>,
        now_ms: u128,
    ) -> Result<Profile, ProfileError> {
        let name = name.trim();
        if name.is_empty() {
            return Err(ProfileError::Invalid(
                "profile name must not be empty".into(),
            ));
        }
        if name.contains('/') || name.contains('\\') || name.contains("..") {
            return Err(ProfileError::Invalid(
                "profile name must not contain path separators".into(),
            ));
        }
        let mut map = self.load()?;
        if !map.contains_key(name) && map.len() >= self.max_profiles {
            return Err(ProfileError::Invalid(format!(
                "at most {} profiles may be stored",
                self.max_profiles
            )));
        }
        let profile = Profile {
            name: name.to_string(),
            cookies,
            local_storage,
            session_storage,
            url,
            updated_ms: now_ms,
        };
        map.insert(name.to_string(), profile.clone());
        self.persist(&map)?;
        Ok(profile)
    }

    pub fn list(&self) -> Result<Vec<ProfileSummary>, ProfileError> {
        let map = self.load()?;
        Ok(map
            .into_iter()
            .map(|(name, p)| {
                let ls_count = p.local_storage.as_object().map(|o| o.len()).unwrap_or(0);
                ProfileSummary {
                    name,
                    cookies_count: p.cookies.len(),
                    local_storage_count: ls_count,
                    url: p.url,
                    updated_ms: p.updated_ms,
                }
            })
            .collect())
    }

    pub fn delete(&self, name: &str) -> Result<bool, ProfileError> {
        let mut map = self.load()?;
        let removed = map.remove(name.trim()).is_some();
        if removed {
            self.persist(&map)?;
        }
        Ok(removed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_store(tag: &str, cap: usize) -> (ProfileStore, PathBuf) {
        let mut p = std::env::temp_dir();
        p.push(format!(
            "agentctl-profile-test-{tag}-{}.json",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&p);
        let store = ProfileStore::new(p.clone(), cap);
        (store, p)
    }

    #[test]
    fn save_get_list_delete_round_trip() {
        let (store, path) = temp_store("roundtrip", 5);
        let cookies = vec![serde_json::json!({"name":"sid","value":"secret123"})];
        let ls = serde_json::json!({"theme":"dark"});
        let ss = serde_json::json!({"tab":1});

        let saved = store
            .save(
                "admin",
                cookies.clone(),
                ls.clone(),
                ss.clone(),
                Some("https://example.com".into()),
                1000,
            )
            .unwrap();
        assert_eq!(saved.name, "admin");

        let loaded = store.get("admin").unwrap().expect("profile should exist");
        assert_eq!(loaded.cookies, cookies);
        assert_eq!(loaded.local_storage, ls);
        assert_eq!(loaded.session_storage, ss);
        assert_eq!(loaded.url.as_deref(), Some("https://example.com"));

        let list = store.list().unwrap();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].name, "admin");
        assert_eq!(list[0].cookies_count, 1);
        assert_eq!(list[0].local_storage_count, 1);

        assert!(store.delete("admin").unwrap());
        assert!(store.get("admin").unwrap().is_none());
        assert_eq!(store.list().unwrap().len(), 0);

        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn invalid_names_are_rejected() {
        let (store, path) = temp_store("invalid", 5);
        assert!(store
            .save("", vec![], Value::Null, Value::Null, None, 0)
            .is_err());
        assert!(store
            .save("  ", vec![], Value::Null, Value::Null, None, 0)
            .is_err());
        assert!(store
            .save("../evil", vec![], Value::Null, Value::Null, None, 0)
            .is_err());
        assert!(store
            .save("foo/bar", vec![], Value::Null, Value::Null, None, 0)
            .is_err());
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn profile_cap_is_enforced() {
        let (store, path) = temp_store("cap", 2);
        assert!(store
            .save("p1", vec![], Value::Null, Value::Null, None, 0)
            .is_ok());
        assert!(store
            .save("p2", vec![], Value::Null, Value::Null, None, 0)
            .is_ok());
        assert!(store
            .save("p3", vec![], Value::Null, Value::Null, None, 0)
            .is_err());
        // Replacing existing profile is allowed at cap
        assert!(store
            .save("p2", vec![], Value::Null, Value::Null, None, 1)
            .is_ok());
        let _ = std::fs::remove_file(path);
    }

    #[cfg(unix)]
    #[test]
    fn profile_file_permissions_are_restrictive_on_unix() {
        use std::os::unix::fs::PermissionsExt;
        let (store, path) = temp_store("perms", 5);
        store
            .save("auth", vec![], Value::Null, Value::Null, None, 0)
            .unwrap();
        let meta = std::fs::metadata(&path).unwrap();
        let mode = meta.permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "profile file must be 0600 on unix");
        let _ = std::fs::remove_file(path);
    }
}
