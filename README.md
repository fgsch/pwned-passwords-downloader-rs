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
- **Cache comparison**: Optionally skip ranges that match a previous ETag snapshot
- **Compression support**: Save storage space with Brotli, Gzip, or no compression
- **Hash mode support**: Download SHA-1 (default) or NTLM hash ranges
- **Progress tracking**: Visual progress bar with ETA
- **Run statistics**: Final hash outcomes, HTTP response counts, network errors,
  request latency summaries, throughput, and measured average duration per hash
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

# Quiet mode (no progress bar and no final statistics summary)
pwned-passwords-downloader-rs --quiet

# Force a full download
pwned-passwords-downloader-rs --incremental false

# Sync-style run using an existing .etag_cache.json (skips unchanged ranges even if files are missing)
pwned-passwords-downloader-rs --ignore-missing-hash-file
```

### All options

```
Options:
  -c, --compression <COMPRESSION>
          Compression format for storing downloaded hashes [default: none] [possible values: none, brotli, gzip]
      --hash-mode <HASH_MODE>
          Download hashes using the selected mode [default: sha1] [possible values: ntlm, sha1]
      --ignore-missing-hash-file
          In incremental mode, issue conditional requests even if the hash file is missing
      --incremental [<INCREMENTAL>]
          Continue from a previous download and fetch only changed or missing hashes [default: true] [possible values: true, false]
      --max-concurrent-requests <MAX_CONCURRENT_REQUESTS>
          Maximum number of concurrent requests [default: 64]
      --max-retries <MAX_RETRIES>
          Number of retry attempts for failed requests [default: 5]
  -o, --output-directory <OUTPUT_DIRECTORY>
          Directory for storing downloaded hashes [default: .]
  -q, --quiet
          Disable progress bar output
      --request-timeout <REQUEST_TIMEOUT>
          Request timeout in seconds [default: 30]
  -u, --user-agent <USER_AGENT>
          User-Agent string for HTTP requests [default: hibp-downloader/0.10]
  -h, --help
          Print help (see more with '--help')
  -V, --version
          Print version
```

### Incremental mode

Each run creates or updates an `.etag_cache.json` file in the output
directory. This cache stores the ETag for each downloaded range so
later runs can avoid starting over.

With `--incremental` (the default), the tool uses this cache to
skip unchanged ranges via `If-None-Match`. A `200` response with an ETag
equivalent to the one sent is also treated as unchanged, without rewriting
the file. If a range’s file is missing, it is downloaded unconditionally. Use
`--ignore-missing-hash-file` to always issue conditional requests,
even when files are missing (useful if you copy `.etag_cache.json`
to another machine and only want to fetch ranges that changed). Deleting
the cache or using `--incremental false` forces a full re-download.

Because the cache tracks the last-seen ETags, incremental mode also
fetches any ranges that have changed since the previous run.

### Run statistics

The final summary separates hash outcomes from HTTP attempts. `requests` counts
completed attempts, including retries and transport errors; an attempt cancelled
before either result is not counted. `network_errors` includes retried failures,
whereas `errors` counts hashes that ultimately failed.

The `response` counts reflect the actual HTTP status. A `200` response whose ETag
matches the conditional request therefore counts as HTTP `200` and an unchanged
hash, not an updated file.

`request_latency` measures time until response headers or a transport error,
with minimum, average, approximate p50/p95 bounds, and maximum in milliseconds.
`avg` is the measured average end-to-end duration per processed hash, including
retry waits and file writing, also in milliseconds. `rate_hps` is processed hashes
divided by the statistics' elapsed time; it is not the reciprocal of `avg` when
downloads run concurrently.

## License

MIT - see [LICENSE](LICENSE) file for details.

## Contributing

Contributions are welcome! Please feel free to submit pull requests
or open issues on the [GitHub
repository](https://github.com/fgsch/pwned-passwords-downloader-rs).
