//! `hatch skills` — list, show, load and unload user-installed skills.
//!
//! Skills live in `$HATCH_HOME/skills/<name>/SKILL.md` (or `./skills/`).
//! `load` activates a skill for future chat turns; `show` just prints it.

use crate::app::App;
use anyhow::{anyhow, Result};
use clap::Subcommand;
use hatch_skills::registry::{activate, active_skills, deactivate};
use std::path::Path;

#[derive(Subcommand)]
pub enum SkillsCmd {
    /// 列出已安装的技能（只显示名称与说明）
    List,
    /// 查看一个技能的正文
    Show { name: String },
    /// 激活一个技能（之后对话会加载它；工具按它的 allowlist 过滤）
    Load { name: String },
    /// 取消激活
    Unload { name: String },
}

pub fn run(home: Option<&Path>, cmd: SkillsCmd) -> Result<()> {
    let app = App::open(home)?;
    let reg = app.skill_registry();
    match cmd {
        SkillsCmd::List => {
            let skills = reg.scan()?;
            let active = active_skills(&app.home);
            if skills.is_empty() {
                println!("没有安装任何技能（技能放在 $HATCH_HOME/skills/<名称>/SKILL.md）。");
                return Ok(());
            }
            for s in skills {
                let mark = if active.iter().any(|a| a == &s.name) {
                    "● 已激活"
                } else {
                    "○"
                };
                let tools = if s.tools.is_empty() {
                    "不限工具".into()
                } else {
                    format!("可用工具：{}", s.tools.join(", "))
                };
                println!("{mark} {}  {}", s.name, s.description);
                println!("    {tools}  作用域 {}  版本 {}", s.scope, s.version);
            }
        }
        SkillsCmd::Show { name } => {
            let skill = reg.load(&name)?;
            println!(
                "技能 {}（{}，版本 {}）",
                skill.name, skill.description, skill.version
            );
            if skill.tools.is_empty() {
                println!("可用工具：不限定");
            } else {
                println!("可用工具：{}", skill.tools.join(", "));
            }
            println!("\n{}", skill.body);
        }
        SkillsCmd::Load { name } => {
            let skill = reg.load(&name)?;
            activate(&app.home, &skill.name)?;
            println!("技能 {} 已激活，之后对话会加载它。", skill.name);
            if skill.tools.is_empty() {
                println!("该技能不限定工具。");
            } else {
                println!(
                    "本技能限定工具为：{}（对话中只提供这些工具 + list_skills / load_skill）。",
                    skill.tools.join("、")
                );
            }
        }
        SkillsCmd::Unload { name } => {
            if deactivate(&app.home, &name)? {
                println!("技能 {name} 已取消激活。");
            } else {
                return Err(anyhow!("技能 {name} 本来就没有激活"));
            }
        }
    }
    Ok(())
}
