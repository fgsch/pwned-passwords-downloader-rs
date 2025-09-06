// Copyright (c) 2024-2025 Federico G. Schwindt

mod args;
mod download;
mod etag;

use futures::StreamExt;
use indicatif::{ProgressBar, ProgressDrawTarget, ProgressStyle};
use std::sync::Arc;
use tokio::{fs, sync::Mutex};
use tokio_util::sync::CancellationToken;

use args::parse_args;
use download::download_hash;
use etag::ETagCache;

const ETAG_CACHE_FILENAME: &str = ".etag_cache.json";
// Maximum hash value for HIBP API. This covers all possible SHA-1 hash prefixes (5 hex digits).
const HASH_MAX: u64 = 0xFFFFF;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let (args, client) = parse_args()?;

    // Create progress bar.
    let progress_bar = ProgressBar::with_draw_target(
        Some(HASH_MAX + 1),
        if args.quiet {
            ProgressDrawTarget::hidden()
        } else {
            ProgressDrawTarget::stderr()
        },
    )
    .with_style(
        ProgressStyle::with_template(
            "[{elapsed_precise}] [{wide_bar}] {pos:>7}/{len:7} ({percent:>3}%) ETA: {eta}",
        )
        .unwrap()
        .progress_chars("#>-"),
    );

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

    futures::stream::iter(0..=HASH_MAX)
        .take_until(token.cancelled())
        .map(|hash| {
            let client = client.clone();
            let etag_cache = etag_cache.clone();
            let hash = format!("{:05X}", hash);
            let progress_bar = progress_bar.clone();
            let args = args.clone();

            async move {
                let etag = if args.resume {
                    etag_cache.lock().await.etags.get(&hash).cloned()
                } else {
                    None
                };
                let result =
                    download_hash(&hash, client, progress_bar, etag.as_deref(), &args).await;
                (hash, result)
            }
        })
        .buffer_unordered(args.max_concurrent_requests)
        .for_each(|(hash, result)| {
            let etag_cache = etag_cache.clone();
            async move {
                match result {
                    Err(err) => {
                        eprintln!("{err}");
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

    progress_bar.finish_and_clear();

    // Save ETag cache
    let final_cache = etag_cache.lock().await;
    if let Err(err) = final_cache.save().await {
        eprintln!("{err}");
    }

    Ok(())
}
