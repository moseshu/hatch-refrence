# hatch — Personal Agent Assistant

[中文](README.md)

hatch is a personal agent assistant you can deploy to the cloud (a single Rust binary, backend only). It runs resident, manages your mail, calendar, memory and workspace, calls tools in conversation, uses a browser, delegates tasks asynchronously to sub-agents, works in the background on a schedule, and puts every outward action through an auditable, revocable channel that requires your confirmation.

A few key design choices:

- **Backend first**: this repo is backend only and serves clients through a product API (`/v1/...`, HTTP/1.1 + SSE); the frontend lives in a separate repo and is not mixed in here
- **Approval first**: irreversible actions (sending, deleting, running commands, sharing) go to an approval queue by default and only execute after you approve, executing exactly what you saw when approving
- **Acceptance first**: "done" is not the model's word — after execution the system independently reads back real state and checks it against a frozen acceptance contract (deterministic predicates + semantic grading); no pass, no completion
- **Structured memory**: memory is stored as claims with source, trust level and confidence; corrections form a traceable chain instead of overwriting history
- **Async sub-agents**: delegation returns a run id immediately; the main agent and the user keep going, and results flow back as handoff events next turn

## Install

Requires Rust ≥ 1.80 and a C compiler (SQLite is bundled). One-shot install:

```bash
git clone https://github.com/moseshu/hatch-refrence.git
cd hatch-refrence
./install.sh            # builds release and installs to ~/.local/bin
```

Manual build:

```bash
cargo build --release -p hatch-cli   # binary: target/release/hatch
```

Initialize and self-check:

```bash
hatch init              # creates ~/.hatch/config.toml and the database (HATCH_HOME overrides the dir)
hatch doctor            # self-check: config, database, model, mail, sandbox, scheduler (never prints secrets)
```

Configure a model (env vars only; secrets never go into config files):

```bash
export HATCH_LLM_BASE_URL=<your model service URL>
export HATCH_LLM_MODEL=<model name>
export HATCH_LLM_API_KEY=<redacted>
export HATCH_LLM_PROVIDER=openai_compatible  # or: openai_responses / anthropic / gemini
```

| `provider` | Protocol | Request URL |
|---|---|---|
| `openai_compatible` (alias `openai_chat`) | Chat Completions | `{base_url}/chat/completions` |
| `openai_responses` | OpenAI Responses | `{base_url}/responses` |
| `anthropic` | Anthropic Messages | `{base_url}/v1/messages` |
| `gemini` | Gemini `generateContent` | `{base_url}/v1beta/models/{model}:generateContent` |

Model names and service URLs are always supplied by you via config/env — no vendor is hardcoded in the code.

## Quick start

```bash
# One-shot question from the CLI
hatch ask "Am I free Wednesday afternoon?"

# Resident daemon + product API (the frontend entry point, default 127.0.0.1:8787)
hatch daemon run --api

# One chat turn over the API (JSON shape; add Accept: text/event-stream for SSE)
curl -X POST http://127.0.0.1:8787/v1/sessions -d '{}'
curl -X POST http://127.0.0.1:8787/v1/sessions/<id>/messages \
  -H 'Content-Type: application/json' -d '{"text":"check my mail today"}'
```

## Architecture

```
                        ┌─────────────────────────────────────────────┐
   frontend / clients ──HTTP──▶│  product API (/v1/..., HTTP/1.1 + SSE)     │
   CLI (hatch command)───────▶│  auth: per-client token (read/write scope) │
                        └──────────────────┬──────────────────────────┘
                                           │
  ┌────────────────────────────────────────▼────────────────────────────────────────┐
  │  scheduling  hatch-scheduler (task store·runner·daemon)  hatch-workers (bg jobs) │
  │  scenarios   hatch-agent (chat loop·context·compaction·sub-agents)               │
  │              hatch-triage (mail triage)  hatch-plan (plan mode·acceptance)       │
  │              hatch-eval (evaluation)                                             │
  │  capability  hatch-tools (tool SDK·single dispatch·executors·checkpoints)        │
  │  domain      hatch-policy (gate·approvals·idempotency ledger)                    │
  │              hatch-memory (claim memory)  hatch-connectors (mail·calendar)       │
  │              hatch-llm (4-protocol model routing)  hatch-sandbox (command box)   │
  │              hatch-browser (Chromium)  hatch-inference (proxy·usage)             │
  │              hatch-mcp (MCP client/server)                                       │
  │  foundation  hatch-store (single-file SQLite)  hatch-core (errors·IDs·trust·…)    │
  └──────────────────────────────────────────────────────────────────────────────────┘
                                           │
          outside world: your model service · IMAP/SMTP mail · CalDAV · web · MCP servers
```

