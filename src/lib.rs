#![forbid(unsafe_code)]
#![deny(warnings)]
#![warn(clippy::pedantic)]
#![allow(clippy::module_name_repetitions)]
#![allow(clippy::missing_errors_doc)]
#![allow(clippy::missing_panics_doc)]
#![allow(clippy::must_use_candidate)]
#![allow(clippy::cast_precision_loss)]
#![allow(clippy::cast_possible_truncation)]
#![allow(clippy::cast_sign_loss)]
#![allow(clippy::cast_possible_wrap)]
#![allow(clippy::too_many_lines)]
#![allow(clippy::doc_markdown)]

//! # mcptrace
//!
//! Transparent observability proxy and OpenTelemetry exporter for MCP
//! (Model Context Protocol) servers. Sits between an agent and a JSON-RPC
//! MCP server, records every tool call as a span, exports to OTLP / Zipkin /
//! stdout, and enforces declarative SLO budgets with burn-rate alerts.
//!
//! See [crate README](https://github.com/JSLEEKR/mcptrace-rs) for the
//! high-level pitch. Key modules:
//!
//! - [`span`] — the [`span::Span`] observation record and its schema.
//! - [`digest`] — sha256 digest helpers (arguments are never logged
//!   in plaintext; only their hash is kept).
//! - [`jsonrpc`] — minimal JSON-RPC 2.0 framing and parser used by the proxy.
//! - [`store`] — pluggable [`store::SpanStore`] backends (null, jsonl).
//! - [`exporter`] — pluggable [`exporter::Exporter`] backends
//!   (stdout, zipkin, otlp-json).
//! - [`slo`] — rolling-window SLO evaluator and burn-rate math.
//! - [`stats`] — p50/p95/p99 aggregation for the `stats` subcommand.
//! - [`proxy`] — stdio proxy loop (library surface; the actual child
//!   process is spawned by [`crate::proxy::run_stdio_proxy`]).

pub mod cli;
pub mod digest;
pub mod duration;
pub mod error;
pub mod exporter;
pub mod jsonrpc;
pub mod proxy;
pub mod replay;
pub mod slo;
pub mod span;
pub mod stats;
pub mod store;
pub mod trace_id;

pub use error::{Error, Result};
pub use span::{Span, SpanStatus};
