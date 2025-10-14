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

mod args;
mod combined_writer;
mod download;
mod etag;
mod hash_writer;
mod individual_writer;

use indicatif::ProgressStyle;
use std::sync::Arc;
use tokio::{fs, sync::Mutex};
use tokio_util::sync::CancellationToken;
use tracing::Level;
use tracing_indicatif::{IndicatifLayer, span_ext::IndicatifSpanExt as _};
use tracing_subscriber::{
    fmt::writer::MakeWriterExt as _, layer::SubscriberExt as _, util::SubscriberInitExt as _,
};

use crate::args::Args;
use crate::download::{DownloadResult, download_hash};
use crate::etag::ETagCache;
use crate::hash_writer::HashWriter;
use futures::StreamExt;

use args::parse_args;
use combined_writer::CombinedWriter;
use individual_writer::IndividualFileWriter;

const ETAG_CACHE_FILENAME: &str = ".etag_cache.json";
// Maximum hash value for HIBP API. This covers all possible SHA-1 hash prefixes (5 hex digits).
const HASH_MAX: u64 = 0xFFFFF;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let (args, client) = parse_args()?;

    let indicatif_layer = IndicatifLayer::new().with_progress_style(
        ProgressStyle::with_template(
            "[{elapsed_precise}] [{wide_bar}] {pos:>7}/{len:7} ({percent:>3}%) ETA: {eta}",
        )
        .unwrap()
        .progress_chars("#>-"),
    );
    tracing_subscriber::registry()
        .with(
            tracing_subscriber::fmt::layer()
                .with_writer(
                    indicatif_layer
                        .get_stderr_writer()
                        .with_max_level(Level::INFO),
                )
                .with_target(false),
        )
        .with(indicatif_layer)
        .init();
    let span = tracing::info_span!("span");
    span.pb_set_length(HASH_MAX + 1);

    // Create output directory.
    fs::create_dir_all(&args.output_directory).await?;

    // Load resume cache (for current download session with --resume)
    let resume_cache_path = args.output_directory.join(ETAG_CACHE_FILENAME);
    let resume_cache = Arc::new(Mutex::new(ETagCache::load(&resume_cache_path).await?));

    // Load sync cache if --sync is provided (represents previous state)
    let sync_cache = if let Some(sync_path) = &args.sync {
        Some(Arc::new(Mutex::new(ETagCache::load(sync_path).await?)))
    } else {
        None
    };

    // Handle ctrl-c
    let token = CancellationToken::new();
    tokio::task::spawn({
        let token = token.clone();
        async move {
            _ = tokio::signal::ctrl_c().await;
            token.cancel();
        }
    });

    if !args.quiet {
        span.pb_start();
    }

    // Choose writer strategy based on --combine flag
    let writer: Arc<dyn hash_writer::HashWriter> = if args.combine {
        Arc::new(
            CombinedWriter::new(&args.output_directory, args.resume, resume_cache.clone()).await?,
        )
    } else {
        Arc::new(IndividualFileWriter::new(args.clone(), resume_cache.clone()).await?)
    };

    // Download all hashes using the unified function
    download_all_hashes(
        writer,
        args.clone(),
        client,
        resume_cache.clone(),
        sync_cache,
        token,
        span,
        HASH_MAX,
    )
    .await?;

    // Save resume cache
    let final_cache = resume_cache.lock().await;
    if let Err(err) = final_cache.save().await {
        tracing::error!("{err}");
    }

    Ok(())
}

/// Download all hash ranges using the provided writer strategy.
///
/// This function handles:
/// - Resume cache lookups for resume functionality
/// - Sync cache lookups to skip unchanged ranges
/// - Concurrent downloads with configurable concurrency
/// - Progress bar updates
/// - Cache updates on success/error
/// - Cancellation support
///
/// # Arguments
/// * `writer` - The writer strategy (individual files or combined file)
/// * `args` - CLI arguments and configuration
/// * `client` - HTTP client for downloading
/// * `resume_cache` - Resume cache for --resume support (current download session)
/// * `sync_cache` - Optional sync cache for --sync support (previous state for comparison)
/// * `token` - Cancellation token for Ctrl-C handling
/// * `span` - Tracing span for progress bar
/// * `hash_max` - Maximum hash value (0xFFFFF for SHA-1)
pub async fn download_all_hashes<W: HashWriter + ?Sized + 'static>(
    writer: Arc<W>,
    args: Args,
    client: reqwest::Client,
    resume_cache: Arc<Mutex<ETagCache>>,
    sync_cache: Option<Arc<Mutex<ETagCache>>>,
    token: CancellationToken,
    span: tracing::Span,
    hash_max: u64,
) -> anyhow::Result<()> {
    // Process all hash ranges
    let results: Vec<Result<(), anyhow::Error>> = futures::stream::iter(0..=hash_max)
        .take_until(token.cancelled())
        .map(|sequence| {
            let client = client.clone();
            let writer = writer.clone();
            let resume_cache = resume_cache.clone();
            let sync_cache = sync_cache.clone();
            let args = args.clone();
            let span = span.clone();

            async move {
                let hash_prefix = format!("{sequence:05X}");

                // Determine which ETag to use for the If-None-Match header
                // Priority: resume cache (current session) > sync cache (previous state)
                let resume_etag = if args.resume {
                    let cache = resume_cache.lock().await;
                    cache.etags.get(&hash_prefix).cloned()
                } else {
                    None
                };
                let sync_etag = if let Some(ref sync_cache) = sync_cache {
                    let cache = sync_cache.lock().await;
                    cache.etags.get(&hash_prefix).cloned()
                } else {
                    None
                };

                let existing_etag = if resume_etag.is_some() {
                    resume_etag
                } else {
                    sync_etag
                };

                // Download the range
                let result =
                    match download_hash(&hash_prefix, &client, existing_etag.as_deref(), &args)
                        .await
                    {
                        Ok(DownloadResult::Success { etag, data }) => {
                            // New data received, write using the writer strategy
                            // Writer handles ETag cache updates and error cleanup internally
                            writer.write_range(sequence, &hash_prefix, data, etag).await
                        }
                        Ok(DownloadResult::NotModified) => {
                            // 304 Not Modified - file hasn't changed, nothing to do
                            tracing::debug!("Hash range {} was not modified", hash_prefix);
                            Ok(())
                        }
                        Err(e) => Err(anyhow::anyhow!(
                            "Failed to download range {hash_prefix}: {e}"
                        )),
                    };

                span.pb_inc(1);
                result
            }
        })
        .buffer_unordered(args.max_concurrent_requests)
        .collect::<Vec<_>>()
        .await;

    // Check for any failures
    let failures: Vec<_> = results.iter().filter_map(|r| r.as_ref().err()).collect();
    if !failures.is_empty() {
        tracing::error!("Download completed with {} failures", failures.len());
        for (idx, err) in failures.iter().take(10).enumerate() {
            tracing::error!("  Failure {}: {}", idx + 1, err);
        }
        if failures.len() > 10 {
            tracing::error!("  ... and {} more failures", failures.len() - 10);
        }
        return Err(anyhow::anyhow!(
            "Download incomplete: {} ranges failed",
            failures.len()
        ));
    }

    // Finalize the writer (flushes any remaining buffered data)
    writer.finalize().await?;

    Ok(())
}
