//! Clap-derive command-line surface.
//!
//! Each subcommand has its own struct so tests can validate parsing
//! (and validation rules) without going through `fn main`.

use clap::{Args, Parser, Subcommand};

/// Top-level CLI.
#[derive(Debug, Parser)]
#[command(
    name = "mcptrace",
    version,
    about = "Observability proxy and OpenTelemetry exporter for MCP servers",
    long_about = "mcptrace is a transparent observability proxy for MCP (Model \
                  Context Protocol) servers. It sits between an agent and a \
                  JSON-RPC MCP server, records tool-call spans, exports to \
                  OTLP / Zipkin / stdout, and enforces SLO budgets."
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,
}

/// All top-level subcommands.
#[derive(Debug, Subcommand)]
pub enum Command {
    /// Run a child MCP server under the stdio proxy.
    Proxy(ProxyArgs),
    /// Replay a JSONL file of captured spans to an exporter.
    Replay(ReplayArgs),
    /// Check SLO budgets against a captured JSONL file.
    Slo(SloArgs),
    /// Summarise a captured JSONL file (counts, percentiles).
    Stats(StatsArgs),
}

#[derive(Debug, Args)]
pub struct ProxyArgs {
    #[command(subcommand)]
    pub mode: ProxyMode,
}

#[derive(Debug, Subcommand)]
pub enum ProxyMode {
    /// stdio proxy: spawn a child and relay JSON-RPC over its stdin/stdout.
    Stdio(StdioArgs),
    /// HTTP proxy (DEFERRED to v1.1 — errors at runtime).
    Http(HttpArgs),
}

#[derive(Debug, Args, Clone)]
pub struct StdioArgs {
    /// Storage backend: null | jsonl.
    #[arg(long, default_value = "null")]
    pub store: String,

    /// Path for jsonl store (required when --store jsonl).
    #[arg(long)]
    pub store_path: Option<String>,

    /// Exporters to enable. Repeatable. Choices: stdout | zipkin | otlp.
    #[arg(long = "exporter")]
    pub exporters: Vec<String>,

    /// Full Zipkin endpoint URL.
    #[arg(long, default_value = "http://localhost:9411/api/v2/spans")]
    pub zipkin_url: String,

    /// Full OTLP HTTP endpoint URL.
    #[arg(long, default_value = "http://localhost:4318/v1/traces")]
    pub otlp_url: String,

    /// Max bytes for any single JSON-RPC frame. Default 1 MiB.
    #[arg(long, default_value_t = 1_048_576)]
    pub max_frame_bytes: u64,

    /// service.name attribute emitted to exporters.
    #[arg(long, default_value = "mcptrace")]
    pub service_name: String,

    /// Child command and arguments, after `--`.
    #[arg(last = true, num_args = 1.., required = true, value_name = "CMD")]
    pub child_cmd: Vec<String>,
}

#[derive(Debug, Args)]
pub struct HttpArgs {
    #[arg(long)]
    pub upstream: String,
    #[arg(long, default_value_t = 7070)]
    pub listen: u16,
}

#[derive(Debug, Args)]
pub struct ReplayArgs {
    #[arg(value_name = "JSONL")]
    pub spans: String,
    #[arg(long = "exporter")]
    pub exporters: Vec<String>,
    #[arg(long, default_value = "http://localhost:9411/api/v2/spans")]
    pub zipkin_url: String,
    #[arg(long, default_value = "http://localhost:4318/v1/traces")]
    pub otlp_url: String,
    #[arg(long, default_value = "mcptrace")]
    pub service_name: String,
}

#[derive(Debug, Args)]
pub struct SloArgs {
    #[command(subcommand)]
    pub action: SloAction,
}

#[derive(Debug, Subcommand)]
pub enum SloAction {
    /// Evaluate SLOs and exit non-zero if any are burning.
    Check(SloCheckArgs),
}

