//! Database schema. Every table is created with `IF NOT EXISTS` so migration is
//! idempotent. Timestamps are RFC3339 UTC strings.

pub const TABLES: &[&str] = &[
    "meta",
    "audit",
    "mail_messages",
    "mail_cursor",
    "triage_results",
    "calendar_events",
    "caldav_calendars",
    "caldav_event_state",
    "caldav_conflicts",
    "claims",
    "memory_index_docs",
    "memory_index_terms",
    "memory_index_meta",
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
    "subagent_runs",
    "subagent_events",
    "subagent_handoffs",
    "jobs",
    "job_runs",
    "job_dedup",
    "briefs",
    "worker_outputs",
    "ideas",
    "feed_items",
    "browser_sessions",
    "plans",
    "plan_steps",
    "tasks",
    "task_steps",
    "task_events",
    "task_units",
    "task_unit_results",
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

-- ------------------------------------------------------------ caldav
-- Phase 6: bidirectional CalDAV sync state. `calendar_events` stays the
-- single source of truth for the events themselves; these tables only
-- record *sync* bookkeeping so a re-run is idempotent and a conflict
-- can be reconstructed after the server version overwrote the local one.
CREATE TABLE IF NOT EXISTS caldav_calendars (
    url          TEXT PRIMARY KEY,
    display_name TEXT NOT NULL DEFAULT '',
    sync_token   TEXT NOT NULL DEFAULT '',
    ctag         TEXT NOT NULL DEFAULT '',
    last_sync_at TEXT
);

-- One row per event ever synced. `etag` / `server_updated_at` are the
-- last-seen server version; `local_hash` is the hash of the local event
-- as it was after the last sync (a mismatch means "edited locally").
-- `conflicted` marks an event with an open conflict: it is never pushed
-- until the conflict is resolved.
CREATE TABLE IF NOT EXISTS caldav_event_state (
    uid               TEXT PRIMARY KEY,
    calendar_url      TEXT NOT NULL,
    etag              TEXT NOT NULL DEFAULT '',
    server_updated_at TEXT NOT NULL DEFAULT '',
    local_hash        TEXT NOT NULL DEFAULT '',
    conflicted        INTEGER NOT NULL DEFAULT 0
);

-- Conflict records (see caldav/sync.rs for the rule). `kind` is one of
-- `edit` (both sides changed), `server_deleted` (server deleted, local
-- edited), `local_deleted` (local deleted, server changed).
-- `local_snapshot` is the JSON of the local event version that the server
-- version replaced (or the literal `__deleted__` when the local side had
-- deleted the event), so `resolve --keep local` can restore and push it.
-- `kept` is `server` or `local`; `status` is `open` or `resolved`.
-- `local_updated_at` is the time the local change was *detected* (local
-- edits carry no edit timestamp); `server_updated_at` comes from the
-- server .ics LAST-MODIFIED / DTSTAMP when present.
CREATE TABLE IF NOT EXISTS caldav_conflicts (
    uid               TEXT PRIMARY KEY,
    calendar_url      TEXT NOT NULL,
    kind              TEXT NOT NULL DEFAULT 'edit',
    local_updated_at  TEXT NOT NULL,
    server_updated_at TEXT NOT NULL,
    kept              TEXT NOT NULL,
    local_snapshot    TEXT NOT NULL,
    status            TEXT NOT NULL DEFAULT 'open',
    created_at        TEXT NOT NULL,
    resolved_at       TEXT
);

-- -------------------------------------------------------------- memory
-- P2 (memory depth): `salience` (0.0–1.0, how much this claim matters
-- when ranking recall) and `last_used_at` (NULL = never injected into a
-- turn; ranking falls back to `created_at`). Added in schema v9; see
-- `Db::migrate` for the ALTER TABLE path on pre-v9 databases.
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
    inferred     INTEGER NOT NULL DEFAULT 0,
    salience     REAL NOT NULL DEFAULT 0.5,
    last_used_at TEXT
);
CREATE INDEX IF NOT EXISTS claims_subject ON claims(subject);

