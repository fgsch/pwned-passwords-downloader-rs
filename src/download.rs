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

use futures::{TryFutureExt as _, TryStreamExt as _};
use reqwest::{StatusCode, header};
use std::{error::Error as _, path::PathBuf, time::Duration};
use thiserror::Error;
use tokio::{fs, io::AsyncWriteExt as _, time::sleep};
use tokio_util::io::StreamReader;

use crate::args::Args;

#[derive(Error, Debug)]
pub enum DownloadError {
    #[error("{operation} operation on {path} failed: {source}")]
    FileOperation {
        operation: &'static str,
        path: String,
        #[source]
        source: std::io::Error,
    },
    #[error("Client error {status_code} for hash {hash}: not retrying")]
    Client {
        hash: String,
        status_code: StatusCode,
    },
    #[error("HTTP error {status_code} for hash {hash}: failed after {retries} retries")]
    Http {
        hash: String,
        status_code: StatusCode,
        retries: usize,
    },
    #[error("Network error {error} for hash {hash}: failed after {retries} retries")]
    Network {
        hash: String,
        error: String,
        retries: usize,
    },
}

#[derive(Debug)]
enum InternalDownloadError {
    Fatal(DownloadError),
    Retriable(DownloadError),
}

impl From<InternalDownloadError> for DownloadError {
    fn from(err: InternalDownloadError) -> Self {
        match err {
            InternalDownloadError::Fatal(e) | InternalDownloadError::Retriable(e) => e,
        }
    }
}

async fn write_hash_to_file(
    response: reqwest::Response,
    final_path: &PathBuf,
) -> Result<(), InternalDownloadError> {
    let part_path = final_path.with_extension("part");

    let mut file = fs::File::create(&part_path).await.map_err(|source| {
        InternalDownloadError::Fatal(DownloadError::FileOperation {
            operation: "create",
            path: part_path.display().to_string(),
            source,
        })
    })?;

    let stream = response.bytes_stream().map_err(std::io::Error::other);
    let mut reader = StreamReader::new(stream);

    match tokio::io::copy(&mut reader, &mut file).await {
        Ok(_) => {
            file.flush()
                .and_then(|_| fs::rename(&part_path, &final_path))
                .or_else(|source| async {
                    _ = fs::remove_file(&part_path).await;
                    Err(InternalDownloadError::Fatal(DownloadError::FileOperation {
                        operation: "rename",
                        path: part_path.display().to_string(),
                        source,
                    }))
                })
                .await?;
            Ok(())
        }
        Err(err) => {
            _ = fs::remove_file(&part_path).await;
            Err(InternalDownloadError::Retriable(
                DownloadError::FileOperation {
                    operation: "read/write",
                    path: final_path.display().to_string(),
                    source: err,
                },
            ))
        }
    }
}

