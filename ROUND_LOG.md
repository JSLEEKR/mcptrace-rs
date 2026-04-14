# Round Log — mcptrace-rs

- **Name:** mcptrace-rs
- **Category:** MCP Observability / Distributed Tracing
- **Language:** Rust 2021 (edition), rustc 1.75+
- **Date:** 2026-04-14
- **Type:** New build (V1 Round — second V1 Rust project after benchdiff-rs)
- **Test count:** 199 (187 unit + 12 integration), all passing in debug and release
- **Release build:** `cargo build --release` clean, LTO thin, strip, opt-level 3
- **Warnings:** zero (`#![deny(warnings)]` in both lib and bin)
- **Unsafe:** forbidden (`#![forbid(unsafe_code)]`)
- **Clippy:** pedantic lints warn-level enabled

## Scope

Built a transparent stdio proxy + observability exporter for MCP
(Model Context Protocol) servers. Sits between an agent and any
JSON-RPC MCP server, correlates requests to responses, emits
privacy-safe spans (sha256 arg digests, not plaintext), and ships
them to stdout / Zipkin / OTLP JSON. Includes a declarative SLO engine
with burn-rate math.

## Modules

```
src/
  lib.rs         - crate root, pub use re-exports
  error.rs       - thiserror-based error enum
  digest.rs      - sha256 helpers (arg_digest / error_digest)
  duration.rs    - 5m / 1h / 30d parser, nanos output
  trace_id.rs    - splitmix64-based 128-bit trace id generator
  span.rs        - Span + SpanBuilder + SpanStatus + jsonl roundtrip
  jsonrpc.rs     - Frame parser with size cap, id correlation helpers
  store.rs       - SpanStore trait + NullStore + JsonlStore
  exporter.rs    - Exporter trait + Stdout + Zipkin + OtlpJson
  slo.rs         - SloMetric + rolling window + burn rate + config parse
  stats.rs       - p50/p95/p99 per-tool + comfy-table output
  proxy.rs       - tokio stdio proxy loop + Pending + finalize
  replay.rs      - offline replay from JSONL to exporters
  cli.rs         - clap-derive CLI structs + validators
  main.rs        - thin main dispatching subcommands
tests/
  integration.rs - 12 end-to-end workflow tests
```

## Deferred features (documented in README + CHANGELOG)

- HTTP-mode proxy (stdio is the must; HTTP pushed to v1.1)
- SQLite storage backend
- Native Jaeger thrift exporter (OTLP JSON covers Jaeger >= 1.35)
- Webhook SLO alert streaming

## Dependencies

Minimal, pinned to minor versions:
- clap 4.5 (derive)
- serde 1 + serde_json 1
- toml 0.8
- sha2 0.10
- anyhow 1 + thiserror 1
- tokio 1.38 (rt-multi-thread, io-util, process, sync, time, io-std)
- reqwest 0.12 (rustls-tls, blocking, json; no native-tls)
- comfy-table 7.1
- hex 0.4

Dev: tempfile 3.10.

## Security posture

- sha256-only on all tool arguments and error messages
- 1 MiB default frame cap (`--max-frame-bytes` overridable 1 KiB..64 MiB)
- 5s exporter HTTP timeout
- no inbound listening (outbound HTTP only)
- fn main returns anyhow::Result — no panics on the happy path
- no `.unwrap()`/`.expect()` in library code outside #[cfg(test)]
