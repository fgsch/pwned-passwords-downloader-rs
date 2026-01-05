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

use reqwest::{StatusCode, header};
use std::{error::Error as _, sync::Arc, time::Duration};
use thiserror::Error;
use tokio::{sync::RwLock, time::sleep};
use tokio_util::sync::CancellationToken;

use crate::args::{Args, HashMode};
use crate::etag::ETagCache;
use crate::writer::{HashWriter, WriteError};

// Maximum number of seconds we will sleep on retry.
const MAX_BACKOFF_SECS: u64 = 60;

#[derive(Error, Debug)]
pub enum DownloadError {
    #[error(transparent)]
    FileOperation(#[from] WriteError),
    #[error("Downloads for hash {hash} cancelled")]
    Cancelled { hash: String },
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

pub async fn download_hash(
    client: reqwest::Client,
    args: &Args,
    base_url: &str,
    hash: &str,
    etag: Option<&str>,
    writer: &dyn HashWriter,
) -> Result<Option<String>, DownloadError> {
    for retry in 0..args.max_retries {
        let mut request = client.get(format!("{base_url}{hash}"));

        if matches!(args.hash_mode, HashMode::Ntlm) {
            request = request.query(&[("mode", "ntlm")]);
        }

        if let Some(etag) = etag {
            request = request.header(header::IF_NONE_MATCH, etag);
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

                        match writer.write_response(hash, response).await {
                            Ok(_) => {
                                return Ok(etag);
                            }
                            Err(err) => {
                                if retry == args.max_retries - 1 || !err.is_retriable() {
                                    return Err(DownloadError::FileOperation(err));
                                }
                            }
                        }
                    }
                    StatusCode::NOT_MODIFIED if etag.is_some() => {
                        return Ok(None);
                    }
                    status_code
                        if status_code.is_client_error()
                            && status_code != StatusCode::TOO_MANY_REQUESTS =>
                    {
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
                        error: err
                            .source()
                            .map_or_else(|| err.to_string(), |e| e.to_string()),
                        retries: args.max_retries,
                    });
                }
            }
        }

        if retry < args.max_retries - 1 {
            let delay = 2u64.saturating_pow(retry as u32).min(MAX_BACKOFF_SECS);
            let jitter = rand::random_range(delay / 2..=delay).max(1);
            sleep(Duration::from_secs(jitter)).await;
        }
    }

    unreachable!()
}

