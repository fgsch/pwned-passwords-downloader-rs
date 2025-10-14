# pwned-passwords-downloader-rs

[![Build Status](https://github.com/fgsch/pwned-passwords-downloader-rs/actions/workflows/ci.yml/badge.svg?branch=main)](https://github.com/fgsch/pwned-passwords-downloader-rs/actions/workflows/ci.yml)
[![Crate](https://img.shields.io/crates/v/pwned_passwords_downloader_rs.svg)](https://crates.io/crates/pwned_passwords_downloader_rs)

A fast, async Rust tool to download password hashes from the [Have
I Been Pwned](https://haveibeenpwned.com/) Pwned Passwords API.  
This tool downloads all available password hash ranges (00000-FFFFF)
with support for resuming interrupted downloads, concurrent requests,
multiple compression formats, and efficient incremental sync.

## Features

- **Fast concurrent downloads**: Configurable number of concurrent requests (default: 64)
- **Resume support**: Automatically resumes interrupted downloads using ETag caching
- **Sync mode**: Download only changed ranges since a previous download
- **Combined mode**: Merge all ranges into a single sorted file for database import
- **Compression support**: Save storage space with Brotli, Gzip, or no compression
- **Progress tracking**: Visual progress bar with ETA
- **Retry mechanism**: Configurable retry attempts with exponential backoff

## Installation

### From source

```sh
git clone https://github.com/fgsch/pwned-passwords-downloader-rs.git
cd pwned-passwords-downloader-rs
cargo install --path .
```

## Usage

### Basic usage

Download all password hashes to the current directory (creates individual files per range):

```sh
pwned-passwords-downloader-rs
```

### Common workflows

#### Initial download (individual files)

Download all ranges as separate files with compression:

```sh
pwned-passwords-downloader-rs \
  --output-directory ./db-2025-01 \
  --compression brotli \
  --max-concurrent-requests 128
```

This creates:
- 1,048,576 individual hash range files (00000-FFFFF)
- `.etag_cache.json` for resume/sync support
- `download_debug.log` for validation

#### Initial download (combined file)

Download all ranges into a single sorted file (ideal for database import):

```sh
pwned-passwords-downloader-rs \
  --output-directory ./db-2025-01 \
  --combine \
  --max-concurrent-requests 256
```

This creates:
- `pwned-passwords-combined.txt` with all hashes in sorted order
- `.etag_cache.json` for resume/sync support
- `download_debug.log` for validation

#### Resume interrupted download

If a download is interrupted (Ctrl-C, network failure, etc.), simply run the same command again with `--resume`:

```sh
pwned-passwords-downloader-rs \
  --output-directory ./db-2025-01 \
  --resume \
  --combine \
  --max-concurrent-requests 256
```

The downloader will skip already-downloaded ranges using the ETag cache.

#### Incremental sync (download only changes)

Download only ranges that changed since a previous download:

```sh
# First download in January
pwned-passwords-downloader-rs --output-directory ./db-2025-01 --combine

# Later download in February (only changed ranges)
pwned-passwords-downloader-rs \
  --output-directory ./db-2025-02 \
  --sync ./db-2025-01/.etag_cache.json \
  --combine \
  --max-concurrent-requests 256
```

The `--sync` flag uses the previous ETag cache to send `If-None-Match` headers, resulting in HTTP 304 responses for unchanged ranges. This significantly reduces download time and bandwidth.

#### Resume with sync

You can combine `--resume` and `--sync` for maximum efficiency:

```sh
pwned-passwords-downloader-rs \
  --output-directory ./db-2025-02 \
  --resume \
  --sync ./db-2025-01/.etag_cache.json \
  --combine \
  --max-concurrent-requests 256
```

Priority order: resume cache (current session) > sync cache (previous state)

### All options

```
Options:
  -c, --compression <COMPRESSION>
          Compression format for storing downloaded hashes [default: none] [possible values: none, brotli, gzip]
      --combine
          Combine all hash ranges into a single sorted output file
      --max-concurrent-requests <MAX_CONCURRENT_REQUESTS>
          Maximum number of concurrent requests [default: 64]
      --max-retries <MAX_RETRIES>
          Number of retry attempts for failed requests [default: 5]
      --resume [<RESUME>]
          Resume previous download session [default: true] [possible values: true, false]
      --sync <SYNC>
          Path to previous ETag cache for incremental sync
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

## File Structure

### Individual file mode (default)

```
output-directory/
├── 00000                    # Hash range 00000
├── 00001                    # Hash range 00001
├── ...                      # (1,048,576 total files)
├── FFFFF                    # Hash range FFFFF
├── .etag_cache.json        # ETag cache for resume/sync
└── download_debug.log      # Debug log with timestamps
```

Each range file contains lines in the format:
```
<35-char-hash-suffix>:<count>
```

Example (`00000` file):
```
0005AD76BD555C1D6D771DE417A4B87E4B4:10
000A8DAE4228F821FB418F59826079BF368:4
000DD7F2A1C68A35673713783CA390C9E93:876
```

### Combined file mode (`--combine`)

```
output-directory/
├── pwned-passwords-combined.txt    # All ranges in single sorted file
├── .etag_cache.json                # ETag cache for resume/sync
└── download_debug.log              # Debug log with timestamps
```

The combined file contains lines in the format:
```
<40-char-full-sha1-hash>:<count>
```

Example (`pwned-passwords-combined.txt`):
```
000000005AD76BD555C1D6D771DE417A4B87E4B4:10
0000000A8DAE4228F821FB418F59826079BF368:4
0000000DD7F2A1C68A35673713783CA390C9E93:876
```

All hashes are sorted lexicographically, making the file ready for database import.

### ETag cache format

The `.etag_cache.json` file stores ETags for each hash range:

```json
{
  "00000": "\"abc123def456\"",
  "00001": "\"xyz789uvw012\"",
  ...
}
```

This cache enables:
- Resume functionality (skip already-downloaded ranges)
- Sync functionality (detect changed ranges via `If-None-Match` headers)
- HTTP 304 responses for unchanged data

### Debug log format

The `download_debug.log` file contains timestamped entries for validation:

```
2025-10-01T12:34:56.789Z Range 00000 written (1048576 bytes)
2025-10-01T12:34:56.890Z Range 00001 written (1048576 bytes)
...
```

## License

MIT - see [LICENSE](LICENSE) file for details.

## Contributing

Contributions are welcome! Please feel free to submit pull requests
or open issues on the [GitHub
repository](https://github.com/fgsch/pwned-passwords-downloader-rs).
