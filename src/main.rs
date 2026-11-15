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

mod args;
mod download;
mod etag;
mod metrics;
mod stats;
mod writer;

use futures::StreamExt as _;
use indicatif::ProgressStyle;
use std::{collections::HashMap, ops::RangeInclusive, sync::Arc};
use tokio_util::sync::CancellationToken;
use tracing::Level;
use tracing_indicatif::{IndicatifLayer, span_ext::IndicatifSpanExt as _};
use tracing_subscriber::{
    filter::{LevelFilter, filter_fn},
    fmt::writer::MakeWriterExt as _,
    layer::{Layer as _, SubscriberExt as _},
    util::SubscriberInitExt as _,
};

use args::{Args, parse_args};
use download::{DownloadContext, DownloadError, DownloadStatus};
use etag::{ETagCache, ETagDeltas};
use stats::RunStats;
use writer::{HashFileWriter, HashWriter, ScanOutcome, ScanProgressObserver};

const ETAG_CACHE_FILENAME: &str = ".etag_cache.json";
// HIBP API base URL.
const HIBP_BASE_URL: &str = "https://api.pwnedpasswords.com/range/";
// Maximum hash value for HIBP API. This covers all possible SHA-1 hash prefixes (5 hex digits).
const HASH_MAX: u64 = 0xFFFFF;

struct ScanProgress {
    span: tracing::Span,
}

impl ScanProgress {
    fn new() -> Self {
        let span = tracing::info_span!("scan");
        span.pb_set_style(&scan_progress_style());
        span.pb_set_length(u64::MAX);
        span.pb_set_message("Scanning existing hashes (entries): ");
        span.pb_start();
        Self { span }
    }
}

impl ScanProgressObserver for ScanProgress {
    fn entry_scanned(&self) {
        self.span.pb_inc(1);
    }
}

#[tokio::main]
async fn main() {
    match try_main().await {
        Err(err) => {
            tracing::error!("{err}");
            std::process::exit(1);
        }
        Ok(true) => {
            // Operation was cancelled or had download errors
            std::process::exit(1);
        }
        Ok(_) => {}
    }
}

async fn try_main() -> Result<bool, Box<dyn std::error::Error>> {
    init_tracing();

    let (args, client) = parse_args()?;

    let etag_cache_path = args.output_directory.join(ETAG_CACHE_FILENAME);
    let mut etag_cache =
        ETagCache::load(&etag_cache_path, args.hash_mode.clone(), args.incremental).await?;

    let token = spawn_ctrl_c_handler();
    let stats = Arc::new(RunStats::new(HASH_MAX + 1));
    let mut file_writer =
        HashFileWriter::new(args.output_directory.clone(), args.compression.as_str());

    if args.incremental && !args.ignore_missing_hash_file && !etag_cache.etags.is_empty() {
        let scan_progress = (!args.quiet).then(ScanProgress::new);
        let outcome = file_writer
            .scan_existing_hashes(
                &token,
                scan_progress
                    .as_ref()
                    .map(|progress| progress as &dyn ScanProgressObserver),
            )
            .await?;
        drop(scan_progress);

        if matches!(outcome, ScanOutcome::Cancelled) {
            if !args.quiet {
                stats.log_summary(true);
            }
            return Ok(true);
        }
    }

    let span = new_download_span();
    if !args.quiet {
        span.pb_start();
    }

    let (etag_deltas, had_errors) = process_hashes(ProcessInput {
        span: &span,
        client: &client,
        args: &args,
        writer: &file_writer,
        token: &token,
        cached_etags: &etag_cache.etags,
        stats: stats.clone(),
        base_url: HIBP_BASE_URL,
        hashes: 0..=HASH_MAX,
    })
    .await;

    let cancelled = token.is_cancelled();

    etag_cache.apply_deltas(etag_deltas);
    etag_cache.save().await?;

    if !args.quiet {
        stats.log_summary(cancelled);
    }

    Ok(cancelled || had_errors)
}