-- ------------------------------------------------- memory index (bank/index split)
-- The bank is `claims`; the index is a persistent BM25 inverted index over
-- the current (not replaced) claims. It is maintained incrementally on every
-- bank write and verified/rebuilt by `memory_flush` and `hatch memory
-- reindex`. `term` is the leftmost column of the PK, so term lookups need
-- no extra index. Expired claims stay indexed (expiry is time-dependent)
-- and are filtered at query time.
CREATE TABLE IF NOT EXISTS memory_index_docs (
    claim_id TEXT PRIMARY KEY,
    len      INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS memory_index_terms (
    term     TEXT NOT NULL,
    claim_id TEXT NOT NULL,
    tf       INTEGER NOT NULL,
    PRIMARY KEY (term, claim_id)
);
CREATE TABLE IF NOT EXISTS memory_index_meta (
    key   TEXT PRIMARY KEY,
    value TEXT NOT NULL
);

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
    delivered  INTEGER NOT NULL DEFAULT 0,
    -- Schema v18 (task 8): which task the notification is about ('' =
    -- not task-linked) and what kind of task event it is
    -- ('approval' / 'completed' / 'failed' / '').
    task_id    TEXT NOT NULL DEFAULT '',
    kind       TEXT NOT NULL DEFAULT ''
);
CREATE INDEX IF NOT EXISTS notifications_task ON notifications(task_id, delivered);

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

-- ----------------------------------------------------------- sub-agents
-- One row per delegated run. `parent` is `session:<id>` or `subagent:<id>`;
-- `session` is the top-level chat session the delegation tree belongs to.
CREATE TABLE IF NOT EXISTS subagent_runs (
    id                 TEXT PRIMARY KEY,
    parent             TEXT NOT NULL,
    session            TEXT NOT NULL,
    child_session      TEXT NOT NULL DEFAULT '',
    depth              INTEGER NOT NULL,
    agent_type         TEXT NOT NULL,
    task               TEXT NOT NULL,
    status             TEXT NOT NULL,
    started_at         TEXT NOT NULL,
    finished_at        TEXT,
    steps              INTEGER NOT NULL DEFAULT 0,
    answer             TEXT NOT NULL DEFAULT '',
    error              TEXT NOT NULL DEFAULT '',
    model_calls        INTEGER NOT NULL DEFAULT 0,
    prompt_tokens      INTEGER NOT NULL DEFAULT 0,
    completion_tokens  INTEGER NOT NULL DEFAULT 0,
    child_pid          INTEGER NOT NULL DEFAULT 0
);
CREATE INDEX IF NOT EXISTS subagent_runs_by_session ON subagent_runs(session, started_at);

-- Progress events: one short line per step / tool call, never full tool output.
CREATE TABLE IF NOT EXISTS subagent_events (
    run_id  TEXT NOT NULL,
    seq     INTEGER NOT NULL,
    ts      TEXT NOT NULL,
    kind    TEXT NOT NULL,
    detail  TEXT NOT NULL,
    PRIMARY KEY (run_id, seq)
);

-- Handoffs: a finished sub-agent's report, waiting to be delivered into
-- a later turn. `delivered` flips the first time a turn collects it, so
-- the same handoff is never injected twice.
CREATE TABLE IF NOT EXISTS subagent_handoffs (
    run_id    TEXT PRIMARY KEY,
    handoff   TEXT NOT NULL,
    tainted   INTEGER NOT NULL DEFAULT 0,
    delivered INTEGER NOT NULL DEFAULT 0,
    created_at TEXT NOT NULL
);

-- ----------------------------------------------------------- scheduler
-- Job definitions. Runs and dedup state live in their own tables so the
-- definition never mixes with execution history.
CREATE TABLE IF NOT EXISTS jobs (
    id                    TEXT PRIMARY KEY,
    name                  TEXT NOT NULL,
    kind                  TEXT NOT NULL,
    schedule              TEXT NOT NULL,
    params                TEXT NOT NULL DEFAULT '{}',
    scope                 TEXT NOT NULL DEFAULT '[]',
    owner                 TEXT NOT NULL,
    delivery              TEXT NOT NULL,
    done_condition        TEXT NOT NULL DEFAULT '',
    catch_up              TEXT NOT NULL,
    max_failures          INTEGER NOT NULL,
    timeout_secs          INTEGER NOT NULL,
    enabled               INTEGER NOT NULL,
    disabled_reason       TEXT NOT NULL DEFAULT '',
    created_at            TEXT NOT NULL,
    anchor_at             TEXT NOT NULL,
    next_run_at           TEXT,
    retry_at              TEXT,
    consecutive_failures  INTEGER NOT NULL DEFAULT 0,
    last_run_at           TEXT,
    last_status           TEXT NOT NULL DEFAULT ''
);