| crate | Responsibility |
|---|---|
| `hatch-core` | Error types, content-addressed IDs, trust labels and untrusted envelopes, CJK-aware token estimation, secret detection/redaction, cancel tokens |
| `hatch-store` | Single-file SQLite, idempotent migrations, transactions, auto-redacted audit log |
| `hatch-llm` | Four model protocols with streaming, per-workload routing, fallback ladder, call budgets |
| `hatch-memory` | Claim memory, correction chains and provenance, Chinese BM25 persistent index (bank/index split), forgetting, people pages |
| `hatch-policy` | Action class × reversibility × autonomy, gate rules, standing grants, async approval queue, idempotency ledger |
| `hatch-connectors` | IMAP / SMTP / MIME (incl. GBK), ICS parsing, conflict & free-slot detection; connector framework + two reference connectors |
| `hatch-sandbox` | Process sandbox: namespaces, pivot_root, capability drop, no_new_privs, seccomp, rlimit, timeout kills the process tree |
| `hatch-browser` | Chromium management (per-profile isolation, cross-process reconnect, idle reaping), CDP sessions, page snapshots, target-address blocking |
| `hatch-tools` | Tool SDK, mail/calendar/memory/file/command/web/browser tools, workspace path guard, checkpoints, single dispatch |
| `hatch-agent` | Behavior spec, layered context assembly, compaction, sessions (fork / rewind), cancellable turn loop, async sub-agents, intent gate and semantic grading |
| `hatch-triage` | Triage workflow: isolated classification, code post-processing, meeting handling, drafting, durable steps, receipts |
| `hatch-scheduler` | Schedule expressions (DST-correct), tasks / run records / dedup tables, watchdog & retries, auto-disable, daemon |
| `hatch-workers` | Daily brief, scheduled triage, memory maintenance & extraction, background-output handoff |
| `hatch-goals` | Goal workspaces, idea library, daily feed |
| `hatch-eval` | Evaluation worlds and reports (triage, workspace, sub-agents, scheduler, browser) |
| `hatch-plan` | Plan mode: structured-plan parsing/validation, SQLite persistence & state machine, per-step execution with deviation detection, cross-step acceptance contracts |
| `hatch-artifacts` | Widget cards and versioned artifacts (share approval, quotas, share tokens & revocation) |
| `hatch-transports` | Multi-channel transport interface: per-chat transcripts, `DISCRETION.md` sharing rules, worker relay rules |
| `hatch-mcp` | MCP client (stdio / HTTP-SSE) + server (stdio / HTTP, per-client tokens and TLS) |
| `hatch-vault` | Credential vault: 0600 file store + dedicated helper process |
| `hatch-skills` | Skill registry: SKILL.md parsing, allowlist, activation |
| `hatch-inference` | Inference proxy: loopback reverse proxy (forwarding, usage accounting, retries, rate limiting) |
| `hatch-cli` | CLI, product API server, config & env overrides, Ctrl-C handling |

## How a run works: from user input to task completion

### One chat turn

