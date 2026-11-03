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
    println!(
        "{} 配置文件 {}",
        mark(cfg_path.exists()),
        cfg_path.display()
    );
    println!(
        "   用户：{}  时区：{}  行为规约版本：{}",
        app.config.user.name,
        app.config.user.timezone,
        hatch_agent::prompt::prompt_version()
    );

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
            if m.api_key.is_some() {
                " 已设置"
            } else {
                " 未设置"
            }
        );
    }
    let l = &app.config.limits;
    println!(
        "   费用护栏：每次命令最多 {} 次调用，每次 max_tokens {}",
        l.max_calls_per_run
            .map(|n| n.to_string())
            .unwrap_or_else(|| "不限".into()),
        l.max_tokens
            .map(|n| n.to_string())
            .unwrap_or_else(|| "不限".into())
    );

    let mc = &app.config.mail;
    println!("\n邮件：");
    println!(
        "   来源 {}  IMAP {}:{}  SMTP {}:{}  账号 {}",
        mc.source,
        mc.imap_host,
        mc.imap_port,
        mc.smtp_host,
        mc.smtp_port,
        if mc.username.is_empty() {
            "(未配置)"
        } else {
            &mc.username
        }
    );
    let pw = std::env::var(&mc.password_env)
        .map(|v| !v.is_empty())
        .unwrap_or(false);
    println!(
        "   {} 授权码环境变量 {}",
        mark(pw || mc.source == "fixture"),
        mc.password_env
    );
    println!(
        "   发信方式：{}",
        if mc.sender == "smtp" {
            "SMTP 真实发送（仍需逐封审批或常设授权）".to_string()
        } else {
            format!("仅写入 outbox：{}", app.outbox_dir().display())
        }
    );

    println!("\n工作区与沙箱：");
    match app.workspace(None) {
        Ok(Some(ws)) => println!("   ✅ 工作区 {}（文件与命令工具可用）", ws.root().display()),
        Ok(None) => println!("   ➖ 未配置 [workspace] root：文件与命令工具关闭（可用 `hatch chat --workspace <目录>` 临时开启）"),
        Err(e) => println!("   ❌ 工作区无效：{e}"),
    }
    match app.sandbox() {
        Ok(sb) if sb.mode == hatch_sandbox::SandboxMode::None => {
            println!("   ⚠ sandbox.mode = \"none\"：命令不隔离运行，结果会标注“未隔离”")
        }
        Ok(sb) => match sb.probe() {
            Ok(r) => {
                println!("   ✅ 命名空间沙箱可用：{}", r.summary());
                for n in &r.notes {
                    println!("      注意：{n}");
                }
            }
            Err(e) => println!("   ❌ 沙箱不可用，shell_run 会拒绝执行：{e}"),
        },
        Err(e) => println!("   ❌ 沙箱配置错误：{e}"),
    }
    println!("\n浏览器：");
    browser_check(&app);

    println!("\n技能：");
    match app.skill_registry().scan() {
        Ok(skills) => {
            if skills.is_empty() {
                println!("   ➖ 没有安装技能（$HATCH_HOME/skills/<名称>/SKILL.md）");
            } else {
                let active = hatch_skills::registry::active_skills(&app.home);
                for s in skills {
                    println!(
                        "   {} {}  {}",
                        if active.iter().any(|a| a == &s.name) {
                            "● 已激活"
                        } else {
                            "○"
                        },
                        s.name,
                        s.description
                    );
                }
            }
        }
        Err(e) => println!("   ❌ 技能目录扫描失败：{e}"),
    }
    if !app.config.skills.load.is_empty() {
        println!(
            "   配置 [skills] load：{}",
            app.config.skills.load.join(", ")
        );
    }

    println!("\nMCP 服务器：");
    if app.config.mcp_servers.is_empty() {
        println!("   ➖ 没有配置 [[mcp_servers]]");
    } else {
        for s in &app.config.mcp_servers {
            println!(
                "   {} {}  {}  token_ref={}",
                mark(s.enabled),
                s.name,
                s.command,
                s.token_ref.as_deref().unwrap_or("—")
            );
        }
        println!("   （stdio 传输；服务器输出一律视为不可信内容，调用走审批队列）");
    }

    println!("\n连接器：");
    let connectors_dir = hatch_connectors::framework::default_dir(&app.home);
    let (manifests, warnings) = hatch_connectors::framework::load_dir(&connectors_dir);
    for w in warnings {
        println!("   ⚠ 清单警告：{w}");
    }
    if manifests.is_empty() {
        println!(
            "   ➖ 没有安装连接器（{} 里放 <名字>.toml 清单）",
            connectors_dir.display()
        );
    } else {
        for m in &manifests {
            let (env, _missing) = hatch_connectors::framework::resolve_env(m);
            let s = hatch_connectors::framework::status(m, &env);
            println!(
                "   {} {}  {} 个操作  token_ref={}",
                mark(s.ok),
                m.name,
                m.ops.len(),
                m.token_ref.as_deref().unwrap_or("—")
            );
            if s.ok {
                println!("      {}", s.detail);
            } else {
                println!("      {}（指引：{}）", s.detail, s.guidance);
            }
        }
    }

    println!("\n凭据保管（vault helper 进程 + 0600 文件）：");
    match hatch_vault::helper_status(&app.home) {
        st if st.running => println!(
            "   ✅ vault helper 运行中（pid {}）",
            st.pid
                .map(|p| p.to_string())
                .unwrap_or_else(|| "未知".into())
        ),
        _ => println!("   ➖ vault helper 未运行（需要 token 时自动启动；空闲 5 分钟后自动退出）"),
    }
    match hatch_vault::open(&app.home) {
        Ok(vault) => {
            let problems = vault.check_permissions();
            if problems.is_empty() {
                println!(
                    "   ✅ vault 目录/文件权限正常（目录 0700，tokens.json 0600），{} 个 token",
                    vault.list().map(|v| v.len()).unwrap_or(0)
                );
            } else {
                for p in problems {
                    println!("   ⚠ {p}");
                }
            }
            println!("   注意：token 由独立 helper 进程持有；agent 进程只持有 token 名字，注入时 helper 直接把值设进子进程环境（MCP 长连接是例外：生成瞬间短暂经过本进程内存，见 docs/SECURITY.md）。");
        }
        Err(e) => println!("   ❌ vault 打不开：{e}"),
    }
    println!(
        "   网页读取：{}；搜索：{}",
        if app.config.web.fetch {
            "开启"
        } else {
            "关闭"
        },
        if app.config.web.search_url.is_empty() {
            "未配置（不提供 web_search）"
        } else {
            &app.config.web.search_url
        }
    );

    println!("\n多通道传输：");
    crate::commands::transport::doctor_section(&app)?;

    println!("\n支付与其它应用：");
    println!("   ➖ 支付：未接入（payment 只建议、不执行；调用即拒绝，见 docs/SECURITY.md §9i）");
    println!("   ➖ 其它应用：除已安装连接器与 pipe 参考传输外，未接入真实服务（微信/飞书/Telegram 等按需新增连接器/传输，见 docs/ARCHITECTURE.md §7）");

    println!("\nCalDAV 同步：");
    caldav_check(&app);

    println!("\n数据库：");
    for t in Db::tables() {
        if matches!(*t, "meta" | "audit") {
            continue;
        }
        println!("   {:<18} {}", t, app.db.count(t)?);
    }
    let pending = ApprovalQueue::new(&app.db)
        .list(Some(ApprovalStatus::Pending))?
        .len();
    println!("\n待审批：{pending}");
    let used: (i64, i64, i64) = app
        .db
        .conn()
        .query_row("SELECT COALESCE(SUM(prompt_tokens),0), COALESCE(SUM(completion_tokens),0), COALESCE(SUM(cached_tokens),0) FROM usage_log", [], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
        .unwrap_or((0, 0, 0));
    println!(
        "累计 tokens：prompt {} / completion {} / cached {}（只记 token，不估算金额）",
        used.0, used.1, used.2
    );
    // Per-workload prompt-cache stats, from provider-reported usage only:
    // hit ratio = cached / prompt. A workload with no cached tokens shows
    // 0% — that is an observation, not a claim that caching is off.
    {
        let c = app.db.conn();
        let mut st = c
            .prepare(
                "SELECT workload, COUNT(*), COALESCE(SUM(prompt_tokens),0), COALESCE(SUM(cached_tokens),0), \
                 COALESCE(SUM(prompt_tokens + completion_tokens),0) FROM usage_log GROUP BY workload ORDER BY workload",
            )
            .unwrap();
        let rows = st
            .query_map([], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, i64>(1)?,
                    r.get::<_, i64>(2)?,
                    r.get::<_, i64>(3)?,
                    r.get::<_, i64>(4)?,
                ))
            })
            .unwrap();
        let rows: Vec<_> = rows.collect::<Result<_, _>>().unwrap_or_default();
        if !rows.is_empty() {
            println!("\n各 workload 缓存（provider 上报的 cached_tokens / prompt_tokens）：");
            for (w, calls, prompt, cached, total) in rows {
                let ratio = if prompt == 0 { 0.0 } else { cached as f64 / prompt as f64 * 100.0 };
                println!("   {:<13} 调用 {:>4}  prompt {:>8}  cached {:>8}  合计 {:>8}  命中 {:.1}%", w, calls, prompt, cached, total, ratio);
            }
            println!("   说明：Anthropic 接受 cache_ttl_secs（300/3600，见 [routing.cache_ttl_secs]）；OpenAI 系自动缓存、不接受 TTL；Gemini 的显式缓存未实现、TTL 被忽略。");
        }
    }

    println!("\n定时任务：");
    let paths = crate::sched::daemon_paths(&app);
    let st = hatch_scheduler::daemon::status(&app.db, &paths)?;
    match (st.alive, st.pid) {
        (true, Some(p)) => println!(
            "   ✅ daemon 运行中（pid {p}），最近检查 {}",
            if st.last_tick.is_empty() {
                "—"
            } else {
                &st.last_tick
            }
        ),
        (false, Some(p)) => println!("   ⚠ daemon 未运行，锁文件残留 pid {p}（下次启动自动清理）"),
        _ => println!("   ➖ daemon 未运行（`hatch daemon run`）；任务只在 daemon 运行时触发"),
    }
    if crate::sched::router_factory(&app).is_none() {
        println!("   ⚠ 没有配置模型：简报只列出事项原文，triage_sync 会失败");
    }
    let jobs = hatch_scheduler::JobStore::new(&app.db).list()?;
    if jobs.is_empty() {
        println!("   （没有任务）");
    }
    for j in jobs {
        println!(
            "   {} {:<16} {:<13} {:<22} {}",
            mark(j.enabled),
            j.name,
            j.kind,
            j.schedule.to_string(),
            crate::commands::jobs::status_line(&j, &app.tz)
        );
    }
    Ok(())
}

