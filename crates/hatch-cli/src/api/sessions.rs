//! Session endpoints: create, list, get, fork, rewind, delete.
//!
//! Sessions are the unit of a chat: `SessionStore::active` returns the
//! latest summary plus the uncompacted messages, so a summary written
//! by the background compaction worker shows up here automatically.

