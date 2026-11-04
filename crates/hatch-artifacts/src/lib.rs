//! `hatch-artifacts` — widgets (cards) and versioned artifacts.
//!
//! Widgets live in [`widget`]: a small JSON card schema (`options` /
//! `list` / `html`), an allowlist HTML sanitiser, and the embed-token
//! rule ([`widget::render`] — inert without the token, a card with it),
//! plus the plain-text fallback the CLI always uses.
//!
//! Artifacts live in [`store`]: versioned directories under
//! `$HATCH_HOME/artifacts/<id>/v<n>/` with a manifest (checksum
//! included) per version. Sharing is an approval-gated egress
//! ([`executor::ShareArtifactExecutor`]); delivery is an explicit step
//! whose message must contain the artifact's path
//! ([`store::ArtifactStore::deliver`]).
//!
//! [`tools`] wires both into the agent's tool registry
//! (`show_options`, `create_artifact`, `share_artifact`,
//! `deliver_artifact`) through the same gate → approval queue →
//! executor path as every other tool.

pub mod executor;
pub mod quota;
pub mod sanitize;
pub mod share;
pub mod store;
pub mod tools;
pub mod widget;

pub use quota::{Quota, Usage};
pub use sanitize::sanitize_html;
pub use share::{ShareRecord, ShareRegistry};
pub use store::{ArtifactKind, ArtifactMeta, ArtifactStore, BuiltArtifact, VersionManifest};
pub use widget::{render, text_fallback, Widget, WidgetKind};