fn init_tracing() {
    let indicatif_layer = IndicatifLayer::new().with_progress_style(
        ProgressStyle::with_template(
            "[{elapsed_precise}] [{wide_bar}] {pos:>7}/{len:7} ({percent:>3}%) ETA: {eta}",
        )
        .unwrap()
        .progress_chars("#>-"),
    );
    let app_span_filter = filter_fn(|meta| {
        // Only create progress bars for spans emitted by this crate.
        meta.is_span() && meta.target().starts_with(env!("CARGO_CRATE_NAME"))
    });

    tracing_subscriber::registry()
        .with(LevelFilter::INFO)
        .with(
            tracing_subscriber::fmt::layer()
                .with_writer(
                    indicatif_layer
                        .get_stderr_writer()
                        .with_max_level(Level::INFO),
                )
                .with_target(false),
        )
        .with(indicatif_layer.with_filter(app_span_filter))
        .init();
}

fn new_download_span() -> tracing::Span {
    let span = tracing::info_span!("download");
    span.pb_set_length(HASH_MAX + 1);
    span
}

fn scan_progress_style() -> ProgressStyle {
    ProgressStyle::with_template("[{elapsed_precise}] {msg}{pos}").unwrap()
}

fn spawn_ctrl_c_handler() -> CancellationToken {
    let token = CancellationToken::new();
    tokio::task::spawn({
        let token = token.clone();
        async move {
            _ = tokio::signal::ctrl_c().await;
            token.cancel();
            tracing::info!("Received ctrl-c; terminating.");
        }
    });
    token
}

struct ProcessInput<'a> {
    span: &'a tracing::Span,
    client: &'a reqwest::Client,
    args: &'a Args,
    writer: &'a dyn HashWriter,
    token: &'a CancellationToken,
    cached_etags: &'a HashMap<String, String>,
    stats: Arc<RunStats>,
    base_url: &'a str,
    hashes: RangeInclusive<u64>,
}

