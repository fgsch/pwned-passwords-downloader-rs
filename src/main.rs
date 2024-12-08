// Copyright (c) 2024 Federico G. Schwindt

use clap::Parser;
use indicatif::{ProgressBar, ProgressDrawTarget, ProgressStyle};
use reqwest::header::{ACCEPT_ENCODING, CONTENT_ENCODING};
use std::fs::{self, File};
use std::io::Write;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Semaphore;
use tokio::time::sleep;

const HIBP_BASE_URL: &str = "https://api.pwnedpasswords.com/range/";
const HASH_MAX: u64 = 0xFFFFF;

#[derive(clap::ValueEnum, PartialEq, Debug, Clone)]
enum CompressionAlgo {
    Brotli,
    Gzip,
}

#[derive(Parser, Debug)]
#[command(version, about, long_about = None)]
struct Args {
    /// Algorithm to use to compress the dataset files
    #[arg(long, value_enum)]
    compression: Option<CompressionAlgo>,

    /// Create output directory if it doesn't exist
    #[arg(long, default_value_t = false)]
    create_directory: bool,

    /// Number of parallel requests
    #[arg(long, default_value_t = 64)]
    max_parallel_requests: usize,

    /// Number of retries before giving up
    #[arg(long, default_value_t = 10)]
    max_retries: usize,

    /// Where to download the files
    #[arg(long, default_value = ".")]
    output_directory: String,

    /// Show progress bar
    #[arg(long, default_value_t = false)]
    progress_bar: bool,

    /// Set the User agent
    #[arg(long, default_value = "hibp-downloader")]
    user_agent: String,
}

#[tokio::main]
async fn main() {
    let args = Args::parse();

    let output_directory = PathBuf::from(args.output_directory);
    if !output_directory.exists() && args.create_directory {
        _ = fs::create_dir(&output_directory);
    }

    let progress_bar = ProgressBar::with_draw_target(Some(HASH_MAX), ProgressDrawTarget::hidden())
        .with_style(
            ProgressStyle::with_template(
                "[{elapsed_precise}] [{wide_bar}] [{pos:>7}/{percent:>3}%] [{eta}]",
            )
            .unwrap()
            .progress_chars("#>-"),
        );
    if args.progress_bar {
        progress_bar.set_draw_target(ProgressDrawTarget::stderr());
    }

    let mut client_builder = reqwest::Client::builder().user_agent(format!(
        "{}/{}",
        args.user_agent,
        env!("CARGO_PKG_VERSION")
    ));
    if args.compression.is_some() {
        client_builder = client_builder.no_gzip().no_brotli();
    }
    let client = client_builder.build().expect("client builder succeeded");

    let accept_encoding = match args.compression {
        Some(CompressionAlgo::Gzip) => "gzip",
        None | Some(CompressionAlgo::Brotli) => "br",
    };

    let sem = Arc::new(Semaphore::new(args.max_parallel_requests));

    for suffix in 0..=HASH_MAX {
        let permit = Arc::clone(&sem).acquire_owned().await;

        let client = client.clone();
        let suffix_hex = format!("{:05X}", suffix);
        let output_path = output_directory.clone();
        let progress_bar = progress_bar.clone();

        tokio::spawn(async move {
            let _permit = permit;
            for retry in 0..=args.max_retries {
                match client
                    .get(HIBP_BASE_URL.to_string() + &suffix_hex)
                    .header(ACCEPT_ENCODING, accept_encoding)
                    .send()
                    .await
                {
                    Ok(response) => {
                        let extension = match response
                            .headers()
                            .get(CONTENT_ENCODING)
                            .map(|s| s.to_str().unwrap_or_default())
                        {
                            Some("br") => ".br",
                            Some("gzip") => ".gz",
                            Some(_) => ".bin",
                            _ => "",
                        };
                        let mut file = File::create(output_path.join(suffix_hex + extension))
                            .expect("file creation succeeded");
                        _ = file
                            .write_all(&response.bytes().await.expect("full response in bytes"));
                        progress_bar.inc(1);
                        break;
                    }
                    _ => {
                        sleep(Duration::from_secs(u64::pow(2, retry as u32))).await;
                    }
                };
            }
        });
    }
    progress_bar.finish();
}
