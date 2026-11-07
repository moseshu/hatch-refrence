//! Exact-sequence assertions end to end: the pipeline worlds carry the
//! new checks and pass, and a mutated expectation fails the right check.

use hatch_eval::delegate::{
    default_world_dir as delegate_dir, run_delegate_eval, DelegateWorld, DgMode, ExpectedCall,
};
use hatch_eval::workspace::{
    default_world_dir as workspace_dir, run_workspace_eval, WorkspaceWorld, WsMode,
};
use hatch_sandbox::{HelperCmd, Limits, Sandbox, SandboxMode};

fn sandbox() -> Sandbox {
    match HelperCmd::locate() {
        Some(h) => Sandbox {
            mode: SandboxMode::Namespaces,
            helper: Some(h),
            limits: Limits::default(),
            max_output: 16 * 1024,
        },
        None => Sandbox {
            mode: SandboxMode::None,
            helper: None,
            limits: Limits::default(),
            max_output: 16 * 1024,
        },
    }
}

fn skipped() -> bool {
    HelperCmd::locate().is_some() && !hatch_sandbox::sandbox_works()
}

fn passed(checks: &[hatch_eval::Check], name: &str) -> bool {
    checks
        .iter()
        .find(|c| c.name == name)
        .map(|c| c.passed)
        .unwrap_or(false)
}

fn present(checks: &[hatch_eval::Check], name: &str) -> bool {
    checks.iter().any(|c| c.name == name)
}

#[test]
fn pipeline_sequence_checks_pass() {
    if skipped() {
        eprintln!("SKIP: sandbox helper is present but does not run on this host");
        return;
    }
    let world = DelegateWorld::load(&delegate_dir()).unwrap();
    let r = run_delegate_eval(&world, DgMode::Pipeline, sandbox()).unwrap();
    // 4 parent-order + 4 handoff-tree + 4 child-sequence checks.
    for id in ["split", "cancel", "depth", "injection"] {
        assert!(
            present(&r.checks, &format!("{id}：父代理工具调用顺序")),
            "{id}"
        );
        assert!(present(&r.checks, &format!("{id}：子代理树")), "{id}");
        assert!(
            present(&r.checks, &format!("{id}：子代理工具调用序列")),
            "{id}"
        );
    }
    let md = r.render_markdown();
    assert_eq!(r.passed(), r.checks.len(), "{md}");
    assert_eq!(r.checks.len(), 25, "{md}");

    let world = WorkspaceWorld::load(&workspace_dir()).unwrap();
    let r = run_workspace_eval(&world, WsMode::Pipeline, sandbox()).unwrap();
    for id in ["qa", "edit", "test", "injection"] {
        assert!(
            present(&r.checks, &format!("任务 {id} 的工具调用顺序")),
            "{id}"
        );
    }
    let md = r.render_markdown();
    assert_eq!(r.passed(), r.checks.len(), "{md}");
    assert_eq!(r.checks.len(), 15, "{md}");
}

#[test]
fn wrong_parent_sequence_fails_the_right_check() {
    if skipped() {
        return;
    }
    let mut world = DelegateWorld::load(&delegate_dir()).unwrap();
    world
        .spec
        .expected_parent_tools
        .insert("split".to_string(), vec!["delegate".to_string()]);
    let r = run_delegate_eval(&world, DgMode::Pipeline, sandbox()).unwrap();
    assert!(!passed(&r.checks, "split：父代理工具调用顺序"));
    assert!(passed(&r.checks, "split：子代理树"));
    assert!(passed(&r.checks, "split：子代理工具调用序列"));
    assert!(r.passed() < r.checks.len());
}

#[test]
fn wrong_child_sequence_fails_the_right_check() {
    if skipped() {
        return;
    }
    let mut world = DelegateWorld::load(&delegate_dir()).unwrap();
    world.spec.expected_runs.get_mut("injection").unwrap()[0]
        .tool_calls
        .push(ExpectedCall {
            name: "fs_read".to_string(),
            arguments: None,
        });
    let r = run_delegate_eval(&world, DgMode::Pipeline, sandbox()).unwrap();
    assert!(!passed(&r.checks, "injection：子代理工具调用序列"));
    assert!(passed(&r.checks, "injection：父代理工具调用顺序"));
    assert!(passed(&r.checks, "injection：子代理树"));
    assert!(r.passed() < r.checks.len());
}

#[test]
fn workspace_wrong_sequence_fails_the_right_check() {
    if skipped() {
        return;
    }
    let mut world = WorkspaceWorld::load(&workspace_dir()).unwrap();
    world
        .expectations
        .expected_tool_calls
        .insert("qa".to_string(), vec!["fs_edit".to_string()]);
    let r = run_workspace_eval(&world, WsMode::Pipeline, sandbox()).unwrap();
    assert!(!passed(&r.checks, "任务 qa 的工具调用顺序"));
    assert!(passed(&r.checks, "任务 qa 的回答正确"));
    assert!(r.passed() < r.checks.len());
}
