// Copyright (c) 2024-2025 Federico G. Schwindt

use serde::{Deserialize, Serialize};
use std::{collections::HashMap, path::PathBuf};
use thiserror::Error;
use tokio::fs;

#[derive(Error, Debug)]
pub enum CacheError {
    #[error("Failed to parse ETag cache from {path}: {source}")]
    Parse {
        path: String,
        #[source]
        source: serde_json::Error,
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

#[derive(Default, Serialize, Deserialize)]
pub struct ETagCache {
    pub etags: HashMap<String, String>,
    #[serde(skip)]
    pub path: PathBuf,
}

impl ETagCache {
    pub async fn load(path: &PathBuf) -> Result<Self, CacheError> {
        let etags = match fs::read_to_string(path).await {
            Ok(content) => {
                serde_json::from_str::<ETagCache>(&content)
                    .map_err(|source| CacheError::Parse {
                        path: path.display().to_string(),
                        source,
                    })?
                    .etags
            }
            Err(_) => HashMap::new(),
        };
        Ok(Self {
            etags,
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
