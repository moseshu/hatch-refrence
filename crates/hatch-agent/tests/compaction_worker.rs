//! End to end: a chat turn requests compaction, the background worker
//! runs it, and the next turn sees the summary. Also covers the hard
//! limit: a history past 2x the threshold compacts synchronously with
//! the extractive fallback and no model call.

use hatch_agent::assemble::message_tokens;
use hatch_agent::worker::{job_name, CompactionWorker};
use hatch_agent::{Agent, AgentConfig, SessionStore};
use hatch_core::time::now;
use hatch_core::{CancelToken, Trust};
use hatch_llm::scripted::ScriptedModel;
use hatch_llm::{ChatMessage, ChatResponse, Router};
use hatch_policy::{Gatekeeper, PolicyConfig};
use hatch_scheduler::{JobContext, JobHandler, JobStore, Trigger};
use hatch_store::Db;
use hatch_tools::{ExecutorRegistry, Registry};
use std::sync::Arc;
use tempfile::TempDir;

struct Fixture {
    _dir: TempDir,
    db: Db,
    router: Router,
    tools: Registry,
    executors: ExecutorRegistry,
    gate: Gatekeeper,
    session: String,
}

impl Fixture {
    fn new(responses: Vec<ChatResponse>) -> Self {
        let dir = TempDir::new().unwrap();
        let db = Db::open(&dir.path().join("hatch.db")).unwrap();
        let session = SessionStore::new(&db).create("e2e", &now()).unwrap();
        Fixture {
            _dir: dir,
            db,
            router: Router::single(Arc::new(ScriptedModel::ok("test", responses))),
            tools: Registry::new(),
            executors: ExecutorRegistry::new(),
            gate: Gatekeeper::new(PolicyConfig::default()),
            session,
        }
    }

    fn agent(&self) -> Agent<'_> {
        Agent {
            db: &self.db,
            router: &self.router,
            tools: &self.tools,
            executors: &self.executors,
            gate: &self.gate,
            config: AgentConfig::default(),
        }
    }

    fn fill_until(&self, target_tokens: usize) {
        let sessions = SessionStore::new(&self.db);
        let now = now();
        for _ in 0..60 {
            let (_, history) = sessions.active(&self.session).unwrap();
            let total: usize = history.iter().map(|m| message_tokens(&m.message)).sum();
            if total > target_tokens {
                return;
            }
            sessions
                .append(
                    &self.session,
                    &ChatMessage::user("用户长消息 ".repeat(500)),
                    Trust::User,
                    &now,
                )
                .unwrap();
            sessions
                .append(
                    &self.session,
                    &ChatMessage::assistant("助手长回复 ".repeat(500)),
                    Trust::System,
                    &now,
                )
                .unwrap();
        }
        panic!("fill loop did not reach {target_tokens} tokens");
    }
}

#[test]
fn turn_requests_compaction_worker_runs_next_turn_sees_summary() {
    let f = Fixture::new(vec![
        ChatResponse::text("第一轮回复"),
        ChatResponse::text("第二轮回复"),
    ]);
    f.fill_until(12_000);

    // Turn 1: over the threshold, so it requests a background compaction
    // and continues with the full history — no synchronous summary.
    let r1 = f.agent().run_turn(&f.session, "继续", &now()).unwrap();
    assert!(r1.compaction_requested, "turn 1 must request compaction");
    assert!(!r1.compacted, "turn 1 must not compact synchronously");
    assert_eq!(format!("{:?}", r1.stop), "Answered");

    // The job is queued with a snapshot boundary.
    let store = JobStore::new(&f.db);
    let job = store.get(&job_name(&f.session)).unwrap();
    assert_eq!(job.kind, "compaction");

    // The worker runs (no model configured here: extractive fallback)
    // and the summary lands for the next turn.
    let ctx = JobContext {
        job: &job,
        db: &f.db,
        now: now(),
        tz: chrono_tz::Asia::Shanghai,
        cancel: CancelToken::new(),
        trigger: Trigger::Manual,
        run_id: "e2e-run".into(),
    };
    let outcome = CompactionWorker { router: None }.run(&ctx).unwrap();
    assert!(
        matches!(outcome, hatch_scheduler::JobOutcome::Done { .. }),
        "{outcome:?}"
    );

    let (summary, live) = SessionStore::new(&f.db).active(&f.session).unwrap();
    assert!(summary.is_some(), "next turn must see the summary");
    assert!(
        live.iter().any(|m| m.message.content == "第一轮回复"),
        "the reply appended after the request stays live"
    );

    // Turn 2: history is small again, no new request, summary still there.
    let r2 = f.agent().run_turn(&f.session, "还有吗", &now()).unwrap();
    assert!(!r2.compaction_requested);
    assert!(!r2.compacted);
    let (summary, _) = SessionStore::new(&f.db).active(&f.session).unwrap();
    assert!(summary.is_some());
}

#[test]
fn hard_limit_compacts_synchronously_without_a_model_call() {
    // Exactly one scripted response: the turn's own answer. If compaction
    // called the model too, the queue would be exhausted and the turn
    // would end in ModelError instead of Answered.
    let f = Fixture::new(vec![ChatResponse::text("好的")]);
    f.fill_until(24_000);

    let r = f.agent().run_turn(&f.session, "继续", &now()).unwrap();
    assert!(r.compacted, "past the hard limit the turn compacts itself");
    assert!(
        !r.compaction_requested,
        "no background job past the hard limit"
    );
    assert_eq!(format!("{:?}", r.stop), "Answered");
    assert!(
        JobStore::new(&f.db).list().unwrap().is_empty(),
        "no job enqueued"
    );
    let (summary, _) = SessionStore::new(&f.db).active(&f.session).unwrap();
    assert!(summary.is_some(), "extractive summary must be stored");
}
