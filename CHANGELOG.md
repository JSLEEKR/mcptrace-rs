# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/).

## [1.0.0] - 2026-04-14

### Added

- Initial release.
- **Transparent stdio proxy**: spawn a child MCP server, relay JSON-RPC
  frames in both directions, correlate requests to responses by
  JSON-RPC id, and emit a `Span` per tool call.
- **Span schema v1**: self-describing `schema_version=1` records with
  `trace_id`, `span_id`, `tool_name`, sha256 `arg_digest`, `duration_ms`,
  `request_bytes`, `response_bytes`, `error_code`, and
  `error_message_digest`. Arguments and error messages are **never**
  persisted in plaintext.
- **Storage backends**: `null` (default, drops spans) and `jsonl`
  (append-only file, one span per line, LF only).
- **Exporters**:
  - `stdout`: one JSON line per span, for local dev.
  - `zipkin`: POST to `/api/v2/spans`, Zipkin v2 JSON shape.
  - `otlp-json`: POST to `/v1/traces`, OTLP HTTP+JSON envelope with
    `resourceSpans → scopeSpans → spans`.
- **SLO engine**: declarative TOML config, three metrics
  (`latency_p95_ms`, `error_rate`, `availability`), rolling-window
  evaluation with burn-rate math from the Google SRE workbook, per-tool
  glob filtering.
- **Offline subcommands**: `mcptrace stats --spans X.jsonl`,
  `mcptrace replay X.jsonl --exporter ...`,
  `mcptrace slo check --spans X.jsonl --config slo.toml` (exit non-zero
  if anything is burning).
- **Safety**: `#![forbid(unsafe_code)]`, `#![deny(warnings)]`,
  `#![warn(clippy::pedantic)]`, no `.unwrap()`/`.expect()`/`panic!` in
  production code paths, 5 second exporter timeout, 1 MiB default
  frame size cap.
- **Cross-platform**: Windows + Linux + macOS; LF-only output
  regardless of host platform; `rustls-tls` (no native TLS) so the
  binary is portable and single-file.
- **199 tests**: 187 unit + 12 integration, all deterministic and
  hermetic (no network, no child process spawning in CI).

### Fixed

- **JSON-RPC id collision is now observable**: when an agent re-uses a
  JSON-RPC id while the previous request with that id is still in
  flight, the proxy used to silently overwrite the in-flight `Pending`
  entry. The first request never finalized into a span and never
  bumped any counter, so a misbehaving agent could silently drop
  observations. Added a new `spans_dropped_id_collision` counter to
  `ProxyMetrics` (also surfaced in the proxy shutdown summary line),
  bumped on every overwrite. Regression test
  `pending_insert_collision_is_observable` exercises the path. The
  forwarding behavior is unchanged — id reuse is still a JSON-RPC
  protocol violation per the spec, and operators can now see when it
  happens.
- **SLO `error_rate` burn math**: in the pre-release, `error_rate`
  targets were interpreted via the SRE-workbook availability formula
  (`budget = 1 - target`), which produces absurd results when `target`
  is phrased as "maximum allowed error rate" (the README's own
  example). The corrected model treats `target` as the budget directly
  for `error_rate`, so `target = 0.01` with a 33% observed error rate
  now produces `burn_rate = 33.3` (fires the alert) instead of
  `burn_rate = 0.34` (silent). `target = 0` is correctly treated as
  zero-tolerance. `availability` math is unchanged (still SRE-workbook
  form). Regression tests cover both flavors plus zero-tolerance and
  empty-window edge cases.

### Deferred (to v1.1)

- **HTTP-mode proxy** (`mcptrace proxy http --upstream URL --listen PORT`):
  reqwest-as-server plumbing is out of scope for a 1-week build.
  Documented in README and the binary errors out clearly when the
  subcommand is invoked.
- **SQLite storage backend**: JSONL + ad-hoc `sqlite :memory: .import`
  covers the analytics case.
- **Jaeger native thrift exporter**: modern Jaeger accepts OTLP JSON.
- **Webhook SLO alert streaming**: the offline `slo check` command
  covers CI/cron use cases today.