/// `hatch doctor` 的 CalDAV 一节：只检查配置与可达性，不同步、不写库。
fn caldav_check(app: &App) {
    let Some(settings) = app.config.caldav_settings() else {
        println!("   ➖ 未配置 [caldav] server_url（不同步；见 config/hatch.example.toml）");
        return;
    };
    println!(
        "   服务器 {}  账号 {}  token_ref={}（只存名字，值在 vault 里）",
        settings.server_url,
        if settings.username.is_empty() {
            "（未填）"
        } else {
            &settings.username
        },
        settings.token_ref
    );
    match hatch_vault::VaultClient::start(&app.home) {
        Err(e) => println!("   ❌ vault helper 启动失败，无法检查 token：{e}"),
        Ok(vc) => {
            match vc.list_names() {
                Ok(names) if names.iter().any(|n| n == &settings.token_ref) => {
                    println!("   ✅ vault 里有 token `{}`", settings.token_ref)
                }
                Ok(_) => println!(
                    "   ❌ vault 里没有 token `{}`（`hatch vault set {}` 先存一个假的/真的 token；同步会失败）",
                    settings.token_ref, settings.token_ref
                ),
                Err(e) => println!("   ❌ vault token 列表读取失败：{e}"),
            }
            let http = hatch_connectors::caldav::VaultHttp::new(vc, settings.token_ref.clone());
            match hatch_connectors::caldav::CaldavClient::new(&settings.server_url, Box::new(http))
            {
                Err(e) => println!("   ❌ 服务器 URL 无效：{e}"),
                Ok(client) => match client.probe() {
                    Ok(status) if (200..300).contains(&status) || status == 207 => {
                        println!("   ✅ 服务器可达（PROPFIND Depth 0 → HTTP {status}；只是探活，没有同步）")
                    }
                    Ok(status) => {
                        println!("   ❌ 服务器返回 HTTP {status}（401 = token 或账号不对，404 = URL 不对）")
                    }
                    Err(e) => println!("   ❌ 服务器不可达：{e}"),
                },
            }
        }
    }
}

