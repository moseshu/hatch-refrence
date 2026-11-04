//! Inference proxy: a loopback HTTP reverse proxy in front of model
//! providers.
//!
//! The agent talks to the proxy exactly as it would talk to a provider
//! (same Responses / Chat Completions wire format, same `base_url`
//! shape), and the proxy forwards each request to the real upstream,
//! streaming the response back unchanged. Because every model call
//! passes through one choke point, the proxy is where three things
//! live that do not belong in the agent:
//!
//! * **Usage accounting** — one JSONL ledger (`<home>/inference/
//!   usage.jsonl`) recording the provider-reported usage of every
//!   call, per model and per day, with a cost estimate from
//!   configured per-million-token prices (no prices are invented:
//!   without configured prices there is no estimate).
//! * **Retry** — 429 / 5xx / transport failures are retried with
//!   exponential backoff (default 3 retries), but only before the
//!   first upstream byte is forwarded; 400 / 401 / 403 pass through
//!   untouched. Provider-specific retries stay where they belong —
//!   the `temperature` 400 retry and the JSON-mode word injection
//!   live in the provider client and still run, because the proxy
//!   forwards upstream 400s verbatim to the client that knows what
//!   they mean.
//! * **Rate limiting** — a fixed-window per-minute cap (default 60).
//!   Over the cap the proxy answers 429 with `Retry-After`; it never
//!   silently drops a request.
//!
//! The proxy only binds loopback addresses and only holds the
//! upstream API key in memory (from the environment, never from a
//! file). It logs one line per request — method, path, status,
//! model, tokens, duration — never request bodies or keys.

pub mod config;
pub mod forward;
pub mod http;
pub mod ledger;
pub mod server;
pub mod usage_extract;

pub use config::{ledger_path, ProxyConfig};
pub use ledger::{Pricing, UsageEntry, UsageLedger, UsageSummary};
pub use server::ProxyServer;
