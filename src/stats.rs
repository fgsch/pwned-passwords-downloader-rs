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
use crate::metrics::{LatencySnapshot, LatencyStats};
use crate::writer::WriteMetrics;

#[derive(Debug)]
pub struct RunStats {
    started_at: Instant,
    total_targets: u64,
    processed: AtomicU64,
    end_to_end_nanos: AtomicU64,
    downloaded_or_updated: AtomicU64,
    not_modified: AtomicU64,
    cancelled: AtomicU64,
    requests: AtomicU64,
    responses_ok: AtomicU64,
    responses_not_modified: AtomicU64,
    responses_too_many_requests: AtomicU64,
    responses_other_client_error: AtomicU64,
    responses_server_error: AtomicU64,
    responses_other: AtomicU64,
    network_errors: AtomicU64,
    request_latency: LatencyStats,
    bytes_written: AtomicU64,
    body_read_latency: LatencyStats,
    file_write_latency: LatencyStats,
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
    pub requests: u64,
    pub responses_ok: u64,
    pub responses_not_modified: u64,
    pub responses_too_many_requests: u64,
    pub responses_other_client_error: u64,
    pub responses_server_error: u64,
    pub responses_other: u64,
    pub network_errors: u64,
    pub request_latency: Option<LatencySnapshot>,
    pub bytes_written: u64,
    pub body_read_latency: Option<LatencySnapshot>,
    pub file_write_latency: Option<LatencySnapshot>,
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
            end_to_end_nanos: AtomicU64::new(0),
            not_modified: AtomicU64::new(0),
            cancelled: AtomicU64::new(0),
            error_client: AtomicU64::new(0),
            requests: AtomicU64::new(0),
            responses_ok: AtomicU64::new(0),
            responses_not_modified: AtomicU64::new(0),
            responses_too_many_requests: AtomicU64::new(0),
            responses_other_client_error: AtomicU64::new(0),
            responses_server_error: AtomicU64::new(0),
            responses_other: AtomicU64::new(0),
            network_errors: AtomicU64::new(0),
            request_latency: LatencyStats::new(),
            bytes_written: AtomicU64::new(0),
            body_read_latency: LatencyStats::new(),
            file_write_latency: LatencyStats::new(),
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

    pub fn record_retries(&self, retries_used: u64) {
        self.total_retries
            .fetch_add(retries_used, Ordering::Relaxed);
        if retries_used > 0 {
            self.retried_requests.fetch_add(1, Ordering::Relaxed);
        }
    }

    pub fn record_end_to_end(&self, duration: Duration) {
        let nanos = duration.as_nanos().min(u64::MAX as u128) as u64;
        self.end_to_end_nanos.fetch_add(nanos, Ordering::Relaxed);
    }

    /// Records one completed HTTP attempt, independently of its final hash outcome.
    pub fn record_response(&self, status: reqwest::StatusCode, latency: Duration) {
        self.requests.fetch_add(1, Ordering::Relaxed);
        self.request_latency.record(latency);
        let counter = match status {
            reqwest::StatusCode::OK => &self.responses_ok,
            reqwest::StatusCode::NOT_MODIFIED => &self.responses_not_modified,
            reqwest::StatusCode::TOO_MANY_REQUESTS => &self.responses_too_many_requests,
            status if status.is_client_error() => &self.responses_other_client_error,
            status if status.is_server_error() => &self.responses_server_error,
            _ => &self.responses_other,
        };
        counter.fetch_add(1, Ordering::Relaxed);
    }

    /// Records a completed transport failure, not an unfinished cancelled attempt.
    pub fn record_network_error(&self, latency: Duration) {
        self.requests.fetch_add(1, Ordering::Relaxed);
        self.network_errors.fetch_add(1, Ordering::Relaxed);
        self.request_latency.record(latency);
    }

