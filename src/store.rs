//! Storage backends for observed spans.
//!
//! The trait is deliberately tiny so adding a backend is a one-file PR:
//!
//! ```rust,ignore
//! pub trait SpanStore: Send + Sync {
//!     fn record(&self, span: &Span) -> Result<()>;
//!     fn flush(&self) -> Result<()>;
//! }
//! ```
//!
//! Built-in implementations:
//!
//! - [`NullStore`]: drops all spans. Default. Zero overhead, zero disk.
//! - [`JsonlStore`]: append-only JSONL file, one span per line. Safe
//!   for concurrent writers as long as each record fits in one OS write
//!   (≤1 MB is guaranteed by the frame cap).
//!
//! A SQLite backend is deferred to v1.1 — it adds a significant native
//! dependency and the JSONL path covers the common case of "drop this
//! into a log stream / S3 / Loki".

use crate::error::Result;
use crate::span::Span;
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

/// Abstract span sink.
pub trait SpanStore: Send + Sync {
    /// Persist a single span. Must not block the proxy data path for long.
    fn record(&self, span: &Span) -> Result<()>;
    /// Flush any buffered data to durable storage.
    fn flush(&self) -> Result<()>;
    /// Human-friendly name, used in logs.
    fn name(&self) -> &'static str;
}

/// The default span sink: drop everything.
///
/// Useful when you only care about exporters or SLO alerting and don't
/// want local persistence overhead.
#[derive(Debug, Default)]
pub struct NullStore;

impl SpanStore for NullStore {
    fn record(&self, _span: &Span) -> Result<()> {
        Ok(())
    }
    fn flush(&self) -> Result<()> {
        Ok(())
    }
    fn name(&self) -> &'static str {
        "null"
    }
}

/// Append-only JSONL store.
///
/// Each call to [`SpanStore::record`] serializes the span to a line and writes it
/// with a single `write_all` (which on POSIX `O_APPEND` is atomic for
/// writes ≤ PIPE_BUF; our frame cap of 1 MB is larger, so we serialize
/// the writes via a Mutex for cross-platform safety).
#[derive(Debug)]
pub struct JsonlStore {
    path: PathBuf,
    inner: Mutex<File>,
}

impl JsonlStore {
    /// Open (or create) the file for append.
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref().to_path_buf();
        let f = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)?;
        Ok(JsonlStore {
            path,
            inner: Mutex::new(f),
        })
    }

    /// Path the store is writing to.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Count of spans already in the file (by counting lines).
    /// O(n) — use sparingly, mostly for tests.
    pub fn count_lines(&self) -> Result<usize> {
        let content = std::fs::read_to_string(&self.path)?;
        Ok(content.lines().filter(|l| !l.is_empty()).count())
    }
}

impl SpanStore for JsonlStore {
    fn record(&self, span: &Span) -> Result<()> {
        let line = span.to_jsonl_line()?;
        // LF only, regardless of platform
        let mut guard = self
            .inner
            .lock()
            .map_err(|e| crate::error::Error::Observation(format!("store mutex poisoned: {e}")))?;
        guard.write_all(line.as_bytes())?;
        guard.write_all(b"\n")?;
        Ok(())
    }
    fn flush(&self) -> Result<()> {
        let mut guard = self
            .inner
            .lock()
            .map_err(|e| crate::error::Error::Observation(format!("store mutex poisoned: {e}")))?;
        guard.flush()?;
        Ok(())
    }
    fn name(&self) -> &'static str {
        "jsonl"
    }
}

