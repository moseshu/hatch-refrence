//! L0 intent gate: a pre-execution review of prepared actions.
//!
//! The gate asks an isolated judge whether the action about to be
//! dispatched matches the user's intent. It complements the
//! completion gate (L1–L3), which verifies results after execution:
//! L0 checks the intention before, the completion gate checks the
//! evidence after.
//!
//! Observe mode records verdicts without affecting dispatch;
//! Enforce mode blocks on deny/uncertain. The judge itself lives
//! outside this crate (the agent owns the model router); dispatch
//! only knows the [`IntentJudge`] trait, mirroring
//! `RunDeps.grade` in hatch-plan.

use hatch_core::{CancelToken, Error, Result};
use hatch_policy::ActionClass;
use hatch_store::{params, Db, DbResultExt};
use serde_json::Value;
use std::time::{Duration, Instant};

/// `HATCH_INTENT_GATE` switch. Observe records verdicts without
/// affecting dispatch; Enforce blocks on deny/uncertain (batch 3).
/// Default off — Enforce is only ever entered by explicit config,
/// after the Observe data supports it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum IntentGateMode {
    #[default]
    Off,
    Observe,
    Enforce,
}

impl IntentGateMode {
    pub fn configured() -> Result<Self> {
        match std::env::var("HATCH_INTENT_GATE").as_deref() {
            Err(std::env::VarError::NotPresent) | Ok("off") => Ok(Self::Off),
            Ok("observe") => Ok(Self::Observe),
            Ok("enforce") => Ok(Self::Enforce),
            _ => Err(Error::Invalid(
                "HATCH_INTENT_GATE 必须是 off、observe 或 enforce".into(),
            )),
        }
    }
}

/// Whether L0 reviews this class at all: irreversible and
/// compensable actions only (the taint gate covers irreversible;
/// calendar writes are L0's one extra class).
pub fn in_scope(class: ActionClass) -> bool {
    class.reversibility() != hatch_policy::Reversibility::Reversible
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IntentVerdict {
    Allow,
    Deny,
    Uncertain,
}

impl IntentVerdict {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Allow => "allow",
            Self::Deny => "deny",
            Self::Uncertain => "uncertain",
        }
    }
}

/// Why a verdict is not a clean allow/deny. The two kinds are
/// recorded and handled separately: an ambiguous intent is the
/// user's to clarify; an unavailable judge is a fault to retry.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IntentFailure {
    AmbiguousIntent,
    JudgeUnavailable,
}

impl IntentFailure {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::AmbiguousIntent => "ambiguous_intent",
            Self::JudgeUnavailable => "judge_unavailable",
        }
    }
}

#[derive(Clone, Debug)]
pub struct IntentJudgement {
    pub verdict: IntentVerdict,
    pub reason: String,
    pub failure: Option<IntentFailure>,
}

/// The prepared action as the judge sees it: the effective class
/// (after `tool.plan` + `recheck_class`), the human summary, the
/// targets, and a redacted projection of the action fields. The
/// full action is bound by digest (see [`action_digest`]); the
/// judge only ever receives this projection.
#[derive(Clone, Debug)]
pub struct PreparedActionView {
    pub tool: String,
    pub class: ActionClass,
    pub summary: String,
    pub recipients: Vec<String>,
    pub labels: Vec<String>,
    pub fields: Value,
}

#[derive(Clone, Debug)]
pub struct IntentJudgeInput {
    pub intent: String,
    pub action: PreparedActionView,
    pub tainted: bool,
}

/// The isolated intent judge. Implementations must not execute
/// anything and must treat everything inside the action fields as
/// untrusted data. An `Err` means the judge was unavailable
/// (timeout, parse failure, model error) — never an allow.
pub trait IntentJudge: Send + Sync {
    fn judge(&self, input: &IntentJudgeInput, cancel: &CancelToken) -> Result<IntentJudgement>;
    /// Judge identity for cache binding (prompt/model version).
    fn version(&self) -> &str;
}

/// Everything dispatch needs to run L0 for one turn. Carried on
/// the `ToolContext` so the normal, bridged (`use_tool`) and
/// recovery-replay paths share the one review entry.
pub struct IntentReview<'a> {
    pub judge: &'a dyn IntentJudge,
    pub intent: String,
    pub turn_id: String,
    /// The turn context was rebuilt without its persisted batch
    /// context (pre-v24 record): verdicts must not be reused.
    pub context_missing: bool,
    /// Observe records only; Enforce blocks dispatch on
    /// deny/uncertain (never on skipped/deterministic outcomes —
    /// those keep the original route).
    pub mode: IntentGateMode,
}