#[derive(Debug, Args)]
pub struct SloCheckArgs {
    #[arg(long)]
    pub spans: String,
    #[arg(long)]
    pub config: String,
}

#[derive(Debug, Args)]
pub struct StatsArgs {
    #[arg(long)]
    pub spans: String,
    /// Optional: only show stats for this tool.
    #[arg(long)]
    pub tool: Option<String>,
}

/// Validate an HTTP/HTTPS URL. Rejects empty, missing scheme, NaN-port
/// nonsense. Intended to give early CLI-side errors rather than
/// ambiguous reqwest errors later.
pub fn validate_http_url(url: &str) -> crate::error::Result<()> {
    if url.is_empty() {
        return Err(crate::error::Error::InvalidArgument(
            "URL must not be empty".into(),
        ));
    }
    if !url.starts_with("http://") && !url.starts_with("https://") {
        return Err(crate::error::Error::InvalidArgument(format!(
            "URL must start with http:// or https://: {url}"
        )));
    }
    Ok(())
}

/// Validate a listening port. Ports must be in 1..=65535.
pub fn validate_port(port: u16) -> crate::error::Result<()> {
    if port == 0 {
        return Err(crate::error::Error::InvalidArgument(
            "port must be in 1..=65535".into(),
        ));
    }
    Ok(())
}

/// Validate a max_frame_bytes value. Must be >= 1024 and <= 64 MiB.
pub fn validate_max_frame_bytes(n: u64) -> crate::error::Result<()> {
    if n < 1024 {
        return Err(crate::error::Error::InvalidArgument(
            "max_frame_bytes must be >= 1024".into(),
        ));
    }
    if n > 64 * 1024 * 1024 {
        return Err(crate::error::Error::InvalidArgument(
            "max_frame_bytes must be <= 64 MiB".into(),
        ));
    }
    Ok(())
}

/// Validate the store name against the supported set.
pub fn validate_store_name(s: &str) -> crate::error::Result<()> {
    match s {
        "null" | "jsonl" => Ok(()),
        other => Err(crate::error::Error::InvalidArgument(format!(
            "unknown store backend: {other} (expected null|jsonl)"
        ))),
    }
}

