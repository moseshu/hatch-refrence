//! `hatch checkpoint list|show|rewind`

use crate::app::App;
use anyhow::Result;
use clap::Subcommand;
use hatch_core::time::fmt_local;
use hatch_tools::CheckpointStore;
use std::path::Path;

#[derive(Subcommand)]
pub enum CheckpointCmd {
    /// 最近的文件检查点（每次 hatch 修改文件前自动创建）
    List {
        #[arg(long, default_value_t = 20)]
        limit: usize,
    },
    /// 查看某个检查点保存的内容
    Show { id: String },
    /// 把文件恢复到检查点时的状态（恢复前会再建一个检查点，可撤销）
    Rewind { id: String },
}

pub fn run(home: Option<&Path>, cmd: CheckpointCmd) -> Result<()> {
    let app = App::open(home)?;
    let store = CheckpointStore::new(&app.db);
    match cmd {
        CheckpointCmd::List { limit } => {
            let list = store.list(limit)?;
            if list.is_empty() {
                println!("还没有检查点。");
            }
            for c in list {
                println!(
                    "{}  {}  {}{}  {} 字节  来源 {}",
                    c.id,
                    fmt_local(&c.created_at, &app.tz),
                    c.path,
                    if c.existed { "" } else { "（当时不存在）" },
                    c.size,
                    c.origin
                );
            }
        }
        CheckpointCmd::Show { id } => {
            let (c, content) = store.get(&id)?;
            println!("{}  工作区 {}  文件 {}  审批 {}", c.id, c.root, c.path, c.approval_id);
            match content {
                Some(b) => println!("----------------------------------------\n{}", String::from_utf8_lossy(&b)),
                None => println!("（检查点时文件不存在）"),
            }
        }
        CheckpointCmd::Rewind { id } => println!("{}", store.rewind(&id, &hatch_core::time::now())?),
    }
    Ok(())
}
