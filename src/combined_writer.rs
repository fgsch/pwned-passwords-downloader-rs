use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;
use tokio::fs;
use tokio::io::AsyncWriteExt;
use tokio::sync::Mutex;
use tokio_util::bytes::Bytes;

use crate::etag::ETagCache;
use crate::hash_writer::HashWriter;

const COMBINED_OUTPUT_FILENAME: &str = "pwned-passwords-combined.txt";
const DEBUG_LOG_FILENAME: &str = "download_debug.log";

/// Buffered range data: (hash_prefix, data, etag)
type BufferedRange = (String, Bytes, Option<String>);

/// Prepend the 5-character range prefix to each line in the data
/// HIBP API returns hash suffixes (35 chars), we need full 40-char SHA-1 hashes
fn prepend_prefix_to_lines(prefix: &str, data: &[u8]) -> Vec<u8> {
    let data_str = String::from_utf8_lossy(data);
    let mut result = Vec::new();

    for line in data_str.lines() {
        if !line.is_empty() {
            result.extend_from_slice(prefix.as_bytes());
            result.extend_from_slice(line.as_bytes());
            result.push(b'\n');
        }
    }

    // Remove trailing newline if original data didn't have one
    if !data.ends_with(b"\n") && !result.is_empty() {
        result.pop();
    }

    result
}

/// Manages ordered writing of ranges to a single combined file
pub struct CombinedWriter {
    file: Arc<Mutex<fs::File>>,
    debug_log: Arc<Mutex<fs::File>>,
    pending_buffer: Arc<Mutex<BTreeMap<u64, BufferedRange>>>,
    next_sequence: Arc<Mutex<u64>>,
    total_bytes: Arc<Mutex<u64>>,
    total_ranges: Arc<Mutex<u64>>,
    resume_cache: Arc<Mutex<ETagCache>>,
}

impl CombinedWriter {
    pub async fn new(
        output_dir: &Path,
        resume: bool,
        resume_cache: Arc<Mutex<ETagCache>>,
    ) -> anyhow::Result<Self> {
        let output_path = output_dir.join(COMBINED_OUTPUT_FILENAME);
        let debug_log_path = output_dir.join(DEBUG_LOG_FILENAME);

        // If resuming, append to existing file; otherwise truncate
        let file = if resume && output_path.exists() {
            tracing::info!("Resuming combined download, appending to existing file");
            fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&output_path)
                .await?
        } else {
            fs::OpenOptions::new()
                .create(true)
                .write(true)
                .truncate(true)
                .open(&output_path)
                .await?
        };

        // Create debug log (append mode to preserve history across resumes)
        let debug_log = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&debug_log_path)
            .await?;

        Ok(Self {
            file: Arc::new(Mutex::new(file)),
            debug_log: Arc::new(Mutex::new(debug_log)),
            pending_buffer: Arc::new(Mutex::new(BTreeMap::new())),
            next_sequence: Arc::new(Mutex::new(0)),
            total_bytes: Arc::new(Mutex::new(0)),
            total_ranges: Arc::new(Mutex::new(0)),
            resume_cache,
        })
    }

    /// Add a downloaded range to the buffer
    /// Writes any consecutive ranges that are now ready
    async fn add_range(
        &self,
        sequence: u64,
        hash_prefix: String,
        data: Bytes,
        etag: Option<String>,
    ) -> anyhow::Result<()> {
        let mut pending = self.pending_buffer.lock().await;
        pending.insert(sequence, (hash_prefix, data, etag));
        drop(pending);

        // Try to write any consecutive ranges (ETags are cached internally)
        self.write_pending_ranges().await?;

        Ok(())
    }

    /// Write all consecutive pending ranges to disk in order
    /// Updates resume cache immediately for all written ranges
    async fn write_pending_ranges(&self) -> anyhow::Result<()> {
        loop {
            // Check what ranges are ready to write (minimal lock scope)
            let ranges_to_write = {
                let next_seq = self.next_sequence.lock().await;
                let mut pending = self.pending_buffer.lock().await;

                let current_next = *next_seq;
                let mut batch = Vec::new();
                let mut seq = current_next;

                // Collect up to 100 consecutive ranges
                while batch.len() < 100 {
                    if let Some(range) = pending.remove(&seq) {
                        batch.push((seq, range));
                        seq += 1;
                    } else {
                        break;
                    }
                }

                batch
            };

            if ranges_to_write.is_empty() {
                break;
            }

            // Write the batch (locks only what's needed, when needed)
            let mut file = self.file.lock().await;
            let mut debug_log = self.debug_log.lock().await;

            for (seq, (hash_prefix, data, etag)) in ranges_to_write {
                // HIBP API returns hash suffix (35 chars) without the 5-char prefix
                // We need to prepend the range prefix to each line for full 40-char SHA-1 hashes
                let data_with_prefix = prepend_prefix_to_lines(&hash_prefix, &data);

                let needs_newline = !data_with_prefix.ends_with(b"\n");
                let bytes_written = data_with_prefix.len() + if needs_newline { 1 } else { 0 };

                // Try to write this range - if it fails, remove from cache and propagate error
                let write_result = async {
                    file.write_all(&data_with_prefix).await?;
                    if needs_newline {
                        file.write_all(b"\n").await?;
                    }
                    Ok::<(), std::io::Error>(())
                }
                .await;

                match write_result {
                    Ok(()) => {
                        // Update counters
                        {
                            let mut total_bytes = self.total_bytes.lock().await;
                            let mut total_ranges = self.total_ranges.lock().await;
                            *total_bytes += bytes_written as u64;
                            *total_ranges += 1;
                        }

                        // Update resume cache immediately when range is written to disk
                        if let Some(etag_value) = &etag {
                            let mut cache = self.resume_cache.lock().await;
                            cache.etags.insert(hash_prefix.clone(), etag_value.clone());
                        }

                        // Write debug log entry
                        let debug_entry = format!(
                            "seq={:06X} range={} bytes={} etag={}\n",
                            seq,
                            hash_prefix,
                            bytes_written,
                            etag.as_deref().unwrap_or("none")
                        );
                        let _ = debug_log.write_all(debug_entry.as_bytes()).await;

                        // Advance next sequence
                        {
                            let mut next_seq = self.next_sequence.lock().await;
                            *next_seq = seq + 1;
                        }
                    }
                    Err(e) => {
                        drop(file); // Release file lock before other operations
                        drop(debug_log);

                        // Write failed - remove from cache to ensure retry
                        let mut cache = self.resume_cache.lock().await;
                        cache.etags.remove(&hash_prefix);
                        return Err(anyhow::anyhow!("Failed to write range {hash_prefix}: {e}"));
                    }
                }
            }

            // Flush after batch
            file.flush().await?;
            debug_log.flush().await?;
            drop(file); // Release lock before logging/checking
            drop(debug_log);

            // Log progress periodically
            {
                let total_ranges = *self.total_ranges.lock().await;
                if total_ranges % 1000 == 0 {
                    let next_seq = *self.next_sequence.lock().await;
                    let total_bytes = *self.total_bytes.lock().await;
                    let progress_pct = (next_seq as f64 / 0x100000 as f64) * 100.0;
                    let size_gb = total_bytes as f64 / (1024.0 * 1024.0 * 1024.0);

                    tracing::info!(
                        "Combined progress: {:05X}/{:05X} ({:.1}%) - {:.2} GB written",
                        next_seq,
                        0xFFFFF,
                        progress_pct,
                        size_gb
                    );
                }
            }
        }

        Ok(())
    }
}