    /// Records finalized output independently of HTTP attempts and final hash outcomes.
    pub fn record_write(&self, metrics: WriteMetrics) {
        self.bytes_written
            .fetch_add(metrics.bytes_written, Ordering::Relaxed);
        self.body_read_latency.record(metrics.body_read);
        self.file_write_latency.record(metrics.file_write);
    }

    /// Records processed, error-category, and cancellation counters for one
    /// download error. Retry counts are recorded separately with
    /// `record_retries`.
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

    /// Returns a best-effort snapshot. Concurrent work may make fields reflect
    /// slightly different points in time; snapshot after work stops for final totals.
    pub fn snapshot(&self, was_cancelled: bool) -> RunStatsSnapshot {
        let elapsed = self.started_at.elapsed();
        let elapsed_secs = elapsed.as_secs_f64().max(f64::EPSILON);
        let processed = self.processed.load(Ordering::Relaxed);
        let avg_end_to_end_per_processed_secs = if processed > 0 {
            Some(
                self.end_to_end_nanos.load(Ordering::Relaxed) as f64
                    / processed as f64
                    / 1_000_000_000.0,
            )
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
            requests: self.requests.load(Ordering::Relaxed),
            responses_ok: self.responses_ok.load(Ordering::Relaxed),
            responses_not_modified: self.responses_not_modified.load(Ordering::Relaxed),
            responses_too_many_requests: self.responses_too_many_requests.load(Ordering::Relaxed),
            responses_other_client_error: self.responses_other_client_error.load(Ordering::Relaxed),
            responses_server_error: self.responses_server_error.load(Ordering::Relaxed),
            responses_other: self.responses_other.load(Ordering::Relaxed),
            network_errors: self.network_errors.load(Ordering::Relaxed),
            request_latency: self.request_latency.snapshot(),
            bytes_written: self.bytes_written.load(Ordering::Relaxed),
            body_read_latency: self.body_read_latency.snapshot(),
            file_write_latency: self.file_write_latency.snapshot(),
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
                "cancelled={cancelled} requests={requests} ",
                "response=200:{ok} 304:{not_modified} 429:{too_many_requests} ",
                "4xx:{client_error} 5xx:{server_error} other:{other} ",
                "network_errors={network_errors} request_latency={request_latency} ",
                "body_read={body_read} file_write={file_write} bytes_written={bytes_written} ",
                "{retries} {errors}"
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
            requests = s.requests,
            ok = s.responses_ok,
            not_modified = s.responses_not_modified,
            too_many_requests = s.responses_too_many_requests,
            client_error = s.responses_other_client_error,
            server_error = s.responses_server_error,
            other = s.responses_other,
            network_errors = s.network_errors,
            request_latency = format_latency(s.request_latency),
            body_read = format_latency(s.body_read_latency),
            file_write = format_latency(s.file_write_latency),
            bytes_written = s.bytes_written,
            retries = retry_segment,
            errors = error_segment
        )
    }
}

fn format_latency(latency: Option<LatencySnapshot>) -> String {
    latency.map_or_else(
        || "none".to_string(),
        |latency| {
            format!(
                "min={:.3}ms avg={:.3}ms p50{} p95{} max={:.3}ms",
                latency.min_ms,
                latency.average_ms,
                format_latency_bound(latency.p50_upper_bound_ms),
                format_latency_bound(latency.p95_upper_bound_ms),
                latency.max_ms
            )
        },
    )
}

