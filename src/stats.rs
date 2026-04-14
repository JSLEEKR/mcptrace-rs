//! Aggregate statistics over a slice of spans.
//!
//! Powers the `mcptrace stats` subcommand. Given a `Vec<Span>`, compute:
//!
//! - total count
//! - per-tool breakdown
//! - p50 / p95 / p99 latency in milliseconds
//! - error rate
//! - average / total bytes moved
//!
//! Percentiles are computed with the classic "nearest-rank" method
//! (Hyndman & Fan type 1), which is both correct and stable and, most
//! importantly, trivially testable with hand-computed vectors.

use crate::span::Span;
use std::collections::BTreeMap;

/// Aggregate stats for a homogeneous group of spans (e.g., one tool).
#[derive(Debug, Clone, PartialEq)]
pub struct GroupStats {
    pub count: usize,
    pub errors: usize,
    pub p50_ms: u64,
    pub p95_ms: u64,
    pub p99_ms: u64,
    pub min_ms: u64,
    pub max_ms: u64,
    pub avg_ms: f64,
    pub total_request_bytes: u64,
    pub total_response_bytes: u64,
}

impl GroupStats {
    /// Fraction of spans that ended in a non-OK status.
    #[must_use]
    pub fn error_rate(&self) -> f64 {
        if self.count == 0 {
            0.0
        } else {
            self.errors as f64 / self.count as f64
        }
    }
}

/// Compute the nearest-rank percentile of a sorted `Vec<u64>`.
///
/// `p` is a fraction in `[0.0, 1.0]`. Returns 0 for empty input.
#[must_use]
pub fn percentile_sorted(sorted: &[u64], p: f64) -> u64 {
    if sorted.is_empty() {
        return 0;
    }
    let p = p.clamp(0.0, 1.0);
    // nearest-rank: ceil(p * N)
    let rank = (p * sorted.len() as f64).ceil() as usize;
    let idx = rank.saturating_sub(1).min(sorted.len() - 1);
    sorted[idx]
}

/// Compute stats for a slice of spans.
#[must_use]
pub fn compute(spans: &[Span]) -> GroupStats {
    let mut durs: Vec<u64> = spans.iter().map(|s| s.duration_ms).collect();
    durs.sort_unstable();
    let count = spans.len();
    let errors = spans.iter().filter(|s| !s.status.is_ok()).count();
    let total_request_bytes = spans.iter().map(|s| s.request_bytes).sum();
    let total_response_bytes = spans.iter().map(|s| s.response_bytes).sum();
    let sum_ms: u64 = durs.iter().sum();
    let avg_ms = if count == 0 {
        0.0
    } else {
        sum_ms as f64 / count as f64
    };
    GroupStats {
        count,
        errors,
        p50_ms: percentile_sorted(&durs, 0.50),
        p95_ms: percentile_sorted(&durs, 0.95),
        p99_ms: percentile_sorted(&durs, 0.99),
        min_ms: *durs.first().unwrap_or(&0),
        max_ms: *durs.last().unwrap_or(&0),
        avg_ms,
        total_request_bytes,
        total_response_bytes,
    }
}

/// Compute per-tool stats. Spans without a tool name are grouped under
/// `"<no-tool>"`.
#[must_use]
pub fn compute_by_tool(spans: &[Span]) -> BTreeMap<String, GroupStats> {
    let mut groups: BTreeMap<String, Vec<Span>> = BTreeMap::new();
    for s in spans {
        let key = s
            .tool_name
            .clone()
            .unwrap_or_else(|| "<no-tool>".to_string());
        groups.entry(key).or_default().push(s.clone());
    }
    groups
        .into_iter()
        .map(|(k, v)| (k, compute(&v)))
        .collect()
}

