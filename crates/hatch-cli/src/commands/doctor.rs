//! `hatch doctor` — deterministic, read-only self check. Never prints secrets.

use crate::app::App;
use crate::config::{AppConfig, ENV_API_KEY, ENV_BASE_URL, ENV_MODEL};
use anyhow::Result;
use hatch_policy::{ApprovalQueue, ApprovalStatus};
use hatch_store::Db;
use std::path::Path;

fn mark(ok: bool) -> &'static str {
    if ok {
        "✅"
    } else {
        "❌"
    }
}

pub fn run(home: Option<&Path>) -> Result<()> {
    let app = App::open(home)?;
    let cfg_path = app.home.join("config.toml");
    println!("数据目录：{}", app.home.display());
    println!("{} 配置文件 {}", mark(cfg_path.exists()), cfg_path.display());
    println!("   用户：{}  时区：{}  行为规约版本：{}", app.config.user.name, app.config.user.timezone, hatch_agent::prompt::prompt_version());

    println!("\n模型：");
    if AppConfig::env_model().is_some() {
        println!("   ✅ 使用环境变量模型（{ENV_BASE_URL} / {ENV_MODEL} / {ENV_API_KEY}），覆盖配置文件中的全部模型");
    }
    let models = app.config.resolved_models();
    if models.is_empty() {
        println!("   ❌ 没有可用模型");
    }
    for m in &models {
        println!(
            "   {} {}  {}  {}  {}（密钥环境变量 {}{}）",
            mark(m.api_key.is_some()),
            m.cfg.name,
            m.cfg.provider,
            m.cfg.model,
            m.cfg.base_url,
            m.cfg.api_key_env,
            if m.api_key.is_some() { " 已设置" } else { " 未设置" }
        );
    }
    let l = &app.config.limits;
    println!("   费用护栏：每次命令最多 {} 次调用，每次 max_tokens {}", l.max_calls_per_run.map(|n| n.to_string()).unwrap_or_else(|| "不限".into()), l.max_tokens.map(|n| n.to_string()).unwrap_or_else(|| "不限".into()));

    let mc = &app.config.mail;
    println!("\n邮件：");
    println!("   来源 {}  IMAP {}:{}  SMTP {}:{}  账号 {}", mc.source, mc.imap_host, mc.imap_port, mc.smtp_host, mc.smtp_port, if mc.username.is_empty() { "(未配置)" } else { &mc.username });
    let pw = std::env::var(&mc.password_env).map(|v| !v.is_empty()).unwrap_or(false);
    println!("   {} 授权码环境变量 {}", mark(pw || mc.source == "fixture"), mc.password_env);
    println!("   发信方式：{}", if mc.sender == "smtp" { "SMTP 真实发送（仍需逐封审批或常设授权）".to_string() } else { format!("仅写入 outbox：{}", app.outbox_dir().display()) });

    println!("\n数据库：");
    for t in Db::tables() {
        if matches!(*t, "meta" | "audit") {
            continue;
        }
        println!("   {:<18} {}", t, app.db.count(t)?);
    }
    let pending = ApprovalQueue::new(&app.db).list(Some(ApprovalStatus::Pending))?.len();
    println!("\n待审批：{pending}");
    let used: (i64, i64, i64) = app
        .db
        .conn()
        .query_row("SELECT COALESCE(SUM(prompt_tokens),0), COALESCE(SUM(completion_tokens),0), COALESCE(SUM(cached_tokens),0) FROM usage_log", [], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
        .unwrap_or((0, 0, 0));
    println!("累计 tokens：prompt {} / completion {} / cached {}（只记 token，不估算金额）", used.0, used.1, used.2);
    Ok(())
}