CREATE TABLE IF NOT EXISTS job_runs (
    id           TEXT PRIMARY KEY,
    job_id       TEXT NOT NULL,
    trigger      TEXT NOT NULL,
    scheduled_for TEXT,
    started_at   TEXT NOT NULL,
    finished_at  TEXT,
    status       TEXT NOT NULL,
    missed       INTEGER NOT NULL DEFAULT 0,
    summary      TEXT NOT NULL DEFAULT '',
    error        TEXT NOT NULL DEFAULT ''
);
CREATE INDEX IF NOT EXISTS job_runs_by_job ON job_runs(job_id, started_at);

CREATE TABLE IF NOT EXISTS job_dedup (
    job_id      TEXT NOT NULL,
    key         TEXT NOT NULL,
    created_at  TEXT NOT NULL,
    PRIMARY KEY (job_id, key)
);

-- ------------------------------------------------------------- workers
CREATE TABLE IF NOT EXISTS briefs (
    id          TEXT PRIMARY KEY,
    local_date  TEXT NOT NULL,
    created_at  TEXT NOT NULL,
    content     TEXT NOT NULL,
    facts       TEXT NOT NULL,
    model       TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS briefs_by_date ON briefs(local_date, created_at);

-- Output of background workers waiting to be surfaced in the next chat.
-- `goal_id` links an output to a goal workspace (empty = no goal);
-- attention for a goal counts its unread linked outputs.
CREATE TABLE IF NOT EXISTS worker_outputs (
    id          TEXT PRIMARY KEY,
    kind        TEXT NOT NULL,
    created_at  TEXT NOT NULL,
    title       TEXT NOT NULL,
    content     TEXT NOT NULL,
    read_at     TEXT,
    goal_id     TEXT NOT NULL DEFAULT ''
);

-- ------------------------------------------------------------- goals
-- Ideas corpus (hatch-goals). `status` is `new` / `acted` / `dismissed`;
-- dismissed ideas are excluded from listings by construction and never
-- resurface. `confidence` is 1.0 for user ideas, 0.3 for worker
-- suggestions (a suggestion, never a decision).
CREATE TABLE IF NOT EXISTS ideas (
    id          TEXT PRIMARY KEY,
    text        TEXT NOT NULL,
    source      TEXT NOT NULL,
    status      TEXT NOT NULL,
    confidence  REAL NOT NULL,
    created_at  TEXT NOT NULL
);

-- Feed items (hatch-goals). `source_facts` is a JSON list of fact ids
-- (`F1`, …) the item cites; empty only for the fixed intro item
-- (`is_intro = 1`), which is inserted once and never regenerated.
CREATE TABLE IF NOT EXISTS feed_items (
    id          TEXT PRIMARY KEY,
    created_at  TEXT NOT NULL,
    title       TEXT NOT NULL,
    content     TEXT NOT NULL,
    source_facts TEXT NOT NULL,
    model       TEXT NOT NULL,
    is_intro    INTEGER NOT NULL DEFAULT 0
);

-- ------------------------------------------------------------- browser
-- One row per browser tab the assistant opened. `target_id` lets any hatch
-- process re-attach to a tab of a still-running browser.
CREATE TABLE IF NOT EXISTS browser_sessions (
    id          TEXT PRIMARY KEY,
    profile     TEXT NOT NULL,
    origin      TEXT NOT NULL,
    target_id   TEXT NOT NULL DEFAULT '',
    url         TEXT NOT NULL DEFAULT '',
    title       TEXT NOT NULL DEFAULT '',
    state       TEXT NOT NULL,
    reason      TEXT NOT NULL DEFAULT '',
    created_at  TEXT NOT NULL,
    updated_at  TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS browser_sessions_by_state ON browser_sessions(state, updated_at);

-- --------------------------------------------------------------- plans
-- A plan is a structured, approved sequence of tool calls. `status` is one
-- of: draft, pending_approval, approved, executing, blocked, completed,
-- rejected, superseded, cancelled, needs_reapproval, failed. `blocked`
-- pauses an executing plan while it waits on something external (an
-- approval, the user, an external dependency); it resumes to `executing`
-- when that arrives — unlike `needs_reapproval`, which means the content
-- changed and a fresh approval round is required. `failed` is the terminal
-- state for a run that died irrecoverably (distinct from a single failed
-- step, `plan_steps.status = 'failed'`). `approved_steps_json` is the
-- exact step list the user approved (canonical JSON); the runner compares
-- every step against it before executing, so an edit after approval can
-- never silently run. `deviation` records why a run stopped for re-approval.
-- `goal` is the durable goal this plan serves, as distinct from `task`
-- (the prompt that produced the plan). `current_step` is the id of the
-- step execution has reached ('' = none yet / finished). `milestones_json`
-- is a JSON array of `{title, done}` markers, read and written with the
-- plan as a whole (no separate table).
CREATE TABLE IF NOT EXISTS plans (
    id                  TEXT PRIMARY KEY,
    task                TEXT NOT NULL,
    status              TEXT NOT NULL,
    -- Schema v12 (M0 §10.2): plan revision. A user change commits a
    -- new version; approvals bound to an old version no longer apply.
    version             INTEGER NOT NULL DEFAULT 1,
    model               TEXT NOT NULL DEFAULT '',
    created_at          TEXT NOT NULL,
    updated_at          TEXT NOT NULL,
    decided_at          TEXT,
    decided_by          TEXT NOT NULL DEFAULT '',
    decision_note       TEXT NOT NULL DEFAULT '',
    approved_steps_json TEXT NOT NULL DEFAULT '',
    approved_hash       TEXT NOT NULL DEFAULT '',
    deviation           TEXT NOT NULL DEFAULT '',
    goal                TEXT NOT NULL DEFAULT '',
    current_step        TEXT NOT NULL DEFAULT '',
    milestones_json     TEXT NOT NULL DEFAULT '[]'
);
CREATE INDEX IF NOT EXISTS plans_status ON plans(status, created_at);

CREATE TABLE IF NOT EXISTS plan_steps (
    plan_id   TEXT NOT NULL,
    idx       INTEGER NOT NULL,
    step_id   TEXT NOT NULL,
    title     TEXT NOT NULL,
    tool      TEXT NOT NULL,
    arguments TEXT NOT NULL,
    files     TEXT NOT NULL,
    expected  TEXT NOT NULL,
    risk      TEXT NOT NULL,
    status    TEXT NOT NULL DEFAULT 'pending',
    output    TEXT NOT NULL DEFAULT '',
    approval_id TEXT NOT NULL DEFAULT '',
    -- Schema v16 (task 6): how the step failed, when it failed
    -- ('' = not failed; 'transient' = retryable, 'permanent' =
    -- not retryable, 'needs_human' = needs a human decision).
    failure_kind TEXT NOT NULL DEFAULT '',
    updated_at TEXT NOT NULL,
    PRIMARY KEY (plan_id, idx)
);

-- ------------------------------------------------------------- tasks
-- Unified task ledger (P1 D6). One row per task of any kind — worker job,
-- sub-agent run, scheduled job run, one-off task — so a process restart
-- can reconcile every unfinished task in one place instead of each
-- subsystem keeping its own half-ledger.
--
-- Why a new table, not `jobs` / `job_runs` / `subagent_runs`:
-- `jobs` is a *definition* table (schedules: name/kind/schedule/owner/
-- next_run_at/catch_up/max_failures); its own comment says runs live in
-- their own tables so the definition never mixes with execution history.
-- `job_runs` is scheduler-run history tied to a job definition (no owner
-- / parent / retry count / heartbeat / idempotency key). `subagent_runs`
-- is sub-agent-specific (child pid, sessions, handoffs). None of them is
-- a unified task, so the ledger gets its own table.
--
-- `status` is one of: queued, running, waiting, blocked, done, failed.
-- `blocked` here is a *task* state (this task waits on something
-- external); it is a different object from `PlanStatus::Blocked` (a
-- *plan* state, D1) — same word, different ledger, do not confuse them.
-- `idempotency_key` is UNIQUE but nullable: a NULL key means "no dedup",
-- and SQLite lets many NULLs share a UNIQUE column. `retries` counts
-- restarts after waiting; `max_retries` bounds them (default 3, the same
-- bound D8 uses for transient failures). `last_heartbeat` is set at
-- creation and refreshed by `heartbeat()` while the task runs; the
-- startup reconcile treats a `running` task whose heartbeat is older
-- than the stale threshold as dead.
CREATE TABLE IF NOT EXISTS tasks (
    id              TEXT PRIMARY KEY,
    kind            TEXT NOT NULL,
    title           TEXT NOT NULL,
    owner           TEXT NOT NULL,
    parent_task     TEXT NOT NULL DEFAULT '',
    status          TEXT NOT NULL,
    retries         INTEGER NOT NULL DEFAULT 0,
    max_retries     INTEGER NOT NULL DEFAULT 3,
    last_heartbeat  TEXT NOT NULL,
    idempotency_key TEXT UNIQUE,
    error           TEXT NOT NULL DEFAULT '',
    created_at      TEXT NOT NULL,
    updated_at      TEXT NOT NULL,
    finished_at     TEXT,
    -- Schema v12 (M0): execution lease + structured blocking reason.
    -- `lease_owner`/`lease_generation`/`lease_expires_at` name the one
    -- worker allowed to write this task right now; `lease_ttl_secs` is
    -- the TTL the owner asked for (heartbeat refreshes the expiry by
    -- the same TTL). A worker with an old generation, a different
    -- owner, or an expired lease cannot submit state — its UPDATEs
    -- match zero rows. `blocked_kind` is one of '', 'approval',
    -- 'login', 'missing_info', 'verification' ('' = not blocked).
    -- Schema v13 (M1): `plan_id` names the hatch-plan plan this task
    -- runs ('' = none — the meeting-brief kind keeps its steps in
    -- task_steps); `plan_version` is the §10.2 revision (commands carry
    -- the expected version; approvals are bound to the version they
    -- were requested under); `max_model_calls` is the model-call budget
    -- (0 = unlimited — task 3: an over-budget task stops with its
    -- completed steps still readable).
    lease_owner     TEXT NOT NULL DEFAULT '',
    lease_generation INTEGER NOT NULL DEFAULT 0,
    lease_ttl_secs  INTEGER NOT NULL DEFAULT 0,
    lease_expires_at TEXT NOT NULL DEFAULT '',
    blocked_kind    TEXT NOT NULL DEFAULT '',
    plan_id         TEXT NOT NULL DEFAULT '',
    plan_version    INTEGER NOT NULL DEFAULT 1,
    max_model_calls INTEGER NOT NULL DEFAULT 0,
    -- Schema v15 (M2): the task's mail source (`fixture` for an eval
    -- world, `gmail` for a real-account run; '' = pre-v15 fixture
    -- task). An approval bound to a task executes with the task's
    -- sender, so a Gmail invite is really sent.
    source          TEXT NOT NULL DEFAULT ''
);
CREATE INDEX IF NOT EXISTS tasks_status ON tasks(status, created_at);
CREATE INDEX IF NOT EXISTS tasks_owner ON tasks(owner, created_at);
CREATE INDEX IF NOT EXISTS tasks_parent ON tasks(parent_task, created_at);

-- ------------------------------------------------- task state contract (M0)
-- Primary keys, one row per thing the runtime must never lose or double:
-- * tasks(id): the task itself. Status is one of queued, running,
--   waiting, blocked, done, failed, cancelled (cancelled is terminal,
--   added in v12; cancel is a control-plane write, it needs no lease).
-- * plans(id): the plan; `version` counts plan revisions (M0 §10.2:
--   a user change commits a new version and invalidates approvals
--   bound to the old one). plan_steps are keyed (plan_id, idx).
-- * approvals(id): the approval; action_ledger(idempotency_key) is
--   the receipt: one row per executed side effect, keyed by the stable
--   operation id, so a retry reuses the receipt instead of re-executing.
-- * task_steps(task_id, idx): the durable step record — step id, title,
--   tool + argument summary, attempts, input refs, approval id, receipt
--   id, output summary. A step's status is the task's progress.
-- * task_events(task_id, seq): the append-only event log, one sequence
--   per task. A client resumes from a cursor (task_id, seq) and dedups
--   on it; state changes and their events commit in one transaction.
-- * task_units(task_id, unit_seq) + task_unit_results(task_id,
--   unit_seq, call_id): one model decision batch and its per-call
--   results. A call is complete iff its result row exists, so a killed
--   worker resumes exactly the calls without results — request and
--   observation stay paired.
CREATE TABLE IF NOT EXISTS task_steps (
    task_id     TEXT NOT NULL,
    idx         INTEGER NOT NULL,
    step_id     TEXT NOT NULL,
    title       TEXT NOT NULL DEFAULT '',
    tool        TEXT NOT NULL DEFAULT '',
    arguments   TEXT NOT NULL DEFAULT '{}',
    expected    TEXT NOT NULL DEFAULT '',
    status      TEXT NOT NULL DEFAULT 'pending',
    attempts    INTEGER NOT NULL DEFAULT 0,
    input_refs  TEXT NOT NULL DEFAULT '[]',
    approval_id TEXT NOT NULL DEFAULT '',
    receipt_id  TEXT NOT NULL DEFAULT '',
    output      TEXT NOT NULL DEFAULT '',
    -- Schema v16 (task 6): how the step failed, mirrored from the
    -- plan's failure kind so the plan card shows it without joining
    -- the plan ('' = not failed; 'transient' / 'permanent' /
    -- 'needs_human').
    failure_kind TEXT NOT NULL DEFAULT '',
    created_at  TEXT NOT NULL,
    updated_at  TEXT NOT NULL,
    PRIMARY KEY (task_id, idx)
);
CREATE TABLE IF NOT EXISTS task_events (
    task_id    TEXT NOT NULL,
    seq        INTEGER NOT NULL,
    kind       TEXT NOT NULL,
    step_id    TEXT NOT NULL DEFAULT '',
    detail     TEXT NOT NULL DEFAULT '',
    created_at TEXT NOT NULL,
    PRIMARY KEY (task_id, seq)
);
CREATE INDEX IF NOT EXISTS task_events_kind ON task_events(task_id, kind);
CREATE TABLE IF NOT EXISTS task_units (
    task_id     TEXT NOT NULL,
    unit_seq    INTEGER NOT NULL,
    step_id     TEXT NOT NULL DEFAULT '',
    calls_json  TEXT NOT NULL DEFAULT '[]',
    status      TEXT NOT NULL DEFAULT 'pending',
    model_calls INTEGER NOT NULL DEFAULT 0,
    tool_calls  INTEGER NOT NULL DEFAULT 0,
    created_at  TEXT NOT NULL,
    updated_at  TEXT NOT NULL,
    PRIMARY KEY (task_id, unit_seq)
);
CREATE TABLE IF NOT EXISTS task_unit_results (
    task_id  TEXT NOT NULL,
    unit_seq INTEGER NOT NULL,
    call_id  TEXT NOT NULL,
    tool     TEXT NOT NULL,
    content  TEXT NOT NULL DEFAULT '',
    trust    TEXT NOT NULL DEFAULT '',
    created_at TEXT NOT NULL,
    PRIMARY KEY (task_id, unit_seq, call_id)
);
-- Schema v16 (task 6): the long-task context — one row per task, the
-- loadable state a long task carries across turns, compressions and
-- restarts. `goal` is the delivery goal; `facts_json` the confirmed
-- facts; `constraints_json` the hard constraints (including permission
-- constraints — what the task must never do); `evidence_json` the
-- evidence references `{kind, id, detail}`; `open_items_json` the
-- unfinished items. The remaining budget is NOT stored here: it is
-- derived from the ledger (`tasks.max_model_calls` minus the units'
-- `model_calls` sum), so a replan can never reset it by rewriting
-- this row. The task list itself stays the authority (§10.3): this
-- table holds the context, the ledger holds the state.
CREATE TABLE IF NOT EXISTS task_context (
    task_id         TEXT PRIMARY KEY,
    goal            TEXT NOT NULL DEFAULT '',
    facts_json      TEXT NOT NULL DEFAULT '[]',
    constraints_json TEXT NOT NULL DEFAULT '[]',
    evidence_json   TEXT NOT NULL DEFAULT '[]',
    open_items_json TEXT NOT NULL DEFAULT '[]',
    updated_at      TEXT NOT NULL
);
-- Schema v17 (task 7): durable delegation. `task_delegations` is
-- one row per (parent, step) delegation: the child is a first-class
-- task in `tasks` (parent_task set, its own budget), the row binds it
-- to the parent's step and carries its structured result + consume
-- state. `task_consumptions` is the exactly-once marker: a parent
-- consumes a child at most once, and a replay after a crash finds
-- the marker and does nothing. `task_resource_versions` is what the
-- merge checks file writes against: a write applies only when the
-- version the child read is still the version the parent knows;
-- anything else is a conflict, never a silent overwrite.
CREATE TABLE IF NOT EXISTS task_delegations (
    parent_task  TEXT NOT NULL,
    child_task   TEXT NOT NULL,
    step_id      TEXT NOT NULL DEFAULT '',
    status       TEXT NOT NULL DEFAULT 'delegated',
    result_json  TEXT NOT NULL DEFAULT '',
    failure_kind TEXT NOT NULL DEFAULT '',
    consumed_at  TEXT NOT NULL DEFAULT '',
    created_at   TEXT NOT NULL,
    updated_at   TEXT NOT NULL,
    PRIMARY KEY (parent_task, child_task)
);
CREATE INDEX IF NOT EXISTS task_delegations_parent ON task_delegations(parent_task, step_id);
CREATE INDEX IF NOT EXISTS task_delegations_child ON task_delegations(child_task);
-- One live delegation per (parent, step): a concurrent double-spawn
-- for the same step collides here, and `spawn` re-reads the winner
-- instead of creating a second child. Failed / cancelled / superseded
-- rows leave the step free for a re-spawn.
CREATE UNIQUE INDEX IF NOT EXISTS task_delegations_live_step ON task_delegations(parent_task, step_id) WHERE status IN ('delegated', 'running', 'done', 'consumed');
CREATE TABLE IF NOT EXISTS task_consumptions (
    parent_task TEXT NOT NULL,
    child_task  TEXT NOT NULL,
    event_id    INTEGER NOT NULL DEFAULT 0,
    consumed_at TEXT NOT NULL,
    PRIMARY KEY (parent_task, child_task)
);
CREATE TABLE IF NOT EXISTS task_resource_versions (
    task_id    TEXT NOT NULL,
    resource   TEXT NOT NULL,
    version    TEXT NOT NULL DEFAULT '',
    hash       TEXT NOT NULL DEFAULT '',
    updated_by TEXT NOT NULL DEFAULT '',
    updated_at TEXT NOT NULL,
    PRIMARY KEY (task_id, resource)
);
-- Schema v13 (M1): approvals bound to a task's plan version, and the
-- durable §10.2 command record. `task_approvals` links an approval to
-- the plan version it was requested under — the version check at
-- approve time (and the eager invalidation on a version bump) reads
-- this, never the approval payload. `task_commands` is the command
-- itself: task id, the version the user expected, the version it
-- landed on, who sent it, and the outcome (`applied` / `stale`).
CREATE TABLE IF NOT EXISTS task_approvals (
    task_id      TEXT NOT NULL,
    approval_id  TEXT NOT NULL,
    plan_version INTEGER NOT NULL,
    step_id      TEXT NOT NULL DEFAULT '',
    created_at   TEXT NOT NULL,
    PRIMARY KEY (task_id, approval_id)
);
CREATE INDEX IF NOT EXISTS task_approvals_approval ON task_approvals(approval_id);
CREATE TABLE IF NOT EXISTS task_commands (
    id               TEXT PRIMARY KEY,
    task_id          TEXT NOT NULL,
    expected_version INTEGER NOT NULL,
    new_version      INTEGER NOT NULL DEFAULT 0,
    user_id          TEXT NOT NULL DEFAULT '',
    summary          TEXT NOT NULL DEFAULT '',
    status           TEXT NOT NULL DEFAULT 'applied',
    created_at       TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS task_commands_task ON task_commands(task_id, created_at);
"#;
