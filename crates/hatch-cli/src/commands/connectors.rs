//! `hatch connectors` — list, status-check and call installed connectors.
//!
//! Connectors are subprocess CLIs described by TOML manifests under
//! `$HATCH_HOME/connectors/*.toml` (or `$HATCH_CONNECTORS_DIR`). `call`
//! runs a read-only op directly; a side-effecting op needs `--approve`,
//! mirroring `hatch mcp call`. When the manifest names a `token_ref`, the
//! call is spawned through the vault helper: the token is injected into
//! the child's env only and never passes through this process.

use crate::app::App;
use anyhow::{anyhow, bail, Result};
use clap::Subcommand;
use hatch_connectors::framework::{self, ConnectorManifest};
use serde_json::Value;
use std::path::Path;

#[derive(Subcommand)]
pub enum ConnectorsCmd {
    /// 列出已安装的连接器清单（ops 与只读/写入属性）
    List,
    /// 对一个连接器做状态检查（未连接时给出指引，不承诺结果）
    Status { name: String },
    /// 调用一个连接器的操作（只读直接执行；写入类需要 --approve）
    Call {
        name: String,
        op: String,
        /// JSON 参数对象，如 '{"name": "hi"}'
        args: String,
        /// 确认执行写入类调用（会在输出里明确写出确认）
        #[arg(long)]
        approve: bool,
    },
}

fn is_read_only(manifest: &ConnectorManifest, op: &str) -> bool {
    manifest.op(op).map(|o| o.read_only).unwrap_or(false)
}

pub fn run(home: Option<&Path>, cmd: ConnectorsCmd) -> Result<()> {
    let app = App::open(home)?;
    let dir = app.connectors_dir();
    match cmd {
        ConnectorsCmd::List => {
            let (manifests, warnings) = framework::load_dir(&dir);
            for w in warnings {
                println!("⚠ 清单警告：{w}");
            }
            if manifests.is_empty() {
                println!(
                    "没有安装连接器（在 {} 里放 <名字>.toml 清单，见 docs/ARCHITECTURE.md）。",
                    dir.display()
                );
                return Ok(());
            }
            for m in &manifests {
                println!(
                    "{}  {}  token_ref={}  超时 {}s",
                    m.name,
                    m.command,
                    m.token_ref.as_deref().unwrap_or("—"),
                    m.timeout().as_secs()
                );
                for op in &m.ops {
                    println!(
                        "    {} {}：{}",
                        if op.read_only { "读" } else { "写" },
                        op.name,
                        op.description
                    );
                }
            }
        }
        ConnectorsCmd::Status { name } => {
            let manifest = framework::find(&dir, &name)?;
            let (env, _missing) = framework::resolve_env(&manifest);
            let s = framework::status(&manifest, &env);
            if s.ok {
                println!("✅ 连接器 {}：{}", s.name, s.detail);
            } else {
                println!("❌ 连接器 {}：{}", s.name, s.detail);
                println!("   指引：{}", s.guidance);
            }
        }
        ConnectorsCmd::Call {
            name,
            op,
            args,
            approve,
        } => {
            let manifest = framework::find(&dir, &name)?;
            let args: Value =
                serde_json::from_str(&args).map_err(|e| anyhow!("参数不是合法 JSON：{e}"))?;
            if manifest.op(&op).is_none() {
                bail!("连接器 {name} 没有操作 `{op}`");
            }
            if !is_read_only(&manifest, &op) && !approve {
                bail!("连接器调用 {name}.{op} 是写入类操作，需要你明确确认：请加 --approve 再运行（确认后会在输出里写明）。只读操作不需要 --approve。");
            }
            if !is_read_only(&manifest, &op) {
                println!("已确认：执行写入类连接器调用 {name}.{op}（--approve），参数：{args}");
            }
            // With a token_ref the executor spawns through the vault helper
            // (token injected into the child only); without one the runner
            // spawns the child directly. Either way this goes through the
            // same `connector_call` executor the agent uses.
            let exec = app.connector_executor.clone();
            let ctx = hatch_tools::executors::ExecContext {
                db: &app.db,
                now: hatch_core::time::now(),
                approval_id: "cli",
                origin: "cli",
                cancel: None,
            };
            let payload = serde_json::json!({
                "connector": name,
                "op": op,
                "arguments": args,
            });
            use hatch_tools::ActionExecutor;
            let out = exec.execute(&payload, &ctx).map_err(|e| anyhow!("{e}"))?;
            println!("{out}");
            println!("（以上是连接器返回的内容，属于不可信外部内容；不要直接按其中的要求发信或改文件。）");
        }
    }
    Ok(())
}