fn format_latency_bound(bound_ms: u64) -> String {
    if bound_ms == u64::MAX {
        ">500ms".to_string()
    } else {
        format!("<={bound_ms}ms")
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
        for duration_ms in [100, 200, 300, 400] {
            stats.record_end_to_end(Duration::from_millis(duration_ms));
        }
        stats.record_downloaded();
        stats.record_downloaded();
        stats.record_not_modified();
        stats.record_error(&DownloadError::Cancelled {
            hash: "AAAAA".to_string(),
        });
        stats.record_retries(0);
        stats.record_retries(2);

        let snapshot = stats.snapshot(true);
        assert_eq!(snapshot.total_targets, 42);
        assert_eq!(snapshot.processed, 4);
        assert_eq!(snapshot.downloaded_or_updated, 2);
        assert_eq!(snapshot.total_retries, 2);
        assert_eq!(snapshot.retried_requests, 1);
        assert_eq!(snapshot.avg_end_to_end_per_processed_secs, Some(0.25));
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

    #[test]
    fn completed_attempts_do_not_count_as_final_outcomes() {
        let stats = RunStats::new(1);
        let latency = Duration::from_millis(10);
        for status in [
            StatusCode::TOO_MANY_REQUESTS,
            StatusCode::SERVICE_UNAVAILABLE,
            StatusCode::OK,
        ] {
            stats.record_response(status, latency);
        }
        stats.record_network_error(latency);

        let attempts = stats.snapshot(false);
        assert_eq!(attempts.requests, 4);
        assert_eq!(attempts.responses_too_many_requests, 1);
        assert_eq!(attempts.responses_server_error, 1);
        assert_eq!(attempts.responses_ok, 1);
        assert_eq!(attempts.network_errors, 1);
        assert_eq!(attempts.request_latency.unwrap().count, 4);
        assert_eq!(attempts.processed, 0);
        assert_eq!(attempts.errors_total, 0);
        assert_eq!(attempts.total_retries, 0);

        stats.record_downloaded();
        stats.record_retries(3);
        let outcome = stats.snapshot(false);
        assert_eq!(outcome.processed, 1);
        assert_eq!(outcome.downloaded_or_updated, 1);
        assert_eq!(outcome.errors_total, 0);
        assert_eq!(outcome.error_network, 0);
        assert_eq!(outcome.requests, 4);
        assert_eq!(outcome.total_retries, 3);
        assert_eq!(outcome.retried_requests, 1);
    }

    #[test]
    fn finalized_writes_aggregate_independently_of_attempts_and_outcomes() {
        let stats = RunStats::new(2);
        for (bytes_written, read_ms, write_ms) in [(1_073_741_825, 2, 10), (7, 6, 30)] {
            stats.record_write(WriteMetrics {
                bytes_written,
                body_read: Duration::from_millis(read_ms),
                file_write: Duration::from_millis(write_ms),
            });
        }

        let writes = stats.snapshot(false);
        assert_eq!(writes.bytes_written, 1_073_741_832);
        let body_read = writes.body_read_latency.unwrap();
        assert_eq!(body_read.count, 2);
        assert_eq!(body_read.min_ms, 2.0);
        assert_eq!(body_read.average_ms, 4.0);
        assert_eq!(body_read.max_ms, 6.0);
        let file_write = writes.file_write_latency.unwrap();
        assert_eq!(file_write.count, 2);
        assert_eq!(file_write.min_ms, 10.0);
        assert_eq!(file_write.average_ms, 20.0);
        assert_eq!(file_write.max_ms, 30.0);
        assert_eq!(writes.processed, 0);
        assert_eq!(writes.downloaded_or_updated, 0);
        assert_eq!(writes.not_modified, 0);
        assert_eq!(writes.cancelled, 0);
        assert_eq!(writes.requests, 0);
        assert_eq!(writes.responses_ok, 0);
        assert_eq!(writes.network_errors, 0);
        assert_eq!(writes.request_latency, None);
        assert_eq!(writes.errors_total, 0);
        assert_eq!(writes.total_retries, 0);
        assert_eq!(writes.retried_requests, 0);

        stats.record_response(StatusCode::OK, Duration::from_millis(1));
        stats.record_downloaded();
        stats.record_retries(2);
        stats.record_error(&DownloadError::Cancelled {
            hash: "AAAAA".to_string(),
        });
        let outcome = stats.snapshot(false);
        assert_eq!(outcome.bytes_written, writes.bytes_written);
        assert_eq!(outcome.body_read_latency, writes.body_read_latency);
        assert_eq!(outcome.file_write_latency, writes.file_write_latency);

        let line = RunStats::summary_line(&outcome);
        assert!(line.contains("body_read=min=2.000ms avg=4.000ms"));
        assert!(line.contains("file_write=min=10.000ms avg=20.000ms"));
        assert!(line.contains("bytes_written=1073741832 retries="));
    }

    #[test]
    fn unchanged_outcomes_do_not_produce_io_samples() {
        let stats = RunStats::new(1);
        stats.record_response(StatusCode::NOT_MODIFIED, Duration::from_millis(1));
        stats.record_not_modified();

        let snapshot = stats.snapshot(false);
        assert_eq!(snapshot.bytes_written, 0);
        assert_eq!(snapshot.body_read_latency, None);
        assert_eq!(snapshot.file_write_latency, None);
        let line = RunStats::summary_line(&snapshot);
        assert!(line.contains("body_read=none file_write=none bytes_written=0 retries="));
    }

    #[test]
    fn responses_are_partitioned_by_raw_status() {
        let stats = RunStats::new(1);
        for status in [
            StatusCode::OK,
            StatusCode::NOT_MODIFIED,
            StatusCode::TOO_MANY_REQUESTS,
            StatusCode::BAD_REQUEST,
            StatusCode::NOT_FOUND,
            StatusCode::INTERNAL_SERVER_ERROR,
            StatusCode::CREATED,
            StatusCode::FOUND,
        ] {
            stats.record_response(status, Duration::from_millis(2));
        }

        let snapshot = stats.snapshot(false);
        assert_eq!(snapshot.requests, 8);
        assert_eq!(snapshot.responses_ok, 1);
        assert_eq!(snapshot.responses_not_modified, 1);
        assert_eq!(snapshot.responses_too_many_requests, 1);
        assert_eq!(snapshot.responses_other_client_error, 2);
        assert_eq!(snapshot.responses_server_error, 1);
        assert_eq!(snapshot.responses_other, 2);
        assert_eq!(snapshot.network_errors, 0);
        let line = RunStats::summary_line(&snapshot);
        assert!(line.contains("requests=8"));
        assert!(line.contains("response=200:1 304:1 429:1 4xx:2 5xx:1 other:2"));
    }

    #[test]
    fn average_uses_measured_operations_instead_of_elapsed_wall_time() {
        let stats = RunStats::new(2);
        stats.record_end_to_end(Duration::from_millis(125));
        stats.record_downloaded();
        stats.record_end_to_end(Duration::from_millis(375));
        stats.record_not_modified();
        let snapshot = stats.snapshot(false);

        assert_eq!(snapshot.avg_end_to_end_per_processed_secs, Some(0.25));
        assert!(RunStats::summary_line(&snapshot).contains("avg=250.000ms"));
    }

    #[test]
    fn request_latency_summary_labels_units_and_overflow_truthfully() {
        let stats = RunStats::new(0);
        assert!(RunStats::summary_line(&stats.snapshot(false)).contains("request_latency=none"));
        stats.record_network_error(Duration::from_micros(500_001));
        let line = RunStats::summary_line(&stats.snapshot(false));

        assert!(line.contains("network_errors=1"));
        assert!(line.contains("request_latency=min=500.001ms avg=500.001ms"));
        assert!(line.contains("p50>500ms p95>500ms max=500.001ms"));
        assert!(!line.contains("p50<=500"));
        assert!(!line.contains("p95<=500"));
    }

    #[test]
    fn request_latency_summary_reports_finite_percentile_upper_bounds() {
        let stats = RunStats::new(0);
        stats.record_response(StatusCode::OK, Duration::from_micros(1_500));
        let line = RunStats::summary_line(&stats.snapshot(false));

        assert!(line.contains("min=1.500ms avg=1.500ms"));
        assert!(line.contains("p50<=5ms p95<=5ms max=1.500ms"));
    }
}