#[async_trait::async_trait]
impl HashWriter for CombinedWriter {
    async fn write_range(
        &self,
        sequence: u64,
        hash_prefix: &str,
        data: Bytes,
        etag: Option<String>,
    ) -> anyhow::Result<()> {
        self.add_range(sequence, hash_prefix.to_string(), data, etag)
            .await
    }

    async fn finalize(&self) -> anyhow::Result<()> {
        // Write any remaining pending ranges
        self.write_pending_ranges().await?;

        let mut file = self.file.lock().await;
        file.flush().await?;

        let total_ranges = *self.total_ranges.lock().await;
        let total_bytes = *self.total_bytes.lock().await;
        let size_gb = total_bytes as f64 / (1024.0 * 1024.0 * 1024.0);

        tracing::info!(
            "Combined download complete: {} ranges, {:.2} GB written",
            total_ranges,
            size_gb
        );

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_prepend_prefix_to_lines() {
        // Test basic case with multiple lines
        let prefix = "00000";
        let data =
            b"0005AD76BD555C1D6D771DE417A4B87E4B4:10\n000A8DAE4228F821FB418F59826079BF368:4\n";
        let result = prepend_prefix_to_lines(prefix, data);
        let expected = b"000000005AD76BD555C1D6D771DE417A4B87E4B4:10\n00000000A8DAE4228F821FB418F59826079BF368:4\n";
        assert_eq!(result, expected);
    }

    #[test]
    fn test_prepend_prefix_no_trailing_newline() {
        let prefix = "FFFFF";
        // HIBP returns 35 characters (without the 5-char range prefix)
        let data = b"FFEE791CBAC0F6305CAF0CEE06BBE131160:4";
        let result = prepend_prefix_to_lines(prefix, data);
        // Full hash should be 40 chars: 5 (prefix) + 35 (suffix) = 40
        let expected = b"FFFFFFFEE791CBAC0F6305CAF0CEE06BBE131160:4";
        assert_eq!(result, expected);
        assert_eq!(result.iter().take_while(|&&b| b != b':').count(), 40); // Verify hash is 40 chars
    }

    #[test]
    fn test_prepend_prefix_empty_lines_ignored() {
        let prefix = "00001";
        let data =
            b"0005AD76BD555C1D6D771DE417A4B87E4B4:10\n\n000A8DAE4228F821FB418F59826079BF368:4\n";
        let result = prepend_prefix_to_lines(prefix, data);
        let expected = b"000010005AD76BD555C1D6D771DE417A4B87E4B4:10\n00001000A8DAE4228F821FB418F59826079BF368:4\n";
        assert_eq!(result, expected);
    }

    #[test]
    fn test_prepend_prefix_single_line() {
        let prefix = "ABC12";
        let data = b"34567890123456789012345678901234:999\n";
        let result = prepend_prefix_to_lines(prefix, data);
        let expected = b"ABC1234567890123456789012345678901234:999\n";
        assert_eq!(result, expected);
    }
}