async fn process_hashes(input: ProcessInput<'_>) -> (ETagDeltas, bool) {
    let ProcessInput {
        span,
        client,
        args,
        writer,
        token,
        cached_etags,
        stats,
        base_url,
        hashes,
    } = input;
    let mut etag_deltas = ETagDeltas::default();
    let mut had_errors = false;

    let download = DownloadContext {
        client,
        args,
        base_url,
        writer,
        stats: stats.as_ref(),
    };

    let stream = futures::stream::iter(hashes)
        .take_until(token.cancelled())
        .map(|hash| {
            let hash = format!("{hash:05X}");

            download.process_single_hash(hash, cached_etags, token)
        })
        .buffer_unordered(args.max_concurrent_requests);
    tokio::pin!(stream);

    while let Some((hash, result)) = stream.next().await {
        span.pb_inc(1);
        let stats = stats.as_ref();
        match result {
            Ok(outcome) => {
                stats.record_retries(outcome.retries_used);
                match outcome.status {
                    DownloadStatus::Downloaded { etag } => {
                        stats.record_downloaded();
                        match etag {
                            Some(etag) => {
                                etag_deltas.updates.insert(hash, etag);
                            }
                            None => {
                                etag_deltas.removals.insert(hash);
                            }
                        }
                    }
                    DownloadStatus::NotModified => {
                        stats.record_not_modified();
                    }
                }
            }
            Err(err @ DownloadError::Cancelled { .. }) => {
                stats.record_error(&err);
                // Exit quickly
            }
            Err(err) => {
                had_errors = true;
                if let Some(retries_used) = err.retries() {
                    stats.record_retries(retries_used);
                }
                stats.record_error(&err);
                tracing::error!("{err}");
                // Remove ETag only when local file state may be inconsistent.
                if matches!(err, DownloadError::FileOperation { .. }) {
                    etag_deltas.removals.insert(hash);
                }
            }
        }
    }

    (etag_deltas, had_errors)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{args::create_test_args, writer::create_test_writer};
    use mockito::Server;
    use std::{collections::HashMap, sync::Arc};
    use tempfile::TempDir;
    use tokio_util::sync::CancellationToken;

    #[tokio::test]
    async fn process_hashes_reports_non_cancellation_errors() {
        let mut server = Server::new_async().await;
        let mock = server
            .mock("GET", "/range/00000")
            .with_status(500)
            .expect(1)
            .create_async()
            .await;

        let temp_dir = TempDir::new().unwrap();
        let mut args = create_test_args(temp_dir.path().to_path_buf());
        args.max_retries = 0;
        let writer = create_test_writer(&args);
        let client = reqwest::Client::new();
        let token = CancellationToken::new();
        let cached_etags = HashMap::new();
        let stats = Arc::new(RunStats::new(1));
        let span = tracing::info_span!("test");
        let base_url = format!("{}/range/", server.url());

        let (deltas, had_errors) = process_hashes(ProcessInput {
            span: &span,
            client: &client,
            args: &args,
            writer: &writer,
            token: &token,
            cached_etags: &cached_etags,
            stats: stats.clone(),
            base_url: &base_url,
            hashes: 0..=0,
        })
        .await;

        mock.assert_async().await;
        assert!(had_errors);
        assert!(deltas.updates.is_empty());
        assert!(deltas.removals.is_empty());
        let snapshot = stats.snapshot(false);
        assert_eq!(snapshot.errors_total, 1);
        assert_eq!(snapshot.processed, 1);
        assert_eq!(snapshot.requests, 1);
        assert_eq!(snapshot.responses_server_error, 1);
        assert_eq!(snapshot.network_errors, 0);
        assert_eq!(snapshot.request_latency.unwrap().count, 1);
    }

    #[tokio::test(start_paused = true)]
    async fn process_hashes_counts_retry_responses_separately_from_unchanged_hashes() {
        let mut server = Server::new_async().await;
        let retry_mock = server
            .mock("GET", "/range/00000")
            .match_header("if-none-match", "W/\"same-etag\"")
            .with_status(500)
            .expect(1)
            .create_async()
            .await;
        let unchanged_mock = server
            .mock("GET", "/range/00000")
            .match_header("if-none-match", "W/\"same-etag\"")
            .with_status(200)
            .with_header("etag", "\"same-etag\"")
            .with_body("existing content")
            .expect(1)
            .create_async()
            .await;
        let temp_dir = TempDir::new().unwrap();
        tokio::fs::write(temp_dir.path().join("00000"), "existing content")
            .await
            .unwrap();
        let mut args = create_test_args(temp_dir.path().to_path_buf());
        args.incremental = true;
        args.max_retries = 1;
        let token = CancellationToken::new();
        let mut writer = create_test_writer(&args);
        writer.scan_existing_hashes(&token, None).await.unwrap();
        let client = reqwest::Client::new();
        let cached_etags = HashMap::from([("00000".to_string(), "W/\"same-etag\"".to_string())]);
        let stats = Arc::new(RunStats::new(1));
        let span = tracing::info_span!("test");
        let base_url = format!("{}/range/", server.url());

        let (deltas, had_errors) = process_hashes(ProcessInput {
            span: &span,
            client: &client,
            args: &args,
            writer: &writer,
            token: &token,
            cached_etags: &cached_etags,
            stats: stats.clone(),
            base_url: &base_url,
            hashes: 0..=0,
        })
        .await;

        retry_mock.assert_async().await;
        unchanged_mock.assert_async().await;
        assert!(!had_errors);
        assert!(deltas.updates.is_empty());
        assert!(deltas.removals.is_empty());
        let snapshot = stats.snapshot(false);
        assert_eq!(snapshot.processed, 1);
        assert_eq!(snapshot.downloaded_or_updated, 0);
        assert_eq!(snapshot.not_modified, 1);
        assert_eq!(snapshot.requests, 2);
        assert_eq!(snapshot.responses_ok, 1);
        assert_eq!(snapshot.responses_server_error, 1);
        assert_eq!(snapshot.responses_not_modified, 0);
        assert_eq!(snapshot.network_errors, 0);
        assert_eq!(snapshot.errors_total, 0);
        assert_eq!(snapshot.total_retries, 1);
        assert_eq!(snapshot.retried_requests, 1);
        let request_latency = snapshot.request_latency.unwrap();
        assert_eq!(request_latency.count, 2);
        assert!(
            snapshot.avg_end_to_end_per_processed_secs.unwrap() * 1_000.0
                >= request_latency.average_ms
        );
    }
}
