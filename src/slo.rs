//! SLO definition, rolling-window evaluation, and burn-rate math.
//!
//! # Model
//!
//! An SLO is an error budget over a rolling time window. We support three
//! metrics:
//!
//! - `latency_p95_ms` — successful-call p95 latency in the window exceeds
//!   `target` → the "bad-event" for budget math is any latency over target.
//! - `error_rate` — fraction of error responses exceeds `target`.
//! - `availability` — 1 - error_rate; useful for monotonic phrasing
//!   (higher = better).
//!
//! # Burn-rate math
//!
//! From the Google SRE workbook, "Alerting on SLOs":
//!
//! ```text
//! error_budget      = 1 - target          (for error_rate)
//! actual_error_rate = errors / total      (over the window)
//! burn_rate         = actual_error_rate / error_budget
//! alert if burn_rate >= burn_rate_threshold
//! ```
//!
//! For latency SLOs we adapt: the "budget" is the target latency and the
//! "actual" is the measured p95. This lets operators phrase both error
//! and latency SLOs in the same alerting model.
//!
//! # Rolling window
//!
//! We keep an in-memory [`RollingWindow`] that drops expired observations
//! on every push. Windows are parsed from strings like `"5m"` or `"1h"`
//! via [`crate::duration::parse_duration_nanos`].

use crate::duration::parse_duration_nanos;
use crate::error::{Error, Result};
use crate::span::{Span, SpanStatus};
use crate::stats::percentile_sorted;
use serde::{Deserialize, Serialize};
use std::collections::VecDeque;

/// Which metric an SLO tracks.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SloMetric {
    LatencyP95Ms,
    ErrorRate,
    Availability,
}

impl SloMetric {
    /// Parse from the TOML string representation.
    pub fn from_str(s: &str) -> Result<Self> {
        match s {
            "latency_p95_ms" => Ok(SloMetric::LatencyP95Ms),
            "error_rate" => Ok(SloMetric::ErrorRate),
            "availability" => Ok(SloMetric::Availability),
            other => Err(Error::InvalidConfig(format!("unknown metric: {other}"))),
        }
    }
}

/// A single SLO from the config file.
#[derive(Debug, Clone, Deserialize)]
pub struct SloConfigEntry {
    pub name: String,
    pub metric: String,
    pub target: f64,
    pub window: String,
    pub burn_rate_threshold: f64,
    #[serde(default = "default_tool_glob")]
    pub tool: String,
}

fn default_tool_glob() -> String {
    "*".to_string()
}

/// Top-level config file shape.
#[derive(Debug, Clone, Deserialize)]
pub struct SloConfigFile {
    #[serde(default)]
    pub slo: Vec<SloConfigEntry>,
}

/// A parsed, validated SLO ready to be evaluated.
#[derive(Debug, Clone)]
pub struct Slo {
    pub name: String,
    pub metric: SloMetric,
    pub target: f64,
    pub window_nanos: u128,
    pub burn_rate_threshold: f64,
    pub tool_glob: String,
}

impl Slo {
    /// Parse and validate an entry.
    pub fn from_entry(e: &SloConfigEntry) -> Result<Self> {
        let metric = SloMetric::from_str(&e.metric)?;
        if !e.target.is_finite() {
            return Err(Error::InvalidConfig(format!(
                "slo {}: target is not finite",
                e.name
            )));
        }
        match metric {
            SloMetric::ErrorRate => {
                if !(0.0..=1.0).contains(&e.target) {
                    return Err(Error::InvalidConfig(format!(
                        "slo {}: error_rate target must be in [0,1], got {}",
                        e.name, e.target
                    )));
                }
            }
            SloMetric::Availability => {
                if !(0.0..=1.0).contains(&e.target) {
                    return Err(Error::InvalidConfig(format!(
                        "slo {}: availability target must be in [0,1], got {}",
                        e.name, e.target
                    )));
                }
            }
            SloMetric::LatencyP95Ms => {
                if e.target < 0.0 {
                    return Err(Error::InvalidConfig(format!(
                        "slo {}: latency target must be non-negative",
                        e.name
                    )));
                }
            }
        }
        if !e.burn_rate_threshold.is_finite() || e.burn_rate_threshold <= 0.0 {
            return Err(Error::InvalidConfig(format!(
                "slo {}: burn_rate_threshold must be positive finite",
                e.name
            )));
        }
        if e.name.trim().is_empty() {
            return Err(Error::InvalidConfig("slo name must be non-empty".into()));
        }
        let window_nanos = parse_duration_nanos(&e.window)?;
        if window_nanos == 0 {
            return Err(Error::InvalidConfig(format!(
                "slo {}: window must be > 0",
                e.name
            )));
        }
        Ok(Slo {
            name: e.name.clone(),
            metric,
            target: e.target,
            window_nanos,
            burn_rate_threshold: e.burn_rate_threshold,
            tool_glob: if e.tool.is_empty() {
                "*".into()
            } else {
                e.tool.clone()
            },
        })
    }

