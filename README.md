# pwned-passwords-downloader-rs

[![Build Status](https://github.com/fgsch/pwned-passwords-downloader-rs/actions/workflows/ci.yml/badge.svg?branch=main)](https://github.com/fgsch/pwned-passwords-downloader-rs/actions/workflows/ci.yml)
[![Crate](https://img.shields.io/crates/v/pwned_passwords_downloader_rs.svg)](https://crates.io/crates/pwned_passwords_downloader_rs)

A fast, async Rust tool to download password hashes from the [Have
I Been Pwned](https://haveibeenpwned.com/) Pwned Passwords API.  
This tool downloads all available password hash ranges (00000-FFFFF)
with support for resuming interrupted downloads, concurrent requests,
and multiple compression formats.

## Features

- **Fast concurrent downloads**: Configurable number of concurrent requests
- **Resume support**: Automatically resumes interrupted downloads using ETag caching
- **Compression support**: Save storage space with Brotli, Gzip, or no compression
- **Progress tracking**: Visual progress bar with ETA
- **Retry mechanism**: Configurable retry attempts for failed requests

## Installation

### From source

```sh
git clone https://github.com/fgsch/pwned-passwords-downloader-rs.git
cd pwned-passwords-downloader-rs
cargo install --path .
```

## Usage

### Basic usage

Download all password hashes to the current directory:

```sh
pwned-passwords-downloader-rs
```

### Common options

```sh
# Download to a specific directory with Brotli compression
pwned-passwords-downloader-rs --output-directory pwned-passwords --compression brotli

# Use more concurrent requests for faster downloads
pwned-passwords-downloader-rs --max-concurrent-requests 100

# Quiet mode (no progress bar)
pwned-passwords-downloader-rs --quiet

# Disable resume functionality
pwned-passwords-downloader-rs --resume false
```

### All options

```
Options:
  -c, --compression <COMPRESSION>
          Compression format for storing downloaded hashes [default: none] [possible values: none, brotli, gzip]
      --max-concurrent-requests <MAX_CONCURRENT_REQUESTS>
          Maximum number of concurrent requests [default: 64]
      --max-retries <MAX_RETRIES>
          Number of retry attempts for failed requests [default: 5]
      --resume [<RESUME>]
          Resume previous download session [default: true] [possible values: true, false]
  -o, --output-directory <OUTPUT_DIRECTORY>
          Directory for storing downloaded hashes [default: .]
  -q, --quiet
          Disable progress bar output
  -u, --user-agent <USER_AGENT>
          User-Agent string for HTTP requests [default: hibp-downloader/0.1]
  -h, --help
          Print help (see more with '--help')
  -V, --version
          Print version
```

## License

MIT - see [LICENSE](LICENSE) file for details.

## Contributing

Contributions are welcome! Please feel free to submit pull requests
or open issues on the [GitHub
repository](https://github.com/fgsch/pwned-passwords-downloader-rs).