```
User input (CLI ask/chat, or POST /v1/sessions/{id}/messages)
  │
  ├─ 1. Context assembly: behavior spec → user profile/preferences → retrieved memory
  │      → sub-agent handoffs → background updates → session history
  │      (stable prefix first for provider prefix-cache hits; compaction over budget)
  ├─ 2. Model loop: the model answers or calls tools (step/token budgets, cancellable)
  ├─ 3. Tool calls go through the single dispatch entry:
  │      a. pre-execution intent gate (optional): an independent judge checks
  │         "does this action match what the user asked for" — mismatches stop here
  │      b. policy gate: by action class / reversibility / autonomy / standing
  │         grants / taint → execute now / queue for approval / deny
  │         (irreversible actions default to approval)
  │      c. executor runs it; the idempotency ledger records it
  │         (unknown outcome after a crash is reported, never resent)
  ├─ 4. Tool results flow back to the model until it produces a final answer
  ├─ 5. Completion check: independently read back real state (sent box / calendar /
  │      files) and judge against this turn's acceptance conditions; on failure it
  │      keeps fixing or reports honestly — never claims done falsely
  └─ 6. Turn report to the frontend: one JSON body, or SSE events
         (message deltas, tool_call/tool_result progress, approval_needed, done)
```

### Durable plan tasks (cross-step acceptance)

```
User goal → model produces a structured plan (steps + params + acceptance contract;
            the contract freezes at approval)
  → user approves (all steps and the contract are snapshotted)
  → steps execute one by one (each checked against the approved snapshot;
    any deviation stops the run; outward actions go through approvals/grants)
  → after all steps: one shared evidence bundle is read back independently →
    L2 deterministic predicates judge each condition →
    L3 semantic grading (independent judge, must cite evidence)
  → Done only if acceptance passes; otherwise blocked / needs_review —
    recovery reconciles read-only, never resends
```

### Background runs

`hatch daemon run` executes scheduled jobs (daily brief, scheduled triage, memory maintenance, …); outputs reach the assistant as "background updates" next conversation. Triggers missed while down are recorded and run at most once; failures retry with backoff and auto-disable after repeated failure, with a notification.

## External service interface (for frontend integration)

A complete product API already exists: `hatch daemon run --api` serves HTTP/1.1 (default `127.0.0.1:8787`) and hosts no frontend files — the frontend lives in a separate repo and integrates through this API.

**Auth**: per-client tokens (`hatch daemon token create <name> [--scope read|write]`; the registry stores hashes only, plaintext shows once, revocation takes effect on the next request). Read scope may call read endpoints only; write endpoints need write scope (otherwise 403 `insufficient_scope`). `GET /v1/health` and share links `GET /v1/shared/<token>` are unauthenticated. With no token configured the API binds loopback only; binding a non-loopback address without a token makes the daemon refuse to start. Rate limit: 20 req/s per IP (429 + `Retry-After: 1`). The server itself does no TLS — put it behind a reverse proxy for exposure.

**Endpoints** (all under `/v1`):

| Group | Endpoints | Notes |
|---|---|---|
| Health/status | `GET /health`, `GET /status` | Liveness probe and runtime status |
| Sessions & chat | `POST/GET /sessions`, `GET/DELETE /sessions/{id}`, `POST /sessions/{id}/messages`, `POST /sessions/{id}/cancel`, `POST /sessions/{id}/fork`, `POST /sessions/{id}/rewind` | Messages support JSON and SSE shapes (`Accept: text/event-stream`); one turn at a time per session, conflicts get 409 |
| Tasks | `GET/POST /tasks`, `GET /tasks/{id}`, `GET /tasks/{id}/events`, `POST /tasks/{id}/cancel\|pause\|resume\|command` | Event stream supports SSE + `Last-Event-ID` resume |
| Approvals | `GET /approvals`, `GET /approvals/{id}`, `POST /approvals/{id}/approve\|deny` | What executes is exactly what `GET` showed (payload hash check) |
| Sub-agents | `GET /agents`, `GET /agents/{id}`, `POST /agents/{id}/cancel\|pause\|resume` | Async delegation status and control |
| Memory | `GET /memory/search`, `POST /memory/claims`, `GET /memory/claims/{id}`, `GET /memory/people`, `GET /memory/people/{email}` | Claim search and people pages |
| Jobs | `GET/POST /jobs`, `GET/PATCH/DELETE /jobs/{id}`, `POST /jobs/{id}/run`, `GET /jobs/{id}/runs` | Schedule management |
| Artifacts | `POST /artifacts`, `GET /artifacts/{id}`, `DELETE /artifacts/{id}`, `GET /artifacts/quota`, `GET /artifacts/{id}/shares`, `POST /artifacts/{id}/share\|revoke`, `GET /shared/<token>` | Versioned deliverables and share links |
| Misc | `GET /goals`, `POST /goals`, `GET /goals/{id}`, `GET /feed`, `GET /ideas`, `GET /calendar/events`, `GET /mail/triage`, `GET /skills`, `GET /connectors`, `GET /connectors/{name}`, `GET /inference/usage` | Goals/feed/ideas/calendar/mail triage (read-only)/skills/connectors/usage |

