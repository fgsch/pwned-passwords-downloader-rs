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
use std::{error::Error as _, time::Duration};
use thiserror::Error;
use tokio::time::sleep;
use tokio_util::bytes::Bytes;

use crate::args::Args;

pub const HIBP_BASE_URL: &str = "https://api.pwnedpasswords.com/range/";

#[derive(Error, Debug)]
pub enum DownloadError {
    #[allow(dead_code)]
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

/// Result of a successful download operation
#[derive(Debug)]
pub enum DownloadResult {
    /// Successfully downloaded new data with optional ETag
    Success { etag: Option<String>, data: Bytes },
    /// Resource not modified (304 response)
    NotModified,
}

/// Internal error classification for retry logic
#[derive(Debug)]
enum InternalDownloadError {
    /// Fatal error that should not be retried
    Fatal(DownloadError),
    /// Retriable error that may succeed on retry
    Retriable(DownloadError),
}

impl From<InternalDownloadError> for DownloadError {
    fn from(err: InternalDownloadError) -> Self {
        match err {
            InternalDownloadError::Fatal(e) | InternalDownloadError::Retriable(e) => e,
        }
    }
}

/// Returns InternalDownloadError to distinguish fatal vs retriable errors
async fn download_hash_once(
    hash: &str,
    client: &reqwest::Client,
    etag: Option<&str>,
    args: &Args,
    base_url: &str,
) -> Result<DownloadResult, InternalDownloadError> {
    let mut request = client.get(format!("{base_url}{hash}"));

    // Add If-None-Match header if we have an etag and (resuming OR syncing)
    if (args.resume || args.sync.is_some())
        && let Some(etag_value) = etag
    {
        request = request.header(header::IF_NONE_MATCH, etag_value);
    }

    match request.send().await {
        Ok(response) => {
            let status_code = response.status();
            match status_code {
                StatusCode::OK => {
                    let new_etag = response
                        .headers()
                        .get(header::ETAG)
                        .and_then(|s| s.to_str().ok())
                        .map(String::from);

                    // Download the entire response body
                    match response.bytes().await {
                        Ok(data) => Ok(DownloadResult::Success {
                            etag: new_etag,
                            data,
                        }),
                        Err(err) => Err(InternalDownloadError::Retriable(DownloadError::Network {
                            hash: hash.to_string(),
                            error: err.to_string(),
                            retries: 0,
                        })),
                    }
                }
                StatusCode::NOT_MODIFIED if args.resume || args.sync.is_some() => {
                    Ok(DownloadResult::NotModified)
                }
                status_code if status_code.is_client_error() => {
                    Err(InternalDownloadError::Fatal(DownloadError::Client {
                        hash: hash.to_string(),
                        status_code,
                    }))
                }
                status_code => Err(InternalDownloadError::Retriable(DownloadError::Http {
                    hash: hash.to_string(),
                    status_code,
                    retries: 0,
                })),
            }
        }
        Err(err) => Err(InternalDownloadError::Retriable(DownloadError::Network {
            hash: hash.to_string(),
            error: err
                .source()
                .map(|e| e.to_string())
                .unwrap_or_else(|| err.to_string()),
            retries: 0,
        })),
    }
}

pub async fn download_hash(
    hash: &str,
    client: &reqwest::Client,
    etag: Option<&str>,
    args: &Args,
) -> Result<DownloadResult, DownloadError> {
    for retry in 1..=args.max_retries {
        match download_hash_once(hash, client, etag, args, HIBP_BASE_URL).await {
            Ok(result) => return Ok(result),
            Err(InternalDownloadError::Fatal(err)) => return Err(err),
            Err(InternalDownloadError::Retriable(err)) => {
                if retry == args.max_retries {
                    return Err(err);
                }
                // Exponential backoff before retry
                sleep(Duration::from_secs(u64::pow(2, (retry - 1) as u32))).await;
            }
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

    fn create_test_args(output_dir: PathBuf) -> Args {
        Args {
            compression: CompressionFormat::None,
            max_concurrent_requests: 1,
            max_retries: 3,
            resume: false,
            sync: None,
            output_directory: output_dir,
            quiet: true,
            combine: false,
            user_agent: "test-agent/1.0".to_string(),
        }
    }

    /// Test helper that allows specifying a custom base URL for mocking
    async fn download_hash_with_base_url(
        hash: &str,
        client: &reqwest::Client,
        etag: Option<&str>,
        args: &Args,
        base_url: &str,
    ) -> Result<DownloadResult, DownloadError> {
        for retry in 1..=args.max_retries {
            match download_hash_once(hash, client, etag, args, base_url).await {
                Ok(result) => return Ok(result),
                Err(InternalDownloadError::Fatal(err)) => return Err(err),
                Err(InternalDownloadError::Retriable(mut err)) => {
                    if retry == args.max_retries {
                        // Update retry count in error before returning
                        match &mut err {
                            DownloadError::Http { retries, .. } => *retries = args.max_retries,
                            DownloadError::Network { retries, .. } => *retries = args.max_retries,
                            _ => {}
                        }
                        return Err(err);
                    }
                    // Exponential backoff before retry
                    sleep(Duration::from_secs(u64::pow(2, (retry - 1) as u32))).await;
                }
            }
        }
        unreachable!()
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

        let result = download_hash_with_base_url(
            "AAAAA",
            &client,
            None,
            &args,
            &format!("{base_url}/range/"),
        )
        .await;

        mock.assert_async().await;

        assert!(result.is_ok());
        match result.unwrap() {
            DownloadResult::Success { etag, data } => {
                assert_eq!(etag, Some("\"test-etag\"".to_string()));
                assert_eq!(data, mock_data.as_bytes());
            }
            _ => panic!("Expected Success result"),
        }
    }

    #[tokio::test]
    async fn test_download_hash_with_data() {
        let mut server = Server::new_async().await;
        let temp_dir = TempDir::new().unwrap();
        let args = create_test_args(temp_dir.path().to_path_buf());

        let mock_data = "000000000000000000000000000000000AA:1\r\n";
        let mock = server
            .mock("GET", "/range/00000")
            .with_status(200)
            .with_header("etag", "\"test-etag-2\"")
            .with_body(mock_data)
            .create_async()
            .await;

        let client = reqwest::Client::builder().build().unwrap();
        let base_url = server.url();

        let result = download_hash_with_base_url(
            "00000",
            &client,
            None,
            &args,
            &format!("{base_url}/range/"),
        )
        .await;

        mock.assert_async().await;

        assert!(result.is_ok());
        match result.unwrap() {
            DownloadResult::Success { etag, data } => {
                assert_eq!(etag, Some("\"test-etag-2\"".to_string()));
                assert_eq!(data, mock_data.as_bytes());
            }
            _ => panic!("Expected Success result"),
        }
    }

    #[tokio::test]
    async fn test_download_hash_not_modified() {
        let mut server = Server::new_async().await;
        let temp_dir = TempDir::new().unwrap();
        let mut args = create_test_args(temp_dir.path().to_path_buf());
        args.resume = true;

        let mock = server
            .mock("GET", "/range/BBBBB")
            .match_header("if-none-match", "\"existing-etag\"")
            .with_status(304)
            .create_async()
            .await;

        let client = reqwest::Client::new();
        let base_url = server.url();

        let result = download_hash_with_base_url(
            "BBBBB",
            &client,
            Some("\"existing-etag\""),
            &args,
            &format!("{base_url}/range/"),
        )
        .await;

        mock.assert_async().await;

        assert!(result.is_ok());
        assert!(matches!(result.unwrap(), DownloadResult::NotModified));
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

        let result = download_hash_with_base_url(
            "CCCCC",
            &client,
            None,
            &args,
            &format!("{base_url}/range/"),
        )
        .await;

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

        let result = download_hash_with_base_url(
            "DDDDD",
            &client,
            None,
            &args,
            &format!("{base_url}/range/"),
        )
        .await;

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
    async fn test_download_hash_network_error() {
        let temp_dir = TempDir::new().unwrap();
        let args = create_test_args(temp_dir.path().to_path_buf());

        // Use an invalid URL that will cause a network error
        let client = reqwest::Client::new();

        // Override with a test helper that uses an invalid base URL
        let result = download_hash_with_base_url(
            "FFFFF",
            &client,
            None,
            &args,
            "http://invalid-domain-that-does-not-exist-12345.com/",
        )
        .await;

        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(matches!(err, DownloadError::Network { .. }));
    }

    #[tokio::test]
    async fn test_download_hash_sync_not_modified() {
        let mut server = Server::new_async().await;
        let temp_dir = TempDir::new().unwrap();
        let mut args = create_test_args(temp_dir.path().to_path_buf());

        // Enable sync mode with a path (content doesn't matter for this test)
        args.sync = Some(temp_dir.path().join("sync_cache.json"));

        let mock = server
            .mock("GET", "/range/SSSSS")
            .match_header("if-none-match", "\"sync-etag\"")
            .with_status(304)
            .create_async()
            .await;

        let client = reqwest::Client::new();
        let base_url = server.url();

        let result = download_hash_with_base_url(
            "SSSSS",
            &client,
            Some("\"sync-etag\""),
            &args,
            &format!("{base_url}/range/"),
        )
        .await;

        mock.assert_async().await;

        assert!(result.is_ok());
        assert!(matches!(result.unwrap(), DownloadResult::NotModified));
    }

    #[tokio::test]
    async fn test_download_hash_sync_modified() {
        let mut server = Server::new_async().await;
        let temp_dir = TempDir::new().unwrap();
        let mut args = create_test_args(temp_dir.path().to_path_buf());

        // Enable sync mode
        args.sync = Some(temp_dir.path().join("sync_cache.json"));

        let new_data = "updated data";
        let mock = server
            .mock("GET", "/range/TTTTT")
            .match_header("if-none-match", "\"old-etag\"")
            .with_status(200)
            .with_header("etag", "\"new-etag\"")
            .with_body(new_data)
            .create_async()
            .await;

        let client = reqwest::Client::new();
        let base_url = server.url();

        let result = download_hash_with_base_url(
            "TTTTT",
            &client,
            Some("\"old-etag\""),
            &args,
            &format!("{base_url}/range/"),
        )
        .await;

        mock.assert_async().await;

        assert!(result.is_ok());
        match result.unwrap() {
            DownloadResult::Success { etag, data } => {
                assert_eq!(etag, Some("\"new-etag\"".to_string()));
                assert_eq!(data, new_data.as_bytes());
            }
            _ => panic!("Expected Success result"),
        }
    }
}
