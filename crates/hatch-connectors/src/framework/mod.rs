//! Connector framework: manifest, runner protocol, runner, registry.
//!
//! This is the framework layer of `hatch-connectors`. The older mail /
//! calendar modules are the original hardcoded connectors; new connectors
//! are subprocess CLIs described by a manifest and spoken to through the
//! runner protocol. Two reference connectors ship as binaries of this
//! crate: `hatch-notes-connector` (local file notes) and
//! `hatch-mockhttp-connector` (local mock HTTP service, tests only).
//!
//! Only **two** reference connectors are implemented on purpose. Real
//! provider connectors are out of scope for this phase: each one is a new
//! manifest plus a subprocess CLI, no framework change.

pub mod manifest;
pub mod protocol;
pub mod registry;
pub mod runner;

pub use manifest::{ConnectorManifest, OpDef};
pub use protocol::{
    ConnectorError, ConnectorRequest, ConnectorResponse, ERR_AUTH, ERR_BAD_RESPONSE,
    ERR_INTERNAL, ERR_INVALID_ARGS, ERR_NOT_CONFIGURED, ERR_NOT_FOUND, ERR_TIMEOUT,
    ERR_UNKNOWN_OP, ERR_UPSTREAM,
};
pub use registry::{default_dir, find, load_dir, resolve_env};
pub use runner::{call, parse_stdout, request_line, run_child, status, ChildOutcome, StatusCheck};