/// Find Chromium and, when the browser tools are enabled, start it once in a
/// throwaway profile and load `about:blank`.
fn browser_check(app: &App) {
    let cfg = app.browser_config();
    let Some(exe) = hatch_browser::find_chromium(cfg.chromium_path.as_deref()) else {
        println!(
            "   {} 找不到 Chromium/Chrome（[browser] chromium_path 或 HATCH_CHROMIUM）",
            if app.config.browser.enabled {
                "❌"
            } else {
                "➖"
            }
        );
        return;
    };
    if !app.config.browser.enabled {
        println!(
            "   ➖ 浏览器工具未开启（[browser] enabled = false）；找到 {}",
            exe.display()
        );
        return;
    }
    let dir = std::env::temp_dir().join(format!("hatch-doctor-browser-{}", std::process::id()));
    let mut probe = cfg.clone();
    probe.profiles_dir = dir.join("profiles");
    probe.screenshots_dir = dir.join("shots");
    let started = std::time::Instant::now();
    let broker = hatch_browser::Broker::new(probe);
    let db = match hatch_store::Db::open_in_memory() {
        Ok(d) => d,
        Err(e) => return println!("   ❌ {e}"),
    };
    match broker.open(&db, "doctor", "about:blank", None) {
        Ok(_) => println!(
            "   ✅ {} 可以启动并打开页面（{:.1} 秒）；profile 目录 {}",
            exe.display(),
            started.elapsed().as_secs_f32(),
            cfg.profiles_dir.display()
        ),
        Err(e) => println!("   ❌ 浏览器启动失败：{e}"),
    }
    drop(broker);
    let _ = std::fs::remove_dir_all(&dir);
    if cfg.allow_loopback || cfg.ignore_cert_errors {
        println!("   ⚠ 测试用设置已开启（allow_loopback / ignore_cert_errors），生产环境请关闭");
    }
}
