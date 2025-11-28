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

use clap::{
    Parser,
    error::{Error, ErrorKind},
};
use reqwest::{
    Client,
    header::{self, HeaderMap, HeaderValue},
};
use serde::{Deserialize, Serialize};
use std::{path::PathBuf, time::Duration};
use thiserror::Error;

#[derive(Error, Debug)]
pub enum ArgsError {
    #[error("Failed to build HTTP client: {0}")]
    ClientBuild(#[from] reqwest::Error),
    #[error("Failed to create output directory: {0}")]
    CreateDirectory(#[from] std::io::Error),
}

#[derive(clap::ValueEnum, Clone, Debug)]
pub enum CompressionFormat {
    None,
    Brotli,
    Gzip,
}

impl CompressionFormat {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::None => "",
            Self::Brotli => "br",
            Self::Gzip => "gz",
        }
    }
}

#[derive(clap::ValueEnum, Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub enum HashMode {
    Ntlm,
    #[default]
    Sha1,
}

impl HashMode {
    pub fn as_str(&self) -> &'static str {
        match self {
            HashMode::Ntlm => "ntlm",
            HashMode::Sha1 => "sha1",
        }
    }
}

#[derive(Parser, Debug, Clone)]
#[command(
    version,
    about,
    long_about = "Download password hashes from Have I Been Pwned"
)]
pub struct Args {
    /// Compression format for storing downloaded hashes
    #[arg(long, short, value_enum, default_value = "none")]
    pub compression: CompressionFormat,

    /// Download hashes using the selected mode
    #[arg(long, value_enum, default_value_t = HashMode::Sha1)]
    pub hash_mode: HashMode,

    /// In incremental mode, issue conditional requests even if the hash file is missing
    #[arg(long, default_value_t = false)]
    pub ignore_missing_hash_file: bool,

    /// Continue from a previous download and fetch only changed or missing hashes
    #[arg(long, num_args(0..=1), default_value_t = true)]
    pub incremental: bool,

    /// Maximum number of concurrent requests
    #[arg(long, default_value_t = 64, value_parser = parse_greater_than_zero)]
    pub max_concurrent_requests: usize,

    /// Number of retry attempts for failed requests
    #[arg(long, default_value_t = 5, value_parser = parse_greater_than_zero)]
    pub max_retries: usize,

    /// Directory for storing downloaded hashes
    #[arg(long, short, default_value = ".")]
    pub output_directory: PathBuf,

    /// Disable progress bar output
    #[arg(long, short, default_value_t = false)]
    pub quiet: bool,

    /// Request timeout in seconds
    #[arg(long, default_value = "30", value_parser = parse_duration_seconds)]
    pub request_timeout: Duration,

    /// User-Agent string for HTTP requests
    #[arg(long, short, default_value_t = concat!("hibp-downloader/",
        env!("CARGO_PKG_VERSION_MAJOR"),
        ".",
        env!("CARGO_PKG_VERSION_MINOR")).to_string())]
    pub user_agent: String,
}

fn parse_greater_than_zero(s: &str) -> Result<usize, Error> {
    let v = s.parse().map_err(|_| {
        Error::raw(
            ErrorKind::InvalidValue,
            format!("`{s}` isn't a valid integer"),
        )
    })?;
    if v == 0 {
        Err(Error::raw(
            ErrorKind::InvalidValue,
            "Value must be greater than 0",
        ))
    } else {
        Ok(v)
    }
}

fn parse_duration_seconds(s: &str) -> Result<Duration, Error> {
    let seconds = parse_greater_than_zero(s)?;
    Ok(Duration::from_secs(seconds as u64))
}

pub fn parse_args() -> Result<(Args, Client), ArgsError> {
    let args = Args::parse();

    let mut headers = HeaderMap::new();

    // Always use compression when downloading the files.
    let accept_encoding = match args.compression {
        CompressionFormat::Brotli | CompressionFormat::None => "br",
        CompressionFormat::Gzip => "gzip",
    };
    headers.insert(
        header::ACCEPT_ENCODING,
        HeaderValue::from_static(accept_encoding),
    );

    let mut client_builder = reqwest::Client::builder()
        .default_headers(headers)
        .timeout(args.request_timeout)
        .user_agent(&args.user_agent);

    // If compression is enabled, disable auto-decompression.
    if !matches!(args.compression, CompressionFormat::None) {
        client_builder = client_builder.no_gzip().no_brotli();
    }

    // Create output directory.
    std::fs::create_dir_all(&args.output_directory)?;

    Ok((args, client_builder.build()?))
}

#[cfg(test)]
pub fn create_test_args(output_dir: PathBuf) -> Args {
    Args {
        compression: CompressionFormat::None,
        max_concurrent_requests: 1,
        max_retries: 3,
        incremental: false,
        hash_mode: HashMode::Sha1,
        ignore_missing_hash_file: false,
        output_directory: output_dir,
        request_timeout: Duration::from_secs(30),
        quiet: true,
        user_agent: "test-agent/1.0".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::error::ErrorKind;
    use std::time::Duration;

    #[test]
    fn parse_greater_than_zero_valid() {
        assert_eq!(parse_greater_than_zero("1").unwrap(), 1);
        assert_eq!(parse_greater_than_zero("5").unwrap(), 5);
        assert_eq!(parse_greater_than_zero("100").unwrap(), 100);
        assert_eq!(parse_greater_than_zero("999999").unwrap(), 999_999);
    }

    #[test]
    fn parse_greater_than_zero_invalid_zero() {
        let result = parse_greater_than_zero("0");
        assert!(result.is_err());
        let error = result.unwrap_err();
        assert_eq!(error.kind(), ErrorKind::InvalidValue);
        assert!(error.to_string().contains("Value must be greater than 0"));
    }

    #[test]
    fn parse_greater_than_zero_invalid_non_numeric() {
        let result = parse_greater_than_zero("abc");
        assert!(result.is_err());
        let error = result.unwrap_err();
        assert_eq!(error.kind(), ErrorKind::InvalidValue);
        assert!(error.to_string().contains("`abc` isn't a valid integer"));
    }

    #[test]
    fn parse_greater_than_zero_invalid_negative() {
        let result = parse_greater_than_zero("-1");
        assert!(result.is_err());
        let error = result.unwrap_err();
        assert_eq!(error.kind(), ErrorKind::InvalidValue);
        assert!(error.to_string().contains("`-1` isn't a valid integer"));
    }

    #[test]
    fn parse_greater_than_zero_invalid_empty() {
        let result = parse_greater_than_zero("");
        assert!(result.is_err());
        let error = result.unwrap_err();
        assert_eq!(error.kind(), ErrorKind::InvalidValue);
        assert!(error.to_string().contains("`` isn't a valid integer"));
    }

    #[test]
    fn parse_duration_seconds_valid() {
        assert_eq!(parse_duration_seconds("1").unwrap(), Duration::from_secs(1));
        assert_eq!(
            parse_duration_seconds("30").unwrap(),
            Duration::from_secs(30)
        );
    }

    #[test]
    fn parse_duration_seconds_invalid_zero() {
        let result = parse_duration_seconds("0");
        assert!(result.is_err());
        let error = result.unwrap_err();
        assert_eq!(error.kind(), ErrorKind::InvalidValue);
        assert!(error.to_string().contains("Value must be greater than 0"));
    }
}