/// Read an entire JSONL file into memory as a `Vec<Span>`. Used by the
/// offline subcommands (`replay`, `slo check`, `stats`).
pub fn read_jsonl(path: impl AsRef<Path>) -> Result<Vec<Span>> {
    let content = std::fs::read_to_string(path)?;
    let mut out = Vec::new();
    for (lineno, line) in content.lines().enumerate() {
        let l = line.trim();
        if l.is_empty() || l.starts_with('#') {
            continue;
        }
        match Span::from_jsonl_line(l) {
            Ok(s) => out.push(s),
            Err(e) => {
                return Err(crate::error::Error::InvalidConfig(format!(
                    "line {}: {e}",
                    lineno + 1
                )))
            }
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::span::{Span, SpanStatus};

    fn sample(id: &str) -> Span {
        Span::builder()
            .trace_id("0123456789abcdef0123456789abcdef")
            .span_id(id.to_string())
            .method("tools/call")
            .tool_name(Some("search".into()))
            .start_unix_nanos(1_700_000_000_000_000_000)
            .duration_ms(10)
            .status(SpanStatus::Ok)
            .build()
            .unwrap()
    }

    #[test]
    fn null_store_accepts_everything() {
        let s = NullStore;
        assert!(s.record(&sample("a")).is_ok());
        assert!(s.flush().is_ok());
        assert_eq!(s.name(), "null");
    }

    #[test]
    fn jsonl_store_writes_one_line_per_span() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        let store = JsonlStore::open(tmp.path()).unwrap();
        store.record(&sample("a")).unwrap();
        store.record(&sample("b")).unwrap();
        store.flush().unwrap();

        let content = std::fs::read_to_string(tmp.path()).unwrap();
        let lines: Vec<&str> = content.lines().collect();
        assert_eq!(lines.len(), 2);
        for l in &lines {
            assert!(!l.is_empty());
            assert!(l.starts_with('{'));
        }
    }

    #[test]
    fn jsonl_store_uses_lf_not_crlf() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        let store = JsonlStore::open(tmp.path()).unwrap();
        store.record(&sample("a")).unwrap();
        let bytes = std::fs::read(tmp.path()).unwrap();
        assert!(!bytes.contains(&b'\r'));
        assert!(bytes.ends_with(b"\n"));
    }

    #[test]
    fn jsonl_store_appends() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        {
            let store = JsonlStore::open(tmp.path()).unwrap();
            store.record(&sample("a")).unwrap();
        }
        {
            let store = JsonlStore::open(tmp.path()).unwrap();
            store.record(&sample("b")).unwrap();
            assert_eq!(store.count_lines().unwrap(), 2);
        }
    }

    #[test]
    fn jsonl_store_read_back() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        let store = JsonlStore::open(tmp.path()).unwrap();
        for id in &["a", "b", "c"] {
            store.record(&sample(id)).unwrap();
        }
        let spans = read_jsonl(tmp.path()).unwrap();
        assert_eq!(spans.len(), 3);
        assert_eq!(spans[0].span_id, "a");
        assert_eq!(spans[2].span_id, "c");
    }

    #[test]
    fn jsonl_store_path_accessor() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        let store = JsonlStore::open(tmp.path()).unwrap();
        assert_eq!(store.path(), tmp.path());
        assert_eq!(store.name(), "jsonl");
    }

    #[test]
    fn read_jsonl_skips_blank_lines() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(
            tmp.path(),
            format!(
                "{}\n\n{}\n",
                sample("a").to_jsonl_line().unwrap(),
                sample("b").to_jsonl_line().unwrap()
            ),
        )
        .unwrap();
        let spans = read_jsonl(tmp.path()).unwrap();
        assert_eq!(spans.len(), 2);
    }

    #[test]
    fn read_jsonl_skips_comment_lines() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(
            tmp.path(),
            format!(
                "# header\n{}\n",
                sample("a").to_jsonl_line().unwrap()
            ),
        )
        .unwrap();
        let spans = read_jsonl(tmp.path()).unwrap();
        assert_eq!(spans.len(), 1);
    }

    #[test]
    fn read_jsonl_surfaces_bad_line() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(tmp.path(), "not json\n").unwrap();
        assert!(read_jsonl(tmp.path()).is_err());
    }

    #[test]
    fn read_jsonl_missing_file() {
        assert!(read_jsonl("/tmp/definitely/does/not/exist/xyz.jsonl").is_err());
    }

    #[test]
    fn jsonl_store_count_lines_empty() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        let store = JsonlStore::open(tmp.path()).unwrap();
        assert_eq!(store.count_lines().unwrap(), 0);
    }

    #[test]
    fn jsonl_store_many_spans() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        let store = JsonlStore::open(tmp.path()).unwrap();
        for i in 0..200u32 {
            let s = Span::builder()
                .trace_id("t")
                .span_id(format!("s{i}"))
                .method("m")
                .build()
                .unwrap();
            store.record(&s).unwrap();
        }
        store.flush().unwrap();
        assert_eq!(store.count_lines().unwrap(), 200);
    }
}
