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
mod stats;
mod writer;

use futures::StreamExt as _;
use indicatif::ProgressStyle;
use std::{collections::HashMap, sync::Arc};
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
use download::{DownloadError, DownloadStatus, process_single_hash};
use etag::{ETagCache, ETagDeltas};
use stats::RunStats;
use writer::{HashFileWriter, HashWriter};

const ETAG_CACHE_FILENAME: &str = ".etag_cache.json";
// HIBP API base URL.
const HIBP_BASE_URL: &str = "https://api.pwnedpasswords.com/range/";
// Maximum hash value for HIBP API. This covers all possible SHA-1 hash prefixes (5 hex digits).
const HASH_MAX: u64 = 0xFFFFF;

#[tokio::main]
async fn main() {
    match try_main().await {
        Err(err) => {
            tracing::error!("{err}");
            std::process::exit(1);
        }
        Ok(true) => {
            // Operation was cancelled
            std::process::exit(1);
        }
        Ok(_) => {}
    }
}

async fn try_main() -> Result<bool, Box<dyn std::error::Error>> {
    let span = init_tracing();

    let (args, client) = parse_args()?;

    // Load ETag cache
    let etag_cache_path = args.output_directory.join(ETAG_CACHE_FILENAME);
    let mut etag_cache =
        ETagCache::load(&etag_cache_path, args.hash_mode.clone(), args.incremental).await?;
    // Keep an immutable snapshot for lock-free read access in workers.
    let cached_etags = Arc::new(std::mem::take(&mut etag_cache.etags));

    // Handle ctrl-c
    let token = spawn_ctrl_c_handler();

    if !args.quiet {
        span.pb_start();
    }

    let args = Arc::new(args);
    let writer: Arc<dyn HashWriter> = Arc::new(HashFileWriter::new(
        args.output_directory.clone(),
        args.compression.as_str().to_string(),
    ));
    let stats = Arc::new(RunStats::new(HASH_MAX + 1));

    let etag_deltas = process_hashes(
        &span,
        client,
        args.clone(),
        writer,
        token.clone(),
        cached_etags.clone(),
        stats.clone(),
    )
    .await;

    let cancelled = token.is_cancelled();

    etag_cache.apply_deltas(cached_etags, etag_deltas);

    // Save ETag cache
    etag_cache.save().await?;

    if !args.quiet {
        stats.log_summary(cancelled);
    }

    Ok(cancelled)
}

fn init_tracing() -> tracing::Span {
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
    let span = tracing::info_span!("span");
    span.pb_set_length(HASH_MAX + 1);
    span
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

async fn process_hashes(
    span: &tracing::Span,
    client: reqwest::Client,
    args: Arc<Args>,
    writer: Arc<dyn HashWriter>,
    token: CancellationToken,
    cached_etags: Arc<HashMap<String, String>>,
    stats: Arc<RunStats>,
) -> ETagDeltas {
    let mut etag_deltas = ETagDeltas::default();

    let stream = futures::stream::iter(0..=HASH_MAX)
        .take_until(token.cancelled())
        .map(|hash| {
            let client = client.clone();
            let cached_etags = cached_etags.clone();
            let hash = format!("{hash:05X}");
            let args = args.clone();
            let writer = writer.clone();
            let token = token.clone();

            process_single_hash(
                client,
                args,
                HIBP_BASE_URL,
                hash,
                cached_etags,
                token,
                writer,
            )
        })
        .buffer_unordered(args.max_concurrent_requests);
    tokio::pin!(stream);

    while let Some((hash, result)) = stream.next().await {
        span.pb_inc(1);
        let stats = stats.as_ref();
        let etag_deltas: &mut ETagDeltas = &mut etag_deltas;
        match result {
            Ok(outcome) => {
                stats.record_retries(outcome.retries_used);
                match outcome.status {
                    DownloadStatus::Downloaded { etag: Some(etag) } => {
                        stats.record_downloaded();
                        etag_deltas.updates.insert(hash, etag);
                    }
                    DownloadStatus::Downloaded { etag: None } => {
                        stats.record_downloaded();
                        etag_deltas.removals.insert(hash);
                    }
                    DownloadStatus::NotModified => {
                        stats.record_not_modified();
                    }
                }
            }
            Err(DownloadError::Cancelled { .. }) => {
                stats.record_cancelled();
                // Exit quickly
            }
            Err(err) => {
                if let Some(retries_used) = match err {
                    DownloadError::Client { retries, .. }
                    | DownloadError::FileOperation { retries, .. }
                    | DownloadError::Http { retries, .. }
                    | DownloadError::Network { retries, .. } => Some(retries as u64),
                    DownloadError::Cancelled { .. } => None,
                } {
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

    etag_deltas
}