pub async fn process_single_hash(
    client: reqwest::Client,
    args: Arc<Args>,
    base_url: &str,
    hash: String,
    etag_cache: Arc<RwLock<ETagCache>>,
    token: CancellationToken,
    writer: Arc<dyn HashWriter>,
) -> (String, Result<Option<String>, DownloadError>) {
    let etag =
        if args.incremental && (args.ignore_missing_hash_file || writer.hash_exists(&hash).await) {
            etag_cache.read().await.etags.get(&hash).cloned()
        } else {
            None
        };
    let result = tokio::select! {
        res = download_hash(client, &args, base_url, &hash, etag.as_deref(), writer.as_ref()) => res,
        _ = token.cancelled() => Err(DownloadError::Cancelled { hash: hash.clone() }),
    };
    (hash, result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        args::{CompressionFormat, HashMode, create_test_args},
        writer::create_test_writer,
    };
    use mockito::{Matcher, Server};
    use tempfile::TempDir;
    use tokio::fs;

    #[tokio::test]
    async fn download_hash_success() {
        let mut server = Server::new_async().await;
        let temp_dir = TempDir::new().unwrap();
        let args = create_test_args(temp_dir.path().to_path_buf());
        let writer = create_test_writer(&args);

        let client = reqwest::Client::new();

        let mock_data = "test data";
        let mock = server
            .mock("GET", "/range/AAAAA")
            .with_status(200)
            .with_header("etag", "\"test-etag\"")
            .with_body(mock_data)
            .create_async()
            .await;
        let base_url = format!("{}/range/", server.url());

        let result = download_hash(client, &args, &base_url, "AAAAA", None, &writer).await;

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
    async fn download_hash_ntlm_mode() {
        let mut server = Server::new_async().await;
        let temp_dir = TempDir::new().unwrap();
        let mut args = create_test_args(temp_dir.path().to_path_buf());
        args.hash_mode = HashMode::Ntlm;
        let writer = create_test_writer(&args);

        let client = reqwest::Client::new();

        let mock = server
            .mock("GET", "/range/HHHHH")
            .match_query(Matcher::UrlEncoded("mode".to_string(), "ntlm".to_string()))
            .with_status(200)
            .with_body("ntlm data")
            .create_async()
            .await;
        let base_url = format!("{}/range/", server.url());

        let result = download_hash(client, &args, &base_url, "HHHHH", None, &writer).await;

        mock.assert_async().await;

        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn download_hash_not_modified() {
        let mut server = Server::new_async().await;
        let temp_dir = TempDir::new().unwrap();
        let mut args = create_test_args(temp_dir.path().to_path_buf());
        args.incremental = true;
        let writer = create_test_writer(&args);

        let file_path = temp_dir.path().join("BBBBB");
        fs::write(&file_path, "existing content").await.unwrap();

        let client = reqwest::Client::new();

        let mock = server
            .mock("GET", "/range/BBBBB")
            .match_header("if-none-match", "\"existing-etag\"")
            .with_status(304)
            .create_async()
            .await;
        let base_url = format!("{}/range/", server.url());

        let result = download_hash(
            client,
            &args,
            &base_url,
            "BBBBB",
            Some("\"existing-etag\""),
            &writer,
        )
        .await;

        mock.assert_async().await;

        assert!(result.is_ok());
        let etag = result.unwrap();
        assert_eq!(etag, None);
    }

    #[tokio::test]
    async fn download_hash_with_ignore_missing_hash_file() {
        let mut server = Server::new_async().await;
        let temp_dir = TempDir::new().unwrap();
        let mut args = create_test_args(temp_dir.path().to_path_buf());
        args.incremental = true;
        args.ignore_missing_hash_file = true;
        let writer = create_test_writer(&args);

        let client = reqwest::Client::new();

        let mock = server
            .mock("GET", "/range/GGGGG")
            .match_header("if-none-match", "\"cached-etag\"")
            .with_status(304)
            .create_async()
            .await;
        let base_url = format!("{}/range/", server.url());

        let result = download_hash(
            client,
            &args,
            &base_url,
            "GGGGG",
            Some("\"cached-etag\""),
            &writer,
        )
        .await;

        mock.assert_async().await;

        assert!(result.is_ok());
        assert!(result.unwrap().is_none());
    }

    #[tokio::test]
    async fn download_hash_client_error() {
        let mut server = Server::new_async().await;
        let temp_dir = TempDir::new().unwrap();
        let args = create_test_args(temp_dir.path().to_path_buf());
        let writer = create_test_writer(&args);

        let client = reqwest::Client::new();

        let mock = server
            .mock("GET", "/range/CCCCC")
            .with_status(404)
            .create_async()
            .await;
        let base_url = format!("{}/range/", server.url());

        let result = download_hash(client, &args, &base_url, "CCCCC", None, &writer).await;

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
    async fn download_hash_server_error_with_retry() {
        let mut server = Server::new_async().await;
        let temp_dir = TempDir::new().unwrap();
        let mut args = create_test_args(temp_dir.path().to_path_buf());
        args.max_retries = 5;
        let writer = create_test_writer(&args);

        let client = reqwest::Client::new();

        let mock = server
            .mock("GET", "/range/DDDDD")
            .with_status(500)
            .expect(5)
            .create_async()
            .await;
        let base_url = format!("{}/range/", server.url());

        let result = download_hash(client, &args, &base_url, "DDDDD", None, &writer).await;

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
    async fn download_hash_with_compression() {
        let mut server = Server::new_async().await;
        let temp_dir = TempDir::new().unwrap();
        let mut args = create_test_args(temp_dir.path().to_path_buf());
        args.compression = CompressionFormat::Gzip;
        let writer = create_test_writer(&args);

        let client = reqwest::Client::new();

        let mock_data = "test compressed data";
        let mock = server
            .mock("GET", "/range/EEEEE")
            .with_status(200)
            .with_header("etag", "\"gzip-etag\"")
            .with_body(mock_data)
            .create_async()
            .await;
        let base_url = format!("{}/range/", server.url());

        let result = download_hash(client, &args, &base_url, "EEEEE", None, &writer).await;

        mock.assert_async().await;

        assert!(result.is_ok());

        let file_path = temp_dir.path().join("EEEEE.gz");
        assert!(file_path.exists());

        let content = fs::read_to_string(&file_path).await.unwrap();
        assert_eq!(content, mock_data);
    }
}
