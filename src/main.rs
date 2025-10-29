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
    args: Args,
    etag_cache: Arc<Mutex<ETagCache>>,
    previous_etag_cache: Option<Arc<ETagCache>>,
    base_url: &str,
) -> (String, Result<Option<String>, DownloadError>) {
    if let Some(prev_cache) = previous_etag_cache.as_ref()
        && { etag_cache.lock().await.has_same_etag(prev_cache, &hash) }
    {
        return (hash, Ok(None));
    }

    let etag = if args.resume {
        etag_cache.lock().await.etags.get(&hash).cloned()
    } else {
        None
    };
    let result = download_hash(&hash, client, etag.as_deref(), &args, base_url).await;
    (hash, result)
}

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
    let previous_etag_cache = match args.compare_cache.as_ref() {
        Some(path) => Some(Arc::new(ETagCache::load(path).await?)),
        None => None,
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

    futures::stream::iter(0..=HASH_MAX)
        .take_until(token.cancelled())
        .map(|hash| {
            let client = client.clone();
            let compare_cache = previous_etag_cache.clone();
            let etag_cache = etag_cache.clone();
            let hash = format!("{hash:05X}");
            let args = args.clone();

            let result =
                process_single_hash(hash, client, args, etag_cache, compare_cache, HIBP_BASE_URL);
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
                        etag_cache.lock().await.etags.remove(&hash);
                    }
                    Ok(Some(etag)) => {
                        etag_cache.lock().await.etags.insert(hash, etag);
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

#[cfg(test)]
mod tests {
    use super::*;
    use mockito::{Server, ServerGuard};
    use std::collections::HashMap;
    use tempfile::TempDir;
    use tokio::fs;

    use crate::args::create_test_args;

    struct CompareCacheSetup {
        server: ServerGuard,
        temp_dir: TempDir,
        args: Args,
        hash: String,
        etag_cache: Arc<Mutex<ETagCache>>,
        previous_cache: Option<Arc<ETagCache>>,
    }

    async fn build_compare_cache_setup(
        hash: &str,
        resume: bool,
        current_etag: &str,
        previous_etag: Option<&str>,
        existing_content: Option<&str>,
    ) -> CompareCacheSetup {
        let server = Server::new_async().await;
        let temp_dir = TempDir::new().unwrap();
        let mut args = create_test_args(temp_dir.path().to_path_buf());
        args.resume = resume;
        let hash = hash.to_string();

        let current_path = temp_dir.path().join("current.json");
        let mut current_cache = ETagCache {
            etags: HashMap::new(),
            path: current_path,
        };
        current_cache
            .etags
            .insert(hash.clone(), current_etag.to_string());

        let previous_path = temp_dir.path().join("previous.json");
        let previous_cache = previous_etag.map(|etag| {
            let mut cache = ETagCache {
                etags: HashMap::new(),
                path: previous_path.clone(),
            };
            cache.etags.insert(hash.clone(), etag.to_string());
            Arc::new(cache)
        });

        if let Some(content) = existing_content {
            let file_path = temp_dir.path().join(&hash);
            fs::write(&file_path, content).await.unwrap();
        }

        CompareCacheSetup {
            server,
            temp_dir,
            args,
            hash,
            etag_cache: Arc::new(Mutex::new(current_cache)),
            previous_cache,
        }
    }

    #[tokio::test]
    async fn test_process_single_hash_skips_with_matching_compare_cache() {
        let mut setup =
            build_compare_cache_setup("AAAAA", false, "\"etag\"", Some("\"etag\""), None).await;

        let base_url = format!("{}/range/", setup.server.url());
        let client = reqwest::Client::new();

        let mock = setup
            .server
            .mock("GET", "/range/AAAAA")
            .expect_at_most(0)
            .create_async()
            .await;

        let (returned_hash, result) = process_single_hash(
            setup.hash.clone(),
            client,
            setup.args.clone(),
            setup.etag_cache.clone(),
            setup.previous_cache.clone(),
            &base_url,
        )
        .await;

        mock.assert_async().await;

        assert_eq!(returned_hash, setup.hash);
        assert!(matches!(result, Ok(None)));
    }

    #[tokio::test]
    async fn test_process_single_hash_downloads_with_compare_cache_mismatch() {
        let mut setup = build_compare_cache_setup(
            "BBBBB",
            false,
            "\"current-etag\"",
            Some("\"previous-etag\""),
            None,
        )
        .await;

        let base_url = format!("{}/range/", setup.server.url());
        let client = reqwest::Client::new();

        let mock_data = "new content";
        let mock = setup
            .server
            .mock("GET", "/range/BBBBB")
            .with_status(200)
            .with_header("etag", "\"new-etag\"")
            .with_body(mock_data)
            .expect(1)
            .create_async()
            .await;

        let (returned_hash, result) = process_single_hash(
            setup.hash.clone(),
            client,
            setup.args.clone(),
            setup.etag_cache.clone(),
            setup.previous_cache.clone(),
            &base_url,
        )
        .await;

        mock.assert_async().await;

        assert_eq!(returned_hash, setup.hash);
        let etag = result.unwrap();
        assert_eq!(etag, Some("\"new-etag\"".to_string()));

        let file_path = setup.temp_dir.path().join(&setup.hash);
        let content = fs::read_to_string(&file_path).await.unwrap();
        assert_eq!(content, mock_data);
    }

    #[tokio::test]
    async fn test_process_single_hash_skips_with_compare_cache_and_resume() {
        let mut setup =
            build_compare_cache_setup("CCCCC", true, "\"etag\"", Some("\"etag\""), None).await;

        let base_url = format!("{}/range/", setup.server.url());
        let client = reqwest::Client::new();

        let mock = setup
            .server
            .mock("GET", "/range/CCCCC")
            .expect_at_most(0)
            .create_async()
            .await;

        let (returned_hash, result) = process_single_hash(
            setup.hash.clone(),
            client,
            setup.args.clone(),
            setup.etag_cache.clone(),
            setup.previous_cache.clone(),
            &base_url,
        )
        .await;

        mock.assert_async().await;

        assert_eq!(returned_hash, setup.hash);
        assert!(matches!(result, Ok(None)));
    }

    #[tokio::test]
    async fn test_process_single_hash_downloads_with_compare_cache_and_resume() {
        let mut setup = build_compare_cache_setup(
            "DDDDD",
            true,
            "\"existing-etag\"",
            Some("\"stale-etag\""),
            Some("old content"),
        )
        .await;

        let base_url = format!("{}/range/", setup.server.url());
        let client = reqwest::Client::new();

        let mock_data = "refreshed content";
        let mock = setup
            .server
            .mock("GET", "/range/DDDDD")
            .match_header("if-none-match", "\"existing-etag\"")
            .with_status(200)
            .with_header("etag", "\"newer-etag\"")
            .with_body(mock_data)
            .expect(1)
            .create_async()
            .await;

        let (returned_hash, result) = process_single_hash(
            setup.hash.clone(),
            client,
            setup.args.clone(),
            setup.etag_cache.clone(),
            setup.previous_cache.clone(),
            &base_url,
        )
        .await;

        mock.assert_async().await;

        assert_eq!(returned_hash, setup.hash);
        let etag = result.unwrap();
        assert_eq!(etag, Some("\"newer-etag\"".to_string()));

        let final_path = setup.temp_dir.path().join(&setup.hash);
        let content = fs::read_to_string(&final_path).await.unwrap();
        assert_eq!(content, mock_data);
    }
}
