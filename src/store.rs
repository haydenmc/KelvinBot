use anyhow::{Context, Result};
use serde::{Serialize, de::DeserializeOwned};
use serde_json::Value;
use std::{
    collections::HashMap,
    io::Write,
    path::{Path, PathBuf},
};
use tokio::sync::Mutex;

/// A persistent, JSON-backed key-value store that saves to disk on every write.
///
/// Loaded once at startup; each `set` call updates the in-memory map and
/// replaces the backing file. The file is replaced atomically (written to a
/// temporary file, then renamed over the old one), so a crash or power loss
/// mid-write leaves the previous contents intact rather than a truncated file.
/// Use `in_memory` in tests to skip disk I/O.
pub struct PersistentStore {
    path: Option<PathBuf>,
    data: Mutex<HashMap<String, Value>>,
}

impl PersistentStore {
    /// Load the store from `path`, creating an empty store if the file does not exist.
    pub fn load(path: impl Into<PathBuf>) -> Result<Self> {
        let path = path.into();
        let data = if path.exists() {
            let content = std::fs::read_to_string(&path)
                .with_context(|| format!("reading store file {}", path.display()))?;
            serde_json::from_str(&content)
                .with_context(|| format!("parsing store file {}", path.display()))?
        } else {
            HashMap::new()
        };
        Ok(Self { path: Some(path), data: Mutex::new(data) })
    }

    /// Create an in-memory store that never writes to disk. Useful for testing.
    pub fn in_memory() -> Self {
        Self { path: None, data: Mutex::new(HashMap::new()) }
    }

    /// Get the value stored under `key`, deserializing it to `T`.
    ///
    /// Returns `None` if the key is absent. A value that is present but cannot
    /// be read as `T` (the stored shape changed, say) is also `None`, and is
    /// logged as an error so it is not mistaken for a key that was never set.
    pub async fn get<T: DeserializeOwned>(&self, key: &str) -> Option<T> {
        let data = self.data.lock().await;
        let value = data.get(key)?;
        match serde_json::from_value(value.clone()) {
            Ok(value) => Some(value),
            Err(e) => {
                tracing::error!(
                    key,
                    store = ?self.path,
                    error = %e,
                    "stored value has an unexpected shape; treating it as absent"
                );
                None
            }
        }
    }

    /// Insert or update `key`, then persist the whole store to disk (if a path is configured).
    ///
    /// If persisting fails the error is returned, the file on disk keeps its
    /// previous contents, and the new value is still held in memory.
    pub async fn set<T: Serialize>(&self, key: &str, value: &T) -> Result<()> {
        let serialized = serde_json::to_value(value)?;
        let mut data = self.data.lock().await;
        data.insert(key.to_string(), serialized);
        if let Some(path) = &self.path {
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            let content = serde_json::to_string_pretty(&*data)?;
            write_atomically(path, content.as_bytes())
                .with_context(|| format!("writing store file {}", path.display()))?;
        }
        Ok(())
    }
}

/// Where `path`'s replacement is written before being renamed into place. It
/// sits in the same directory so the rename never crosses filesystems.
fn temp_path(path: &Path) -> PathBuf {
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(".tmp");
    path.with_file_name(name)
}

/// Replaces the contents of `path` with `content`, such that a reader (or a
/// restart after a crash) sees either the old contents or the new, never a
/// partial file.
fn write_atomically(path: &Path, content: &[u8]) -> std::io::Result<()> {
    let temp = temp_path(path);

    let mut file = std::fs::File::create(&temp)?;
    file.write_all(content)?;
    // Flush to disk before the rename, or a power loss could leave the new
    // name pointing at data that was never written.
    file.sync_all()?;
    drop(file);

    std::fs::rename(&temp, path)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store_path(dir: &tempfile::TempDir) -> PathBuf {
        dir.path().join("middleware.json")
    }

    #[tokio::test]
    async fn values_survive_a_reload() {
        let dir = tempfile::tempdir().unwrap();
        let path = store_path(&dir);

        let store = PersistentStore::load(&path).unwrap();
        store.set("count", &3u32).await.unwrap();
        store.set("names", &vec!["a", "b"]).await.unwrap();

        let reloaded = PersistentStore::load(&path).unwrap();
        assert_eq!(reloaded.get::<u32>("count").await, Some(3));
        assert_eq!(reloaded.get::<Vec<String>>("names").await, Some(vec!["a".into(), "b".into()]));
    }

    #[tokio::test]
    async fn set_creates_missing_parent_directories() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested").join("deeper").join("store.json");

        PersistentStore::load(&path).unwrap().set("key", &1u8).await.unwrap();

        assert!(path.exists());
    }

    #[tokio::test]
    async fn no_temporary_file_is_left_behind() {
        let dir = tempfile::tempdir().unwrap();
        let path = store_path(&dir);

        PersistentStore::load(&path).unwrap().set("key", &1u8).await.unwrap();

        let entries: Vec<_> =
            std::fs::read_dir(dir.path()).unwrap().map(|e| e.unwrap().file_name()).collect();
        assert_eq!(entries, [path.file_name().unwrap()]);
    }

    #[tokio::test]
    async fn failed_write_leaves_the_previous_file_intact() {
        let dir = tempfile::tempdir().unwrap();
        let path = store_path(&dir);

        let store = PersistentStore::load(&path).unwrap();
        store.set("key", &"old").await.unwrap();
        let before = std::fs::read_to_string(&path).unwrap();

        // Make the write fail part-way: the temporary file cannot be created
        // because a directory is in its place.
        std::fs::create_dir(temp_path(&path)).unwrap();
        let result = store.set("key", &"new").await;

        assert!(result.is_err(), "the failure should be reported");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), before);
        assert_eq!(
            PersistentStore::load(&path).unwrap().get::<String>("key").await.unwrap(),
            "old"
        );
    }

    #[tokio::test]
    async fn stale_temporary_file_from_a_crash_is_overwritten() {
        let dir = tempfile::tempdir().unwrap();
        let path = store_path(&dir);
        std::fs::write(temp_path(&path), "{ \"half\": ").unwrap();

        let store = PersistentStore::load(&path).unwrap();
        store.set("key", &1u8).await.unwrap();

        assert_eq!(PersistentStore::load(&path).unwrap().get::<u8>("key").await, Some(1));
        assert!(!temp_path(&path).exists());
    }

    #[tokio::test]
    async fn value_of_the_wrong_shape_reads_as_absent() {
        let store = PersistentStore::in_memory();
        store.set("key", &"not a number").await.unwrap();

        assert_eq!(store.get::<u32>("key").await, None);
        assert_eq!(store.get::<String>("key").await.as_deref(), Some("not a number"));
        assert_eq!(store.get::<u32>("missing").await, None);
    }

    #[test]
    fn corrupt_file_fails_to_load_and_names_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = store_path(&dir);
        std::fs::write(&path, "{ not json").unwrap();

        let err = PersistentStore::load(&path).err().expect("corrupt store should not load");
        assert!(format!("{err:#}").contains("middleware.json"), "got: {err:#}");
    }
}
