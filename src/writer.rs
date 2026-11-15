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

use async_trait::async_trait;
use futures::TryStreamExt as _;
use std::{
    path::{Path, PathBuf},
    pin::Pin,
    task::{Context, Poll},
    time::Duration,
};
use thiserror::Error;
use tokio::{
    fs,
    io::{AsyncRead, AsyncWrite, AsyncWriteExt as _, ReadBuf},
    time::Instant,
};
use tokio_util::{io::StreamReader, sync::CancellationToken};

#[derive(Error, Debug)]
pub enum WriteError {
    #[error("create on {path} failed: {source}")]
    Create {
        path: String,
        #[source]
        source: std::io::Error,
    },
    #[error("rename from {path} to {target} failed: {source}")]
    Rename {
        path: String,
        target: String,
        #[source]
        source: std::io::Error,
    },
    #[error("read/write on {path} failed: {source}")]
    ReadWrite {
        path: String,
        #[source]
        source: std::io::Error,
    },
    #[error("scan existing hashes at {path} failed: {source}")]
    Scan {
        path: String,
        #[source]
        source: std::io::Error,
    },
}

impl WriteError {
    pub fn is_retriable(&self) -> bool {
        matches!(self, WriteError::ReadWrite { .. })
    }
}

fn scan_error(path: &Path, source: std::io::Error) -> WriteError {
    WriteError::Scan {
        path: path.display().to_string(),
        source,
    }
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum ScanOutcome {
    Completed,
    Cancelled,
}

pub(crate) trait ScanProgressObserver {
    fn entry_scanned(&self);
}

const HASH_SPACE: usize = 1 << 20;

#[derive(Clone, Debug)]
pub(crate) struct ExistingHashes {
    present: Vec<bool>,
}

impl ExistingHashes {
    fn new() -> Self {
        Self {
            present: vec![false; HASH_SPACE],
        }
    }

    fn insert(&mut self, file_name: &str, extension: &str) {
        if let Some(hash) = hash_from_file_name(file_name, extension) {
            self.present[hash] = true;
        }
    }

    fn contains(&self, hash: &str) -> bool {
        hash_from_file_name(hash, "").is_some_and(|hash| self.present[hash])
    }
}

fn hash_from_file_name(file_name: &str, extension: &str) -> Option<usize> {
    let hash = if extension.is_empty() {
        file_name
    } else {
        let suffix_start = file_name.len().checked_sub(extension.len() + 1)?;
        if file_name.as_bytes().get(suffix_start) != Some(&b'.') || !file_name.ends_with(extension)
        {
            return None;
        }
        &file_name[..suffix_start]
    };

    if hash.len() != 5
        || !hash
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'A'..=b'F').contains(&byte))
    {
        return None;
    }

    usize::from_str_radix(hash, 16).ok()
}

struct TempFileGuard<'a> {
    path: &'a Path,
    delete_on_drop: bool,
}

impl Drop for TempFileGuard<'_> {
    fn drop(&mut self) {
        if self.delete_on_drop {
            let _ = std::fs::remove_file(self.path);
        }
    }
}

struct TimedReader<R> {
    inner: R,
    elapsed: Duration,
    pending_since: Option<Instant>,
}

impl<R> TimedReader<R> {
    fn new(inner: R) -> Self {
        Self {
            inner,
            elapsed: Duration::ZERO,
            pending_since: None,
        }
    }
}

impl<R: AsyncRead + Unpin> AsyncRead for TimedReader<R> {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        let started = *self.pending_since.get_or_insert_with(Instant::now);
        let result = Pin::new(&mut self.inner).poll_read(cx, buf);
        if result.is_ready() {
            self.elapsed += started.elapsed();
            self.pending_since = None;
        }
        result
    }
}

struct TimedWriter<W> {
    inner: W,
    elapsed: Duration,
    pending_since: Option<Instant>,
}

impl<W> TimedWriter<W> {
    fn new(inner: W) -> Self {
        Self {
            inner,
            elapsed: Duration::ZERO,
            pending_since: None,
        }
    }
}

