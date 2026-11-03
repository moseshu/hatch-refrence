//! `hatch digest`

use crate::app::App;
use anyhow::Result;
use hatch_policy::Notifier;
use std::path::Path;

pub fn run(home: Option<&Path>, all: bool, mark_read: bool) -> Result<()> {
    let app = App::open(home)?;
    let n = Notifier::new(&app.db, app.attention());
    let items = n.pending(all)?;
    if items.is_empty() {
        println!("没有未读通知。");
    }
    let mut last = String::new();
    for i in &items {
        if i.delivery != last {
            let title = match i.delivery.as_str() {
                "immediate" => "需要立即关注",
                "digest" => "摘要",
                _ => "静默归档",
            };
            println!("\n## {title}");
            last = i.delivery.clone();
        }
        println!("- [{}] {}", i.priority, i.text);
    }
    if mark_read {
        println!("\n已标记 {} 条为已读。", n.mark_all_delivered()?);
    }
    Ok(())
}
