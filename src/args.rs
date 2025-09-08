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
use std::path::PathBuf;
use thiserror::Error;

#[derive(Error, Debug)]
pub enum ArgsError {
    #[error("Failed to build HTTP client: {0}")]
    ClientBuild(#[from] reqwest::Error),
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

    /// Maximum number of concurrent requests
    #[arg(long, default_value_t = 64, value_parser = parse_greater_than_zero)]
    pub max_concurrent_requests: usize,

    /// Number of retry attempts for failed requests
    #[arg(long, default_value_t = 5, value_parser = parse_greater_than_zero)]
    pub max_retries: usize,

    /// Resume previous download session
    #[arg(long, num_args=0..=1, default_value_t = true)]
    pub resume: bool,

    /// Directory for storing downloaded hashes
    #[arg(long, short, default_value = ".")]
    pub output_directory: PathBuf,

    /// Disable progress bar output
    #[arg(long, short, default_value_t = false)]
    pub quiet: bool,

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
        .user_agent(&args.user_agent);

    // If compression is enabled, disable auto-decompression.
    if !matches!(args.compression, CompressionFormat::None) {
        client_builder = client_builder.no_gzip().no_brotli();
    }

    Ok((args, client_builder.build()?))
}
