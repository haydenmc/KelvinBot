use anyhow::Result;
use serde::{Serialize, de::DeserializeOwned};
use serde_json::Value;
use std::{collections::HashMap, path::PathBuf};
use tokio::sync::Mutex;

/// A persistent, JSON-backed key-value store that saves to disk on every write.
///
/// Loaded once at startup; each `set` call atomically updates the in-memory map
/// and rewrites the backing file. Use `in_memory` in tests to skip disk I/O.
pub struct PersistentStore {
    path: Option<PathBuf>,
    data: Mutex<HashMap<String, Value>>,
}

impl PersistentStore {
    /// Load the store from `path`, creating an empty store if the file does not exist.
    pub fn load(path: impl Into<PathBuf>) -> Result<Self> {
        let path = path.into();
        let data = if path.exists() {
            let content = std::fs::read_to_string(&path)?;
            serde_json::from_str(&content)?
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
    /// Returns `None` if the key is absent or deserialization fails.
    pub async fn get<T: DeserializeOwned>(&self, key: &str) -> Option<T> {
        let data = self.data.lock().await;
        data.get(key).and_then(|v| serde_json::from_value(v.clone()).ok())
    }

    /// Insert or update `key`, then persist the whole store to disk (if a path is configured).
    pub async fn set<T: Serialize>(&self, key: &str, value: &T) -> Result<()> {
        let serialized = serde_json::to_value(value)?;
        let mut data = self.data.lock().await;
        data.insert(key.to_string(), serialized);
        if let Some(path) = &self.path {
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            let content = serde_json::to_string_pretty(&*data)?;
            std::fs::write(path, content)?;
        }
        Ok(())
    }
}
