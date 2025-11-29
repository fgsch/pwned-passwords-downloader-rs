// Copyright (c) 2024-2025 Federico G. Schwindt <fgsch@lodoss.net>
//
// Permission is hereby granted, free of charge, to any person obtaining a copy
// of this software and associated documentation files (the "Software"), to deal
// in the Software without restriction, including without limitation the rights
// to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
// copies of the Software, and to permit persons to whom the Software is
// furnished to do so, subject to the following conditions:
//
// The above copyright notice and this permission notice shall be included in all
// copies or substantial portions of the Software.
//
// THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
// IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
// FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
// AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
// LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
// OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
// SOFTWARE.

use serde::{Deserialize, Serialize, Serializer};
use std::{
    collections::{BTreeMap, HashMap},
    path::{Path, PathBuf},
};
use thiserror::Error;
use tokio::fs;

use crate::args::HashMode;

#[derive(Error, Debug)]
pub enum CacheError {
    #[error("Failed to parse ETag cache from {path}: {source}")]
    Parse {
        path: String,
        #[source]
        source: serde_json::Error,
    },
    #[error(
        "ETag cache at {path} created for mode {cached_mode}; current run requires {current_mode}"
    )]
    ModeMismatch {
        path: String,
        cached_mode: &'static str,
        current_mode: &'static str,
    },
    #[error("Failed to read ETag cache from {path}: {source}")]
    Read {
        path: String,
        #[source]
        source: std::io::Error,
    },
    #[error("Failed to serialize ETag cache: {0}")]
    Serialize(#[from] serde_json::Error),
    #[error("Failed to write ETag cache to {path}: {source}")]
    Write {
        path: String,
        #[source]
        source: std::io::Error,
    },
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct ETagCache {
    #[serde(serialize_with = "ordered_map")]
    pub etags: HashMap<String, String>,
    #[serde(default = "HashMode::default")]
    pub mode: HashMode,
    #[serde(skip)]
    pub path: PathBuf,
}

fn ordered_map<S: Serializer>(
    value: &HashMap<String, String>,
    serializer: S,
) -> Result<S::Ok, S::Error> {
    let ordered: BTreeMap<_, _> = value.iter().collect();
    ordered.serialize(serializer)
}

impl ETagCache {
    pub async fn load(path: &Path, mode: HashMode, incremental: bool) -> Result<Self, CacheError> {
        let etags = match fs::read_to_string(path).await {
            Ok(content) if incremental => serde_json::from_str::<ETagCache>(&content)
                .map_err(|source| CacheError::Parse {
                    path: path.display().to_string(),
                    source,
                })
                .and_then(|cache| {
                    if cache.mode == mode {
                        Ok(cache.etags)
                    } else {
                        Err(CacheError::ModeMismatch {
                            path: path.display().to_string(),
                            cached_mode: cache.mode.as_str(),
                            current_mode: mode.as_str(),
                        })
                    }
                })?,
            Ok(_) => {
                tracing::warn!(
                    "Ignoring existing ETag cache at {}; incremental mode disabled",
                    path.display()
                );
                HashMap::new()
            }
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => HashMap::new(),
            Err(err) => {
                return Err(CacheError::Read {
                    path: path.display().to_string(),
                    source: err,
                });
            }
        };
        Ok(Self {
            etags,
            mode,
            path: path.to_path_buf(),
        })
    }

    pub async fn save(&self) -> Result<(), CacheError> {
        let content = serde_json::to_string_pretty(self)?;
        fs::write(&self.path, content)
            .await
            .map_err(|source| CacheError::Write {
                path: self.path.display().to_string(),
                source,
            })?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use tempfile::TempDir;

    #[tokio::test]
    async fn load_accepts_matching_mode() {
        let temp_dir = TempDir::new().unwrap();
        let cache_path = temp_dir.path().join(".etag_cache.json");
        let cache = json!({
            "etags": { "ABCDE": "\"etag\"" },
            "mode": HashMode::Ntlm
        });
        fs::write(&cache_path, cache.to_string()).await.unwrap();

        let cache = ETagCache::load(&cache_path, HashMode::Ntlm, true)
            .await
            .unwrap();

        assert_eq!(cache.mode, HashMode::Ntlm);
        assert_eq!(
            cache.etags.get("ABCDE").map(String::as_str),
            Some("\"etag\"")
        );
    }

    #[tokio::test]
    async fn load_rejects_mode_mismatch() {
        let temp_dir = TempDir::new().unwrap();
        let cache_path = temp_dir.path().join(".etag_cache.json");
        let cache = json!({
            "etags": { "ABCDE": "\"etag\"" },
            "mode": HashMode::Sha1
        });
        fs::write(&cache_path, cache.to_string()).await.unwrap();

        let err = ETagCache::load(&cache_path, HashMode::Ntlm, true)
            .await
            .unwrap_err();

        if let CacheError::ModeMismatch {
            cached_mode,
            current_mode,
            ..
        } = err
        {
            assert_eq!(cached_mode, "sha1");
            assert_eq!(current_mode, "ntlm");
        } else {
            panic!("Expected ModeMismatch error");
        }
    }

    #[tokio::test]
    async fn load_ignores_mode_mismatch() {
        let temp_dir = TempDir::new().unwrap();
        let cache_path = temp_dir.path().join(".etag_cache.json");
        let cache = json!({
            "etags": { "ABCDE": "\"etag\"" },
            "mode": HashMode::Sha1
        });
        fs::write(&cache_path, cache.to_string()).await.unwrap();

        let cache = ETagCache::load(&cache_path, HashMode::Ntlm, false)
            .await
            .unwrap();

        assert_eq!(cache.mode, HashMode::Ntlm);
        assert!(cache.etags.is_empty());
    }
}
