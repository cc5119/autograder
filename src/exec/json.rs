//! Generic JSON persistence: one serializable value at one path, creating
//! parent directories as needed. Shared by fetch records, evaluation
//! results, and anything else that's "one value, serialized as JSON."

use std::path::Path;

use crate::error::{Error, Result};

pub fn write_json<T: serde::Serialize>(path: &Path, value: &T) -> Result<()> {
    if let Some(dir) = path.parent() {
        crate::exec::fs::create_dir_all(dir)?;
    }
    let json = serde_json::to_string_pretty(value).map_err(|e| Error::Other(e.to_string()))?;
    crate::exec::fs::write(path, json)
}

pub fn read_json<T: serde::de::DeserializeOwned>(path: &Path) -> Result<T> {
    let contents = crate::exec::fs::read_to_string(path)?;
    serde_json::from_str(&contents).map_err(|e| Error::Other(e.to_string()))
}
