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
    path::PathBuf,
};
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

#[derive(Default, Serialize, Deserialize)]
pub struct ETagCache {
    #[serde(serialize_with = "ordered_map")]
    pub etags: HashMap<String, String>,
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
            path: path.clone(),
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
