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
mod download;
mod etag;

use futures::StreamExt as _;
use indicatif::ProgressStyle;
use std::sync::Arc;
use tokio::sync::RwLock;
use tokio_util::sync::CancellationToken;
use tracing::Level;
use tracing_indicatif::{IndicatifLayer, span_ext::IndicatifSpanExt as _};
use tracing_subscriber::{
    fmt::writer::MakeWriterExt as _, layer::SubscriberExt as _, util::SubscriberInitExt as _,
};

use args::{Args, parse_args};
use download::{DownloadError, download_hash};
use etag::ETagCache;

const ETAG_CACHE_FILENAME: &str = ".etag_cache.json";
// HIBP API base URL.
const HIBP_BASE_URL: &str = "https://api.pwnedpasswords.com/range/";
// Maximum hash value for HIBP API. This covers all possible SHA-1 hash prefixes (5 hex digits).
const HASH_MAX: u64 = 0xFFFFF;

async fn process_single_hash(
    hash: String,
    client: reqwest::Client,
    args: Arc<Args>,
    etag_cache: Arc<RwLock<ETagCache>>,
    token: CancellationToken,
    base_url: &str,
) -> (String, Result<Option<String>, DownloadError>) {
    let etag = if args.incremental {
        etag_cache.read().await.etags.get(&hash).cloned()
    } else {
        None
    };
    let hash_clone = hash.clone();
    let result = tokio::select! {
        res = download_hash(&hash, client, etag.as_deref(), &args, base_url) => res,
        _ = token.cancelled() => Err(DownloadError::Cancelled { hash: hash_clone }),
    };
    (hash, result)
}

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

    let (args, client) = parse_args()?;

    // Load ETag cache
    let etag_cache_path = args.output_directory.join(ETAG_CACHE_FILENAME);
    let etag_cache = Arc::new(RwLock::new(
        ETagCache::load(&etag_cache_path, args.hash_mode.clone(), args.incremental).await?,
    ));

    // Handle ctrl-c
    let token = CancellationToken::new();
    tokio::task::spawn({
        let token = token.clone();
        async move {
            _ = tokio::signal::ctrl_c().await;
            token.cancel();
            tracing::info!("Received ctrl-c; terminating.");
        }
    });

    if !args.quiet {
        span.pb_start();
    }

    let args = Arc::new(args);

    futures::stream::iter(0..=HASH_MAX)
        .take_until(token.cancelled())
        .map(|hash| {
            let client = client.clone();
            let etag_cache = etag_cache.clone();
            let hash = format!("{hash:05X}");
            let args = args.clone();
            let token = token.clone();

            process_single_hash(hash, client, args, etag_cache, token, HIBP_BASE_URL)
        })
        .buffer_unordered(args.max_concurrent_requests)
        .for_each(|(hash, result)| {
            span.pb_inc(1);
            let etag_cache = etag_cache.clone();
            async move {
                match result {
                    Ok(Some(etag)) => {
                        etag_cache.write().await.etags.insert(hash, etag);
                    }
                    Ok(None) => {
                        // File was not modified (304 response)
                    }
                    Err(DownloadError::Cancelled { .. }) => {
                        // Exit quickly
                    }
                    Err(err) => {
                        tracing::error!("{err}");
                        // Remove etag on error to force re-download next time
                        etag_cache.write().await.etags.remove(&hash);
                    }
                }
            }
        })
        .await;

    // Save ETag cache
    etag_cache.write().await.save().await?;

    Ok(token.is_cancelled())
}
