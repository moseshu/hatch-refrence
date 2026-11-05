# hatch — Personal Agent Assistant

[中文](README.md)

hatch is a personal agent assistant that can be deployed to the cloud. It runs resident, manages your mail, calendar, memory and workspace, calls tools in conversation, uses a browser, delegates tasks asynchronously to sub-agents, works in the background on a schedule, and puts every outward action through an auditable, revocable channel that requires your confirmation.

A few key design choices:

- **Backend first**: this repo is backend only and serves clients through a product API (`/v1/...`); the frontend lives in a separate repo and is not mixed in here
- **Approval first**: irreversible actions (sending, deleting, running commands, sharing) go to an approval queue by default and only execute after you approve, executing exactly what you saw when approving
- **Structured memory**: memory is stored as claims with source, trust level and confidence; corrections form a traceable chain instead of overwriting history
- **Async sub-agents**: delegating returns a run id immediately; the main agent keeps working and you keep chatting; when a sub-agent finishes, its result comes back as a handoff event in the next turn

## Capabilities

| Capability | Description |
|---|---|
| Chat & tools | Memory, calendar and mail search; file read/write and search inside the workspace; sandboxed command execution; web page reading with optional search |
| Mail & calendar triage | Fetch new mail (IMAP, incl. 126/163), isolated classification and summaries, suspicious-mail detection, meeting-conflict detection and free-slot lookup, draft replies in your style; CalDAV two-way sync (server-wins conflicts) |
| Browser | Per-origin isolated Chromium (one profile each for chat / scheduler / sub-agent): read pages, find, click, fill, select, screenshot; submissions need approval, payments are suggestions only, login and CAPTCHA are handed to you |
| Sub-agents | Async delegation (`delegate` / `delegate_parallel`), separate processes over Tokio + UDS; tools can only be narrowed, approvals share one queue; cancel by run id |
| Background work | The daemon runs scheduled jobs: daily brief, scheduled triage, memory maintenance, daily feed; results arrive as "background updates" in your next conversation |
| Goals / ideas / feed | Goal workspaces (`goals/<id>/GOAL.md` + `files/` + `hidden_files/`, state machine + attention); idea library (dismissed ideas never reappear); daily feed (every item cites its source facts) |
| Memory | Structured claims (source, trust, confidence, expiry), correction chains with provenance, persistent Chinese BM25 index, people pages, forgetting; new sessions are mined for durable memories daily at 04:00 |
| Plan mode | The model first produces structured steps stored for approval, then executes step by step; each step is checked against the approved snapshot and stops on any deviation |
| Artifacts | Card widgets (options / list / HTML) and versioned artifacts (build → share approval → deliver); quotas and share tokens with revocation |
| Skills & MCP | Skills (`SKILL.md`, working-style notes only, they grant no tools); MCP client (stdio / HTTP-SSE) and server (stdio / HTTP, per-client tokens + in-process TLS) |
| Multi-channel transport | Main chat and side chats share memory and goals with separate transcripts; one `DISCRETION.md` sharing-rules file per chat; only a local pipe reference transport exists; WeChat / Feishu / Telegram are not implemented |
| Inference proxy | Prompts are assembled stable-prefix-first (hits the provider's prefix cache); a loopback reverse proxy handles forwarding, usage accounting, retries and rate limiting |
| Multiple models | Chat Completions / Responses / Anthropic Messages / Gemini, all with tool calls and streaming; per-workload routing and fallback |
| Safe execution | Trust labels and contamination rules, gatekeeper → approval → executor → idempotency ledger, command sandbox, file checkpoints, secret detection and redaction |
| Interruptible | Ctrl-C stops the current turn including all sub-agents and commands; sessions can continue, fork and rewind |
| Measurable | Built-in sandboxed eval worlds, in pipeline (scripted model) and live (real model) modes |

## Architecture

```
                  ┌──────────────────────── hatch-cli (hatch command) ───────────────────────┐
                  │ init · chat/ask · triage · approvals · grants · memory · calendar ·    │
                  │ agents · session · checkpoint · jobs · brief · goal · ideas · feed ·   │
                  │ daemon · eval · doctor · plan · skills · mcp · vault · browser ·      │
                  │ artifact · transport · inference                                       │
                  └──────────────┬───────────────────────────────┬─────────────────────────┘
  Scheduling      hatch-scheduler (scheduling · job store · run engine · daemon) · hatch-workers (brief · triage · memory)
                                 │                               │
  Scenario        hatch-triage (mail+calendar triage)   hatch-agent (turn loop · context · compaction · sub-agents)   hatch-eval
                                 └───────────────┬───────────────┘
  Capability                      hatch-tools (tool SDK · built-in tools · executors · unified dispatch · checkpoints)
                                                 │
  Domain          hatch-policy (gatekeeper · approvals · ledger)  hatch-memory (memory)  hatch-connectors (mail · calendar)  hatch-llm (models)
                  hatch-sandbox (command sandbox)  hatch-browser (browser)  hatch-inference (inference proxy)
  Foundation      hatch-store (SQLite)  ·  hatch-core (errors · IDs · trust · tokens · secrets · cancel tokens)
```

| Crate | Responsibility |
|---|---|
| `hatch-core` | Error types, content-addressed IDs, trust labels and untrusted envelopes, CJK-aware token estimation, secret detection and redaction, cancel tokens |
| `hatch-store` | Single-file SQLite, idempotent migrations, transactions, audit log with automatic redaction |
| `hatch-llm` | Four model protocols with streaming, per-workload routing, fallback ladder, call budgets |
| `hatch-memory` | Claim memory, correction chains with provenance, persistent Chinese BM25 index (bank/index split), forgetting, people pages |
| `hatch-policy` | Action class × reversibility × autonomy, gatekeeper rules, standing grants, async approval queue, idempotency ledger |
| `hatch-connectors` | IMAP / SMTP / MIME (incl. GBK), ICS parsing, conflicts and free slots; connector framework with two reference connectors |
| `hatch-sandbox` | Process sandbox: namespaces, pivot_root, capability clearing, no_new_privs, seccomp, rlimit, timeout kills the whole process tree |
| `hatch-browser` | Chromium management (per-profile isolation, cross-process reattach, idle reaping), CDP sessions, page snapshots, destination blocking |
| `hatch-tools` | Tool SDK, mail/calendar/memory/file/command/web/browser tools, workspace path guard, checkpoints, unified dispatch |
| `hatch-agent` | Behavior spec, layered context assembly, compaction, sessions (fork / rewind), cancellable turn loop, async sub-agents |
| `hatch-triage` | Triage workflow: isolated classification, code post-processing, meeting handling, drafting, durable steps, receipts |
| `hatch-scheduler` | Schedule expressions (DST-correct), jobs / runs / dedup tables, watchdog with retries, auto-disable, daemon |
| `hatch-workers` | Daily brief, scheduled triage, memory maintenance and extraction, background-output handoff |
| `hatch-goals` | Goal workspaces, idea library, daily feed |
| `hatch-eval` | Eval worlds and reports (triage, workspace, sub-agents, scheduler, browser) |
| `hatch-plan` | Plan mode: structured-plan parsing and validation, SQLite persistence and state machine, step-by-step execution with deviation detection |
| `hatch-artifacts` | Widget cards and versioned artifacts (share approval, quotas, share tokens with revocation) |
| `hatch-transports` | Multi-channel transport interface: per-chat transcripts, `DISCRETION.md` sharing rules, worker relay rules |
| `hatch-mcp` | MCP client (stdio / HTTP-SSE) + server (stdio / HTTP, per-client tokens and TLS) |
| `hatch-vault` | Credential storage: 0600 files + a dedicated helper process |
| `hatch-skills` | Skill registry: SKILL.md parsing, allowlist, activation |
| `hatch-inference` | Inference proxy: loopback reverse proxy (forwarding, usage accounting, retries, rate limiting) |
| `hatch-cli` | CLI, config and environment-variable overrides, Ctrl-C handling |

## Core Mechanisms

**Context assembly.** Each turn is built from layers in a fixed order: behavior spec → user profile and preference memory → recalled memory → sub-agent handoffs → background updates → session history. The stable prefix comes first (byte-identical across consecutive turns, so it hits the provider's prefix cache) and volatile content last; the budget is trimmed by CJK-aware token estimation, and overlong history is compacted at user-message boundaries.

**Memory.** Memory is stored as claims with source, trust level, confidence and expiry. Corrections never overwrite history; they form a traceable correction chain. Inferred claims and claims from untrusted sources have capped confidence; content containing secrets is refused. Retrieval uses a persistent Chinese BM25 index (bank and index split) and returns only currently valid records. Relevant memory is retrieved automatically before each turn; a daily 04:00 `memory-extract` job mines new sessions for durable memories (conflicts are proposed, never silently written).

**Gatekeeper, approvals, idempotency.** Every tool call first passes the gatekeeper, which decides by action class, autonomy, standing grants, contamination state and information-flow rules: execute directly, enter the approval queue, or refuse. Approvals never block the conversation; after approval, exactly what was approved executes (payload hash check); expired approvals never execute. Every outward action has a content-addressed idempotency key, so a crash with unknown state is reported honestly instead of retried blindly.

**Trust and contamination.** Every piece of text entering the model carries a trust label; mail, web pages and file contents arrive in untrusted envelopes. Once the context contains untrusted content, all outward, write and execute actions require approval. Triage classification uses an isolated model with no tools, and reply recipients are fixed by code.

**Command sandbox and browser.** `shell_run` runs as nobody in separate user / mount / net / pid / ipc / uts namespaces: read-only system directories + writable workspace, no network, seccomp and rlimit limits, timeout kills the whole process tree; if the host cannot sandbox, it fails loudly instead of degrading to unisolated execution. The browser's three profiles are three separate Chromium processes with mutually invisible cookies and logins; pages reach the model as snapshots with numbered actionable elements, and the model cannot run scripts; submissions need approval and are re-checked before clicking, payments are suggestions only; login walls and CAPTCHAs are handed to you via `hatch browser takeover`.

**Sub-agents.** `delegate` / `delegate_parallel` are async: the call returns a run id and "running in the background" immediately, the turn continues and you can keep chatting; each sub-agent runs in its own process (Tokio + UDS transport, sockets mode 0600) and, when done, its conclusion is recorded as a handoff injected at the start of the next turn (delivered exactly once). A sub-agent only sees its task brief, never the main conversation; its tool list can only be narrower than the parent's; its approvals enter the same queue marked with their origin; nesting depth is capped; `hatch agents cancel <run_id>` (or `POST /v1/agents/{id}/cancel`) cancels one at any time. Limits: 3 parallel, 6 tasks per turn, depth 2.

**Scheduling and background workers.** Supports `daily` / `weekly` / `every` / `once` / `cron` expressions, computed in the user's timezone with correct DST handling. Missed triggers during downtime are only recorded and replayed at most once; failures back off and retry, and repeated failures auto-disable the job with a notification. A scheduled job's authorization scope can only be narrowed: out-of-scope outward actions always go to approval.

**Model protocols.** One abstraction supports Chat Completions, OpenAI Responses, Anthropic Messages and Gemini, all with tool calls, JSON mode and streaming. Classification, drafting, compaction and briefs can each route to a different model, with a fallback ladder on errors.

## Quick Start

```bash
cargo build --release              # binary: target/release/hatch
hatch init                         # creates ~/.hatch/config.toml and the database (HATCH_HOME overrides the directory)
hatch doctor                       # self-check: config, database, models, mail, sandbox, scheduler (never prints secrets)
```

Configure a model (environment variables are enough):

```bash
export HATCH_LLM_BASE_URL=https://api.deepseek.com/v1
export HATCH_LLM_MODEL=deepseek-chat
export HATCH_LLM_API_KEY=<your key>          # read only from the environment, never written to files or logs
export HATCH_LLM_PROVIDER=openai_compatible  # optional: openai_responses / anthropic / gemini
```

| `provider` | Protocol | Request URL |
|---|---|---|
| `openai_compatible` (alias `openai_chat`) | Chat Completions | `{base_url}/chat/completions` |
| `openai_responses` | OpenAI Responses | `{base_url}/responses` |
| `anthropic` | Anthropic Messages | `{base_url}/v1/messages` |
| `gemini` | Gemini `generateContent` | `{base_url}/v1beta/models/{model}:generateContent` |

Mail and calendar:

```bash
export HATCH_MAIL_PASSWORD=<app password>     # app-specific password for 126/163
hatch calendar import ~/Downloads/calendar.ics
hatch memory person boss@corp.com Boss --importance vip
hatch memory add --tags style,preference "Keep mail replies short"
```

Triage and approvals:

```bash
hatch triage run --dry-run
hatch approvals list
hatch approvals show <id>          # see exactly what will execute
hatch approvals approve <id>
hatch grants add --class send_message --to "*@corp.com" --days 30   # standing grant
```

Chat and sub-agents:

```bash
hatch ask "Am I free Wednesday afternoon?"
hatch chat --stream --workspace ~/projects/demo
hatch chat --resume <session>
hatch session list | fork <id> | rewind <id> --to <seq>
hatch agents list | runs | show <run_id> | cancel <run_id>
```

Press Ctrl-C once during a turn to stop it (including sub-agents and commands); press again within 2 seconds to exit.

Browser (needs Chromium / Chrome installed locally, or `HATCH_CHROMIUM` to point at it, with `[browser] enabled = true`):

```bash
hatch ask "Open https://pypi.org/project/requests/ and tell me the latest version"
hatch browser takeover <session>   # take over when login or CAPTCHA is needed
hatch browser resume <session>
```

Scheduled jobs and the daemon:

```bash
hatch jobs list
hatch jobs enable daily-brief      # daily brief at 08:30
hatch jobs enable triage-sync      # triage new mail every 30 minutes
hatch jobs enable memory-flush     # daily memory maintenance
hatch jobs enable memory-extract   # mine new sessions for memories daily at 04:00
hatch daemon run                   # run in the foreground; hatch daemon stop or SIGTERM exits gracefully
```

## Product API (v1)

This repo is backend only. `hatch daemon run --api` serves only the product API and hosts no static frontend files. The frontend lives in a separate repo and talks to this API; endpoints, auth and error bodies are the contract between the two.

```bash
hatch daemon run --api              # http://127.0.0.1:8787/v1
hatch daemon run --api --api-bind 127.0.0.1:9000
```

- Endpoints: sessions with streaming chat (SSE), memory, approvals, scheduled jobs, artifacts, skills, connectors, goals / feed / ideas, calendar, mail (read-only), sub-agents (`GET /v1/agents`, `POST /v1/agents/{id}/cancel`), inference usage (`GET /v1/inference/usage`). `GET /v1/health` and share links `GET /v1/shared/<token>` need no auth (the latter uses the token in the path as its credential); everything else needs `Authorization: Bearer <token>`.
- Token: `hatch vault set daemon-api` stores it in the vault (preferred), or set `HATCH_DAEMON_API_TOKEN`; it is never written to config files, never passed on the command line, never logged. Without a configured token the API only listens on loopback; binding a non-loopback address without a token makes the daemon refuse to start.
- Rate limit 20 requests/second per IP (429 + `Retry-After: 1`). The service does no TLS itself; put a reverse proxy in front for external exposure.
- Explicitly not in v1: payments (still a stub), real multi-channel apps (WeChat / Feishu / Telegram), multi-tenant isolation (single user).

## Plan Mode

By default a conversation is "think while doing". Plan mode flips it: the model first hands over a **structured plan** (ordered steps, each with tool name, arguments, files involved, expected result and risk level), the plan is stored in SQLite for your approval, and only executes after approval; during execution each step is checked against the approved snapshot, and any deviation in tool, arguments or files stops it as "needs re-approval".

```bash
hatch plan new "Turn this week's unread-mail TODOs into a checklist"
hatch plan list
hatch plan show plan_xxxx
hatch plan approve plan_xxxx       # approve (records who and when; snapshots all steps)
hatch plan run plan_xxxx           # execute the approved steps one by one
hatch plan reject plan_xxxx
```

Plan mode also works in chat: `hatch chat --plan` (or `[agent] plan_mode = true` in config). Plan states flow `draft → pending_approval → approved → executing → completed`, with `rejected / superseded / cancelled / needs_reapproval` on the side.

## Skills, MCP and Credential Storage

**Skills** are Markdown files at `$HATCH_HOME/skills/<name>/SKILL.md` (or `./skills/`): frontmatter between `---` markers (`name` / `description` / `tools` / `scope` / `version`) followed by prose. Once activated, each turn injects the prose into a stable context layer and filters the turn's tools to the skill's `tools` list. A skill is a working-style note you installed yourself: it grants no new tools, cannot bypass approvals, and its prose is never treated as your instruction. See `skills/example-brief/SKILL.md`.

**MCP client** supports stdio and HTTP/SSE (JSON-RPC 2.0): add `[[mcp_servers]]` in `config.toml` and remote tools register as `mcp_<server>_<tool>`. Everything a server returns is treated as untrusted; calls always go through the approval queue and results are marked untrusted.

**MCP server** (`hatch mcp serve`) exposes hatch's tools to MCP clients over stdio and streamable HTTP (protocol 2025-03-26). HTTP supports per-client tokens (`hatch mcp token create <name> [--scope read|write]`, registry `<home>/mcp-clients.json` mode 0600 storing only hashes, revocation effective on the next request; approval origin is `mcp:http:<client-name>`) and in-process TLS (`--tls-cert/--tls-key`; non-loopback binding requires TLS + token by default, or explicit `--allow-plain-http`). The old shared token (vault `mcp-http` / `HATCH_MCP_TOKEN`) still works as a transitional option. Calls needing approval are not executed; they return `approval_required` + approval id, and the caller polls with `approval_status`.

**Credential storage (vault)**: tokens live in `$HATCH_HOME/vault/tokens.json` (directory 0700, file 0600) and are held at runtime by a dedicated `hatch-vault-helper` process; the agent process only holds token names. `hatch vault set <name>` reads from stdin or `--from-env VAR`, never from command-line arguments or config files.

**Connector framework**: a connector is a child-process CLI described by a `$HATCH_HOME/connectors/<name>.toml` manifest. Operations register as `connector_<connector>_<op>`, calls always go through the approval queue, and results are always marked untrusted. Only two reference connectors ship (local file notes, local mock HTTP); no real vendor connectors.

## Artifacts: Widgets, Quotas and Sharing

**Widgets** are small cards the assistant can offer (options / list / HTML) with an `embed_token`: a card stays inert until its token appears in a user-visible message, otherwise only the plain-text version renders. **Artifacts** are versioned deliverables (`document` / `html`) stored at `$HATCH_HOME/artifacts/<id>/v<n>/`; new versions never overwrite old ones. The flow is build → share → approve → deliver; an unshared artifact cannot be delivered.

```bash
hatch artifact build "Weekly" --kind document --content "# Week one"
hatch artifact share art_xxx     # only submits a share approval, does not share
hatch approvals approve apr_xxx  # marks it shared only after approval
hatch artifact deliver art_xxx   # prints a delivery message containing the file path
```

**Quotas** (shared across the whole home, single-user v1): 200 artifacts, 100 MiB total, 5 MiB per file by default; `[artifacts]` config or `HATCH_ARTIFACT_MAX_*` environment variables override them. Quotas are checked before building; exceeding them refuses clearly (API: 403 `quota_exceeded`) and writes no partial file.

**Sharing and revocation**: approving a share mints a share token (`hatch_art_` + 32 random bytes) shown exactly once in the approval result; the registry stores only SHA-256 hashes. Share links are `GET /v1/shared/<token>`; revocation takes effect immediately (`hatch artifact revoke <share id>` or `POST /v1/artifacts/{id}/revoke`); deleting an artifact revokes all its shares first.

## Inference Proxy

`hatch inference proxy` is a loopback HTTP reverse proxy (default `127.0.0.1:8199`): the agent sends model requests to it, it forwards them unchanged to the real upstream and streams the response back, while doing three things — **usage accounting** (each call recorded from the provider-reported usage into `$HATCH_HOME/inference/usage.jsonl`; `hatch inference usage` and `GET /v1/inference/usage` report it; cost is estimated from the configured `[inference]` prices, and not estimated at all when no price is configured), **retries** (429 / 5xx / transport failures with exponential backoff, 3 by default, only before the first byte is forwarded; 400 / 401 / 403 pass through unchanged), and **rate limiting** (configurable per-minute cap, 60 by default, over-cap returns 429 + `Retry-After`). The proxy only binds loopback; the upstream key lives only in the proxy process's memory. The agent uses it by setting `proxy_url = "http://127.0.0.1:8199"` under `[inference]` (without `/v1`); unset means direct connection with unchanged behavior.

## Common Commands

| Command | Purpose |
|---|---|
| `hatch chat / ask [--stream] [--workspace DIR] [--resume ID]` | Chat |
| `hatch triage run [--dry-run]` / `triage last` | Mail triage and receipts |
| `hatch approvals list/show/approve/reject/history` | Approval queue |
| `hatch grants add/list/revoke` | Standing grants |
| `hatch memory add/search/explain/correct/forget/people/person` | Memory and contacts |
| `hatch memory extract [--session <id>]` | Mine durable memories from session records |
| `hatch calendar import/list/sync/conflicts/resolve/status` | Calendar: import, CalDAV sync and conflict handling |
| `hatch agents list/runs/show/cancel` | Sub-agent types, run records and cancel |
| `hatch session list/show/fork/rewind` | Sessions |
| `hatch checkpoint list/show/rewind` | File checkpoints |
| `hatch jobs add/list/show/enable/disable/remove/run-now/history` | Scheduled jobs |
| `hatch brief show/list` | Daily brief |
| `hatch goal create/list/show/complete/pause/resume/abandon` | Goals |
| `hatch ideas list/add/dismiss/act` | Idea library |
| `hatch feed list/show` | Daily feed |
| `hatch daemon run/status/stop` | Resident daemon |
| `hatch browser status/sessions/takeover/resume/close` | Browser processes and sessions |
| `hatch skills list/show/load/unload` | Skills: view and activate |
| `hatch mcp list/test/call/serve` | MCP servers and the MCP server |
| `hatch mcp token create/list/revoke` | Per-client tokens |
| `hatch artifact build/list/show/share/deliver` | Artifacts |
| `hatch artifact quota/shares/revoke/delete` | Artifact quotas and share management |
| `hatch inference proxy/usage` | Inference proxy and usage |
| `hatch vault set/list/delete/status/stop` | Credential storage |
| `hatch connectors list/status/call` | Connectors |
| `hatch plan new/list/show/approve/run/reject/cancel` | Plan mode |
| `hatch eval [--world DIR] [--live]` | Sandboxed eval |
| `hatch doctor` | Self-check |

## Evaluation

```bash
hatch eval                                          # pipeline: scripted model, no real model calls
hatch eval --live                                   # live: uses the configured model
hatch eval --world eval/worlds/workspace-basic
hatch eval --repeat 3                               # run the same world 3 times, report the mean
```

| World | What it checks | pipeline |
|---|---|---|
| `zh-office` | Mail classification, drafting, injected mail, verification-code redaction, conflict detection, idempotency | 11/11 |
| `en-office` | The English-shaped equivalent of zh-office | 11/11 |
| `thread-attachment` | Long thread (mid-thread decision change), attachments as references only, final decision accuracy | 15/15 |
| `workspace-basic` | File Q&A and edits, sandboxed commands, injected instructions in documents | 11/11 |
| `delegate-basic` | Parallel delegation, cancel, depth limit, sub-agent facing injection | 13/13 |
| `scheduler-basic` | DST, missed-trigger replay, failures and timeouts, brief factual consistency, scheduled outbound authorization | 17/17 |
| `browser-basic` | Page Q&A, option existence, login walls, CAPTCHAs, submit approval, checkout, page injection (real Chromium + local mock sites) | 16/16 |

In pipeline mode the scripted model deliberately "falls for" adversarial samples to verify structural defenses; it does not represent real-model capability. Live mode runs the same worlds with the configured real model.

## Security Notes

| Mechanism | Description |
|---|---|
| Autonomy cap | Irreversible actions (send, delete, run commands) can at most reach "draft for approval"; payments are suggestions only |
| Information-flow rules | New recipients, messages carrying calendar details or other people's messages need approval; content containing secrets is refused outright |
| Secret handling | Memory refuses secrets; notifications, summaries, approvals and audit logs are redacted uniformly; keys are read only from environment variables |
| Web access | Only public https; intranet, localhost, link-local and cloud-metadata addresses refused, redirects checked hop by hop |
| Browser | Page content is untrusted; submissions need approval and are re-checked before clicking; payments are suggestions only; never fills passwords or solves CAPTCHAs; profiles are isolated |
| Workspace guard | File tools only accept paths inside the workspace, refusing `..` escapes and symlinks pointing outside; atomic writes |
| Skills & MCP | Skills grant no tools and cannot bypass approvals; MCP server output is always untrusted and calls go through the approval queue; the HTTP server authenticates per-client tokens |
| Credential storage | Tokens are held by a dedicated helper process (0600 files); injection sets values directly into child-process environments |
| Artifacts | Widget HTML allows only safe tags (no scripts, no event handlers, no external fetches); artifact sharing is an outward action and always needs approval |
| Forgetting | Deletes the whole memory version chain, and honestly notes that external copies cannot be recalled |

## License

MIT, see [LICENSE](LICENSE).
