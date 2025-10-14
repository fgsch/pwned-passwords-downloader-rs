use std::path::PathBuf;
use std::sync::Arc;
use tokio::fs;
use tokio::io::AsyncWriteExt;
use tokio::sync::Mutex;
use tokio_util::bytes::Bytes;

use crate::args::Args;
use crate::etag::ETagCache;
use crate::hash_writer::HashWriter;

const DEBUG_LOG_FILENAME: &str = "download_debug.log";

/// Writer that saves each hash range to a separate file.
pub struct IndividualFileWriter {
    args: Args,
    resume_cache: Arc<Mutex<ETagCache>>,
    debug_log: Arc<Mutex<fs::File>>,
}

impl IndividualFileWriter {
    pub async fn new(args: Args, resume_cache: Arc<Mutex<ETagCache>>) -> anyhow::Result<Self> {
        let debug_log_path = args.output_directory.join(DEBUG_LOG_FILENAME);

        // Create debug log (append mode to preserve history across resumes)
        let debug_log = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&debug_log_path)
            .await?;

        Ok(Self {
            args,
            resume_cache,
            debug_log: Arc::new(Mutex::new(debug_log)),
        })
    }
}

#[async_trait::async_trait]
impl HashWriter for IndividualFileWriter {
    async fn write_range(
        &self,
        sequence: u64,
        hash_prefix: &str,
        data: Bytes,
        etag: Option<String>,
    ) -> anyhow::Result<()> {
        let ext = self.args.compression.as_str();
        let final_path = self
            .args
            .output_directory
            .join(hash_prefix)
            .with_extension(ext);

        let bytes_written = data.len();

        match write_data_to_file(data, &final_path).await {
            Ok(()) => {
                // Update resume cache immediately after successful write
                if let Some(etag_value) = &etag {
                    let mut cache = self.resume_cache.lock().await;
                    cache
                        .etags
                        .insert(hash_prefix.to_string(), etag_value.clone());
                }

                // Write debug log entry
                let mut debug_log = self.debug_log.lock().await;
                let debug_entry = format!(
                    "seq={:06X} range={} bytes={} etag={} file={}\n",
                    sequence,
                    hash_prefix,
                    bytes_written,
                    etag.as_deref().unwrap_or("none"),
                    final_path.display()
                );
                let _ = debug_log.write_all(debug_entry.as_bytes()).await;
                let _ = debug_log.flush().await;

                Ok(())
            }
            Err(e) => {
                // Remove from cache on write error to ensure retry on next run
                let mut cache = self.resume_cache.lock().await;
                cache.etags.remove(hash_prefix);
                Err(anyhow::anyhow!("Failed to write file: {e:?}"))
            }
        }
    }

    async fn finalize(&self) -> anyhow::Result<()> {
        // No finalization needed for individual files
        Ok(())
    }
}

/// Write bytes data to a file using the part file pattern
pub async fn write_data_to_file(data: Bytes, final_path: &PathBuf) -> anyhow::Result<()> {
    let part_path = final_path.with_extension("part");

    let mut file = fs::File::create(&part_path)
        .await
        .map_err(|e| anyhow::anyhow!("Failed to create file {}: {}", part_path.display(), e))?;

    file.write_all(&data)
        .await
        .map_err(|e| anyhow::anyhow!("Failed to write to file {}: {}", part_path.display(), e))?;

    file.flush()
        .await
        .map_err(|e| anyhow::anyhow!("Failed to flush file {}: {}", part_path.display(), e))?;

    if let Err(e) = fs::rename(&part_path, final_path).await {
        // Clean up the part file on rename failure
        let _ = fs::remove_file(&part_path).await;
        return Err(anyhow::anyhow!(
            "Failed to rename {} to {}: {}",
            part_path.display(),
            final_path.display(),
            e
        ));
    }

    Ok(())
}