    /// Simple glob match: `*` matches any, otherwise exact match.
    #[must_use]
    pub fn matches_tool(&self, tool: Option<&str>) -> bool {
        if self.tool_glob == "*" {
            return true;
        }
        matches!(tool, Some(t) if t == self.tool_glob)
    }
}

/// Load and parse a TOML slo config file.
pub fn load_config(path: impl AsRef<std::path::Path>) -> Result<Vec<Slo>> {
    let body = std::fs::read_to_string(path)?;
    parse_config_str(&body)
}

/// Parse a TOML slo config from a string.
pub fn parse_config_str(body: &str) -> Result<Vec<Slo>> {
    let cfg: SloConfigFile = toml::from_str(body)?;
    let mut out = Vec::with_capacity(cfg.slo.len());
    for e in &cfg.slo {
        out.push(Slo::from_entry(e)?);
    }
    Ok(out)
}

/// Rolling window of `(start_unix_nanos, metric_value)` pairs.
///
/// Expired entries are lazily dropped on every push.
#[derive(Debug, Clone, Default)]
pub struct RollingWindow {
    window_nanos: u128,
    data: VecDeque<(u128, f64)>,
}

impl RollingWindow {
    #[must_use]
    pub fn new(window_nanos: u128) -> Self {
        Self {
            window_nanos,
            data: VecDeque::new(),
        }
    }
    pub fn push(&mut self, ts_nanos: u128, value: f64) {
        self.data.push_back((ts_nanos, value));
        self.expire(ts_nanos);
    }
    fn expire(&mut self, now_nanos: u128) {
        let cutoff = now_nanos.saturating_sub(self.window_nanos);
        while let Some(&(ts, _)) = self.data.front() {
            if ts < cutoff {
                self.data.pop_front();
            } else {
                break;
            }
        }
    }
    #[must_use]
    pub fn len(&self) -> usize {
        self.data.len()
    }
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.data.is_empty()
    }
    #[must_use]
    pub fn values(&self) -> Vec<f64> {
        self.data.iter().map(|&(_, v)| v).collect()
    }
}

/// Result of evaluating one SLO over a slice of spans.
#[derive(Debug, Clone, PartialEq)]
pub struct SloReport {
    pub name: String,
    pub metric: SloMetric,
    pub target: f64,
    pub actual: f64,
    pub burn_rate: f64,
    pub threshold: f64,
    pub burning: bool,
    pub sample_count: usize,
}