**SSE events** (chat turn): `message` (answer deltas), `tool_call` / `tool_result` (progress), `approval_needed`, `done` (turn report: answer, verification, stop reason, steps, tool_calls). Candidate answer text streams only after the independent completion check passes — no emit-then-retract.

**MCP server**: `hatch mcp serve` exposes hatch's tools to MCP clients (stdio and streamable HTTP, per-client token auth, optional TLS on HTTP). Calls that need approval do not execute; they return `approval_required` + an approval id for the caller to poll.

Explicitly not in v1: payments (suggest-only, never executed), real multi-channel transports (WeChat / Feishu / Telegram not implemented), multi-tenant isolation (single-user home today).

## Core mechanisms

**Context assembly.** Each turn is built in fixed layers: behavior spec → user profile & preference memory → retrieved memory → sub-agent handoffs → background updates → session history. The stable prefix comes first (byte-identical across turns, so provider prefix caching hits), volatile content last; budgets use CJK-aware token estimation, and long histories compact at user-message boundaries.

**Memory.** Memory is stored as claims with source, trust level, confidence and validity. Corrections never overwrite — they form a traceable chain; inferred and untrusted-sourced claims have capped confidence; secrets are refused. Retrieval is Chinese BM25 over a persistent index (bank/index split) returning only currently-valid records; ranking is BM25 × salience (0–1, scored at extraction, manually adjustable) × time decay (exponential on last use, 90-day half-life, configurable via `[agent] salience_half_life_days`). Relevant memory is auto-retrieved each turn and its last-use time updated; the daily 04:00 `memory-extract` job distills durable memory from new sessions (conflicts are proposed, never written silently). The daily 02:30 `memory-synthesis` job folds high-salience and recent memory into a read-only "current understanding" document injected at session start; view it with `hatch memory synthesis show`, regenerate with `hatch memory synthesize [--force]`.

**Gate, approvals, idempotency.** Every tool call is first decided by the policy gate from action class, autonomy, standing grants, taint and information-flow rules: execute now, queue for approval, or deny. Approvals never block the conversation; what executes after approval is exactly what was shown (payload hash check); expired approvals never execute. Every outward action carries a content-addressed idempotency key; an unknown outcome after a crash is reported honestly instead of resent.

**Pre-execution intent gate (L0, optional).** Before an action runs, an independent judge checks whether the prepared action matches the intent the user expressed this turn (parameter-vs-stated-constraint conflicts, over-limit actions, unverifiable claims) and returns allow / deny / uncertain. Switch: `HATCH_INTENT_GATE=off|observe|enforce` (default off); observe records verdicts without blocking, enforce blocks deny/uncertain with an actionable reason. All verdicts are stored for sampling audits.

**Cross-step acceptance (L2/L3).** A durable task's acceptance contract freezes at approval: structured predicates are judged deterministically over independently read-back evidence (L2); natural-language items are graded by an independent judge over the same evidence bundle (L3 — citations required and code-validated). The two layers grey-release independently (Observe records shadow verdicts / Enforce blocks), controlled by `HATCH_ACCEPTANCE_ENFORCE`; L2 defaults to Enforce, L3 to Observe. Insufficient evidence is Ambiguous → human confirmation; recovery reconciles read-only and never resends.