/// What deterministic routing would have done with this action —
/// recorded with every verdict so Observe statistics can separate
/// "L0 would have prevented an auto-execution" from "L0 would
/// only have saved an approval request".
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Counterfactual {
    WouldAutoExecute,
    WouldQueue,
    WouldDeny,
    ExistingApproval,
    AlreadyHandled,
    DirectCantQueue,
}

impl Counterfactual {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::WouldAutoExecute => "would_auto_execute",
            Self::WouldQueue => "would_queue",
            Self::WouldDeny => "would_deny",
            Self::ExistingApproval => "existing_approval",
            Self::AlreadyHandled => "already_handled",
            Self::DirectCantQueue => "direct_cant_queue",
        }
    }
}

/// One recorded L0 attempt (judged or skipped).
#[derive(Clone, Debug)]
pub struct VerdictRecord {
    pub turn_id: String,
    pub origin: String,
    pub action_digest: String,
    pub tool: String,
    pub class: String,
    pub target: String,
    pub intent_digest: String,
    pub rules_digest: String,
    pub tainted: bool,
    pub judge_version: String,
    /// allow / deny / uncertain / skipped
    pub verdict: String,
    pub reason: String,
    pub cache_hit: bool,
    pub counterfactual: String,
    pub failure_kind: String,
    pub context_missing: bool,
}

/// Digest of the final dispatch object: the approval payload for
/// deferred actions, tool + arguments for direct ones. Any field
/// change changes the digest, so a changed action is re-judged.
pub fn action_digest(tool: &str, class: ActionClass, payload: &Value) -> String {
    let canonical = serde_json::json!({
        "tool": tool,
        "class": class.as_str(),
        "payload": payload,
    });
    crate::checkpoint::content_hash(canonical.to_string().as_bytes())
}

pub fn intent_digest(intent: &str) -> String {
    crate::checkpoint::content_hash(intent.as_bytes())
}

/// Rules digest for the (currently empty) v1 rule set.
pub fn rules_digest() -> String {
    crate::checkpoint::content_hash(b"[]")
}

const MAX_FIELD_CHARS: usize = 1000;

/// Project action fields for the judge: secrets redacted, long
/// strings capped. The judge needs enough to check intent
/// (recipients, subject, command shape, form targets), never the
/// secret values themselves.
pub fn project_fields(value: &Value) -> Value {
    match value {
        Value::String(s) => {
            if hatch_core::secrets::contains_secret(s) {
                Value::String("[redacted]".into())
            } else if s.chars().count() > MAX_FIELD_CHARS {
                let cut: String = s.chars().take(MAX_FIELD_CHARS).collect();
                Value::String(format!("{cut}…[truncated]"))
            } else {
                value.clone()
            }
        }
        Value::Array(items) => Value::Array(items.iter().take(20).map(project_fields).collect()),
        Value::Object(map) => Value::Object(
            map.iter()
                .map(|(k, v)| (k.clone(), project_fields(v)))
                .collect(),
        ),
        other => other.clone(),
    }
}

/// Total budget for one judge attempt sequence (first try + one
/// retry on judge-unavailable).
pub const JUDGE_BUDGET: Duration = Duration::from_secs(30);

/// Run the judge with a watchdog: a child token cancelled when
/// the budget expires (the parent's cancellation still propagates
/// through the child). One retry on judge-unavailable, inside the
/// same budget. Any final failure becomes Uncertain /
/// JudgeUnavailable — never an allow.
pub fn run_judge(
    judge: &dyn IntentJudge,
    input: &IntentJudgeInput,
    cancel: &CancelToken,
) -> IntentJudgement {
    let started = Instant::now();
    let mut last_err = String::new();
    for attempt in 0..2 {
        if attempt > 0 && started.elapsed() >= JUDGE_BUDGET {
            break;
        }
        let child = cancel.child();
        let watchdog = {
            let child = child.clone();
            let left = JUDGE_BUDGET.saturating_sub(started.elapsed());
            std::thread::spawn(move || {
                if !child.sleep(left) {
                    child.cancel("intent judge timeout");
                }
            })
        };
        let outcome = judge.judge(input, &child);
        child.cancel("intent judge attempt done");
        let _ = watchdog.join();
        match outcome {
            Ok(j) if j.failure != Some(IntentFailure::JudgeUnavailable) => return j,
            Ok(j) => last_err = j.reason,
            Err(e) => last_err = e.to_string(),
        }
        if cancel.is_cancelled() {
            break;
        }
    }
    IntentJudgement {
        verdict: IntentVerdict::Uncertain,
        reason: format!("评委不可用：{last_err}"),
        failure: Some(IntentFailure::JudgeUnavailable),
    }
}

/// L0 verdict storage: its own table and write path. The
/// completion-verification store cannot take these rows (it
/// requires a frozen contract and an executed phase).
pub struct IntentVerdictStore<'a> {
    db: &'a Db,
}