/// Evaluate one SLO over the given spans, using `now_nanos` as the
/// window reference (typically the timestamp of the most recent span
/// or `SystemTime::now` at evaluation time).
#[must_use]
pub fn evaluate(slo: &Slo, spans: &[Span], now_nanos: u128) -> SloReport {
    let cutoff = now_nanos.saturating_sub(slo.window_nanos);
    let relevant: Vec<&Span> = spans
        .iter()
        .filter(|s| s.start_unix_nanos >= cutoff && slo.matches_tool(s.tool_name.as_deref()))
        .collect();

    let (actual, burn_rate) = match slo.metric {
        SloMetric::ErrorRate => {
            let total = relevant.len() as f64;
            let errors = relevant.iter().filter(|s| !s.status.is_ok()).count() as f64;
            let actual = if total == 0.0 { 0.0 } else { errors / total };
            let budget = (1.0 - slo.target).max(1e-12);
            (actual, actual / budget)
        }
        SloMetric::Availability => {
            let total = relevant.len() as f64;
            let ok = relevant.iter().filter(|s| s.status.is_ok()).count() as f64;
            let avail = if total == 0.0 { 1.0 } else { ok / total };
            // "burn" interpreted as how far below target we are.
            // If avail >= target, burn_rate = 0. Otherwise (target-avail)/(1-target).
            let burn = if avail >= slo.target {
                0.0
            } else {
                let budget = (1.0 - slo.target).max(1e-12);
                (slo.target - avail) / budget
            };
            (avail, burn)
        }
        SloMetric::LatencyP95Ms => {
            let mut ok_durs: Vec<u64> = relevant
                .iter()
                .filter(|s| matches!(s.status, SpanStatus::Ok))
                .map(|s| s.duration_ms)
                .collect();
            ok_durs.sort_unstable();
            let p95 = percentile_sorted(&ok_durs, 0.95) as f64;
            let budget = slo.target.max(1e-12);
            (p95, p95 / budget)
        }
    };

    SloReport {
        name: slo.name.clone(),
        metric: slo.metric,
        target: slo.target,
        actual,
        burn_rate,
        threshold: slo.burn_rate_threshold,
        burning: burn_rate >= slo.burn_rate_threshold,
        sample_count: relevant.len(),
    }
}

