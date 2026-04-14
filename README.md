# mcptrace-rs

[![for-the-badge](https://img.shields.io/badge/mcptrace-v1.0.0-orange?style=for-the-badge)](https://github.com/JSLEEKR/mcptrace-rs)
[![for-the-badge](https://img.shields.io/badge/language-rust-red?style=for-the-badge&logo=rust)](https://www.rust-lang.org/)
[![for-the-badge](https://img.shields.io/badge/license-MIT-blue?style=for-the-badge)](LICENSE)
[![for-the-badge](https://img.shields.io/badge/unsafe-forbidden-brightgreen?style=for-the-badge)](#security)
[![for-the-badge](https://img.shields.io/badge/tests-199-success?style=for-the-badge)](#tests)

> **Transparent observability proxy and OpenTelemetry exporter for MCP
> (Model Context Protocol) servers. Trace every tool call. Measure
> latencies. Enforce SLO budgets. Single Rust binary.**

---

## Why this exists

The MCP (Model Context Protocol) ecosystem is exploding. Every week a new
tool server ships, agents call hundreds of `tools/call` requests per
session, and — when something goes wrong — operators have **nothing** to
look at.

- **Langfuse, Helicone, Braintrust**: trace LLM *completions*, not MCP
  *tool calls*. They observe the prompt→response, not the tool→result.
- **OpenTelemetry auto-instrumentation**: doesn't know about JSON-RPC
  over stdio, doesn't know how to extract a tool name, doesn't know how
  to redact arguments that carry secrets.
- **MCP SDKs**: inconsistent logging, argument leaks in plaintext, no
  correlation IDs, no percentiles, no alerts.

The result: when a tool call is slow, or errors intermittently, or
starts burning an SLO, operators are flying blind.

**mcptrace-rs** is a transparent proxy you drop between your agent and
any MCP server. Every request/response pair becomes a first-class
observable span with latency, error, and byte counts — exported to any
OpenTelemetry-compatible backend, and checked against declarative SLO
budgets.

### It is NOT

- **Not a full APM.** No CPU flamegraphs, no memory profiling.
- **Not an agent framework.** It doesn't run agents or manage prompts.
- **Not a Langfuse replacement.** Langfuse traces LLM calls. mcptrace
  traces MCP tool calls. They complement each other.
- **Not a policy engine.** mcptrace is read-only on the wire; it
  observes and forwards, never transforms.

## Quick start

```bash
# 1. Install (single binary, no runtime deps except rustls)
cargo install --path .

# 2. Run your favorite MCP server under the proxy, recording to JSONL:
mcptrace proxy stdio \
  --store jsonl \
  --store-path spans.jsonl \
  --exporter stdout \
  -- \
  python -m my_mcp_server

# ... use your agent as normal, make a few tool calls ...
# ... stop the proxy (Ctrl-C) ...

# 3. See per-tool stats:
mcptrace stats --spans spans.jsonl
# +---------+-------+---------+--------+--------+--------+--------+--------+
# | tool    | count | err_rt  | p50_ms | p95_ms | p99_ms | max_ms | avg_ms |
# +---------+-------+---------+--------+--------+--------+--------+--------+
# | search  | 124   | 0.0081  | 47     | 182    | 414    | 701    | 63.92  |
# | write   | 18    | 0.0000  | 12     | 28     | 28     | 28     | 14.11  |
# +---------+-------+---------+--------+--------+--------+--------+--------+

# 4. Check your SLOs:
mcptrace slo check --spans spans.jsonl --config slo.toml
# [ok  ] fast-tools        metric=LatencyP95Ms  target=200  actual=182   burn=0.91  n=142
# [BURN] errors-low        metric=ErrorRate     target=0.01 actual=0.028 burn=2.85  n=142

# 5. Forward captured spans to Zipkin or OTLP later:
mcptrace replay spans.jsonl --exporter zipkin --zipkin-url http://localhost:9411/api/v2/spans
```

That's the entire loop: spawn → observe → stats → SLO → forward.

## Command-line surface

```
mcptrace proxy stdio -- <child> [args...]
    --store null|jsonl
    --store-path <path>
    --exporter stdout|zipkin|otlp
    --zipkin-url http://host:9411/api/v2/spans
    --otlp-url   http://host:4318/v1/traces
    --max-frame-bytes 1048576
    --service-name mcptrace

mcptrace replay <jsonl>
    --exporter zipkin|otlp|stdout

mcptrace slo check --spans <jsonl> --config slo.toml

mcptrace stats --spans <jsonl> [--tool NAME]
```

Run `mcptrace --help` for the full matrix. Every subcommand has its own
`--help`.

## Span schema

Each observed JSON-RPC call is persisted as a single line of JSONL, one
self-describing record per line. The schema is stable (v1) and versioned.

```json
{
  "schema_version": 1,
  "span_id": "fedcba9876543210",
  "trace_id": "0123456789abcdef0123456789abcdef",
  "service_name": "mcptrace",
  "method": "tools/call",
  "request_id": 42,
  "tool_name": "search",
  "arg_digest": "9f86d081884c7d659a2feaa0c55ad015a3bf4f1b2b0b822cd15d6c15b0f00a08",
  "arg_bytes": 42,
  "start_unix_nanos": 1700000000000000000,
  "duration_ms": 17,
  "request_bytes": 120,
  "response_bytes": 340,
  "status": "ok"
}
```

### Field reference

| field | type | meaning |
| --- | --- | --- |
| `schema_version` | `u32` | Always `1` in this release. |
| `span_id` | string (16 hex) | Unique within a proxy session. |
| `trace_id` | string (32 hex) | Shared across every span in one proxy session; lets you filter "all spans from this run" in any APM UI. |
| `service_name` | string | `service.name` attribute forwarded to exporters. |
| `method` | string | JSON-RPC method (`tools/call`, `initialize`, `ping`, ...). |
| `request_id` | JSON | Echoed verbatim from the wire. |
| `tool_name` | string | Extracted from `params.name` for `tools/call`; `null` otherwise. |
| `arg_digest` | string (64 hex) | `sha256(raw params.arguments)` — **never** the plaintext. |
| `arg_bytes` | `u64` | Length of the raw arguments field. |
| `start_unix_nanos` | `u128` | Wall-clock start, nanoseconds since unix epoch. |
| `duration_ms` | `u64` | Request→response latency. |
| `request_bytes` | `u64` | Raw wire byte count of the request frame. |
| `response_bytes` | `u64` | Raw wire byte count of the response frame. |
| `error_code` | `i64?` | Present on JSON-RPC error responses. |
| `error_message_digest` | string (64 hex)? | `sha256(error.message)` — **never** the plaintext. |
| `status` | `ok`\|`error`\|`timeout`\|`orphan` | Final status of the span. |

## Exporters

### stdout

```bash
mcptrace proxy stdio --exporter stdout -- my_mcp_server
```

Renders one JSON-per-span line to stdout, suitable for local dev and
piping into `jq`:

```json
{"t":"span","ts":1700000000000000000,"trace_id":"...","span_id":"...","method":"tools/call","tool":"search","status":"ok","duration_ms":17,"req_bytes":120,"res_bytes":340,"arg_digest":"...","error_code":null}
```

### Zipkin v2 JSON

```bash
mcptrace proxy stdio \
  --exporter zipkin \
  --zipkin-url http://localhost:9411/api/v2/spans \
  -- my_mcp_server
```

Spans become Zipkin v2 client spans with `localEndpoint.serviceName`
taken from `--service-name`, and MCP-specific fields exposed as tags:

```json
[{
  "traceId": "0123456789abcdef0123456789abcdef",
  "id": "fedcba9876543210",
  "name": "search",
  "kind": "CLIENT",
  "timestamp": 1700000000000000,
  "duration": 17000,
  "localEndpoint": { "serviceName": "mcptrace" },
  "tags": {
    "mcp.method": "tools/call",
    "mcp.tool": "search",
    "mcp.arg_digest": "9f86...0a08",
    "mcp.request_bytes": 120,
    "mcp.response_bytes": 340,
    "mcp.status": "ok"
  }
}]
```

### OTLP HTTP + JSON

```bash
mcptrace proxy stdio \
  --exporter otlp \
  --otlp-url http://localhost:4318/v1/traces \
  -- my_mcp_server
```

Spans are shipped as OTLP/HTTP+JSON (`resourceSpans → scopeSpans → spans`)
with a standard `service.name` resource attribute and MCP-specific
fields as span attributes. Modern Jaeger (>=1.35) accepts OTLP JSON,
so this path covers Jaeger too.

### Jaeger (native thrift)

Not in v1.0 — use OTLP JSON instead. See [Deferred features](#deferred-features).

## SLO budgets

mcptrace ships a tiny but complete SLO engine. SLOs are declared in a
TOML file and evaluated offline against a captured JSONL (or, in
streaming mode, incrementally — streaming mode lands in v1.1).

```toml
# slo.toml

[[slo]]
name = "tools-fast"
metric = "latency_p95_ms"
target = 200            # p95 tool-call latency must stay <= 200ms
window = "5m"
burn_rate_threshold = 2.0

[[slo]]
name = "errors-low"
metric = "error_rate"
target = 0.01           # <= 1% of calls can error
window = "1h"
burn_rate_threshold = 14.4   # fast-burn 1h alert
tool = "search"         # only applies to the `search` tool

[[slo]]
name = "availability"
metric = "availability"
target = 0.995
window = "30d"
burn_rate_threshold = 1.0
```

### Burn rate math

mcptrace supports three metric flavors, each with a slightly different
burn-rate interpretation. In every case the convention is "higher
burn = worse, alert fires when `burn_rate >= burn_rate_threshold`".

```
# error_rate   — target is the MAX allowed error rate (e.g. 0.01 = "1% max")
actual_error_rate  = errors / total       over the window
burn_rate          = actual_error_rate / target
# (target == 0 ⇒ zero tolerance ⇒ any error alerts)

# availability — target is the MIN required uptime (e.g. 0.995). SRE workbook form.
error_budget       = 1 - target
actual_error_rate  = 1 - actual_availability
burn_rate          = actual_error_rate / error_budget      if avail < target, else 0

# latency_p95_ms — target is the MAX acceptable p95 latency in ms.
burn_rate          = observed_p95_ms / target
```

Latency and error_rate share the same "budget is the target" framing so
ops can phrase both in the same threshold language; availability follows
the classic SRE formulation because it's how most operators already
think about uptime SLOs.

### Running a check

```bash
mcptrace slo check --spans spans.jsonl --config slo.toml
# Exits 0 if nothing is burning, 1 otherwise. Prints a table.
```

Wire it into CI, cron, or a serverless function — the binary is 8 MB,
has zero runtime deps, and reads a file.

## Security

Observability tools are a tempting place for secrets to leak. mcptrace
is built specifically to **not see them**:

- **`#![forbid(unsafe_code)]`** at crate root.
- **Argument digests only**: the `Span` type has no field for tool
  arguments. We hash the raw wire bytes with sha256 and persist only
  the 64-char hex digest. Identical args still deduplicate to the
  same digest, so repeat-call patterns remain visible.
- **Error message digests only**: tool error messages often echo input.
  We treat them the same way as arguments.
- **Frame size caps**: a 1 MB default cap on every JSON-RPC frame;
  oversized frames are dropped (not forwarded) and a counter is
  bumped. Configurable via `--max-frame-bytes`.
- **Exporter timeouts**: 5 second default HTTP timeout on every
  exporter. Failed exports drop the batch, never stall the proxy.
- **No inbound network**: the proxy itself listens on nothing. Only
  the child's stdin/stdout and (optional) outbound HTTP for exporters.
- **No `.unwrap()` or `.expect()`** in any non-test code path. `fn main`
  returns `anyhow::Result`.

### Threat model

mcptrace is in the "observe and forward" trust tier:

- It *trusts* the agent-side stdio pipe (that's where your secrets
  already live).
- It does not *leak* those secrets into spans/exporters/logs.
- It does not *defend* against a malicious child process — if your
  MCP server is compromised, mcptrace can't save you. Use sandboxing
  (containers, seccomp, Windows Job Objects) for that.

## Platform notes

- **Windows**: we use `tokio::io::stdin()`/`stdout()` which handle
  Windows line endings correctly. Output is always LF, never CRLF —
  we do not trust the platform default.
- **Linux/macOS**: SIGPIPE is handled gracefully by checking write
  errors.
- **No native TLS**: we build `reqwest` with `rustls-tls` only, so the
  binary is portable and statically linked (no `libssl.so` dependency
  on Linux).

## Build & install

```bash
# From source (requires rust 1.75+)
git clone https://github.com/JSLEEKR/mcptrace-rs.git
cd mcptrace-rs
cargo build --release
./target/release/mcptrace --version

# Or install to your cargo bin dir:
cargo install --path .
```

## Tests

```
cargo test
```

This runs:
- **187 unit tests** across the 11 library modules (digest, duration,
  trace-id, span, jsonrpc, store, exporter, slo, stats, proxy, cli)
- **12 integration tests** in `tests/integration.rs` that exercise
  end-to-end workflows (store → read → stats → SLO → replay).

All tests are deterministic and hermetic: no network, no child
processes. The stdio proxy correlation logic is tested by driving
`pending_from_request` and `finalize` directly.

## Comparison

| | mcptrace-rs | Langfuse | Helicone | OTel auto-instrumentation |
| --- | --- | --- | --- | --- |
| MCP tool-call spans | **yes** | no (LLM-only) | no (LLM-only) | no |
| stdio JSON-RPC proxy | **yes** | no | no | no |
| Arg redaction by default | **yes (sha256)** | manual | manual | off |
| Declarative SLO + burn-rate | **yes** | no | no | no (separate tool) |
| Zero-dep single binary | **yes (Rust)** | no | no | no |
| Runtime per-tool percentiles | **yes** | partial | partial | separate pipeline |
| Streaming analytics | v1.1 | yes | yes | yes |
| LLM prompt tracing | **no (out of scope)** | yes | yes | partial |

**TL;DR**: if you already have Langfuse for your prompts and want the
same level of observability for your MCP tool calls, drop mcptrace in
front of your server.

## Deferred features

The following are explicitly out of scope for v1.0, with rationale:

- **HTTP-mode proxy (`proxy http --upstream URL --listen PORT`)**: the
  stdio proxy is the 95% use case for MCP today, and reqwest-as-server
  plumbing pushes v1 over scope. v1.1 target.
- **SQLite storage backend**: would need `rusqlite` + a migration story.
  JSONL covers the common "drop into a log stream" path; relational
  analytics can run on the JSONL file with `sqlite :memory:` +
  `.import`. v1.1 target.
- **Jaeger native thrift exporter**: modern Jaeger accepts OTLP JSON,
  so the Jaeger use case is covered today. Native thrift support
  would add thrift codec complexity for marginal gain. Not planned.
- **Streaming SLO alerts to a webhook**: the offline `slo check`
  command covers CI/cron needs. Webhook streaming lands in v1.1.
- **HTTP proxy / bidirectional stream tap**: see above.

## License

MIT. See [LICENSE](LICENSE).

## Author

Built by [@JSLEEKR](https://github.com/JSLEEKR) as part of the
daily-challenge "ship a real tool every day" pipeline.