/// Render a nicely formatted ASCII table of per-tool stats.
#[must_use]
pub fn render_table(by_tool: &BTreeMap<String, GroupStats>) -> String {
    use comfy_table::{ContentArrangement, Table};
    let mut t = Table::new();
    t.set_content_arrangement(ContentArrangement::Dynamic);
    t.set_header(vec![
        "tool",
        "count",
        "err_rate",
        "p50_ms",
        "p95_ms",
        "p99_ms",
        "max_ms",
        "avg_ms",
        "req_KiB",
        "res_KiB",
    ]);
    for (tool, stats) in by_tool {
        t.add_row(vec![
            tool.clone(),
            stats.count.to_string(),
            format!("{:.4}", stats.error_rate()),
            stats.p50_ms.to_string(),
            stats.p95_ms.to_string(),
            stats.p99_ms.to_string(),
            stats.max_ms.to_string(),
            format!("{:.2}", stats.avg_ms),
            format!("{:.2}", stats.total_request_bytes as f64 / 1024.0),
            format!("{:.2}", stats.total_response_bytes as f64 / 1024.0),
        ]);
    }
    t.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::span::{Span, SpanStatus};

    fn span(dur: u64, tool: &str, status: SpanStatus) -> Span {
        Span::builder()
            .trace_id("t")
            .span_id(format!("s-{dur}-{tool}"))
            .method("tools/call")
            .tool_name(Some(tool.to_string()))
            .start_unix_nanos(1)
            .duration_ms(dur)
            .request_bytes(100)
            .response_bytes(200)
            .status(status)
            .build()
            .unwrap()
    }

    #[test]
    fn percentile_empty_is_zero() {
        assert_eq!(percentile_sorted(&[], 0.5), 0);
    }

    #[test]
    fn percentile_single() {
        assert_eq!(percentile_sorted(&[42], 0.5), 42);
        assert_eq!(percentile_sorted(&[42], 0.99), 42);
    }

    #[test]
    fn percentile_p50_small() {
        // sorted 1..=10, p50 -> ceil(0.5*10)=5, idx=4 -> value 5
        let v: Vec<u64> = (1..=10).collect();
        assert_eq!(percentile_sorted(&v, 0.50), 5);
    }

    #[test]
    fn percentile_p95_p99_small() {
        let v: Vec<u64> = (1..=100).collect();
        // p95: ceil(95) = 95, idx=94 -> 95
        assert_eq!(percentile_sorted(&v, 0.95), 95);
        // p99: ceil(99) = 99, idx=98 -> 99
        assert_eq!(percentile_sorted(&v, 0.99), 99);
    }

    #[test]
    fn percentile_clamps_out_of_range() {
        let v: Vec<u64> = (1..=10).collect();
        assert_eq!(percentile_sorted(&v, -0.5), 1);
        assert_eq!(percentile_sorted(&v, 1.5), 10);
    }

    #[test]
    fn percentile_p0_is_first() {
        // ceil(0*N) = 0, saturating_sub -> 0, idx 0
        let v: Vec<u64> = (1..=10).collect();
        assert_eq!(percentile_sorted(&v, 0.0), 1);
    }

    #[test]
    fn compute_empty() {
        let s = compute(&[]);
        assert_eq!(s.count, 0);
        assert_eq!(s.p50_ms, 0);
        assert_eq!(s.avg_ms, 0.0);
        assert_eq!(s.error_rate(), 0.0);
    }

    #[test]
    fn compute_all_ok() {
        let v: Vec<Span> = (1..=10).map(|d| span(d, "search", SpanStatus::Ok)).collect();
        let s = compute(&v);
        assert_eq!(s.count, 10);
        assert_eq!(s.errors, 0);
        assert_eq!(s.p50_ms, 5);
        assert_eq!(s.min_ms, 1);
        assert_eq!(s.max_ms, 10);
        assert!((s.avg_ms - 5.5).abs() < 1e-9);
        assert_eq!(s.error_rate(), 0.0);
    }

    #[test]
    fn compute_mixed_status() {
        let mut v: Vec<Span> = (1..=8).map(|d| span(d, "t", SpanStatus::Ok)).collect();
        v.push(span(9, "t", SpanStatus::Error));
        v.push(span(10, "t", SpanStatus::Timeout));
        let s = compute(&v);
        assert_eq!(s.count, 10);
        assert_eq!(s.errors, 2);
        assert!((s.error_rate() - 0.2).abs() < 1e-9);
    }

    #[test]
    fn compute_byte_totals() {
        let v: Vec<Span> = (1..=5).map(|d| span(d, "t", SpanStatus::Ok)).collect();
        let s = compute(&v);
        assert_eq!(s.total_request_bytes, 500);
        assert_eq!(s.total_response_bytes, 1000);
    }

    #[test]
    fn group_by_tool_splits_tools() {
        let mut v = Vec::new();
        v.push(span(5, "search", SpanStatus::Ok));
        v.push(span(10, "search", SpanStatus::Ok));
        v.push(span(100, "write", SpanStatus::Error));
        let by = compute_by_tool(&v);
        assert_eq!(by.len(), 2);
        assert_eq!(by["search"].count, 2);
        assert_eq!(by["write"].count, 1);
        assert_eq!(by["write"].errors, 1);
    }

    #[test]
    fn group_by_tool_handles_no_tool() {
        let s = Span::builder()
            .trace_id("t")
            .span_id("s")
            .method("ping")
            .duration_ms(3)
            .build()
            .unwrap();
        let by = compute_by_tool(&[s]);
        assert!(by.contains_key("<no-tool>"));
    }

    #[test]
    fn render_table_contains_headers_and_rows() {
        let v: Vec<Span> = (1..=5).map(|d| span(d, "search", SpanStatus::Ok)).collect();
        let by = compute_by_tool(&v);
        let out = render_table(&by);
        assert!(out.contains("p95"));
        assert!(out.contains("search"));
    }

    #[test]
    fn render_table_error_rate_formatting() {
        let mut v: Vec<Span> = (1..=9).map(|d| span(d, "t", SpanStatus::Ok)).collect();
        v.push(span(10, "t", SpanStatus::Error));
        let by = compute_by_tool(&v);
        let out = render_table(&by);
        // 1/10 = 0.1000
        assert!(out.contains("0.1000"));
    }

    #[test]
    fn compute_odd_count_median() {
        let v: Vec<Span> = [10, 20, 30, 40, 50]
            .iter()
            .map(|&d| span(d, "t", SpanStatus::Ok))
            .collect();
        let s = compute(&v);
        // ceil(0.5*5)=3 -> idx 2 -> 30
        assert_eq!(s.p50_ms, 30);
    }
}
