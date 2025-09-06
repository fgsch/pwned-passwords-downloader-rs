// Copyright (c) 2024-2025 Federico G. Schwindt

use futures::{TryFutureExt as _, TryStreamExt as _};
use indicatif::ProgressBar;
use reqwest::{
    StatusCode,
    header::{self},
};
use std::{error::Error as _, path::PathBuf, time::Duration};
use thiserror::Error;
use tokio::{fs, io::AsyncWriteExt, time::sleep};
use tokio_util::io::StreamReader;

use crate::args::Args;

const HIBP_BASE_URL: &str = "https://api.pwnedpasswords.com/range/";

#[derive(Error, Debug)]
pub enum DownloadError {
    #[error("File operation on {path} failed: {source}")]
    FileOperation {
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
    progress_bar: ProgressBar,
    etag: Option<&str>,
    args: &Args,
) -> Result<Option<String>, DownloadError> {
    download_hash_with_url(hash, client, progress_bar, etag, args, HIBP_BASE_URL).await
}

async fn download_hash_with_url(
    hash: &str,
    client: reqwest::Client,
    progress_bar: ProgressBar,
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
                            Ok(()) => {
                                progress_bar.inc(1);
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
                        progress_bar.inc(1);
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
                        error: err
                            .source()
                            .map(|e| e.to_string())
                            .unwrap_or_else(|| err.to_string()),
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
    use indicatif::{ProgressBar, ProgressDrawTarget};
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

    fn create_progress_bar() -> ProgressBar {
        ProgressBar::with_draw_target(Some(1), ProgressDrawTarget::hidden())
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

        let progress_bar = create_progress_bar();
        let base_url = server.url();

        let result = download_hash_with_url(
            "AAAAA",
            client,
            progress_bar,
            None,
            &args,
            &format!("{}/range/", base_url),
        )
        .await;

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
        let progress_bar = create_progress_bar();
        let base_url = server.url();

        let result = download_hash_with_url(
            "BBBBB",
            client,
            progress_bar,
            Some("\"existing-etag\""),
            &args,
            &format!("{}/range/", base_url),
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
        let progress_bar = create_progress_bar();
        let base_url = server.url();

        let result = download_hash_with_url(
            "CCCCC",
            client,
            progress_bar,
            None,
            &args,
            &format!("{}/range/", base_url),
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
        let progress_bar = create_progress_bar();
        let base_url = server.url();

        let result = download_hash_with_url(
            "DDDDD",
            client,
            progress_bar,
            None,
            &args,
            &format!("{}/range/", base_url),
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
        let progress_bar = create_progress_bar();
        let base_url = server.url();

        let result = download_hash_with_url(
            "EEEEE",
            client,
            progress_bar,
            None,
            &args,
            &format!("{}/range/", base_url),
        )
        .await;

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