/// Evaluate every SLO in the config against all spans.
#[must_use]
pub fn evaluate_all(slos: &[Slo], spans: &[Span], now_nanos: u128) -> Vec<SloReport> {
    slos.iter().map(|s| evaluate(s, spans, now_nanos)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::span::{Span, SpanStatus};

    fn mk(dur: u64, status: SpanStatus, tool: &str, ts: u128) -> Span {
        Span::builder()
            .trace_id("t")
            .span_id(format!("s-{ts}-{dur}"))
            .method("tools/call")
            .tool_name(Some(tool.into()))
            .start_unix_nanos(ts)
            .duration_ms(dur)
            .status(status)
            .build()
            .unwrap()
    }

    #[test]
    fn metric_from_str_ok() {
        assert_eq!(
            SloMetric::from_str("latency_p95_ms").unwrap(),
            SloMetric::LatencyP95Ms
        );
        assert_eq!(
            SloMetric::from_str("error_rate").unwrap(),
            SloMetric::ErrorRate
        );
        assert_eq!(
            SloMetric::from_str("availability").unwrap(),
            SloMetric::Availability
        );
    }

    #[test]
    fn metric_from_str_bad() {
        assert!(SloMetric::from_str("foo").is_err());
    }

    #[test]
    fn parse_config_minimal() {
        let body = r#"
[[slo]]
name = "fast"
metric = "latency_p95_ms"
target = 200
window = "5m"
burn_rate_threshold = 2.0
"#;
        let slos = parse_config_str(body).unwrap();
        assert_eq!(slos.len(), 1);
        assert_eq!(slos[0].name, "fast");
        assert_eq!(slos[0].target, 200.0);
        assert_eq!(slos[0].tool_glob, "*");
    }

    #[test]
    fn parse_config_multiple() {
        let body = r#"
[[slo]]
name = "fast"
metric = "latency_p95_ms"
target = 200
window = "5m"
burn_rate_threshold = 2.0

[[slo]]
name = "errs"
metric = "error_rate"
target = 0.01
window = "1h"
burn_rate_threshold = 14.4
tool = "search"
"#;
        let slos = parse_config_str(body).unwrap();
        assert_eq!(slos.len(), 2);
        assert_eq!(slos[1].tool_glob, "search");
    }

    #[test]
    fn parse_config_rejects_bad_window() {
        let body = r#"
[[slo]]
name = "x"
metric = "error_rate"
target = 0.01
window = "nope"
burn_rate_threshold = 2.0
"#;
        assert!(parse_config_str(body).is_err());
    }

    #[test]
    fn parse_config_rejects_error_rate_out_of_range() {
        let body = r#"
[[slo]]
name = "x"
metric = "error_rate"
target = 2.0
window = "5m"
burn_rate_threshold = 2.0
"#;
        assert!(parse_config_str(body).is_err());
    }

    #[test]
    fn parse_config_rejects_nan_target() {
        let body = r#"
[[slo]]
name = "x"
metric = "latency_p95_ms"
target = nan
window = "5m"
burn_rate_threshold = 2.0
"#;
        assert!(parse_config_str(body).is_err());
    }

    #[test]
    fn parse_config_rejects_zero_burn_rate() {
        let body = r#"
[[slo]]
name = "x"
metric = "error_rate"
target = 0.01
window = "5m"
burn_rate_threshold = 0.0
"#;
        assert!(parse_config_str(body).is_err());
    }

    #[test]
    fn parse_config_rejects_empty_name() {
        let body = r#"
[[slo]]
name = ""
metric = "error_rate"
target = 0.01
window = "5m"
burn_rate_threshold = 2.0
"#;
        assert!(parse_config_str(body).is_err());
    }

    #[test]
    fn tool_glob_matches_everything() {
        let slo = Slo {
            name: "x".into(),
            metric: SloMetric::ErrorRate,
            target: 0.01,
            window_nanos: 1_000_000_000,
            burn_rate_threshold: 2.0,
            tool_glob: "*".into(),
        };
        assert!(slo.matches_tool(Some("any")));
        assert!(slo.matches_tool(None));
    }

    #[test]
    fn tool_glob_specific_match() {
        let slo = Slo {
            name: "x".into(),
            metric: SloMetric::ErrorRate,
            target: 0.01,
            window_nanos: 1_000_000_000,
            burn_rate_threshold: 2.0,
            tool_glob: "search".into(),
        };
        assert!(slo.matches_tool(Some("search")));
        assert!(!slo.matches_tool(Some("write")));
        assert!(!slo.matches_tool(None));
    }

    #[test]
    fn rolling_window_expires_old() {
        let mut w = RollingWindow::new(100);
        w.push(10, 1.0);
        w.push(50, 2.0);
        w.push(200, 3.0); // cutoff = 100, so only 200 survives (50 < 100)
        assert_eq!(w.len(), 1);
        assert_eq!(w.values(), vec![3.0]);
    }

    #[test]
    fn rolling_window_keeps_all_in_window() {
        let mut w = RollingWindow::new(1000);
        for i in 0..10 {
            w.push(i, i as f64);
        }
        assert_eq!(w.len(), 10);
    }

    #[test]
    fn rolling_window_empty() {
        let w = RollingWindow::new(100);
        assert!(w.is_empty());
        assert_eq!(w.len(), 0);
    }

    #[test]
    fn evaluate_error_rate_within_budget() {
        let slo = Slo {
            name: "errs".into(),
            metric: SloMetric::ErrorRate,
            target: 0.10,
            window_nanos: 10_000_000_000,
            burn_rate_threshold: 2.0,
            tool_glob: "*".into(),
        };
        let base: u128 = 5_000_000_000;
        // 9 ok, 1 error -> actual 0.10, budget 0.90, burn ~0.111
        let mut v: Vec<Span> = (0..9)
            .map(|i| mk(5, SpanStatus::Ok, "t", base + i as u128))
            .collect();
        v.push(mk(5, SpanStatus::Error, "t", base + 9));
        let r = evaluate(&slo, &v, base + 1_000);
        assert!((r.actual - 0.10).abs() < 1e-9);
        assert!(!r.burning);
    }

    #[test]
    fn evaluate_error_rate_burning() {
        // target=0.9, budget=0.1, 50 err/100, actual=0.5, burn=5 -> burning.
        let slo2 = Slo {
            name: "errs".into(),
            metric: SloMetric::ErrorRate,
            target: 0.9,
            window_nanos: 10_000_000_000,
            burn_rate_threshold: 2.0,
            tool_glob: "*".into(),
        };
        let base: u128 = 5_000_000_000;
        let mut v2: Vec<Span> = (0..50)
            .map(|i| mk(5, SpanStatus::Ok, "t", base + i as u128))
            .collect();
        for i in 0..50 {
            v2.push(mk(5, SpanStatus::Error, "t", base + 100 + i as u128));
        }
        let r = evaluate(&slo2, &v2, base + 1_000);
        assert!(
            r.burning,
            "expected burning at burn_rate {} (actual={}, samples={})",
            r.burn_rate, r.actual, r.sample_count
        );
    }

    #[test]
    fn evaluate_respects_window() {
        let slo = Slo {
            name: "errs".into(),
            metric: SloMetric::ErrorRate,
            target: 0.01,
            window_nanos: 100, // 100 ns window
            burn_rate_threshold: 2.0,
            tool_glob: "*".into(),
        };
        // Old errors outside window get ignored.
        let mut v = Vec::new();
        for i in 0..10 {
            v.push(mk(5, SpanStatus::Error, "t", i));
        }
        v.push(mk(5, SpanStatus::Ok, "t", 10_000));
        let r = evaluate(&slo, &v, 10_000);
        // Only 1 in window, 0 errors, not burning.
        assert_eq!(r.sample_count, 1);
        assert!(!r.burning);
    }

    #[test]
    fn evaluate_latency_p95_burning() {
        let slo = Slo {
            name: "lat".into(),
            metric: SloMetric::LatencyP95Ms,
            target: 100.0,
            window_nanos: 10_000_000_000,
            burn_rate_threshold: 2.0,
            tool_glob: "*".into(),
        };
        let base: u128 = 5_000_000_000;
        // 90 fast + 10 slow: sorted p95 index = ceil(0.95*100)=95, idx 94
        // which falls into the slow bucket (last 10) -> 500ms.
        let mut v: Vec<Span> = (0..90)
            .map(|i| mk(50, SpanStatus::Ok, "t", base + i as u128))
            .collect();
        for i in 0..10 {
            v.push(mk(500, SpanStatus::Ok, "t", base + 100 + i as u128));
        }
        let r = evaluate(&slo, &v, base + 1_000);
        assert!(
            r.burning,
            "expected burning, got actual={} burn_rate={}",
            r.actual, r.burn_rate
        );
        assert!(r.actual >= 200.0);
    }

    #[test]
    fn evaluate_latency_filters_errors() {
        let slo = Slo {
            name: "lat".into(),
            metric: SloMetric::LatencyP95Ms,
            target: 100.0,
            window_nanos: 10_000_000_000,
            burn_rate_threshold: 2.0,
            tool_glob: "*".into(),
        };
        let base: u128 = 5_000_000_000;
        let mut v: Vec<Span> = (0..10)
            .map(|i| mk(10, SpanStatus::Ok, "t", base + i as u128))
            .collect();
        for i in 0..10 {
            v.push(mk(9999, SpanStatus::Error, "t", base + 100 + i as u128));
        }
        let r = evaluate(&slo, &v, base + 1_000);
        assert!(r.actual < 100.0);
        assert!(!r.burning);
    }

    #[test]
    fn evaluate_availability_at_target() {
        let slo = Slo {
            name: "avail".into(),
            metric: SloMetric::Availability,
            target: 0.95,
            window_nanos: 10_000_000_000,
            burn_rate_threshold: 2.0,
            tool_glob: "*".into(),
        };
        let base: u128 = 5_000_000_000;
        let mut v: Vec<Span> = (0..95)
            .map(|i| mk(5, SpanStatus::Ok, "t", base + i as u128))
            .collect();
        for i in 0..5 {
            v.push(mk(5, SpanStatus::Error, "t", base + 100 + i as u128));
        }
        let r = evaluate(&slo, &v, base + 1_000);
        assert!((r.actual - 0.95).abs() < 1e-9);
        assert!(!r.burning);
    }

    #[test]
    fn evaluate_availability_burning() {
        let slo = Slo {
            name: "avail".into(),
            metric: SloMetric::Availability,
            target: 0.99,
            window_nanos: 10_000_000_000,
            burn_rate_threshold: 2.0,
            tool_glob: "*".into(),
        };
        let base: u128 = 5_000_000_000;
        let mut v: Vec<Span> = (0..50)
            .map(|i| mk(5, SpanStatus::Ok, "t", base + i as u128))
            .collect();
        for i in 0..50 {
            v.push(mk(5, SpanStatus::Error, "t", base + 100 + i as u128));
        }
        let r = evaluate(&slo, &v, base + 1_000);
        assert!(r.burning);
    }

    #[test]
    fn evaluate_empty_is_not_burning() {
        let slo = Slo {
            name: "errs".into(),
            metric: SloMetric::ErrorRate,
            target: 0.01,
            window_nanos: 1_000_000_000,
            burn_rate_threshold: 2.0,
            tool_glob: "*".into(),
        };
        let r = evaluate(&slo, &[], 1_000);
        assert!(!r.burning);
        assert_eq!(r.sample_count, 0);
    }

    #[test]
    fn evaluate_all_runs_each_slo() {
        let slos = vec![
            Slo {
                name: "a".into(),
                metric: SloMetric::ErrorRate,
                target: 0.01,
                window_nanos: 10_000_000_000,
                burn_rate_threshold: 2.0,
                tool_glob: "*".into(),
            },
            Slo {
                name: "b".into(),
                metric: SloMetric::LatencyP95Ms,
                target: 100.0,
                window_nanos: 10_000_000_000,
                burn_rate_threshold: 2.0,
                tool_glob: "*".into(),
            },
        ];
        let base: u128 = 5_000_000_000;
        let v: Vec<Span> = (0..10)
            .map(|i| mk(5, SpanStatus::Ok, "t", base + i as u128))
            .collect();
        let reports = evaluate_all(&slos, &v, base + 1_000);
        assert_eq!(reports.len(), 2);
        assert_eq!(reports[0].name, "a");
        assert_eq!(reports[1].name, "b");
    }

    #[test]
    fn evaluate_respects_tool_glob() {
        let slo = Slo {
            name: "search-only".into(),
            metric: SloMetric::LatencyP95Ms,
            target: 100.0,
            window_nanos: 10_000_000_000,
            burn_rate_threshold: 2.0,
            tool_glob: "search".into(),
        };
        let base: u128 = 5_000_000_000;
        let mut v = Vec::new();
        v.push(mk(9999, SpanStatus::Ok, "write", base));
        for i in 0..5 {
            v.push(mk(10, SpanStatus::Ok, "search", base + 10 + i as u128));
        }
        let r = evaluate(&slo, &v, base + 1_000);
        assert_eq!(r.sample_count, 5);
        assert!(!r.burning);
    }

    #[test]
    fn from_entry_empty_tool_defaults_to_star() {
        let e = SloConfigEntry {
            name: "x".into(),
            metric: "error_rate".into(),
            target: 0.01,
            window: "5m".into(),
            burn_rate_threshold: 2.0,
            tool: String::new(),
        };
        let s = Slo::from_entry(&e).unwrap();
        assert_eq!(s.tool_glob, "*");
    }

    #[test]
    fn load_config_from_file() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(
            tmp.path(),
            r#"
[[slo]]
name = "fast"
metric = "latency_p95_ms"
target = 200
window = "5m"
burn_rate_threshold = 2.0
"#,
        )
        .unwrap();
        let slos = load_config(tmp.path()).unwrap();
        assert_eq!(slos.len(), 1);
    }

    #[test]
    fn rolling_window_saturating_subtract() {
        // window > ts should not panic
        let mut w = RollingWindow::new(1_000_000);
        w.push(500, 1.0);
        assert_eq!(w.len(), 1);
    }
}
