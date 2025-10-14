use tokio_util::bytes::Bytes;

/// Trait for writing downloaded hash range data to storage.
/// Allows different strategies for organizing downloaded data (individual files vs combined file).
#[async_trait::async_trait]
pub trait HashWriter: Send + Sync {
    /// Write a downloaded hash range to storage.
    ///
    /// Implementations are responsible for updating the resume cache when data is written to disk.
    ///
    /// # Arguments
    /// * `sequence` - The numeric sequence (0..=0xFFFFF) for ordering
    /// * `hash_prefix` - The 5-character hex prefix (e.g., "00000")
    /// * `data` - The downloaded hash data
    /// * `etag` - Optional ETag for resume/sync functionality
    ///
    /// # Returns
    /// * `Ok(())` - Data was successfully written or buffered
    /// * `Err(_)` - Write operation failed
    async fn write_range(
        &self,
        sequence: u64,
        hash_prefix: &str,
        data: Bytes,
        etag: Option<String>,
    ) -> anyhow::Result<()>;

    /// Finalize any pending writes and flush buffers.
    async fn finalize(&self) -> anyhow::Result<()>;
}