impl<'a> IntentVerdictStore<'a> {
    pub fn new(db: &'a Db) -> Self {
        Self { db }
    }

    pub fn record(&self, r: &VerdictRecord, now: &hatch_core::time::Timestamp) -> Result<()> {
        self.db
            .conn()
            .execute(
                "INSERT INTO intent_verdicts(turn_id, origin, action_digest, tool, class, target, \
                 intent_digest, rules_digest, tainted, judge_version, verdict, reason, cache_hit, \
                 counterfactual, failure_kind, context_missing, created_at) \
                 VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17)",
                params![
                    r.turn_id,
                    r.origin,
                    r.action_digest,
                    r.tool,
                    r.class,
                    r.target,
                    r.intent_digest,
                    r.rules_digest,
                    r.tainted as i64,
                    r.judge_version,
                    r.verdict,
                    r.reason,
                    r.cache_hit as i64,
                    r.counterfactual,
                    r.failure_kind,
                    r.context_missing as i64,
                    hatch_core::time::to_rfc3339(now),
                ],
            )
            .db()?;
        Ok(())
    }

    /// The latest reusable verdict for this exact action in this
    /// turn. Judge-unavailable rows are never reused (the next
    /// attempt retries the judge); skipped rows are not verdicts.
    pub fn find_cached(
        &self,
        turn_id: &str,
        action_digest: &str,
        intent_digest: &str,
        tainted: bool,
        judge_version: &str,
    ) -> Result<Option<VerdictRecord>> {
        let conn = self.db.conn();
        let mut st = conn
            .prepare(
                "SELECT turn_id, origin, action_digest, tool, class, target, intent_digest, \
                 rules_digest, tainted, judge_version, verdict, reason, cache_hit, \
                 counterfactual, failure_kind, context_missing FROM intent_verdicts \
                 WHERE turn_id=?1 AND action_digest=?2 AND intent_digest=?3 AND tainted=?4 \
                 AND judge_version=?5 AND verdict IN ('allow','deny','uncertain') \
                 AND failure_kind != 'judge_unavailable' ORDER BY id DESC LIMIT 1",
            )
            .db()?;
        let rows = st
            .query_map(
                params![
                    turn_id,
                    action_digest,
                    intent_digest,
                    tainted as i64,
                    judge_version
                ],
                |r| {
                    Ok(VerdictRecord {
                        turn_id: r.get(0)?,
                        origin: r.get(1)?,
                        action_digest: r.get(2)?,
                        tool: r.get(3)?,
                        class: r.get(4)?,
                        target: r.get(5)?,
                        intent_digest: r.get(6)?,
                        rules_digest: r.get(7)?,
                        tainted: r.get::<_, i64>(8)? != 0,
                        judge_version: r.get(9)?,
                        verdict: r.get(10)?,
                        reason: r.get(11)?,
                        cache_hit: r.get::<_, i64>(12)? != 0,
                        counterfactual: r.get(13)?,
                        failure_kind: r.get(14)?,
                        context_missing: r.get::<_, i64>(15)? != 0,
                    })
                },
            )
            .db()?;
        let mut found = None;
        for row in rows {
            found = Some(row.db()?);
        }
        Ok(found)
    }

    /// All verdicts recorded for one turn (tests and reporting).
    pub fn for_turn(&self, turn_id: &str) -> Result<Vec<VerdictRecord>> {
        let conn = self.db.conn();
        let mut st = conn
            .prepare(
                "SELECT turn_id, origin, action_digest, tool, class, target, intent_digest, \
                 rules_digest, tainted, judge_version, verdict, reason, cache_hit, \
                 counterfactual, failure_kind, context_missing FROM intent_verdicts \
                 WHERE turn_id=?1 ORDER BY id",
            )
            .db()?;
        let rows = st
            .query_map(params![turn_id], |r| {
                Ok(VerdictRecord {
                    turn_id: r.get(0)?,
                    origin: r.get(1)?,
                    action_digest: r.get(2)?,
                    tool: r.get(3)?,
                    class: r.get(4)?,
                    target: r.get(5)?,
                    intent_digest: r.get(6)?,
                    rules_digest: r.get(7)?,
                    tainted: r.get::<_, i64>(8)? != 0,
                    judge_version: r.get(9)?,
                    verdict: r.get(10)?,
                    reason: r.get(11)?,
                    cache_hit: r.get::<_, i64>(12)? != 0,
                    counterfactual: r.get(13)?,
                    failure_kind: r.get(14)?,
                    context_missing: r.get::<_, i64>(15)? != 0,
                })
            })
            .db()?;
        rows.collect::<hatch_store::rusqlite::Result<Vec<_>>>().db()
    }
}