impl<W: AsyncWrite + Unpin> AsyncWrite for TimedWriter<W> {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        let started = *self.pending_since.get_or_insert_with(Instant::now);
        let result = Pin::new(&mut self.inner).poll_write(cx, buf);
        if result.is_ready() {
            self.elapsed += started.elapsed();
            self.pending_since = None;
        }
        result
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        let started = *self.pending_since.get_or_insert_with(Instant::now);
        let result = Pin::new(&mut self.inner).poll_flush(cx);
        if result.is_ready() {
            self.elapsed += started.elapsed();
            self.pending_since = None;
        }
        result
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.inner).poll_shutdown(cx)
    }
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct WriteMetrics {
    pub bytes_written: u64,
    pub body_read: Duration,
    /// Write and flush time plus shutdown, close, and atomic rename time.
    pub file_write: Duration,
}

#[async_trait]
pub trait HashWriter: Send + Sync {
    async fn hash_exists(&self, hash: &str) -> bool;
    async fn write_response(
        &self,
        hash: &str,
        response: reqwest::Response,
    ) -> Result<WriteMetrics, WriteError>;
}

#[derive(Clone, Debug)]
pub struct HashFileWriter {
    output_directory: PathBuf,
    extension: &'static str,
    existing_hashes: Option<ExistingHashes>,
}

impl HashFileWriter {
    pub fn new(output_directory: PathBuf, extension: &'static str) -> Self {
        Self {
            output_directory,
            extension,
            existing_hashes: None,
        }
    }

    pub(crate) async fn scan_existing_hashes(
        &mut self,
        cancellation_token: &CancellationToken,
        progress: Option<&dyn ScanProgressObserver>,
    ) -> Result<ScanOutcome, WriteError> {
        let mut existing_hashes = ExistingHashes::new();
        let output_directory = self.output_directory.as_path();
        let mut entries = tokio::select! {
            biased;
            _ = cancellation_token.cancelled() => return Ok(ScanOutcome::Cancelled),
            entries = fs::read_dir(output_directory) => {
                entries.map_err(|source| scan_error(output_directory, source))?
            }
        };

        loop {
            let entry = tokio::select! {
                biased;
                _ = cancellation_token.cancelled() => return Ok(ScanOutcome::Cancelled),
                entry = entries.next_entry() => {
                    entry.map_err(|source| scan_error(output_directory, source))?
                }
            };
            let Some(entry) = entry else {
                break;
            };

            let entry_path = entry.path();
            let file_type = tokio::select! {
                biased;
                _ = cancellation_token.cancelled() => return Ok(ScanOutcome::Cancelled),
                file_type = entry.file_type() => {
                    file_type.map_err(|source| scan_error(&entry_path, source))?
                }
            };

            if let Some(progress) = progress {
                progress.entry_scanned();
            }

            if file_type.is_file()
                && let Some(file_name) = entry.file_name().to_str()
            {
                existing_hashes.insert(file_name, self.extension);
            }
        }

        if cancellation_token.is_cancelled() {
            return Ok(ScanOutcome::Cancelled);
        }

        self.existing_hashes = Some(existing_hashes);
        Ok(ScanOutcome::Completed)
    }

    fn hash_path(&self, hash: &str) -> PathBuf {
        self.output_directory
            .join(hash)
            .with_extension(self.extension)
    }
}

#[async_trait]
impl HashWriter for HashFileWriter {
    async fn hash_exists(&self, hash: &str) -> bool {
        self.existing_hashes
            .as_ref()
            .expect("scan_existing_hashes must complete before hash_exists")
            .contains(hash)
    }

    async fn write_response(
        &self,
        hash: &str,
        response: reqwest::Response,
    ) -> Result<WriteMetrics, WriteError> {
        let final_path = self.hash_path(hash);
        let part_path = final_path.with_extension("part");

        let mut guard = TempFileGuard {
            path: part_path.as_path(),
            delete_on_drop: true,
        };

        // Use blocking creation to ensure the file is created _before_ dropping the
        // TempFileGuard should we cancel the future.
        let file = std::fs::File::create(&part_path).map_err(|source| WriteError::Create {
            path: part_path.display().to_string(),
            source,
        })?;
        let mut file = TimedWriter::new(fs::File::from_std(file));

        let stream = response.bytes_stream().map_err(std::io::Error::other);
        let mut reader = TimedReader::new(StreamReader::new(stream));

        let bytes_written = tokio::io::copy(&mut reader, &mut file)
            .await
            .map_err(|source| WriteError::ReadWrite {
                path: final_path.display().to_string(),
                source,
            })?;
        let finalize_started = Instant::now();
        file.shutdown().await.map_err(|source| WriteError::Rename {
            path: part_path.display().to_string(),
            target: final_path.display().to_string(),
            source,
        })?;
        let file_write = file.elapsed;
        drop(file);
        fs::rename(&part_path, &final_path)
            .await
            .map_err(|source| WriteError::Rename {
                path: part_path.display().to_string(),
                target: final_path.display().to_string(),
                source,
            })?;
        guard.delete_on_drop = false;
        Ok(WriteMetrics {
            bytes_written,
            body_read: reader.elapsed,
            file_write: file_write + finalize_started.elapsed(),
        })
    }
}

