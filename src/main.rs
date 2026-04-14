//! Entry point for the `mcptrace` CLI binary.
//!
//! This file is deliberately thin: all the logic lives in the library
//! modules under `src/`. `main` just dispatches parsed Clap commands.

#![forbid(unsafe_code)]
#![deny(warnings)]

use anyhow::{Context, Result};
use clap::Parser;
use mcptrace::cli::{
    validate_exporter_name, validate_http_url, validate_max_frame_bytes, validate_store_name, Cli,
    Command, ProxyMode, SloAction, StdioArgs,
};
use mcptrace::exporter::{Exporter, OtlpJsonExporter, StdoutExporter, ZipkinExporter};
use mcptrace::proxy::{run_stdio_proxy, ProxyConfig};
use mcptrace::replay::{load_spans, replay_to};
use mcptrace::slo::{evaluate_all, load_config};
use mcptrace::span::Span;
use mcptrace::stats::{compute_by_tool, render_table};
use mcptrace::store::{read_jsonl, JsonlStore, NullStore, SpanStore};
use std::sync::atomic::Ordering;
use std::sync::Arc;

fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Command::Proxy(p) => match p.mode {
            ProxyMode::Stdio(s) => run_proxy_stdio(s),
            ProxyMode::Http(_) => Err(anyhow::anyhow!(
                "HTTP proxy mode is deferred to v1.1 — use `proxy stdio` for this release"
            )),
        },
        Command::Replay(r) => run_replay(r),
        Command::Slo(s) => match s.action {
            SloAction::Check(c) => run_slo_check(c),
        },
        Command::Stats(s) => run_stats(s),
    }
}

fn run_proxy_stdio(args: StdioArgs) -> Result<()> {
    validate_store_name(&args.store).context("invalid --store")?;
    validate_max_frame_bytes(args.max_frame_bytes).context("invalid --max-frame-bytes")?;
    for e in &args.exporters {
        validate_exporter_name(e).context("invalid --exporter")?;
    }
    validate_http_url(&args.zipkin_url).context("invalid --zipkin-url")?;
    validate_http_url(&args.otlp_url).context("invalid --otlp-url")?;

    let store: Arc<dyn SpanStore> = match args.store.as_str() {
        "null" => Arc::new(NullStore),
        "jsonl" => {
            let path = args
                .store_path
                .as_deref()
                .ok_or_else(|| anyhow::anyhow!("--store jsonl requires --store-path"))?;
            Arc::new(JsonlStore::open(path).context("failed to open jsonl store")?)
        }
        _ => unreachable!(),
    };

    let mut exporters: Vec<Arc<dyn Exporter>> = Vec::new();
    for e in &args.exporters {
        match e.as_str() {
            "stdout" => exporters.push(Arc::new(StdoutExporter::new())),
            "zipkin" => exporters.push(Arc::new(
                ZipkinExporter::new(args.zipkin_url.clone(), args.service_name.clone())
                    .context("building zipkin exporter")?,
            )),
            "otlp" | "otlp-json" => exporters.push(Arc::new(
                OtlpJsonExporter::new(args.otlp_url.clone(), args.service_name.clone())
                    .context("building otlp exporter")?,
            )),
            _ => unreachable!(),
        }
    }

    let child_cmd = args
        .child_cmd
        .first()
        .ok_or_else(|| anyhow::anyhow!("missing child command"))?
        .clone();
    let child_args: Vec<String> = args.child_cmd.iter().skip(1).cloned().collect();

    let mut cfg = ProxyConfig::new(child_cmd, child_args);
    cfg.max_frame_bytes = args.max_frame_bytes;
    cfg.service_name = args.service_name;

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("tokio runtime")?;
    let stdin = tokio::io::stdin();
    let stdout = tokio::io::stdout();
    let metrics = runtime
        .block_on(run_stdio_proxy(cfg, stdin, stdout, store, exporters))
        .context("proxy loop")?;
    eprintln!(
        "mcptrace: spans_emitted={} dropped_buffer_full={} frames_too_large={} parse_errors={} orphans={}",
        metrics.spans_emitted.load(Ordering::Relaxed),
        metrics.spans_dropped_buffer_full.load(Ordering::Relaxed),
        metrics.frames_too_large.load(Ordering::Relaxed),
        metrics.parse_errors.load(Ordering::Relaxed),
        metrics.orphan_spans.load(Ordering::Relaxed),
    );
    Ok(())
}

fn run_replay(args: mcptrace::cli::ReplayArgs) -> Result<()> {
    for e in &args.exporters {
        validate_exporter_name(e).context("invalid --exporter")?;
    }
    validate_http_url(&args.zipkin_url).context("invalid --zipkin-url")?;
    validate_http_url(&args.otlp_url).context("invalid --otlp-url")?;
    let spans = load_spans(&args.spans).context("load spans")?;
    let mut exporters: Vec<Arc<dyn Exporter>> = Vec::new();
    for e in &args.exporters {
        match e.as_str() {
            "stdout" => exporters.push(Arc::new(StdoutExporter::new())),
            "zipkin" => exporters.push(Arc::new(
                ZipkinExporter::new(args.zipkin_url.clone(), args.service_name.clone())
                    .context("building zipkin exporter")?,
            )),
            "otlp" | "otlp-json" => exporters.push(Arc::new(
                OtlpJsonExporter::new(args.otlp_url.clone(), args.service_name.clone())
                    .context("building otlp exporter")?,
            )),
            _ => unreachable!(),
        }
    }
    let n = replay_to(&spans, &exporters).context("replay")?;
    eprintln!("mcptrace: replayed {n} spans");
    Ok(())
}

fn run_slo_check(args: mcptrace::cli::SloCheckArgs) -> Result<()> {
    let slos = load_config(&args.config).context("slo config")?;
    let spans = read_jsonl(&args.spans).context("load spans")?;
    let now = latest_or_now(&spans);
    let reports = evaluate_all(&slos, &spans, now);
    let mut any_burning = false;
    for r in &reports {
        let tag = if r.burning { "BURN" } else { "ok  " };
        println!(
            "[{tag}] {:16}  metric={:?}  target={}  actual={:.4}  burn_rate={:.4}  threshold={}  n={}",
            r.name, r.metric, r.target, r.actual, r.burn_rate, r.threshold, r.sample_count
        );
        if r.burning {
            any_burning = true;
        }
    }
    if any_burning {
        std::process::exit(1);
    }
    Ok(())
}

fn run_stats(args: mcptrace::cli::StatsArgs) -> Result<()> {
    let mut spans = read_jsonl(&args.spans).context("load spans")?;
    if let Some(tool) = &args.tool {
        spans.retain(|s| s.tool_name.as_deref() == Some(tool.as_str()));
    }
    let by = compute_by_tool(&spans);
    println!("{}", render_table(&by));
    Ok(())
}

fn latest_or_now(spans: &[Span]) -> u128 {
    spans
        .iter()
        .map(|s| s.start_unix_nanos)
        .max()
        .unwrap_or_else(Span::now_unix_nanos)
}
