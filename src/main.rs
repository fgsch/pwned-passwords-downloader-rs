// Copyright (c) 2024 Federico G. Schwindt

use clap::Parser;
use indicatif::{ProgressBar, ProgressDrawTarget, ProgressStyle};
use reqwest::header::{self, HeaderMap, HeaderValue};
use reqwest::StatusCode;
use std::fs::{self, File};
use std::io::Write;
use std::path::PathBuf;
use std::sync::{Arc, RwLock};
use std::time::Duration;
use tokio::sync::Semaphore;
use tokio::task::JoinSet;
use tokio::time::sleep;
use tokio_util::sync::CancellationToken;
use toml::Table;

const ETAGS_FILENAME: &str = ".etags.toml";
const HIBP_BASE_URL: &str = "https://api.pwnedpasswords.com/range/";
const HASH_MAX: u64 = 0xFFFFF;

#[derive(clap::ValueEnum, Clone, Debug)]
enum CompressionFormat {
    Brotli,
    Gzip,
}

#[derive(Parser, Debug)]
#[command(version, about, long_about = None)]
struct Args {
    /// Specifies the compression format to use when storing the files
    #[arg(long, value_enum)]
    compression: Option<CompressionFormat>,

    /// Automatically creates the output directory if it does not already exist
    #[arg(long, default_value_t = false)]
    create_directory: bool,

    /// Sets the maximum number of requests to run concurrently
    #[arg(long, default_value_t = 64)]
    max_parallel_requests: usize,

    /// Sets the number of retry attempts
    #[arg(long, default_value_t = 5)]
    max_retries: usize,

    /// Specifies the directory where downloaded hashes will be stored
    #[arg(long, default_value = ".")]
    output_directory: PathBuf,

    /// Enables the display of a progress bar during download
    #[arg(long, default_value_t = false)]
    progress_bar: bool,

    /// Resumes previous download
    #[arg(long, default_value_t = false)]
    resume: bool,

    /// Sets the User-Agent string for HTTP requests
    #[arg(long, default_value_t = format!("hibp-downloader/{}.{}",
        env!("CARGO_PKG_VERSION_MAJOR"),
        env!("CARGO_PKG_VERSION_MINOR")))]
    user_agent: String,
}

#[tokio::main]
async fn main() {
    let args = Args::parse();

    // Always use compression when downloading the files.
    let accept_encoding = match args.compression {
        Some(CompressionFormat::Gzip) => "gzip",
        Some(CompressionFormat::Brotli) | None => "br",
    };

    let mut headers = HeaderMap::new();
    headers.insert(
        header::ACCEPT_ENCODING,
        HeaderValue::from_static(accept_encoding),
    );
    let mut client_builder = reqwest::Client::builder()
        .default_headers(headers)
        .user_agent(args.user_agent);

    // If compression is enabled, disable auto-decompression.
    if args.compression.is_some() {
        client_builder = client_builder.no_gzip().no_brotli();
    }
    let client = client_builder.build().expect("client builder succeeded");

    if !args.output_directory.exists() && args.create_directory {
        _ = fs::create_dir(&args.output_directory);
    }

    // Create hidden progress bar.
    let progress_bar = ProgressBar::with_draw_target(Some(HASH_MAX), ProgressDrawTarget::hidden())
        .with_style(
            ProgressStyle::with_template(
                "[{elapsed_precise}] [{wide_bar}] [{pos:>7}/{percent:>3}%] [{eta}]",
            )
            .unwrap()
            .progress_chars("#>-"),
        );
    // If progress bar is enabled, unhide it.
    if args.progress_bar {
        progress_bar.set_draw_target(ProgressDrawTarget::stderr());
    }

    // Handle entity tags.
    let etags = Arc::new(RwLock::new(
        toml::from_str::<Table>(
            &std::fs::read_to_string(args.output_directory.join(ETAGS_FILENAME))
                .unwrap_or_default(),
        )
        .unwrap_or_default(),
    ));

    // Handle ctrl-c
    let token = CancellationToken::new();
    let token_cloned = token.clone();

    tokio::task::spawn(async move {
        _ = tokio::signal::ctrl_c().await;
        token.cancel();
    });

    let sem = Arc::new(Semaphore::new(args.max_parallel_requests));

    let mut set = JoinSet::new();

    for hash_prefix in 0..=HASH_MAX {
        if token_cloned.is_cancelled() {
            break;
        }

        let permit = sem.clone().acquire_owned().await;

        let client = client.clone();
        let etags_cloned = etags.clone();
        let hash_prefix_str = format!("{:05X}", hash_prefix);
        let output_directory = args.output_directory.clone();
        let progress_bar = progress_bar.clone();

        // Get entity tag if resume is enabled.
        let etag = if args.resume {
            etags_cloned
                .read()
                .unwrap()
                .get(&hash_prefix_str)
                .map(|s| s.as_str().unwrap_or_default().to_string())
                .unwrap_or_default()
        } else {
            "".to_string()
        };

        set.spawn(async move {
            let _permit = permit;

            'inner: for retry in 0..args.max_retries {
                let mut request = client.get(HIBP_BASE_URL.to_string() + &hash_prefix_str);
                if !etag.is_empty() {
                    request = request.header(header::IF_NONE_MATCH, &etag);
                }

                match request.send().await {
                    Ok(response) => {
                        let status_code = response.status();
                        match status_code {
                            StatusCode::OK => {}
                            StatusCode::NOT_MODIFIED if args.resume => {
                                progress_bar.inc(1);
                                break 'inner;
                            }
                            _ => break,
                        }

                        let mut filename = output_directory.join(&hash_prefix_str);
                        filename.set_extension(guess_extension(&response));
                        let mut file = match File::create(filename) {
                            Ok(file) => file,
                            Err(_err) => break,
                        };

                        // Get entity tag from the request.
                        let etag = response
                            .headers()
                            .get(header::ETAG)
                            .map(|s| s.to_str().unwrap_or_default().to_string())
                            .unwrap_or_default();

                        match response.bytes().await {
                            Ok(body) => _ = file.write_all(&body),
                            Err(_err) => break,
                        }

                        // Add entity tag to the map.
                        if !etag.is_empty() {
                            etags_cloned
                                .write()
                                .unwrap()
                                .insert(hash_prefix_str, etag.into());
                        }

                        progress_bar.inc(1);
                        break 'inner;
                    }
                    Err(_err) => {}
                }

                // Exponential backoff on error.
                sleep(Duration::from_secs(u64::pow(2, retry as u32))).await;
            }
        });
    }

    set.join_all().await;

    progress_bar.abandon();

    _ = std::fs::write(
        args.output_directory.join(ETAGS_FILENAME),
        etags.read().unwrap().to_string().as_bytes(),
    );
}

fn guess_extension(response: &reqwest::Response) -> &str {
    match response
        .headers()
        .get(header::CONTENT_ENCODING)
        .map(|s| s.to_str().unwrap_or_default())
    {
        Some("br") => "br",
        Some("gzip") => "gz",
        Some(_) => "bin",
        _ => "",
    }
}
