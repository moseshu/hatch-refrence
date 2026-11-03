//! Database schema. Every table is created with `IF NOT EXISTS` so migration is
//! idempotent. Timestamps are RFC3339 UTC strings.

pub const TABLES: &[&str] = &[
    "meta",
    "audit",
    "mail_messages",
    "mail_cursor",
    "triage_results",
    "calendar_events",
    "claims",
    "people",
    "approvals",
    "action_ledger",
    "grants",
    "notifications",
    "workflow_runs",
    "workflow_steps",
    "sessions",
    "session_messages",
    "usage_log",
    "checkpoints",
];

pub const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS meta (
    key   TEXT PRIMARY KEY,
    value TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS audit (
    id     INTEGER PRIMARY KEY AUTOINCREMENT,
    ts     TEXT NOT NULL,
    actor  TEXT NOT NULL,
    event  TEXT NOT NULL,
    detail TEXT NOT NULL
);

-- ---------------------------------------------------------------- mail
CREATE TABLE IF NOT EXISTS mail_messages (
    id          TEXT PRIMARY KEY,
    account     TEXT NOT NULL,
    message_id  TEXT NOT NULL,
    uid         INTEGER,
    from_addr   TEXT NOT NULL,
    from_name   TEXT NOT NULL,
    to_addrs    TEXT NOT NULL,
    subject     TEXT NOT NULL,
    sent_at     TEXT,
    body        TEXT NOT NULL,
    fetched_at  TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS mail_messages_sent ON mail_messages(sent_at);

CREATE TABLE IF NOT EXISTS mail_cursor (
    account  TEXT PRIMARY KEY,
    last_uid INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS triage_results (
    mail_id      TEXT PRIMARY KEY,
    category     TEXT NOT NULL,
    priority     TEXT NOT NULL,
    summary      TEXT NOT NULL,
    needs_reply  INTEGER NOT NULL,
    reply_hint   TEXT NOT NULL,
    events_json  TEXT NOT NULL,
    suspicious   INTEGER NOT NULL,
    notes        TEXT NOT NULL,
    model        TEXT NOT NULL,
    triaged_at   TEXT NOT NULL
);

-- ------------------------------------------------------------ calendar
CREATE TABLE IF NOT EXISTS calendar_events (
    uid        TEXT PRIMARY KEY,
    title      TEXT NOT NULL,
    starts_at  TEXT NOT NULL,
    ends_at    TEXT NOT NULL,
    location   TEXT NOT NULL,
    source     TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS calendar_events_start ON calendar_events(starts_at);

-- -------------------------------------------------------------- memory
CREATE TABLE IF NOT EXISTS claims (
    id           TEXT PRIMARY KEY,
    subject      TEXT NOT NULL,
    text         TEXT NOT NULL,
    tags         TEXT NOT NULL,
    source       TEXT NOT NULL,
    trust        TEXT NOT NULL,
    confidence   REAL NOT NULL,
    created_at   TEXT NOT NULL,
    valid_until  TEXT,
    supersedes   TEXT,
    replaced_by  TEXT,
    inferred     INTEGER NOT NULL DEFAULT 0
);
CREATE INDEX IF NOT EXISTS claims_subject ON claims(subject);

CREATE TABLE IF NOT EXISTS people (
    email        TEXT PRIMARY KEY,
    name         TEXT NOT NULL,
    aliases      TEXT NOT NULL,
    importance   TEXT NOT NULL,
    notes        TEXT NOT NULL,
    last_seen    TEXT,
    interactions INTEGER NOT NULL DEFAULT 0
);

-- -------------------------------------------------------------- policy
CREATE TABLE IF NOT EXISTS approvals (
    id               TEXT PRIMARY KEY,
    idempotency_key  TEXT NOT NULL UNIQUE,
    kind             TEXT NOT NULL,
    action_class     TEXT NOT NULL,
    title            TEXT NOT NULL,
    preview          TEXT NOT NULL,
    payload          TEXT NOT NULL,
    payload_hash     TEXT NOT NULL,
    reasons          TEXT NOT NULL,
    status           TEXT NOT NULL,
    origin           TEXT NOT NULL,
    created_at       TEXT NOT NULL,
    expires_at       TEXT NOT NULL,
    decided_at       TEXT,
    note             TEXT NOT NULL DEFAULT ''
);
CREATE INDEX IF NOT EXISTS approvals_status ON approvals(status);

CREATE TABLE IF NOT EXISTS action_ledger (
    idempotency_key TEXT PRIMARY KEY,
    kind            TEXT NOT NULL,
    status          TEXT NOT NULL,
    result          TEXT NOT NULL,
    started_at      TEXT NOT NULL,
    finished_at     TEXT
);

CREATE TABLE IF NOT EXISTS grants (
    id                  TEXT PRIMARY KEY,
    action_class        TEXT NOT NULL,
    recipients          TEXT NOT NULL,
    max_uses            INTEGER,
    used                INTEGER NOT NULL DEFAULT 0,
    allow_when_tainted  INTEGER NOT NULL DEFAULT 0,
    expires_at          TEXT,
    created_at          TEXT NOT NULL,
    revoked             INTEGER NOT NULL DEFAULT 0,
    note                TEXT NOT NULL DEFAULT ''
);

CREATE TABLE IF NOT EXISTS notifications (
    id         TEXT PRIMARY KEY,
    ts         TEXT NOT NULL,
    delivery   TEXT NOT NULL,
    priority   TEXT NOT NULL,
    text       TEXT NOT NULL,
    ref_id     TEXT NOT NULL,
    delivered  INTEGER NOT NULL DEFAULT 0
);

-- ----------------------------------------------------------- workflows
CREATE TABLE IF NOT EXISTS workflow_runs (
    run_id       TEXT PRIMARY KEY,
    workflow     TEXT NOT NULL,
    started_at   TEXT NOT NULL,
    finished_at  TEXT,
    status       TEXT NOT NULL,
    receipt      TEXT NOT NULL DEFAULT ''
);

CREATE TABLE IF NOT EXISTS workflow_steps (
    workflow    TEXT NOT NULL,
    item        TEXT NOT NULL,
    step        TEXT NOT NULL,
    status      TEXT NOT NULL,
    output      TEXT NOT NULL,
    updated_at  TEXT NOT NULL,
    PRIMARY KEY (workflow, item, step)
);

-- ------------------------------------------------------------ sessions
CREATE TABLE IF NOT EXISTS sessions (
    id          TEXT PRIMARY KEY,
    created_at  TEXT NOT NULL,
    title       TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS session_messages (
    session_id  TEXT NOT NULL,
    seq         INTEGER NOT NULL,
    role        TEXT NOT NULL,
    content     TEXT NOT NULL,
    meta        TEXT NOT NULL,
    compacted   INTEGER NOT NULL DEFAULT 0,
    created_at  TEXT NOT NULL,
    PRIMARY KEY (session_id, seq)
);

CREATE TABLE IF NOT EXISTS usage_log (
    id                 INTEGER PRIMARY KEY AUTOINCREMENT,
    ts                 TEXT NOT NULL,
    workload           TEXT NOT NULL,
    model              TEXT NOT NULL,
    prompt_tokens      INTEGER NOT NULL,
    completion_tokens  INTEGER NOT NULL,
    cached_tokens      INTEGER NOT NULL
);

-- --------------------------------------------------------- checkpoints
-- Snapshot of a workspace file taken right before hatch modifies it.
CREATE TABLE IF NOT EXISTS checkpoints (
    id           TEXT PRIMARY KEY,
    root         TEXT NOT NULL,
    path         TEXT NOT NULL,
    existed      INTEGER NOT NULL,
    hash         TEXT NOT NULL,
    content      BLOB,
    size         INTEGER NOT NULL,
    origin       TEXT NOT NULL,
    approval_id  TEXT NOT NULL,
    created_at   TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS checkpoints_by_path ON checkpoints(root, path, created_at);
"#;
