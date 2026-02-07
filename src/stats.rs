// Copyright (c) 2026 Federico G. Schwindt <fgsch@lodoss.net>
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

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use crate::download::DownloadError;

#[derive(Debug)]
pub struct RunStats {
    started_at: Instant,
    total_targets: u64,
    processed: AtomicU64,
    downloaded_or_updated: AtomicU64,
    not_modified: AtomicU64,
    cancelled: AtomicU64,
    error_client: AtomicU64,
    error_http: AtomicU64,
    error_network: AtomicU64,
    error_file: AtomicU64,
    total_retries: AtomicU64,
    retried_requests: AtomicU64,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RunStatsSnapshot {
    pub elapsed: Duration,
    pub total_targets: u64,
    pub processed: u64,
    pub downloaded_or_updated: u64,
    pub not_modified: u64,
    pub cancelled: u64,
    pub error_client: u64,
    pub error_http: u64,
    pub error_network: u64,
    pub error_file: u64,
    pub errors_total: u64,
    pub total_retries: u64,
    pub retried_requests: u64,
    pub avg_end_to_end_per_processed_secs: Option<f64>,
    pub throughput_hashes_per_sec: f64,
    pub was_cancelled: bool,
}

impl RunStats {
    pub fn new(total_targets: u64) -> Self {
        Self {
            started_at: Instant::now(),
            total_targets,
            processed: AtomicU64::new(0),
            downloaded_or_updated: AtomicU64::new(0),
            not_modified: AtomicU64::new(0),
            cancelled: AtomicU64::new(0),
            error_client: AtomicU64::new(0),
            error_http: AtomicU64::new(0),
            error_network: AtomicU64::new(0),
            error_file: AtomicU64::new(0),
            total_retries: AtomicU64::new(0),
            retried_requests: AtomicU64::new(0),
        }
    }

    fn mark_processed(&self) {
        self.processed.fetch_add(1, Ordering::Relaxed);
    }

    pub fn record_downloaded(&self) {
        self.mark_processed();
        self.downloaded_or_updated.fetch_add(1, Ordering::Relaxed);
    }

    pub fn record_not_modified(&self) {
        self.mark_processed();
        self.not_modified.fetch_add(1, Ordering::Relaxed);
    }

    pub fn record_cancelled(&self) {
        self.mark_processed();
        self.cancelled.fetch_add(1, Ordering::Relaxed);
    }

    pub fn record_retries(&self, retries_used: u64) {
        self.total_retries
            .fetch_add(retries_used, Ordering::Relaxed);
        if retries_used > 0 {
            self.retried_requests.fetch_add(1, Ordering::Relaxed);
        }
    }

    pub fn record_error(&self, err: &DownloadError) {
        self.mark_processed();
        match err {
            DownloadError::Client { .. } => {
                self.error_client.fetch_add(1, Ordering::Relaxed);
            }
            DownloadError::Http { .. } => {
                self.error_http.fetch_add(1, Ordering::Relaxed);
            }
            DownloadError::Network { .. } => {
                self.error_network.fetch_add(1, Ordering::Relaxed);
            }
            DownloadError::FileOperation { .. } => {
                self.error_file.fetch_add(1, Ordering::Relaxed);
            }
            DownloadError::Cancelled { .. } => {
                self.cancelled.fetch_add(1, Ordering::Relaxed);
            }
        }
    }

    pub fn snapshot(&self, was_cancelled: bool) -> RunStatsSnapshot {
        let elapsed = self.started_at.elapsed();
        let elapsed_secs = elapsed.as_secs_f64().max(f64::EPSILON);
        let processed = self.processed.load(Ordering::Relaxed);
        let avg_end_to_end_per_processed_secs = if processed > 0 {
            Some(elapsed.as_secs_f64() / processed as f64)
        } else {
            None
        };
        let error_client = self.error_client.load(Ordering::Relaxed);
        let error_http = self.error_http.load(Ordering::Relaxed);
        let error_network = self.error_network.load(Ordering::Relaxed);
        let error_file = self.error_file.load(Ordering::Relaxed);
        let total_retries = self.total_retries.load(Ordering::Relaxed);
        let retried_requests = self.retried_requests.load(Ordering::Relaxed);
        let errors_total = error_client + error_http + error_network + error_file;

        RunStatsSnapshot {
            elapsed,
            total_targets: self.total_targets,
            processed,
            downloaded_or_updated: self.downloaded_or_updated.load(Ordering::Relaxed),
            not_modified: self.not_modified.load(Ordering::Relaxed),
            cancelled: self.cancelled.load(Ordering::Relaxed),
            error_client,
            error_http,
            error_network,
            error_file,
            errors_total,
            total_retries,
            retried_requests,
            avg_end_to_end_per_processed_secs,
            throughput_hashes_per_sec: processed as f64 / elapsed_secs,
            was_cancelled,
        }
    }

    pub fn log_summary(&self, was_cancelled: bool) {
        let s = self.snapshot(was_cancelled);
        tracing::info!("{}", Self::summary_line(&s));
    }