**Trust and taint.** Every text entering the model carries a trust label; mail, web pages and file contents are wrapped in untrusted envelopes. Once the context contains untrusted content, all outward/write/execute actions require approval. Triage classification runs on a tool-less isolated model, and reply recipients are fixed by code.

**Command sandbox and browser.** `shell_run` executes in separate user/mount/net/pid/ipc/uts namespaces as nobody: read-only system dirs + writable workspace, no network, seccomp and rlimits, timeout kills the whole process tree; if the host can't provide isolation it errors instead of degrading. The browser's three profiles are three separate Chromium processes with isolated cookies/logins; pages reach the model as snapshots of numbered actionable elements and the model cannot run scripts; submits need approval and are re-checked before execution; payments are suggest-only; login walls and captchas hand over to you via `hatch browser takeover`.

**Sub-agents.** `delegate` / `delegate_parallel` are async: they return a run id immediately, the turn continues and the user can keep talking; sub-agents run in separate processes (Tokio + UDS, socket 0600) and their conclusions land as one-time handoffs injected next turn. Sub-agents see only their brief, never the main conversation; their tool list can only be narrower; approvals join the same queue tagged with their origin; nesting depth is capped. Cancel anytime: say so in chat (the model calls `cancel_subagent` in-process), or `hatch agents cancel <run_id>` / `POST /v1/agents/{id}/cancel` via the daemon. Limits: 3 parallel, 6 tasks per turn, depth 2.

**Scheduler and background workers.** `daily` / `weekly` / `every` / `once` / `cron` expressions in the user's timezone, DST-correct. Triggers missed while down are recorded and run at most once; failures retry with backoff and auto-disable with a notification. A scheduled job's authorization scope can only narrow; anything outside goes to approval.

**Model protocols.** One abstraction covers Chat Completions, OpenAI Responses, Anthropic Messages and Gemini — all with tool calls, JSON mode and streaming. Classification, drafting, compaction, briefs etc. can route to different models, with a fallback ladder on errors.

## Common commands

```bash
hatch ask "Am I free Wednesday afternoon?"     # one-shot question
hatch chat --stream --workspace ~/projects/demo
hatch chat --resume <session>
hatch session list | fork <id> | rewind <id> --to <seq>
hatch agents list | runs | show <run_id> | cancel <run_id>
hatch triage run --dry-run                     # mail triage (dry run)
hatch approvals list                           # pending actions
hatch approvals show <id>                      # exact payload that would execute
hatch approvals approve <id>
hatch grants add --class send_message --to "*@corp.com" --days 30   # standing grant
hatch plan new "collect the todos from this week's unread mail"     # plan mode
hatch jobs enable daily-brief                  # daily brief at 08:30
hatch daemon run --api                         # resident + product API
```

One Ctrl-C during a turn stops the turn (including sub-agents and commands); a second within 2 seconds exits.

Browser (needs a local Chromium / Chrome, or `HATCH_CHROMIUM` pointing at one, `[browser] enabled = true`):

```bash
hatch ask "open https://pypi.org/project/requests/ and tell me the latest version"
hatch browser takeover <session>   # take over for logins / captchas
hatch browser resume <session>
```

## Plan mode

Chat is "think while doing" by default. Plan mode inverts it: the model first submits a **structured plan** (ordered steps, each with tool name, parameters, files involved, expected result, risk level); the plan is stored in SQLite for your approval, and only then executes step by step — each step checked against the approved snapshot, any deviation in tool/params/files stops the run as "needs re-approval". The acceptance contract freezes with the approval (see Cross-step acceptance).

```bash
hatch plan new "collect the todos from this week's unread mail"
hatch plan list
hatch plan show plan_xxxx
hatch plan approve plan_xxxx       # approve (approver + time recorded, all steps snapshotted)
hatch plan run plan_xxxx           # execute the approved steps one by one
hatch plan reject plan_xxxx
```

