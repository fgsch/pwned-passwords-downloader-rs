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
use tokio::{fs, sync::Mutex};
use tokio_util::sync::CancellationToken;
use tracing::Level;
use tracing_indicatif::{IndicatifLayer, span_ext::IndicatifSpanExt as _};
use tracing_subscriber::{
    fmt::writer::MakeWriterExt as _, layer::SubscriberExt as _, util::SubscriberInitExt as _,
};

use args::parse_args;
use download::download_hash;
use etag::ETagCache;

const ETAG_CACHE_FILENAME: &str = ".etag_cache.json";
// Maximum hash value for HIBP API. This covers all possible SHA-1 hash prefixes (5 hex digits).
const HASH_MAX: u64 = 0xFFFFF;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
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

    // Load ETag cache
    let etag_cache_path = args.output_directory.join(ETAG_CACHE_FILENAME);
    let etag_cache = Arc::new(Mutex::new(ETagCache::load(&etag_cache_path).await?));

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

    futures::stream::iter(0..=HASH_MAX)
        .take_until(token.cancelled())
        .map(|hash| {
            let client = client.clone();
            let etag_cache = etag_cache.clone();
            let hash = format!("{:05X}", hash);
            let args = args.clone();

            let result = async move {
                let etag = if args.resume {
                    etag_cache.lock().await.etags.get(&hash).cloned()
                } else {
                    None
                };
                let result = download_hash(&hash, client, etag.as_deref(), &args).await;
                (hash, result)
            };
            span.pb_inc(1);
            result
        })
        .buffer_unordered(args.max_concurrent_requests)
        .for_each(|(hash, result)| {
            let etag_cache = etag_cache.clone();
            async move {
                match result {
                    Err(err) => {
                        tracing::error!("{err}");
                        // Remove etag on error to force re-download next time
                        let mut cache = etag_cache.lock().await;
                        cache.etags.remove(&hash);
                    }
                    Ok(Some(etag)) => {
                        let mut cache = etag_cache.lock().await;
                        cache.etags.insert(hash, etag);
                    }
                    Ok(None) => {
                        // File was not modified (304 response)
                    }
                }
            }
        })
        .await;

    // Save ETag cache
    let final_cache = etag_cache.lock().await;
    if let Err(err) = final_cache.save().await {
        tracing::error!("{err}");
    }

    Ok(())
}