#[cfg(test)]
pub fn create_test_writer(args: &crate::args::Args) -> HashFileWriter {
    HashFileWriter::new(args.output_directory.clone(), args.compression.as_str())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::args::create_test_args;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tempfile::TempDir;
    use tokio::{fs, io::AsyncReadExt as _};
    use tokio_util::sync::CancellationToken;

    struct CountingProgress {
        entries: AtomicUsize,
    }

    impl ScanProgressObserver for CountingProgress {
        fn entry_scanned(&self) {
            self.entries.fetch_add(1, Ordering::SeqCst);
        }
    }

    struct CancellingProgress {
        token: CancellationToken,
        entries: AtomicUsize,
    }

    impl ScanProgressObserver for CancellingProgress {
        fn entry_scanned(&self) {
            if self.entries.fetch_add(1, Ordering::SeqCst) == 0 {
                self.token.cancel();
            }
        }
    }

    #[tokio::test]
    async fn write_hash_to_file_success() {
        let temp_dir = TempDir::new().unwrap();
        let final_path = temp_dir.path().join("FFFFF");
        let args = create_test_args(temp_dir.path().to_path_buf());
        let writer = create_test_writer(&args);

        let mock_body = "test content";
        let response = reqwest::Response::from(
            http::Response::builder()
                .status(200)
                .body(mock_body)
                .unwrap(),
        );

        let metrics = writer.write_response("FFFFF", response).await.unwrap();

        assert!(final_path.exists());

        let content = fs::read_to_string(&final_path).await.unwrap();
        assert_eq!(content, mock_body);
        assert_eq!(metrics.bytes_written, content.len() as u64);
    }

    #[tokio::test]
    async fn write_hash_to_file_replaces_existing_file() {
        let temp_dir = TempDir::new().unwrap();
        let final_path = temp_dir.path().join("FFFFF");
        fs::write(&final_path, "old content").await.unwrap();
        let args = create_test_args(temp_dir.path().to_path_buf());
        let writer = create_test_writer(&args);

        let response = reqwest::Response::from(
            http::Response::builder()
                .status(200)
                .body("new content")
                .unwrap(),
        );

        let metrics = writer.write_response("FFFFF", response).await.unwrap();

        let content = fs::read_to_string(&final_path).await.unwrap();
        assert_eq!(content, "new content");
        assert_eq!(metrics.bytes_written, content.len() as u64);
    }

    #[tokio::test]
    async fn write_hash_to_file_body_error_removes_partial_file() {
        let temp_dir = TempDir::new().unwrap();
        let writer = HashFileWriter::new(temp_dir.path().to_path_buf(), "");
        let body = reqwest::Body::wrap_stream(futures::stream::iter([
            Ok("partial content"),
            Err(std::io::Error::other("body failed")),
        ]));
        let response = reqwest::Response::from(http::Response::new(body));

        let result = writer.write_response("FFFFF", response).await;

        assert!(matches!(result, Err(WriteError::ReadWrite { .. })));
        assert!(!temp_dir.path().join("FFFFF").exists());
        assert!(!temp_dir.path().join("FFFFF.part").exists());
    }

    #[tokio::test]
    async fn write_hash_to_file_rename_error_removes_partial_file() {
        let temp_dir = TempDir::new().unwrap();
        let final_path = temp_dir.path().join("FFFFF");
        fs::create_dir(&final_path).await.unwrap();
        let writer = HashFileWriter::new(temp_dir.path().to_path_buf(), "");
        let response = reqwest::Response::from(http::Response::new("content"));

        let result = writer.write_response("FFFFF", response).await;

        assert!(matches!(result, Err(WriteError::Rename { .. })));
        assert!(final_path.is_dir());
        assert!(!temp_dir.path().join("FFFFF.part").exists());
    }

    #[tokio::test(start_paused = true)]
    async fn timed_reader_accumulates_pending_reads() {
        let (mut source, stream) = tokio::io::duplex(1);
        let mut reader = TimedReader::new(stream);

        for delay in [7, 11] {
            let mut byte = [0];
            {
                let read = reader.read_exact(&mut byte);
                tokio::pin!(read);
                assert!(futures::poll!(&mut read).is_pending());
                tokio::time::advance(Duration::from_millis(delay)).await;
                assert!(futures::poll!(&mut read).is_pending());
                source.write_all(b"x").await.unwrap();
                read.await.unwrap();
            }
            assert_eq!(&byte, b"x");
        }

        assert_eq!(reader.elapsed, Duration::from_millis(18));
    }

    #[tokio::test(start_paused = true)]
    async fn timed_writer_includes_pending_write_wait() {
        let (stream, mut destination) = tokio::io::duplex(1);
        let mut writer = TimedWriter::new(stream);
        writer.write_all(b"a").await.unwrap();

        {
            let write = writer.write_all(b"b");
            tokio::pin!(write);
            assert!(futures::poll!(&mut write).is_pending());
            tokio::time::advance(Duration::from_millis(13)).await;
            assert!(futures::poll!(&mut write).is_pending());
            let mut byte = [0];
            destination.read_exact(&mut byte).await.unwrap();
            assert_eq!(&byte, b"a");
            write.await.unwrap();
        }

        assert_eq!(writer.elapsed, Duration::from_millis(13));
        let mut byte = [0];
        destination.read_exact(&mut byte).await.unwrap();
        assert_eq!(&byte, b"b");
    }

    #[tokio::test(start_paused = true)]
    async fn timed_writer_includes_pending_flush_wait() {
        let (mut stream, mut destination) = tokio::io::duplex(1);
        stream.write_all(b"a").await.unwrap();
        let mut writer = TimedWriter::new(tokio::io::BufWriter::with_capacity(2, stream));
        writer.write_all(b"b").await.unwrap();

        {
            let flush = writer.flush();
            tokio::pin!(flush);
            assert!(futures::poll!(&mut flush).is_pending());
            tokio::time::advance(Duration::from_millis(17)).await;
            assert!(futures::poll!(&mut flush).is_pending());
            let mut byte = [0];
            destination.read_exact(&mut byte).await.unwrap();
            assert_eq!(&byte, b"a");
            flush.await.unwrap();
        }

        assert_eq!(writer.elapsed, Duration::from_millis(17));
        let mut byte = [0];
        destination.read_exact(&mut byte).await.unwrap();
        assert_eq!(&byte, b"b");
    }

    #[test]
    fn hash_from_file_name_rejects_empty_suffix() {
        assert_eq!(hash_from_file_name("ABCDF.", ""), None);
    }

    #[tokio::test]
    async fn scan_existing_hashes_recognizes_uncompressed_files() {
        let temp_dir = TempDir::new().unwrap();
        fs::write(temp_dir.path().join("ABCDE"), "existing")
            .await
            .unwrap();
        fs::write(temp_dir.path().join("ABCDF.gz"), "wrong extension")
            .await
            .unwrap();

        let mut writer = HashFileWriter::new(temp_dir.path().to_path_buf(), "");
        let outcome = writer
            .scan_existing_hashes(&CancellationToken::new(), None)
            .await
            .unwrap();

        assert_eq!(outcome, ScanOutcome::Completed);
        assert!(writer.hash_exists("ABCDE").await);
        assert!(!writer.hash_exists("ABCDF").await);
    }

    #[tokio::test]
    async fn scan_existing_hashes_recognizes_configured_compression() {
        for extension in ["gz", "br"] {
            let temp_dir = TempDir::new().unwrap();
            let matching_name = format!("ABCDE.{extension}");
            fs::write(temp_dir.path().join(matching_name), "existing")
                .await
                .unwrap();
            fs::write(temp_dir.path().join("ABCDF"), "wrong extension")
                .await
                .unwrap();

            let mut writer = HashFileWriter::new(temp_dir.path().to_path_buf(), extension);
            let outcome = writer
                .scan_existing_hashes(&CancellationToken::new(), None)
                .await
                .unwrap();

            assert_eq!(outcome, ScanOutcome::Completed);
            assert!(writer.hash_exists("ABCDE").await);
            assert!(!writer.hash_exists("ABCDF").await);
        }
    }

    #[tokio::test]
    async fn scan_existing_hashes_ignores_unrelated_entries_and_reports_progress() {
        let temp_dir = TempDir::new().unwrap();
        let mut expected_entries = 0;

        fs::write(temp_dir.path().join("AAAAA.gz"), "existing")
            .await
            .unwrap();
        expected_entries += 1;
        fs::create_dir(temp_dir.path().join("BBBBB.gz"))
            .await
            .unwrap();
        expected_entries += 1;
        fs::write(temp_dir.path().join("DDDDD.part"), "partial")
            .await
            .unwrap();
        expected_entries += 1;
        fs::write(temp_dir.path().join(".etag_cache.json"), "cache")
            .await
            .unwrap();
        expected_entries += 1;
        fs::write(temp_dir.path().join("EEEEE.br"), "wrong extension")
            .await
            .unwrap();
        expected_entries += 1;
        fs::write(temp_dir.path().join("fffff.gz"), "lowercase")
            .await
            .unwrap();
        expected_entries += 1;
        fs::write(temp_dir.path().join("GGGGG.gz"), "malformed")
            .await
            .unwrap();
        expected_entries += 1;

        #[cfg(unix)]
        let symlink_target_dir = TempDir::new().unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::symlink;

            let target = symlink_target_dir.path().join("target");
            fs::write(&target, "target").await.unwrap();
            symlink(&target, temp_dir.path().join("CCCCC.gz")).unwrap();
            expected_entries += 1;
        }

        #[cfg(all(unix, not(target_os = "macos")))]
        {
            use std::ffi::OsString;
            use std::os::unix::ffi::OsStringExt;

            let non_utf8_name = OsString::from_vec(vec![0xff, b'.', b'g', b'z']);
            fs::write(temp_dir.path().join(non_utf8_name), "non-utf8")
                .await
                .unwrap();
            expected_entries += 1;
        }

        let progress = CountingProgress {
            entries: AtomicUsize::new(0),
        };
        let mut writer = HashFileWriter::new(temp_dir.path().to_path_buf(), "gz");
        let outcome = writer
            .scan_existing_hashes(&CancellationToken::new(), Some(&progress))
            .await
            .unwrap();

        assert_eq!(outcome, ScanOutcome::Completed);
        assert_eq!(progress.entries.load(Ordering::SeqCst), expected_entries);
        assert!(writer.hash_exists("AAAAA").await);
        for hash in ["BBBBB", "CCCCC", "DDDDD", "EEEEE", "FFFFF", "GGGGG"] {
            assert!(!writer.hash_exists(hash).await);
        }
    }

    #[tokio::test]
    async fn scan_existing_hashes_reports_missing_directory() {
        let temp_dir = TempDir::new().unwrap();
        let missing_directory = temp_dir.path().join("missing");
        let mut writer = HashFileWriter::new(missing_directory.clone(), "");

        let error = writer
            .scan_existing_hashes(&CancellationToken::new(), None)
            .await
            .unwrap_err();

        match error {
            WriteError::Scan { path, .. } => {
                assert_eq!(path, missing_directory.display().to_string());
            }
            error => panic!("unexpected scan error: {error:?}"),
        }
    }

    #[tokio::test]
    async fn scan_existing_hashes_cancellation_discards_partial_index() {
        let temp_dir = TempDir::new().unwrap();
        let hash_path = temp_dir.path().join("AAAAA");
        fs::write(&hash_path, "existing").await.unwrap();

        let token = CancellationToken::new();
        let progress = CancellingProgress {
            token: token.clone(),
            entries: AtomicUsize::new(0),
        };
        let mut writer = HashFileWriter::new(temp_dir.path().to_path_buf(), "");

        let outcome = writer
            .scan_existing_hashes(&token, Some(&progress))
            .await
            .unwrap();

        assert_eq!(outcome, ScanOutcome::Cancelled);
        assert_eq!(progress.entries.load(Ordering::SeqCst), 1);
        assert!(writer.existing_hashes.is_none());
    }
}