Plan mode also works in chat: `hatch chat --plan` (or `[agent] plan_mode = true`). Plan states: `draft → pending_approval → approved → executing → completed`, plus `rejected / superseded / cancelled / needs_reapproval`.

## Skills, MCP and the credential vault

**Skills** are Markdown files at `$HATCH_HOME/skills/<name>/SKILL.md` (or `./skills/`): YAML frontmatter (`name` / `description` / `tools` / `scope` / `version`) followed by the body. When active, the body joins a stable context layer every turn and the turn's tool list is filtered to the skill's `tools`. A skill is a user-installed working-style note: it cannot grant new tools, cannot bypass approvals, and its demands are never treated as the user's. See `skills/example-brief/SKILL.md`.

**MCP client**: stdio and HTTP/SSE (JSON-RPC 2.0). Add `[[mcp_servers]]` to `config.toml`; remote tools register as `mcp_<server>_<tool>`. Server output is always untrusted; calls always go through the approval queue and results are tagged untrusted.

**MCP server** (`hatch mcp serve`) exposes hatch's tools to MCP clients over stdio and streamable HTTP (protocol 2025-03-26). HTTP supports per-client tokens (`hatch mcp token create <name> [--scope read|write]`; registry `<home>/mcp-clients.json` 0600, hashes only, revocation effective next request; approval origin `mcp:http:<client>`) and in-process TLS (`--tls-cert/--tls-key`; non-loopback binds require TLS + token by default, or explicit `--allow-plain-http`). Legacy shared tokens (vault `mcp-http` / `HATCH_MCP_TOKEN`) still work as a transition path. Calls needing approval do not execute; they return `approval_required` + approval id, polled via `approval_status`.

**Credential vault**: tokens live in `$HATCH_HOME/vault/tokens.json` (dir 0700, file 0600), held at runtime by a dedicated `hatch-vault-helper` process; the agent process holds only token names. `hatch vault set <name>` reads from stdin or `--from-env VAR` — never from argv or config files.

**Connector framework**: connectors are subprocess CLIs described by `$HATCH_HOME/connectors/<name>.toml`. Operations register as `connector_<connector>_<op>`; calls always go through approvals and results are always untrusted. Only two reference connectors ship (local file notes, local mock HTTP); no real vendor connectors.

## Presentation: widgets, artifacts and quotas

**Widgets** are small cards the assistant can emit (options / lists / HTML) carrying an `embed_token`: a card is inert until its token appears in a user-visible message; without a token only the plain-text rendering shows. **Artifacts** are versioned deliverables (`document` / `html`) under `$HATCH_HOME/artifacts/<id>/v<n>/`; new versions never overwrite old ones; the flow is build → share → approve → deliver, and unshared artifacts cannot be delivered.

```bash
hatch artifact build "Weekly report" --kind document --content "# Week 1"
hatch artifact share art_xxx     # submits a share approval only
hatch approvals approve apr_xxx  # sharing happens after approval
hatch artifact deliver art_xxx   # prints the delivery message with file paths
```

**Quotas** (whole home, single-user v1): 200 artifacts, 100 MiB total, 5 MiB per file by default; override via `[artifacts]` config or `HATCH_ARTIFACT_MAX_*`. Quota is checked before building; overruns are refused cleanly (API: 403 `quota_exceeded`) with no half-written files.

**Share & revoke**: approving a share mints a share token (`hatch_art_` + 32 random bytes), shown once in the approval result; only its SHA-256 hash is stored. Share links are `GET /v1/shared/<token>`; revocation is immediate (`hatch artifact revoke <share id>` or `POST /v1/artifacts/{id}/revoke`); deleting an artifact revokes all its shares first.

## Inference proxy

