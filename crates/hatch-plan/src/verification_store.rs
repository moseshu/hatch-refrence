//! Durable boundaries between dispatch, observation and completion.
use crate::verification::{VerificationMode, VerificationReport};
use hatch_core::time::{to_rfc3339, Timestamp};
use hatch_core::{Error, Result};
use hatch_store::{params, Connection, Db, DbResultExt, OptionalExtension};

#[derive(Clone, Debug)]
pub struct ExecutionRecord {
    pub contract: String,
    pub mode: VerificationMode,
    pub phase: String,
    pub output: String,
    pub receipt: String,
    pub approval_id: String,
    pub attempts: u32,
}
/// One execution receipt to persist before verification reads it back.
pub struct Executed<'a> {
    pub owner: &'a str,
    pub version: i64,
    pub step: &'a str,
    pub output: &'a str,
    pub receipt: &'a str,
    pub approval: &'a str,
    pub now: &'a Timestamp,
}

pub struct VerificationStore<'a> {
    db: &'a Db,
}
impl<'a> VerificationStore<'a> {
    pub fn new(db: &'a Db) -> Self {
        Self { db }
    }

    pub fn get(&self, owner: &str, version: i64, step: &str) -> Result<Option<ExecutionRecord>> {
        let row: Option<(String,String,String,String,String,String,u32)> = self.db.conn().query_row(
            "SELECT contract,mode,phase,output,receipt,approval_id,attempts FROM verification_runs WHERE owner=?1 AND version=?2 AND step_id=?3",
            params![owner,version,step], |r| Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?,r.get(5)?,r.get(6)?)))
            .optional().db()?;
        row.map(
            |(contract, mode, phase, output, receipt, approval_id, attempts)| {
                let mode = match mode.as_str() {
                    "off" => VerificationMode::Off,
                    "observe" => VerificationMode::Observe,
                    "enforce" => VerificationMode::Enforce,
                    _ => return Err(Error::Storage("无效的持久验证模式".into())),
                };
                Ok(ExecutionRecord {
                    contract,
                    mode,
                    phase,
                    output,
                    receipt,
                    approval_id,
                    attempts,
                })
            },
        )
        .transpose()
    }

    /// Frozen once per execution version. Returns false if another runner owns it.
    pub fn begin(
        &self,
        owner: &str,
        version: i64,
        step: &str,
        contract: &str,
        mode: VerificationMode,
        now: &Timestamp,
    ) -> Result<bool> {
        self.db.transaction(|c| {
            guard(c,owner,version)?;
            if !owner.starts_with("chat:") {
                let plan = crate::PlanStore::new(self.db).get(owner)?.ok_or_else(|| Error::NotFound(owner.into()))?;
                let frozen: serde_json::Value = serde_json::from_str(contract)?;
                if frozen.get("goal").and_then(serde_json::Value::as_str) != Some(plan.goal.as_str()) {
                    return Err(Error::Invalid("执行前目标已变化，拒绝旧契约".into()));
                }
                if step != "@task-completion" {
                    let current = plan.steps.iter().find(|s| s.id == step).ok_or_else(|| Error::NotFound(step.into()))?;
                    if crate::verification::contract(&plan,current)? != contract {
                        return Err(Error::Invalid("执行前步骤验收契约已变化".into()));
                    }
                }
            }
            Ok(c.execute("INSERT OR IGNORE INTO verification_runs(owner,version,step_id,contract,mode,phase,updated_at) VALUES(?1,?2,?3,?4,?5,'dispatching',?6)",
                params![owner,version,step,contract,mode.as_str(),to_rfc3339(now)]).db()? == 1)
        })
    }

    /// Keep the original execution output even if later verification fails.
    pub fn executed(&self, e: &Executed) -> Result<()> {
        self.db.transaction(|c| {
            guard(c, e.owner, e.version)?;
            let n = c.execute("UPDATE verification_runs SET phase='executed',output=?4,receipt=?5,approval_id=?6,updated_at=?7 WHERE owner=?1 AND version=?2 AND step_id=?3 AND phase IN ('dispatching','waiting')",
                params![e.owner, e.version, e.step, e.output, e.receipt, e.approval, to_rfc3339(e.now)]).db()?;
            if n != 1 { return Err(Error::Invalid("执行回执已保存或执行版本已失效".into())); }
            Ok(())
        })
    }

    pub fn waiting(
        &self,
        owner: &str,
        version: i64,
        step: &str,
        approval: &str,
        now: &Timestamp,
    ) -> Result<()> {
        self.db.transaction(|c| {
            guard(c,owner,version)?;
            c.execute("UPDATE verification_runs SET phase='waiting',approval_id=?4,updated_at=?5 WHERE owner=?1 AND version=?2 AND step_id=?3 AND phase='dispatching'",
                params![owner,version,step,approval,to_rfc3339(now)]).db()?;
            Ok(())
        })
    }

    /// A positively rejected dispatch can be retried; executed receipts are never removed.
    pub fn rejected(&self, owner: &str, version: i64, step: &str) -> Result<()> {
        self.db.conn().execute("DELETE FROM verification_runs WHERE owner=?1 AND version=?2 AND step_id=?3 AND phase='dispatching'",
            params![owner,version,step]).db()?;
        Ok(())
    }

    pub fn save(&self, report: &VerificationReport) -> Result<()> {
        self.db.transaction(|c| {
            guard(c,&report.owner,report.version)?;
            let frozen: (String,String) = c.query_row("SELECT contract,mode FROM verification_runs WHERE owner=?1 AND version=?2 AND step_id=?3",
                params![report.owner,report.version,report.step_id], |r| Ok((r.get(0)?,r.get(1)?))).db()?;
            if crate::verification::digest(frozen.0.as_bytes()) != report.contract_digest
                || frozen.1 != report.mode.as_str()
                || crate::verification::digest(crate::verification::RULESET.as_bytes()) != report.rules_digest
                || report.attempt == 0 || report.attempt > crate::verification::MAX_OBSERVATIONS {
                return Err(Error::Invalid("验证报告不匹配固定契约、模式、规则版本或预算".into()));
            }
            let n = c.execute("UPDATE verification_runs SET attempts=?4,phase='verified',updated_at=?5 WHERE owner=?1 AND version=?2 AND step_id=?3 AND attempts=?6 AND phase IN ('executed','verified')",
                params![report.owner,report.version,report.step_id,report.attempt,report.observed_at,report.attempt.saturating_sub(1)]).db()?;
            if n != 1 { return Err(Error::Invalid("验证轮次已变化，拒绝迟到的验证报告".into())); }
            c.execute("INSERT INTO verification_reports(owner,version,step_id,attempt,report_json,created_at) VALUES(?1,?2,?3,?4,?5,?6)",
                params![report.owner,report.version,report.step_id,report.attempt,serde_json::to_string(report)?,report.observed_at]).db()?;
            Ok(())
        })
    }

    pub fn reports(&self, owner: &str) -> Result<Vec<VerificationReport>> {
        let c = self.db.conn();
        let mut q = c
            .prepare("SELECT report_json FROM verification_reports WHERE owner=?1 ORDER BY id")
            .db()?;
        let rows = q
            .query_map(params![owner], |r| r.get::<_, String>(0))
            .db()?;
        let json = rows
            .collect::<hatch_store::rusqlite::Result<Vec<_>>>()
            .db()?;
        json.into_iter()
            .map(|s| serde_json::from_str(&s).map_err(Into::into))
            .collect()
    }

    pub fn records(&self, owner: &str) -> Result<Vec<(String, i64, ExecutionRecord)>> {
        let c = self.db.conn();
        let mut q = c
            .prepare("SELECT step_id,version FROM verification_runs WHERE owner=?1 ORDER BY rowid")
            .db()?;
        let keys = q
            .query_map(params![owner], |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?))
            })
            .db()?
            .collect::<hatch_store::rusqlite::Result<Vec<_>>>()
            .db()?;
        keys.into_iter()
            .map(|(id, v)| Ok((id.clone(), v, self.get(owner, v, &id)?.unwrap())))
            .collect()
    }

    pub fn latest(
        &self,
        owner: &str,
        version: i64,
        step: &str,
    ) -> Result<Option<VerificationReport>> {
        let json: Option<String> = self.db.conn().query_row(
            "SELECT report_json FROM verification_reports WHERE owner=?1 AND version=?2 AND step_id=?3 ORDER BY attempt DESC LIMIT 1",
            params![owner,version,step], |r| r.get(0)).optional().db()?;
        json.map(|s| serde_json::from_str(&s).map_err(Into::into))
            .transpose()
    }
}

/// All writes and completion commits check the plan version in the same transaction.
pub(crate) fn guard(c: &Connection, owner: &str, version: i64) -> Result<()> {
    if owner.starts_with("chat:") {
        return Ok(());
    }
    let stopped: bool = c.query_row("SELECT EXISTS(SELECT 1 FROM tasks WHERE plan_id=?1 AND status IN ('cancelled','paused'))",
        params![owner],|r| r.get(0)).db()?;
    if stopped {
        return Err(Error::Invalid("任务已取消或暂停，拒绝提交验证结果".into()));
    }
    let state: Option<(i64, String)> = c
        .query_row(
            "SELECT version,status FROM plans WHERE id=?1",
            params![owner],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()
        .db()?;
    match state {
        Some((v, s)) if v == version && matches!(s.as_str(), "approved" | "executing") => Ok(()),
        _ => Err(Error::Invalid("验证目标的计划版本或状态已失效".into())),
    }
}