pub async fn download_hash(
    hash: &str,
    client: reqwest::Client,
    etag: Option<&str>,
    args: &Args,
    base_url: &str,
) -> Result<Option<String>, DownloadError> {
    let ext = args.compression.as_str();
    let final_path = args.output_directory.join(hash).with_extension(ext);

    for retry in 0..args.max_retries {
        let mut request = client.get(format!("{base_url}{hash}"));

        if args.resume
            && final_path.exists()
            && let Some(etag_value) = etag
        {
            request = request.header(header::IF_NONE_MATCH, etag_value);
        }

        match request.send().await {
            Ok(response) => {
                let status_code = response.status();
                match status_code {
                    StatusCode::OK => {
                        let etag = response
                            .headers()
                            .get(header::ETAG)
                            .and_then(|s| s.to_str().ok())
                            .map(String::from);

                        match write_hash_to_file(response, &final_path).await {
                            Ok(_) => {
                                return Ok(etag);
                            }
                            Err(InternalDownloadError::Fatal(err)) => {
                                return Err(err);
                            }
                            Err(InternalDownloadError::Retriable(err)) => {
                                if retry == args.max_retries - 1 {
                                    return Err(err);
                                }
                            }
                        }
                    }
                    StatusCode::NOT_MODIFIED if args.resume => {
                        return Ok(None);
                    }
                    status_code if status_code.is_client_error() => {
                        return Err(DownloadError::Client {
                            hash: hash.to_string(),
                            status_code,
                        });
                    }
                    status_code => {
                        if retry == args.max_retries - 1 {
                            return Err(DownloadError::Http {
                                hash: hash.to_string(),
                                status_code,
                                retries: args.max_retries,
                            });
                        }
                    }
                }
            }
            Err(err) => {
                if retry == args.max_retries - 1 {
                    return Err(DownloadError::Network {
                        hash: hash.to_string(),
                        error: err.source().map_or(err.to_string(), |e| e.to_string()),
                        retries: args.max_retries,
                    });
                }
            }
        }

        if retry < args.max_retries - 1 {
            sleep(Duration::from_secs(u64::pow(2, retry as u32))).await;
        }
    }

    unreachable!()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::args::{Args, CompressionFormat};
    use mockito::Server;
    use std::path::PathBuf;
    use tempfile::TempDir;
    use tokio::fs;

    fn create_test_args(output_dir: PathBuf) -> Args {
        Args {
            compression: CompressionFormat::None,
            max_concurrent_requests: 1,
            max_retries: 3,
            resume: false,
            output_directory: output_dir,
            quiet: true,
            user_agent: "test-agent/1.0".to_string(),
        }
    }

    #[tokio::test]
    async fn test_download_hash_success() {
        let mut server = Server::new_async().await;
        let temp_dir = TempDir::new().unwrap();
        let args = create_test_args(temp_dir.path().to_path_buf());

        let mock_data = "test data";
        let mock = server
            .mock("GET", "/range/AAAAA")
            .with_status(200)
            .with_header("etag", "\"test-etag\"")
            .with_body(mock_data)
            .create_async()
            .await;

        let client = reqwest::Client::builder().build().unwrap();

        let base_url = server.url();

        let result =
            download_hash("AAAAA", client, None, &args, &format!("{base_url}/range/")).await;

        mock.assert_async().await;

        assert!(result.is_ok());
        let etag = result.unwrap();
        assert_eq!(etag, Some("\"test-etag\"".to_string()));

        let file_path = temp_dir.path().join("AAAAA");
        assert!(file_path.exists());

        let content = fs::read_to_string(&file_path).await.unwrap();
        assert_eq!(content, mock_data);
    }

    #[tokio::test]
    async fn test_download_hash_not_modified() {
        let mut server = Server::new_async().await;
        let temp_dir = TempDir::new().unwrap();
        let mut args = create_test_args(temp_dir.path().to_path_buf());
        args.resume = true;

        let file_path = temp_dir.path().join("BBBBB");
        fs::write(&file_path, "existing content").await.unwrap();

        let mock = server
            .mock("GET", "/range/BBBBB")
            .match_header("if-none-match", "\"existing-etag\"")
            .with_status(304)
            .create_async()
            .await;

        let client = reqwest::Client::new();
        let base_url = server.url();

        let result = download_hash(
            "BBBBB",
            client,
            Some("\"existing-etag\""),
            &args,
            &format!("{base_url}/range/"),
        )
        .await;

        mock.assert_async().await;

        assert!(result.is_ok());
        let etag = result.unwrap();
        assert_eq!(etag, None);
    }

    #[tokio::test]
    async fn test_download_hash_client_error() {
        let mut server = Server::new_async().await;
        let temp_dir = TempDir::new().unwrap();
        let args = create_test_args(temp_dir.path().to_path_buf());

        let mock = server
            .mock("GET", "/range/CCCCC")
            .with_status(404)
            .create_async()
            .await;

        let client = reqwest::Client::new();
        let base_url = server.url();

        let result =
            download_hash("CCCCC", client, None, &args, &format!("{base_url}/range/")).await;

        mock.assert_async().await;

        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(matches!(err, DownloadError::Client { .. }));

        if let DownloadError::Client { hash, status_code } = err {
            assert_eq!(hash, "CCCCC");
            assert_eq!(status_code, StatusCode::NOT_FOUND);
        }
    }

    #[tokio::test(start_paused = true)]
    async fn test_download_hash_server_error_with_retry() {
        let mut server = Server::new_async().await;
        let temp_dir = TempDir::new().unwrap();
        let mut args = create_test_args(temp_dir.path().to_path_buf());
        args.max_retries = 5;

        let mock = server
            .mock("GET", "/range/DDDDD")
            .with_status(500)
            .expect(5)
            .create_async()
            .await;

        let client = reqwest::Client::new();
        let base_url = server.url();

        let result =
            download_hash("DDDDD", client, None, &args, &format!("{base_url}/range/")).await;

        mock.assert_async().await;

        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(matches!(err, DownloadError::Http { .. }));

        if let DownloadError::Http {
            hash,
            status_code,
            retries,
        } = err
        {
            assert_eq!(hash, "DDDDD");
            assert_eq!(status_code, StatusCode::INTERNAL_SERVER_ERROR);
            assert_eq!(retries, 5);
        }
    }

    #[tokio::test]
    async fn test_download_hash_with_compression() {
        let mut server = Server::new_async().await;
        let temp_dir = TempDir::new().unwrap();
        let mut args = create_test_args(temp_dir.path().to_path_buf());
        args.compression = CompressionFormat::Gzip;

        let mock_data = "test compressed data";
        let mock = server
            .mock("GET", "/range/EEEEE")
            .with_status(200)
            .with_header("etag", "\"gzip-etag\"")
            .with_body(mock_data)
            .create_async()
            .await;

        let client = reqwest::Client::new();
        let base_url = server.url();

        let result =
            download_hash("EEEEE", client, None, &args, &format!("{base_url}/range/")).await;

        mock.assert_async().await;

        assert!(result.is_ok());

        let file_path = temp_dir.path().join("EEEEE.gz");
        assert!(file_path.exists());

        let content = fs::read_to_string(&file_path).await.unwrap();
        assert_eq!(content, mock_data);
    }

    #[tokio::test]
    async fn test_write_hash_to_file_success() {
        let temp_dir = TempDir::new().unwrap();
        let final_path = temp_dir.path().join("FFFFF");

        let mock_body = "test content";
        let response = reqwest::Response::from(
            http::Response::builder()
                .status(200)
                .body(mock_body)
                .unwrap(),
        );

        let result = write_hash_to_file(response, &final_path).await;

        assert!(result.is_ok());
        assert!(final_path.exists());

        let content = fs::read_to_string(&final_path).await.unwrap();
        assert_eq!(content, mock_body);
    }
}