`hatch inference proxy` is a loopback HTTP reverse proxy (default `127.0.0.1:8199`): the agent sends model requests to it, it forwards them upstream verbatim and streams responses back, doing three things — **usage accounting** (provider-reported usage per call to `$HATCH_HOME/inference/usage.jsonl`, queryable via `hatch inference usage` and `GET /v1/inference/usage`; cost estimated only if unit prices are configured in `[inference]`), **retries** (429 / 5xx / transport failures with exponential backoff, 3 tries by default, only before the first byte is forwarded; 400 / 401 / 403 pass through), **rate limiting** (per-minute cap, default 60, then 429 + `Retry-After`). Loopback only; upstream keys live only in the proxy process's memory. Point the agent at it with `proxy_url = "http://127.0.0.1:8199"` (no `/v1`) in `[inference]`; unset means direct connection, behavior unchanged.

## Evaluation

```bash
hatch eval                                          # pipeline: scripted model, no real model calls
hatch eval --live                                   # live: uses the configured model
hatch eval --world eval/worlds/workspace-basic
hatch eval --repeat 3                               # same world 3 times, report means
```

| World | What it tests | pipeline |
|---|---|---|
| `zh-office` | Mail classification, drafting, injection mail, code redaction, conflict detection, idempotency | 11/11 |
| `en-office` | Same shape as zh-office, English mail | 11/11 |
| `thread-attachment` | Long threads (decision changes mid-thread), attachments as references only, final-decision accuracy | 15/15 |
| `workspace-basic` | File Q&A and edits, sandboxed commands, injection instructions in documents | 11/11 |
| `delegate-basic` | Parallel delegation, cancellation, depth caps, injection hitting sub-agents | 13/13 |
| `scheduler-basic` | DST, downtime catch-up, failures & timeouts, brief fact consistency, scheduled-send authorization | 17/17 |
| `browser-basic` | Page Q&A, option existence, login walls, captchas, form-submit approval, checkout, page injection (real Chromium + local mock site) | 16/16 |
| `compaction-basic` | Compaction quality: fact retention, fabrication rate, compression ratio (model-summary and extractive paths) | 9/9 |

In pipeline mode the scripted model deliberately "falls for" adversarial samples to verify the structural defenses; it says nothing about real-model ability. Live mode runs the same worlds on the configured real model.

## Security highlights

| Mechanism | Notes |
|---|---|
| Autonomy ceiling | Irreversible actions (send, delete, run commands) top out at "draft for approval"; payments are suggest-only |
| Information-flow rules | New recipients, calendar details or third-party messages going out need approval; secret-bearing content is refused outright |
| Secret handling | Memory refuses secrets; notifications, summaries, approvals and audit logs are uniformly redacted; keys come from env vars only |
| Web access | Public https only; private/loopback/link-local/cloud-metadata addresses refused; redirects checked per hop |
| Browser | Page content untrusted; submits need approval and are re-checked before execution; payments suggest-only; never fills passwords or solves captchas; profiles isolated |
| Workspace guard | File tools accept workspace-internal paths only; `..` escapes and outward symlinks refused; atomic writes |
| Skills & MCP | Skills cannot grant tools or bypass approvals; MCP server output always untrusted, calls queued for approval; HTTP server authenticates per-client tokens |
| Credential vault | Tokens held by a dedicated helper process (0600 file persistence); injection sets values directly into child-process env |
| Presentation | Widget HTML allows safe tags only (no scripts, no event handlers, no external fetches); artifact sharing is an outward action and must be approved |
| Forgetting | Deletes the whole memory version chain and states plainly that external copies cannot be recalled |

## TODO: self-evolution

Next direction: let the agent improve itself instead of relying only on humans to change it:

- **Self-evaluation**: run the eval worlds (`hatch eval`) regularly and record failures as evolution input
- **Learning from failures**: analyze root causes and propose fixes — prompt changes, new skills, memory-policy tweaks
- **Skill growth**: distill recurring new needs into new skills and reusable procedures
- **Memory & prompt iteration**: adjust profile, synthesis documents and the behavior spec from usage feedback
- **Human in the loop**: evolution actions are drafted first and land only after approval; irreversible changes never auto-execute

## License

MIT, see [LICENSE](LICENSE).

## Contact

- Email: moseshu25@gmail.com
