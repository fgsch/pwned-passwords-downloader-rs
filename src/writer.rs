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

use async_trait::async_trait;
use futures::{TryFutureExt as _, TryStreamExt as _};
use std::{
    io::ErrorKind,
    path::{Path, PathBuf},
};
use thiserror::Error;
use tokio::{fs, io::AsyncWriteExt as _};
use tokio_util::io::StreamReader;

#[derive(Error, Debug)]
pub enum WriteError {
    #[error("create on {path} failed: {source}")]
    Create {
        path: String,
        #[source]
        source: std::io::Error,
    },
    #[error("rename from {path} to {target} failed: {source}")]
    Rename {
        path: String,
        target: String,
        #[source]
        source: std::io::Error,
    },
    #[error("read/write on {path} failed: {source}")]
    ReadWrite {
        path: String,
        #[source]
        source: std::io::Error,
    },
}

impl WriteError {
    pub fn is_retriable(&self) -> bool {
        matches!(self, WriteError::ReadWrite { .. })
    }
}

struct TempFileGuard<'a> {
    path: &'a Path,
    delete_on_drop: bool,
}

impl Drop for TempFileGuard<'_> {
    fn drop(&mut self) {
        if self.delete_on_drop {
            let _ = std::fs::remove_file(self.path);
        }
    }
}

#[async_trait]
pub trait HashWriter: Send + Sync {
    async fn hash_exists(&self, hash: &str) -> bool;
    async fn write_response(
        &self,
        hash: &str,
        response: reqwest::Response,
    ) -> Result<(), WriteError>;
}

#[derive(Clone, Debug)]
pub struct HashFileWriter {
    output_directory: PathBuf,
    extension: String,
}

impl HashFileWriter {
    pub fn new(output_directory: PathBuf, extension: String) -> Self {
        Self {
            output_directory,
            extension,
        }
    }

    fn hash_path(&self, hash: &str) -> PathBuf {
        self.output_directory
            .join(hash)
            .with_extension(&self.extension)
    }
}

#[async_trait]
impl HashWriter for HashFileWriter {
    async fn hash_exists(&self, hash: &str) -> bool {
        let final_path = self.hash_path(hash);
        fs::try_exists(&final_path).await.unwrap_or(false)
    }

    async fn write_response(
        &self,
        hash: &str,
        response: reqwest::Response,
    ) -> Result<(), WriteError> {
        let final_path = self.hash_path(hash);
        let part_path = final_path.with_extension("part");

        let mut guard = TempFileGuard {
            path: part_path.as_path(),
            delete_on_drop: true,
        };

        // Use blocking creation to ensure the file is created _before_ dropping the
        // TempFileGuard should we cancel the future.
        let file = std::fs::File::create(&part_path).map_err(|source| WriteError::Create {
            path: part_path.display().to_string(),
            source,
        })?;
        let mut file = fs::File::from_std(file);

        let stream = response.bytes_stream().map_err(std::io::Error::other);
        let mut reader = StreamReader::new(stream);

        match tokio::io::copy(&mut reader, &mut file).await {
            Ok(_) => {
                let final_path_buf = final_path.to_path_buf();
                file.shutdown()
                    .and_then(|_| {
                        let part_path = part_path.clone();
                        async move {
                            match fs::remove_file(&final_path_buf).await {
                                Ok(_) => {}
                                Err(err) if err.kind() == ErrorKind::NotFound => {}
                                Err(err) => return Err(err),
                            }
                            fs::rename(&part_path, &final_path_buf).await
                        }
                    })
                    .await
                    .map_err(|source| WriteError::Rename {
                        path: part_path.display().to_string(),
                        target: final_path.display().to_string(),
                        source,
                    })?;
                guard.delete_on_drop = false;
                Ok(())
            }
            Err(err) => Err(WriteError::ReadWrite {
                path: final_path.display().to_string(),
                source: err,
            }),
        }
    }
}

#[cfg(test)]
pub fn create_test_writer(args: &crate::args::Args) -> HashFileWriter {
    HashFileWriter::new(
        args.output_directory.clone(),
        args.compression.as_str().to_string(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::args::create_test_args;
    use tempfile::TempDir;
    use tokio::fs;

    #[tokio::test]
    async fn write_hash_to_file_success() {
        let temp_dir = TempDir::new().unwrap();
        let final_path = temp_dir.path().join("FFFFF");
        let args = create_test_args(temp_dir.path().to_path_buf());
        let writer = create_test_writer(&args);

        let mock_body = "test content";
        let response = reqwest::Response::from(
            http::Response::builder()
                .status(200)
                .body(mock_body)
                .unwrap(),
        );

        let result = writer.write_response("FFFFF", response).await;

        assert!(result.is_ok());
        assert!(final_path.exists());

        let content = fs::read_to_string(&final_path).await.unwrap();
        assert_eq!(content, mock_body);
    }
}
