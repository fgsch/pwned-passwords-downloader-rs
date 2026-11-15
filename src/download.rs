// Copyright (c) 2024-2026 Federico G. Schwindt <fgsch@lodoss.net>
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
use std::{
    collections::HashMap,
    error::Error as _,
    time::{Duration, Instant},
};
use thiserror::Error;
use tokio::time::sleep;
use tokio_util::sync::CancellationToken;

use crate::args::{Args, HashMode};
use crate::stats::RunStats;
use crate::writer::{HashWriter, WriteError};

// Maximum number of seconds we will sleep on retry.
const MAX_BACKOFF_SECS: u64 = 60;

#[derive(Error, Debug)]
pub enum DownloadError {
    #[error("File operation failed after {retries} retries: {source}")]
    FileOperation {
        retries: usize,
        #[source]
        source: WriteError,
    },
    #[error("Downloads for hash {hash} cancelled")]
    Cancelled { hash: String },
    #[error("Client error {status_code} for hash {hash} after {retries} retries: not retrying")]
    Client {
        hash: String,
        status_code: StatusCode,
        retries: usize,
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

impl DownloadError {
    pub fn retries(&self) -> Option<u64> {
        match self {
            Self::FileOperation { retries, .. }
            | Self::Client { retries, .. }
            | Self::Http { retries, .. }
            | Self::Network { retries, .. } => Some(*retries as u64),
            Self::Cancelled { .. } => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DownloadOutcome {
    pub status: DownloadStatus,
    pub retries_used: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DownloadStatus {
    Downloaded { etag: Option<String> },
    NotModified,
}

async fn write_response_if_modified(
    hash: &str,
    response: reqwest::Response,
    etag: Option<&str>,
    writer: &dyn HashWriter,
    stats: &RunStats,
) -> Result<DownloadStatus, WriteError> {
    let response_etag = response
        .headers()
        .get(header::ETAG)
        .and_then(|s| s.to_str().ok());

    // If-None-Match uses weak comparison, even for redundant 200s.
    if etag.zip(response_etag).is_some_and(|(sent, received)| {
        sent.strip_prefix("W/").unwrap_or(sent) == received.strip_prefix("W/").unwrap_or(received)
    }) {
        return Ok(DownloadStatus::NotModified);
    }

    let etag = response_etag.map(String::from);
    let metrics = writer.write_response(hash, response).await?;
    stats.record_write(metrics);
    Ok(DownloadStatus::Downloaded { etag })
}

pub(crate) struct DownloadContext<'a> {
    pub client: &'a reqwest::Client,
    pub args: &'a Args,
    pub base_url: &'a str,
    pub writer: &'a dyn HashWriter,
    pub stats: &'a RunStats,
}

impl DownloadContext<'_> {
    async fn send_request(
        &self,
        url: &str,
        etag: Option<&str>,
    ) -> reqwest::Result<reqwest::Response> {
        let mut request = self.client.get(url);
        if matches!(self.args.hash_mode, HashMode::Ntlm) {
            request = request.query(&[("mode", "ntlm")]);
        }
        if let Some(etag) = etag {
            request = request.header(header::IF_NONE_MATCH, etag);
        }

        let started_at = Instant::now();
        let result = request.send().await;
        let elapsed = started_at.elapsed();
        match &result {
            Ok(response) => self.stats.record_response(response.status(), elapsed),
            Err(_) => self.stats.record_network_error(elapsed),
        }
        result
    }

    async fn download_hash(
        &self,
        hash: &str,
        etag: Option<&str>,
    ) -> Result<DownloadOutcome, DownloadError> {
        let url = format!("{}{hash}", self.base_url);

        for retry in 0..=self.args.max_retries {
            match self.send_request(&url, etag).await {
                Ok(response) => {
                    let status_code = response.status();
                    match status_code {
                        StatusCode::OK => {
                            match write_response_if_modified(
                                hash,
                                response,
                                etag,
                                self.writer,
                                self.stats,
                            )
                            .await
                            {
                                Ok(status) => {
                                    return Ok(DownloadOutcome {
                                        status,
                                        retries_used: retry as u64,
                                    });
                                }
                                Err(err) => {
                                    if retry == self.args.max_retries || !err.is_retriable() {
                                        return Err(DownloadError::FileOperation {
                                            retries: retry,
                                            source: err,
                                        });
                                    }
                                }
                            }
                        }
                        StatusCode::NOT_MODIFIED if etag.is_some() => {
                            return Ok(DownloadOutcome {
                                status: DownloadStatus::NotModified,
                                retries_used: retry as u64,
                            });
                        }
                        StatusCode::NOT_MODIFIED => {
                            return Err(DownloadError::Http {
                                hash: hash.to_string(),
                                status_code,
                                retries: retry,
                            });
                        }
                        status_code
                            if status_code.is_client_error()
                                && status_code != StatusCode::TOO_MANY_REQUESTS =>
                        {
                            return Err(DownloadError::Client {
                                hash: hash.to_string(),
                                status_code,
                                retries: retry,
                            });
                        }
                        status_code => {
                            // We intentionally ignore any Retry-After on 429s and
                            // keep using the jittered exponential backoff below.
                            if retry == self.args.max_retries {
                                return Err(DownloadError::Http {
                                    hash: hash.to_string(),
                                    status_code,
                                    retries: retry,
                                });
                            }
                        }
                    }
                }
                Err(err) => {
                    if retry == self.args.max_retries {
                        return Err(DownloadError::Network {
                            hash: hash.to_string(),
                            error: err
                                .source()
                                .map_or_else(|| err.to_string(), |e| e.to_string()),
                            retries: retry,
                        });
                    }
                }
            }

            if retry < self.args.max_retries {
                let delay = 2u64.saturating_pow(retry as u32).min(MAX_BACKOFF_SECS);
                let jitter = rand::random_range(delay / 2..=delay).max(1);
                sleep(Duration::from_secs(jitter)).await;
            }
        }

        unreachable!()
    }

    pub async fn process_single_hash(
        &self,
        hash: String,
        cached_etags: &HashMap<String, String>,
        token: &CancellationToken,
    ) -> (String, Result<DownloadOutcome, DownloadError>) {
        let started_at = Instant::now();
        let etag = if self.args.incremental {
            if let Some(etag) = cached_etags.get(&hash)
                && (self.args.ignore_missing_hash_file || self.writer.hash_exists(&hash).await)
            {
                Some(etag.as_str())
            } else {
                None
            }
        } else {
            None
        };
        let result = tokio::select! {
            biased;
            _ = token.cancelled() => Err(DownloadError::Cancelled { hash: hash.clone() }),
            res = self.download_hash(&hash, etag) => res,
        };
        self.stats.record_end_to_end(started_at.elapsed());
        (hash, result)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        args::{CompressionFormat, HashMode, create_test_args},
        writer::{HashFileWriter, HashWriter, WriteError, WriteMetrics, create_test_writer},
    };
    use async_trait::async_trait;
    use mockito::{Matcher, Server, ServerGuard};
    use std::{
        collections::HashMap,
        io,
        sync::{
            Mutex,
            atomic::{AtomicUsize, Ordering},
        },
    };
    use tempfile::TempDir;
    use tokio::fs;

    struct FlakyWriteWriter {
        failures_remaining: Mutex<usize>,
    }

    #[async_trait]
    impl HashWriter for FlakyWriteWriter {
        async fn hash_exists(&self, _hash: &str) -> bool {
            false
        }

        async fn write_response(
            &self,
            _hash: &str,
            _response: reqwest::Response,
        ) -> Result<WriteMetrics, WriteError> {
            let mut failures_remaining = self.failures_remaining.lock().unwrap();
            if *failures_remaining == 0 {
                Ok(WriteMetrics::default())
            } else {
                *failures_remaining -= 1;
                Err(WriteError::ReadWrite {
                    path: "test-path".to_string(),
                    source: io::Error::other("simulated write failure"),
                })
            }
        }
    }

    struct CountingHashWriter {
        hash_exists_calls: AtomicUsize,
    }

    #[async_trait]
    impl HashWriter for CountingHashWriter {
        async fn hash_exists(&self, _hash: &str) -> bool {
            self.hash_exists_calls.fetch_add(1, Ordering::SeqCst);
            false
        }

        async fn write_response(
            &self,
            _hash: &str,
            _response: reqwest::Response,
        ) -> Result<WriteMetrics, WriteError> {
            Ok(WriteMetrics::default())
        }
    }
    struct TestSetup {
        server: ServerGuard,
        temp_dir: TempDir,
        args: Args,
        client: reqwest::Client,
        stats: RunStats,
    }

    impl TestSetup {
        async fn new() -> Self {
            let server = Server::new_async().await;
            let temp_dir = TempDir::new().unwrap();
            let args = create_test_args(temp_dir.path().to_path_buf());
            let client = reqwest::Client::builder().no_proxy().build().unwrap();
            Self {
                server,
                temp_dir,
                args,
                client,
                stats: RunStats::new(1),
            }
        }

        fn base_url(&self) -> String {
            format!("{}/range/", self.server.url())
        }

        fn writer(&self) -> HashFileWriter {
            create_test_writer(&self.args)
        }

        fn context<'a>(
            &'a self,
            base_url: &'a str,
            writer: &'a dyn HashWriter,
        ) -> DownloadContext<'a> {
            DownloadContext {
                client: &self.client,
                args: &self.args,
                base_url,
                writer,
                stats: &self.stats,
            }
        }
    }

    #[tokio::test]
    async fn process_single_hash_skips_hash_exists_without_cached_etag() {
        let mut setup = TestSetup::new().await;
        setup.args.incremental = true;

        let writer = CountingHashWriter {
            hash_exists_calls: AtomicUsize::new(0),
        };

        let mock = setup
            .server
            .mock("GET", "/range/AAAAA")
            .with_status(200)
            .with_body("test data")
            .create_async()
            .await;
        let base_url = setup.base_url();
        let cached_etags = HashMap::new();

        let (hash, result) = setup
            .context(&base_url, &writer)
            .process_single_hash(
                "AAAAA".to_string(),
                &cached_etags,
                &CancellationToken::new(),
            )
            .await;

        mock.assert_async().await;
        assert_eq!(hash, "AAAAA");
        assert!(result.is_ok());
        assert_eq!(writer.hash_exists_calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn process_single_hash_uses_scanned_existing_hash_index() {
        let mut setup = TestSetup::new().await;
        setup.args.incremental = true;
        fs::write(setup.temp_dir.path().join("AAAAA"), "existing")
            .await
            .unwrap();

        let token = CancellationToken::new();
        let mut writer = setup.writer();
        writer.scan_existing_hashes(&token, None).await.unwrap();
        // The scanned index is a snapshot; this file is created afterward.
        fs::write(setup.temp_dir.path().join("BBBBB"), "created-after-scan")
            .await
            .unwrap();

        let cached_etags = HashMap::from([
            ("AAAAA".to_string(), "\"existing-etag\"".to_string()),
            ("BBBBB".to_string(), "\"missing-etag\"".to_string()),
            ("CCCCC".to_string(), "\"ignored-missing-etag\"".to_string()),
        ]);

        let existing_mock = setup
            .server
            .mock("GET", "/range/AAAAA")
            .match_header("if-none-match", "\"existing-etag\"")
            .with_status(304)
            .create_async()
            .await;
        let base_url = setup.base_url();
        let (_, result) = setup
            .context(&base_url, &writer)
            .process_single_hash("AAAAA".to_string(), &cached_etags, &token)
            .await;
        existing_mock.assert_async().await;
        assert_eq!(result.unwrap().status, DownloadStatus::NotModified);

        let missing_mock = setup
            .server
            .mock("GET", "/range/BBBBB")
            .match_header("if-none-match", Matcher::Missing)
            .with_status(304)
            .create_async()
            .await;
        let (_, result) = setup
            .context(&base_url, &writer)
            .process_single_hash("BBBBB".to_string(), &cached_etags, &token)
            .await;
        missing_mock.assert_async().await;
        assert!(matches!(
            result,
            Err(DownloadError::Http {
                status_code: StatusCode::NOT_MODIFIED,
                retries: 0,
                ..
            })
        ));

        setup.args.ignore_missing_hash_file = true;
        let ignored_missing_writer = setup.writer();
        let ignored_missing_mock = setup
            .server
            .mock("GET", "/range/CCCCC")
            .match_header("if-none-match", "\"ignored-missing-etag\"")
            .with_status(304)
            .create_async()
            .await;
        let (_, result) = setup
            .context(&base_url, &ignored_missing_writer)
            .process_single_hash("CCCCC".to_string(), &cached_etags, &token)
            .await;
        ignored_missing_mock.assert_async().await;
        assert_eq!(result.unwrap().status, DownloadStatus::NotModified);
    }

    #[tokio::test]
    async fn download_hash_preserves_file_when_200_etag_is_unchanged() {
        for (request_etag, response_etag) in [
            ("\"same-etag\"", "\"same-etag\""),
            ("W/\"same-etag\"", "\"same-etag\""),
            ("\"same-etag\"", "W/\"same-etag\""),
            ("W/\"same-etag\"", "W/\"same-etag\""),
        ] {
            let mut setup = TestSetup::new().await;
            let writer = setup.writer();
            let file_path = setup.temp_dir.path().join("AAAAA");
            fs::write(&file_path, "existing content").await.unwrap();
            std::fs::File::options()
                .write(true)
                .open(&file_path)
                .unwrap()
                .set_modified(std::time::UNIX_EPOCH + Duration::from_secs(1_600_000_000))
                .unwrap();
            let original_modified = fs::metadata(&file_path).await.unwrap().modified().unwrap();

            let mock = setup
                .server
                .mock("GET", "/range/AAAAA")
                .match_header("if-none-match", request_etag)
                .with_status(200)
                .with_header("etag", response_etag)
                .with_body("existing content")
                .create_async()
                .await;
            let base_url = setup.base_url();
            let result = setup
                .context(&base_url, &writer)
                .download_hash("AAAAA", Some(request_etag))
                .await;

            mock.assert_async().await;
            let snapshot = setup.stats.snapshot(false);
            assert_eq!(snapshot.requests, 1);
            assert_eq!(snapshot.responses_ok, 1);
            assert_eq!(snapshot.responses_not_modified, 0);
            assert_eq!(snapshot.request_latency.unwrap().count, 1);
            assert_eq!(snapshot.bytes_written, 0);
            assert_eq!(snapshot.body_read_latency, None);
            assert_eq!(snapshot.file_write_latency, None);
            assert_eq!(
                result.unwrap(),
                DownloadOutcome {
                    status: DownloadStatus::NotModified,
                    retries_used: 0,
                }
            );
            assert_eq!(
                fs::metadata(&file_path).await.unwrap().modified().unwrap(),
                original_modified
            );
            assert_eq!(
                fs::read_to_string(&file_path).await.unwrap(),
                "existing content"
            );
        }
    }

    #[tokio::test]
    async fn download_hash_writes_200_with_changed_or_missing_etag() {
        for response_etag in [Some("\"new-etag\""), Some("\"SAME-ETAG\""), None] {
            let mut setup = TestSetup::new().await;
            let writer = setup.writer();
            let file_path = setup.temp_dir.path().join("AAAAA");
            fs::write(&file_path, "existing content").await.unwrap();

            let mut mock = setup
                .server
                .mock("GET", "/range/AAAAA")
                .match_header("if-none-match", "\"same-etag\"")
                .with_status(200)
                .with_body("updated content");
            if let Some(etag) = response_etag {
                mock = mock.with_header("etag", etag);
            }
            let mock = mock.create_async().await;
            let base_url = setup.base_url();
            let result = setup
                .context(&base_url, &writer)
                .download_hash("AAAAA", Some("\"same-etag\""))
                .await;

            mock.assert_async().await;
            let snapshot = setup.stats.snapshot(false);
            assert_eq!(snapshot.bytes_written, "updated content".len() as u64);
            assert_eq!(snapshot.body_read_latency.unwrap().count, 1);
            assert_eq!(snapshot.file_write_latency.unwrap().count, 1);
            assert_eq!(
                result.unwrap(),
                DownloadOutcome {
                    status: DownloadStatus::Downloaded {
                        etag: response_etag.map(String::from),
                    },
                    retries_used: 0,
                }
            );
            assert_eq!(
                fs::read_to_string(&file_path).await.unwrap(),
                "updated content"
            );
        }
    }

    #[tokio::test]
    async fn process_single_hash_downloads_missing_file_with_cached_etag() {
        let mut setup = TestSetup::new().await;
        setup.args.incremental = true;
        let token = CancellationToken::new();
        let mut writer = setup.writer();
        writer.scan_existing_hashes(&token, None).await.unwrap();
        let cached_etags = HashMap::from([("AAAAA".to_string(), "W/\"same-etag\"".to_string())]);
        let mock = setup
            .server
            .mock("GET", "/range/AAAAA")
            .match_header("if-none-match", Matcher::Missing)
            .with_status(200)
            .with_header("etag", "W/\"same-etag\"")
            .with_body("downloaded missing file")
            .create_async()
            .await;
        let base_url = setup.base_url();
        let started_at = Instant::now();
        let (_, result) = setup
            .context(&base_url, &writer)
            .process_single_hash("AAAAA".to_string(), &cached_etags, &token)
            .await;
        let elapsed = started_at.elapsed();

        mock.assert_async().await;
        assert_eq!(
            result.unwrap(),
            DownloadOutcome {
                status: DownloadStatus::Downloaded {
                    etag: Some("W/\"same-etag\"".to_string()),
                },
                retries_used: 0,
            }
        );
        assert_eq!(
            fs::read_to_string(setup.temp_dir.path().join("AAAAA"))
                .await
                .unwrap(),
            "downloaded missing file"
        );
        // Final-result accounting belongs to the caller, not the operation.
        assert_eq!(setup.stats.snapshot(false).processed, 0);
        setup.stats.record_downloaded();
        let snapshot = setup.stats.snapshot(false);
        assert_eq!(snapshot.requests, 1);
        assert_eq!(snapshot.responses_ok, 1);
        assert_eq!(snapshot.total_retries, 0);
        let latency = snapshot.request_latency.unwrap();
        assert_eq!(latency.count, 1);
        let average = snapshot.avg_end_to_end_per_processed_secs.unwrap();
        assert!(average * 1_000.0 >= latency.max_ms);
        assert!(average <= elapsed.as_secs_f64());
    }

    #[tokio::test]
    async fn download_hash_success() {
        let mut setup = TestSetup::new().await;
        let writer = setup.writer();

        let mock_data = "test data";
        let mock = setup
            .server
            .mock("GET", "/range/AAAAA")
            .with_status(200)
            .with_header("etag", "\"test-etag\"")
            .with_body(mock_data)
            .create_async()
            .await;
        let base_url = setup.base_url();

        let result = setup
            .context(&base_url, &writer)
            .download_hash("AAAAA", None)
            .await;

        mock.assert_async().await;

        assert!(result.is_ok());
        let outcome = result.unwrap();
        assert_eq!(
            outcome,
            DownloadOutcome {
                status: DownloadStatus::Downloaded {
                    etag: Some("\"test-etag\"".to_string())
                },
                retries_used: 0
            }
        );

        let file_path = setup.temp_dir.path().join("AAAAA");
        assert!(file_path.exists());

        let content = fs::read_to_string(&file_path).await.unwrap();
        assert_eq!(content, mock_data);
    }

    #[tokio::test]
    async fn download_hash_ntlm_mode() {
        let mut setup = TestSetup::new().await;
        setup.args.hash_mode = HashMode::Ntlm;
        let writer = setup.writer();

        let mock = setup
            .server
            .mock("GET", "/range/HHHHH")
            .match_query(Matcher::UrlEncoded("mode".to_string(), "ntlm".to_string()))
            .with_status(200)
            .with_body("ntlm data")
            .create_async()
            .await;
        let base_url = setup.base_url();

        let result = setup
            .context(&base_url, &writer)
            .download_hash("HHHHH", None)
            .await;

        mock.assert_async().await;

        assert_eq!(
            result.unwrap(),
            DownloadOutcome {
                status: DownloadStatus::Downloaded { etag: None },
                retries_used: 0
            }
        );
    }

    #[tokio::test]
    async fn download_hash_not_modified() {
        let mut setup = TestSetup::new().await;
        setup.args.incremental = true;
        let writer = setup.writer();

        let file_path = setup.temp_dir.path().join("BBBBB");
        fs::write(&file_path, "existing content").await.unwrap();

        let mock = setup
            .server
            .mock("GET", "/range/BBBBB")
            .match_header("if-none-match", "\"existing-etag\"")
            .with_status(304)
            .create_async()
            .await;
        let base_url = setup.base_url();

        let result = setup
            .context(&base_url, &writer)
            .download_hash("BBBBB", Some("\"existing-etag\""))
            .await;

        mock.assert_async().await;
        let snapshot = setup.stats.snapshot(false);
        assert_eq!(snapshot.requests, 1);
        assert_eq!(snapshot.responses_ok, 0);
        assert_eq!(snapshot.responses_not_modified, 1);
        assert_eq!(snapshot.request_latency.unwrap().count, 1);
        assert_eq!(snapshot.bytes_written, 0);
        assert_eq!(snapshot.body_read_latency, None);
        assert_eq!(snapshot.file_write_latency, None);

        assert!(result.is_ok());
        let outcome = result.unwrap();
        assert_eq!(
            outcome,
            DownloadOutcome {
                status: DownloadStatus::NotModified,
                retries_used: 0
            }
        );
    }

    #[tokio::test]
    async fn download_hash_not_modified_without_etag() {
        let mut setup = TestSetup::new().await;
        setup.args.max_retries = 3;
        let writer = setup.writer();

        let mock = setup
            .server
            .mock("GET", "/range/CCCCC")
            .with_status(304)
            .expect(1)
            .create_async()
            .await;
        let base_url = setup.base_url();

        let err = setup
            .context(&base_url, &writer)
            .download_hash("CCCCC", None)
            .await
            .unwrap_err();

        mock.assert_async().await;
        let snapshot = setup.stats.snapshot(false);
        assert_eq!(snapshot.requests, 1);
        assert_eq!(snapshot.responses_not_modified, 1);
        assert_eq!(snapshot.request_latency.unwrap().count, 1);

        assert!(matches!(
            err,
            DownloadError::Http {
                status_code: StatusCode::NOT_MODIFIED,
                retries: 0,
                ..
            }
        ));
    }

    #[tokio::test]
    async fn download_hash_with_ignore_missing_hash_file() {
        let mut setup = TestSetup::new().await;
        setup.args.incremental = true;
        setup.args.ignore_missing_hash_file = true;
        let writer = setup.writer();

        let mock = setup
            .server
            .mock("GET", "/range/GGGGG")
            .match_header("if-none-match", "\"cached-etag\"")
            .with_status(304)
            .create_async()
            .await;
        let base_url = setup.base_url();

        let result = setup
            .context(&base_url, &writer)
            .download_hash("GGGGG", Some("\"cached-etag\""))
            .await;

        mock.assert_async().await;

        assert!(result.is_ok());
        let outcome = result.unwrap();
        assert_eq!(
            outcome,
            DownloadOutcome {
                status: DownloadStatus::NotModified,
                retries_used: 0
            }
        );
    }

    #[tokio::test]
    async fn download_hash_client_error() {
        let mut setup = TestSetup::new().await;
        let writer = setup.writer();

        let mock = setup
            .server
            .mock("GET", "/range/CCCCC")
            .with_status(404)
            .create_async()
            .await;
        let base_url = setup.base_url();

        let result = setup
            .context(&base_url, &writer)
            .download_hash("CCCCC", None)
            .await;

        mock.assert_async().await;
        let snapshot = setup.stats.snapshot(false);
        assert_eq!(snapshot.requests, 1);
        assert_eq!(snapshot.responses_other_client_error, 1);
        assert_eq!(snapshot.request_latency.unwrap().count, 1);

        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(matches!(err, DownloadError::Client { .. }));

        if let DownloadError::Client {
            hash,
            status_code,
            retries,
        } = err
        {
            assert_eq!(hash, "CCCCC");
            assert_eq!(status_code, StatusCode::NOT_FOUND);
            assert_eq!(retries, 0);
        }
    }

    #[tokio::test(start_paused = true)]
    async fn download_hash_client_error_after_retry_reports_retry_count() {
        let mut setup = TestSetup::new().await;
        setup.args.max_retries = 3;
        let writer = setup.writer();

        let _server_error = setup
            .server
            .mock("GET", "/range/JJJJJ")
            .with_status(500)
            .expect(1)
            .create_async()
            .await;
        let _client_error = setup
            .server
            .mock("GET", "/range/JJJJJ")
            .with_status(404)
            .expect(1)
            .create_async()
            .await;
        let base_url = setup.base_url();

        let err = setup
            .context(&base_url, &writer)
            .download_hash("JJJJJ", None)
            .await
            .unwrap_err();
        assert!(matches!(err, DownloadError::Client { retries: 1, .. }));
        let snapshot = setup.stats.snapshot(false);
        assert_eq!(snapshot.requests, 2);
        assert_eq!(snapshot.responses_server_error, 1);
        assert_eq!(snapshot.responses_other_client_error, 1);
        assert_eq!(snapshot.request_latency.unwrap().count, 2);
    }

    #[tokio::test(start_paused = true)]
    async fn download_hash_server_error_with_retry() {
        let mut setup = TestSetup::new().await;
        setup.args.max_retries = 5;
        let writer = setup.writer();

        let mock = setup
            .server
            .mock("GET", "/range/DDDDD")
            .with_status(500)
            .expect(6)
            .create_async()
            .await;
        let base_url = setup.base_url();

        let result = setup
            .context(&base_url, &writer)
            .download_hash("DDDDD", None)
            .await;

        mock.assert_async().await;
        let snapshot = setup.stats.snapshot(false);
        assert_eq!(snapshot.requests, 6);
        assert_eq!(snapshot.responses_server_error, 6);
        assert_eq!(snapshot.request_latency.unwrap().count, 6);

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

    #[tokio::test(start_paused = true)]
    async fn download_hash_zero_retries_stops_after_first_http_error() {
        let mut setup = TestSetup::new().await;
        setup.args.max_retries = 0;
        let writer = setup.writer();

        let mock = setup
            .server
            .mock("GET", "/range/KKKKK")
            .with_status(500)
            .expect(1)
            .create_async()
            .await;
        let base_url = setup.base_url();

        let err = setup
            .context(&base_url, &writer)
            .download_hash("KKKKK", None)
            .await
            .unwrap_err();

        mock.assert_async().await;
        assert!(matches!(err, DownloadError::Http { retries: 0, .. }));
    }

    #[tokio::test(start_paused = true)]
    async fn download_hash_zero_retries_stops_after_first_write_error() {
        let mut setup = TestSetup::new().await;
        setup.args.max_retries = 0;
        let writer = FlakyWriteWriter {
            failures_remaining: Mutex::new(1),
        };

        let mock = setup
            .server
            .mock("GET", "/range/LLLLL")
            .with_status(200)
            .with_body("ok")
            .expect(1)
            .create_async()
            .await;
        let base_url = setup.base_url();

        let err = setup
            .context(&base_url, &writer)
            .download_hash("LLLLL", None)
            .await
            .unwrap_err();

        mock.assert_async().await;
        assert!(matches!(
            err,
            DownloadError::FileOperation { retries: 0, .. }
        ));
    }

    #[tokio::test]
    async fn download_hash_does_not_record_output_when_rename_fails() {
        let mut setup = TestSetup::new().await;
        setup.args.max_retries = 0;
        let writer = setup.writer();
        let file_path = setup.temp_dir.path().join("AAAAA");
        fs::create_dir(&file_path).await.unwrap();
        let mock = setup
            .server
            .mock("GET", "/range/AAAAA")
            .with_status(200)
            .with_body("copied but not finalized")
            .expect(1)
            .create_async()
            .await;
        let base_url = setup.base_url();

        let err = setup
            .context(&base_url, &writer)
            .download_hash("AAAAA", None)
            .await
            .unwrap_err();

        mock.assert_async().await;
        assert!(matches!(
            err,
            DownloadError::FileOperation {
                source: WriteError::Rename { .. },
                retries: 0,
            }
        ));
        assert!(fs::metadata(&file_path).await.unwrap().is_dir());
        let snapshot = setup.stats.snapshot(false);
        assert_eq!(snapshot.requests, 1);
        assert_eq!(snapshot.responses_ok, 1);
        assert_eq!(snapshot.bytes_written, 0);
        assert_eq!(snapshot.body_read_latency, None);
        assert_eq!(snapshot.file_write_latency, None);
    }

    #[tokio::test(start_paused = true)]
    async fn download_hash_reports_retry_count_on_eventual_success() {
        let mut setup = TestSetup::new().await;
        setup.args.max_retries = 3;
        let writer = setup.writer();

        let _fail = setup
            .server
            .mock("GET", "/range/IIIII")
            .with_status(500)
            .expect(1)
            .create_async()
            .await;
        let _ok = setup
            .server
            .mock("GET", "/range/IIIII")
            .with_status(200)
            .with_header("etag", "\"ok-etag\"")
            .with_body("ok")
            .expect(1)
            .create_async()
            .await;
        let base_url = setup.base_url();

        let result = setup
            .context(&base_url, &writer)
            .download_hash("IIIII", None)
            .await;
        let outcome = result.unwrap();

        assert_eq!(outcome.retries_used, 1);
        assert_eq!(
            outcome.status,
            DownloadStatus::Downloaded {
                etag: Some("\"ok-etag\"".to_string())
            }
        );
        let snapshot = setup.stats.snapshot(false);
        assert_eq!(snapshot.requests, 2);
        assert_eq!(snapshot.responses_ok, 1);
        assert_eq!(snapshot.responses_server_error, 1);
        assert_eq!(snapshot.request_latency.unwrap().count, 2);
        assert_eq!(snapshot.processed, 0);
        assert_eq!(snapshot.total_retries, 0);
        assert_eq!(snapshot.bytes_written, 2);
        assert_eq!(snapshot.body_read_latency.unwrap().count, 1);
        assert_eq!(snapshot.file_write_latency.unwrap().count, 1);
    }

    #[tokio::test]
    async fn process_single_hash_records_transport_failure_without_final_accounting() {
        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

        let mut setup = TestSetup::new().await;
        setup.args.max_retries = 0;
        let writer = setup.writer();
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .unwrap();
        let base_url = format!("http://{}/range/", listener.local_addr().unwrap());
        let context = setup.context(&base_url, &writer);
        let cached_etags = HashMap::new();
        let token = CancellationToken::new();
        let ((_, result), ()) = tokio::join!(
            context.process_single_hash("AAAAA".to_string(), &cached_etags, &token),
            async {
                let (mut stream, _) = listener.accept().await.unwrap();
                stream.read_exact(&mut [0; 1]).await.unwrap();
                stream
                    .write_all(b"invalid HTTP response\r\n\r\n")
                    .await
                    .unwrap();
            }
        );

        assert!(matches!(
            result,
            Err(DownloadError::Network { retries: 0, .. })
        ));
        let snapshot = setup.stats.snapshot(false);
        assert_eq!(snapshot.requests, 1);
        assert_eq!(snapshot.network_errors, 1);
        assert_eq!(snapshot.responses_ok, 0);
        assert_eq!(snapshot.request_latency.unwrap().count, 1);
        assert_eq!(snapshot.processed, 0);
        assert_eq!(snapshot.errors_total, 0);
        assert_eq!(snapshot.total_retries, 0);
    }

    #[tokio::test]
    async fn process_single_hash_does_not_count_cancelled_unfinished_request() {
        use tokio::io::AsyncReadExt as _;

        let setup = TestSetup::new().await;
        let writer = setup.writer();
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .unwrap();
        let base_url = format!("http://{}/range/", listener.local_addr().unwrap());
        let context = setup.context(&base_url, &writer);
        let token = CancellationToken::new();
        let cached_etags = HashMap::new();
        let ((hash, result), ()) = tokio::join!(
            context.process_single_hash("AAAAA".to_string(), &cached_etags, &token),
            async {
                let (mut stream, _) = listener.accept().await.unwrap();
                stream.read_exact(&mut [0; 1]).await.unwrap();
                token.cancel();
            }
        );

        assert_eq!(hash, "AAAAA");
        assert!(matches!(result, Err(DownloadError::Cancelled { .. })));
        let snapshot = setup.stats.snapshot(false);
        assert_eq!(snapshot.requests, 0);
        assert_eq!(snapshot.network_errors, 0);
        assert!(snapshot.request_latency.is_none());
        assert_eq!(snapshot.processed, 0);
        assert_eq!(snapshot.cancelled, 0);
        assert_eq!(snapshot.bytes_written, 0);
        assert_eq!(snapshot.body_read_latency, None);
        assert_eq!(snapshot.file_write_latency, None);
    }

    #[tokio::test]
    async fn process_single_hash_records_raw_unexpected_http_status() {
        for status in [429_u16, 201] {
            let mut setup = TestSetup::new().await;
            setup.args.max_retries = 0;
            let writer = setup.writer();
            let mock = setup
                .server
                .mock("GET", "/range/AAAAA")
                .with_status(usize::from(status))
                .expect(1)
                .create_async()
                .await;
            let base_url = setup.base_url();

            let (_, result) = setup
                .context(&base_url, &writer)
                .process_single_hash(
                    "AAAAA".to_string(),
                    &HashMap::new(),
                    &CancellationToken::new(),
                )
                .await;

            mock.assert_async().await;
            assert!(matches!(
                result,
                Err(DownloadError::Http { status_code, retries: 0, .. })
                    if status_code.as_u16() == status
            ));
            let snapshot = setup.stats.snapshot(false);
            assert_eq!(snapshot.requests, 1);
            assert_eq!(
                snapshot.responses_too_many_requests,
                u64::from(status == 429)
            );
            assert_eq!(snapshot.responses_other, u64::from(status == 201));
            assert_eq!(snapshot.request_latency.unwrap().count, 1);
            assert_eq!(snapshot.errors_total, 0);
        }
    }

    #[tokio::test]
    async fn download_hash_with_compression() {
        let mut setup = TestSetup::new().await;
        setup.args.compression = CompressionFormat::Gzip;
        let writer = setup.writer();

        let mock_data = "test compressed data";
        let mock = setup
            .server
            .mock("GET", "/range/EEEEE")
            .with_status(200)
            .with_header("etag", "\"gzip-etag\"")
            .with_body(mock_data)
            .create_async()
            .await;
        let base_url = setup.base_url();

        let result = setup
            .context(&base_url, &writer)
            .download_hash("EEEEE", None)
            .await;

        mock.assert_async().await;

        assert!(result.is_ok());

        let file_path = setup.temp_dir.path().join("EEEEE.gz");
        assert!(file_path.exists());

        let content = fs::read_to_string(&file_path).await.unwrap();
        assert_eq!(content, mock_data);
    }

    #[tokio::test]
    async fn download_hash_success_without_etag_is_still_downloaded() {
        let mut setup = TestSetup::new().await;
        let writer = setup.writer();

        let mock_data = "body without etag";
        let mock = setup
            .server
            .mock("GET", "/range/MMMMM")
            .with_status(200)
            .with_body(mock_data)
            .create_async()
            .await;
        let base_url = setup.base_url();

        let result = setup
            .context(&base_url, &writer)
            .download_hash("MMMMM", None)
            .await;

        mock.assert_async().await;

        assert_eq!(
            result.unwrap(),
            DownloadOutcome {
                status: DownloadStatus::Downloaded { etag: None },
                retries_used: 0
            }
        );

        let file_path = setup.temp_dir.path().join("MMMMM");
        assert!(file_path.exists());

        let content = fs::read_to_string(&file_path).await.unwrap();
        assert_eq!(content, mock_data);
    }
}