/// Validate an exporter name against the supported set.
pub fn validate_exporter_name(s: &str) -> crate::error::Result<()> {
    match s {
        "stdout" | "zipkin" | "otlp" | "otlp-json" => Ok(()),
        other => Err(crate::error::Error::InvalidArgument(format!(
            "unknown exporter: {other} (expected stdout|zipkin|otlp)"
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    #[test]
    fn parse_proxy_stdio_minimal() {
        let cli = Cli::try_parse_from([
            "mcptrace",
            "proxy",
            "stdio",
            "--",
            "myserver",
        ])
        .unwrap();
        match cli.command {
            Command::Proxy(p) => match p.mode {
                ProxyMode::Stdio(s) => {
                    assert_eq!(s.child_cmd, vec!["myserver"]);
                    assert_eq!(s.store, "null");
                    assert_eq!(s.max_frame_bytes, 1_048_576);
                }
                _ => panic!("expected stdio"),
            },
            _ => panic!("expected proxy"),
        }
    }

    #[test]
    fn parse_proxy_stdio_with_store_and_exporters() {
        let cli = Cli::try_parse_from([
            "mcptrace",
            "proxy",
            "stdio",
            "--store",
            "jsonl",
            "--store-path",
            "spans.jsonl",
            "--exporter",
            "stdout",
            "--exporter",
            "zipkin",
            "--",
            "python",
            "server.py",
        ])
        .unwrap();
        if let Command::Proxy(p) = cli.command {
            if let ProxyMode::Stdio(s) = p.mode {
                assert_eq!(s.store, "jsonl");
                assert_eq!(s.store_path.as_deref(), Some("spans.jsonl"));
                assert_eq!(s.exporters, vec!["stdout", "zipkin"]);
                assert_eq!(s.child_cmd, vec!["python", "server.py"]);
            } else {
                panic!();
            }
        } else {
            panic!();
        }
    }

    #[test]
    fn parse_stats() {
        let cli = Cli::try_parse_from(["mcptrace", "stats", "--spans", "x.jsonl"]).unwrap();
        match cli.command {
            Command::Stats(s) => assert_eq!(s.spans, "x.jsonl"),
            _ => panic!(),
        }
    }

    #[test]
    fn parse_stats_with_tool_filter() {
        let cli = Cli::try_parse_from([
            "mcptrace",
            "stats",
            "--spans",
            "x.jsonl",
            "--tool",
            "search",
        ])
        .unwrap();
        if let Command::Stats(s) = cli.command {
            assert_eq!(s.tool, Some("search".into()));
        } else {
            panic!();
        }
    }

    #[test]
    fn parse_slo_check() {
        let cli = Cli::try_parse_from([
            "mcptrace",
            "slo",
            "check",
            "--spans",
            "x.jsonl",
            "--config",
            "slo.toml",
        ])
        .unwrap();
        if let Command::Slo(s) = cli.command {
            let SloAction::Check(c) = s.action;
            assert_eq!(c.spans, "x.jsonl");
            assert_eq!(c.config, "slo.toml");
        } else {
            panic!();
        }
    }

    #[test]
    fn parse_replay_with_exporter() {
        let cli = Cli::try_parse_from([
            "mcptrace",
            "replay",
            "spans.jsonl",
            "--exporter",
            "stdout",
        ])
        .unwrap();
        if let Command::Replay(r) = cli.command {
            assert_eq!(r.spans, "spans.jsonl");
            assert_eq!(r.exporters, vec!["stdout"]);
        } else {
            panic!();
        }
    }

    #[test]
    fn parse_fails_without_subcommand() {
        assert!(Cli::try_parse_from(["mcptrace"]).is_err());
    }

    #[test]
    fn parse_proxy_stdio_requires_child_cmd() {
        assert!(Cli::try_parse_from(["mcptrace", "proxy", "stdio", "--"]).is_err());
    }

    #[test]
    fn validate_http_url_ok() {
        assert!(validate_http_url("http://x").is_ok());
        assert!(validate_http_url("https://x/y").is_ok());
    }

    #[test]
    fn validate_http_url_rejects_empty() {
        assert!(validate_http_url("").is_err());
    }

    #[test]
    fn validate_http_url_rejects_bad_scheme() {
        assert!(validate_http_url("ftp://x").is_err());
        assert!(validate_http_url("tcp://x").is_err());
    }

    #[test]
    fn validate_port_rejects_zero() {
        assert!(validate_port(0).is_err());
        assert!(validate_port(1).is_ok());
        assert!(validate_port(65535).is_ok());
    }

    #[test]
    fn validate_max_frame_bytes_rejects_too_small() {
        assert!(validate_max_frame_bytes(100).is_err());
    }

    #[test]
    fn validate_max_frame_bytes_rejects_too_big() {
        assert!(validate_max_frame_bytes(128 * 1024 * 1024).is_err());
    }

    #[test]
    fn validate_max_frame_bytes_accepts_default() {
        assert!(validate_max_frame_bytes(1_048_576).is_ok());
    }

    #[test]
    fn validate_store_names() {
        assert!(validate_store_name("null").is_ok());
        assert!(validate_store_name("jsonl").is_ok());
        assert!(validate_store_name("sqlite").is_err());
    }

    #[test]
    fn validate_exporter_names() {
        assert!(validate_exporter_name("stdout").is_ok());
        assert!(validate_exporter_name("zipkin").is_ok());
        assert!(validate_exporter_name("otlp").is_ok());
        assert!(validate_exporter_name("otlp-json").is_ok());
        assert!(validate_exporter_name("jaeger").is_err());
    }
}
