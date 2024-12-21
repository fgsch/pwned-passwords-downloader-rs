// Copyright (c) 2024 Federico G. Schwindt

use clap::Parser;
use indicatif::{ProgressBar, ProgressDrawTarget, ProgressStyle};
use reqwest::header::{ACCEPT_ENCODING, CONTENT_ENCODING};
use reqwest::StatusCode;
use std::fs::{self, File};
use std::io::Write;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Semaphore;
use tokio::task::JoinSet;
use tokio::time::sleep;

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

    /// Specifies the directory where downloaded files will be stored
    #[arg(long, default_value = ".")]
    output_directory: String,

    /// Enables the display of a progress bar during operations
    #[arg(long, default_value_t = false)]
    progress_bar: bool,

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

    let mut client_builder = reqwest::Client::builder().user_agent(args.user_agent);
    // If compression is enabled, disable auto-decompression.
    if args.compression.is_some() {
        client_builder = client_builder.no_gzip().no_brotli();
    }
    let client = client_builder.build().expect("client builder succeeded");

    let output_directory = PathBuf::from(args.output_directory);
    if !output_directory.exists() && args.create_directory {
        _ = fs::create_dir(&output_directory);
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

    let sem = Arc::new(Semaphore::new(args.max_parallel_requests));

    let mut set = JoinSet::new();

    for hash_prefix in 0..=HASH_MAX {
        let permit = Arc::clone(&sem).acquire_owned().await;

        let client = client.clone();
        let hash_prefix_str = format!("{:05X}", hash_prefix);
        let output_path = output_directory.clone();
        let progress_bar = progress_bar.clone();

        set.spawn(async move {
            let _permit = permit;
            'outer: for retry in 0..args.max_retries {
                match client
                    .get(HIBP_BASE_URL.to_string() + &hash_prefix_str)
                    .header(ACCEPT_ENCODING, accept_encoding)
                    .send()
                    .await
                {
                    Ok(response) => {
                        let status_code = response.status();
                        if status_code != StatusCode::OK {
                            break;
                        }
                        let filename = output_path.join(hash_prefix_str + get_extension(&response));
                        let mut file = match File::create(filename) {
                            Ok(file) => file,
                            Err(_err) => break,
                        };
                        match response.bytes().await {
                            Ok(body) => _ = file.write_all(&body),
                            Err(_err) => break,
                        }
                        progress_bar.inc(1);
                        break 'outer;
                    }
                    Err(_err) => {}
                }

                // Exponential backoff on error.
                sleep(Duration::from_secs(u64::pow(2, retry as u32))).await;
            }
        });
    }

    set.join_all().await;

    progress_bar.finish();
}

fn get_extension(response: &reqwest::Response) -> &str {
    match response
        .headers()
        .get(CONTENT_ENCODING)
        .map(|s| s.to_str().unwrap_or_default())
    {
        Some("br") => ".br",
        Some("gzip") => ".gz",
        Some(_) => ".bin",
        _ => "",
    }
}