    fn summary_line(s: &RunStatsSnapshot) -> String {
        let status = if s.was_cancelled { "cancelled" } else { "ok" };
        let elapsed_secs = s.elapsed.as_secs_f64();
        let progress_pct = if s.total_targets > 0 {
            (s.processed as f64 / s.total_targets as f64) * 100.0
        } else {
            0.0
        };
        let avg_ms = s
            .avg_end_to_end_per_processed_secs
            .map_or(0.0, |secs| secs * 1_000.0);
        let retry_segment = if s.total_retries > 0 {
            format!(
                "retries={} (requests={})",
                s.total_retries, s.retried_requests
            )
        } else {
            format!("retries={}", s.total_retries)
        };
        let error_segment = if s.errors_total > 0 {
            format!(
                "errors={} (client={} http={} network={} file={})",
                s.errors_total, s.error_client, s.error_http, s.error_network, s.error_file
            )
        } else {
            format!("errors={}", s.errors_total)
        };

        format!(
            concat!(
                "Run stats status={status} elapsed={elapsed:.2}s ",
                "targets={targets} processed={processed} progress_pct={progress:.2} ",
                "rate_hps={rate:.1} avg={avg_ms:.3}ms updated={updated} unchanged={unchanged} ",
                "cancelled={cancelled} {retries} {errors}"
            ),
            status = status,
            elapsed = elapsed_secs,
            targets = s.total_targets,
            processed = s.processed,
            progress = progress_pct,
            rate = s.throughput_hashes_per_sec,
            avg_ms = avg_ms,
            updated = s.downloaded_or_updated,
            unchanged = s.not_modified,
            cancelled = s.cancelled,
            retries = retry_segment,
            errors = error_segment
        )
    }
}

#[cfg(test)]
mod tests {
    use reqwest::StatusCode;

    use super::*;
    use crate::writer::WriteError;

    #[test]
    fn snapshot_counts_success_paths() {
        let stats = RunStats::new(42);
        stats.record_downloaded();
        stats.record_downloaded();
        stats.record_not_modified();
        stats.record_cancelled();
        stats.record_retries(0);
        stats.record_retries(2);

        let snapshot = stats.snapshot(true);
        assert_eq!(snapshot.total_targets, 42);
        assert_eq!(snapshot.processed, 4);
        assert_eq!(snapshot.downloaded_or_updated, 2);
        assert_eq!(snapshot.total_retries, 2);
        assert_eq!(snapshot.retried_requests, 1);
        assert!(snapshot.avg_end_to_end_per_processed_secs.is_some());
        assert!(
            snapshot
                .avg_end_to_end_per_processed_secs
                .unwrap()
                .is_sign_positive()
        );
        assert_eq!(snapshot.not_modified, 1);
        assert_eq!(snapshot.cancelled, 1);
        assert_eq!(snapshot.errors_total, 0);
        assert!(snapshot.throughput_hashes_per_sec.is_finite());
        assert!(snapshot.was_cancelled);
    }

    #[test]
    fn snapshot_counts_error_categories() {
        let stats = RunStats::new(4);
        stats.record_error(&DownloadError::Client {
            hash: "AAAAA".to_string(),
            status_code: StatusCode::BAD_REQUEST,
            retries: 0,
        });
        stats.record_error(&DownloadError::Http {
            hash: "BBBBB".to_string(),
            status_code: StatusCode::INTERNAL_SERVER_ERROR,
            retries: 3,
        });
        stats.record_error(&DownloadError::Network {
            hash: "CCCCC".to_string(),
            error: "connection reset".to_string(),
            retries: 3,
        });
        stats.record_error(&DownloadError::FileOperation {
            retries: 0,
            source: WriteError::ReadWrite {
                path: "out/DDDDD".to_string(),
                source: std::io::Error::other("disk full"),
            },
        });
        stats.record_retries(4);
        stats.record_retries(1);

        let snapshot = stats.snapshot(false);
        assert_eq!(snapshot.processed, 4);
        assert_eq!(snapshot.error_client, 1);
        assert_eq!(snapshot.total_retries, 5);
        assert_eq!(snapshot.error_http, 1);
        assert_eq!(snapshot.error_network, 1);
        assert_eq!(snapshot.error_file, 1);
        assert!(snapshot.avg_end_to_end_per_processed_secs.is_some());
        assert_eq!(snapshot.errors_total, 4);
        assert!(!snapshot.was_cancelled);
    }

    #[test]
    fn snapshot_reports_no_average_when_nothing_processed() {
        let stats = RunStats::new(10);
        let snapshot = stats.snapshot(false);

        assert_eq!(snapshot.processed, 0);
        assert_eq!(snapshot.avg_end_to_end_per_processed_secs, None);
        assert_eq!(snapshot.throughput_hashes_per_sec, 0.0);
    }

    #[test]
    fn summary_line_omits_subcategories_when_totals_are_zero() {
        let stats = RunStats::new(10);
        stats.record_downloaded();
        let snapshot = stats.snapshot(false);

        let line = RunStats::summary_line(&snapshot);
        assert!(line.contains("retries=0"));
        assert!(line.contains("errors=0"));
        assert!(!line.contains("(requests="));
        assert!(!line.contains("(client="));
    }

    #[test]
    fn summary_line_includes_subcategories_when_totals_are_non_zero() {
        let stats = RunStats::new(10);
        stats.record_downloaded();
        stats.record_retries(2);
        stats.record_error(&DownloadError::Http {
            hash: "AAAAA".to_string(),
            status_code: StatusCode::INTERNAL_SERVER_ERROR,
            retries: 1,
        });
        let snapshot = stats.snapshot(true);

        let line = RunStats::summary_line(&snapshot);
        assert!(line.contains("status=cancelled"));
        assert!(line.contains("retries=2 (requests=1)"));
        assert!(line.contains("errors=1 (client=0 http=1 network=0 file=0)"));
    }
}
