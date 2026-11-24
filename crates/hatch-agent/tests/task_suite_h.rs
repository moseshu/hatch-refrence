//! Task suite expansion, batch 3 (spec: task-suite-spec-2026-11-23.md):
//! domain generalization — code & development. Scripted models replay
//! fixed decisions; acceptance reads resource state (fixed file,
//! script, real command output), never the agent's own report.
//!
//! Each test documents its four acceptance fields in the header
//! comment: (1) final resource state, (2) allowed side effects,
//! (3) state the answer must name, (4) step/call budget.
//!
//! Shell commands run through the real ExecCommandExecutor with a
//! no-namespace sandbox (workspace-only, no network) under an
//! ExecCommand grant, so the test run is genuine, not a stub.

#[path = "common/suite.rs"]
mod suite;

use hatch_agent::AgentConfig;
use hatch_llm::scripted::ScriptedModel;
use hatch_llm::{ChatResponse, Router, ToolCall};
use hatch_policy::{ActionClass, Autonomy, PolicyConfig};
use serde_json::json;
use std::sync::Arc;
use suite::Suite;

fn call(id: &str, name: &str, args: serde_json::Value) -> ToolCall {
    ToolCall {
        id: id.into(),
        name: name.into(),
        arguments: args,
    }
}

fn answer(r: &hatch_agent::TurnReport) -> &str {
    r.answer.as_deref().unwrap_or("")
}

fn shell_suite() -> Suite {
    let mut p = PolicyConfig::default();
    p.set(ActionClass::WriteFile, Autonomy::ActNotify);
    let s = Suite::with_policy(p).with_shell();
    // Grant patterns are command prefixes; a `-c` one-liner with
    // `;` never matches, so verification goes through a script file
    // and one plain `python3 <file>` call.
    s.seed_grant(ActionClass::ExecCommand, &["python3 *"]);
    s
}

// ---------------- category 2: cross-tool execution (code) ----------------

#[test]
fn code_fix_bug_from_log_and_run_tests() {
    // (1) src/calc.py guards the empty list (the ZeroDivisionError
    //     from logs/error.log) and the verification command exits
    //     with ALL OK; (2) two file writes + one command, zero
    //     sends/events, error log unchanged; (3) the answer names
    //     the bug line, the fix, and the test result; (4) two turns
    //     (fix, then run the check).
    let s = shell_suite();
    s.write_fixture(
        "src/calc.py",
        "def avg(xs):\n    return sum(xs) / len(xs)\n",
    );
    s.write_fixture(
        "logs/error.log",
        "Traceback (most recent call last):\n  File \"src/calc.py\", line 2, in avg\nZeroDivisionError: division by zero\n",
    );
    let log_before = std::fs::read_to_string(s.ws_root.join("logs/error.log")).unwrap();
    let model = Arc::new(ScriptedModel::ok(
        "m",
        vec![
            ChatResponse::calls(vec![
                call("1", "fs_read", json!({"path": "logs/error.log"})),
                call("2", "fs_read", json!({"path": "src/calc.py"})),
            ]),
            ChatResponse::calls(vec![call(
                "3",
                "fs_write",
                json!({"path": "src/calc.py", "content": "def avg(xs):\n    if not xs:\n        return 0\n    return sum(xs) / len(xs)\n"}),
            )]),
            ChatResponse::calls(vec![call(
                "4",
                "fs_write",
                json!({"path": "tests/check_calc.py", "content": "import sys\nsys.path.insert(0, 'src')\nfrom calc import avg\nassert avg([]) == 0\nassert avg([2, 4]) == 3\nprint('ALL OK')\n"}),
            )]),
            ChatResponse::text("已修好 src/calc.py，验证脚本已写好，待运行。"),
            ChatResponse::text("已修好 src/calc.py，验证脚本已写好，待运行。"),
        ],
    ));
    let router = Router::single(model);
    let (_r, _sess) = s.run(
        &router,
        AgentConfig::default(),
        "按 logs/error.log 定位 bug，修掉并跑通验证",
    );
    // Turn 2 runs the check as its first action in a FRESH session:
    // taint is per-session (batch taint), so a turn in the session
    // that read the logs would queue the command; a new session is
    // untainted and the ExecCommand grant executes it in-turn.
    let model = Arc::new(ScriptedModel::ok(
        "m",
        vec![
            ChatResponse::calls(vec![call(
                "5",
                "shell_run",
                json!({"command": "python3 tests/check_calc.py"}),
            )]),
            ChatResponse::text("Bug 在 src/calc.py 第 2 行：空列表触发 ZeroDivisionError。已加空列表保护返回 0，验证命令输出 ALL OK。"),
            ChatResponse::text("Bug 在 src/calc.py 第 2 行：空列表触发 ZeroDivisionError。已加空列表保护返回 0，验证命令输出 ALL OK。"),
        ],
    ));
    let (r, sess) = s.run(&Router::single(model), AgentConfig::default(), "运行验证");
    let fixed = &s.files()["src/calc.py"];
    assert!(fixed.contains("if not xs"), "{fixed}");
    assert!(fixed.contains("return 0"), "{fixed}");
    let output = s.tool_result(&sess, "5");
    assert!(output.contains("ALL OK"), "{output}");
    assert_eq!(
        std::fs::read_to_string(s.ws_root.join("logs/error.log")).unwrap(),
        log_before,
        "error log untouched"
    );
    assert_eq!(s.sent_count(), 0);
    assert_eq!(s.events().len(), 0);
    let a = answer(&r);
    assert!(a.contains("第 2 行"), "{a}");
    assert!(a.contains("ALL OK"), "{a}");
}

#[test]
fn code_write_script_and_run_it() {
    // (1) scripts/parse_logs.py exists and really runs: its output
    //     (read back from the tool result) reports ERROR: 3, and
    //     reports/log-summary.md records the same count; (2) two
    //     file writes + one command, zero sends/events, app log
    //     unchanged; (3) the answer names the count and the report;
    //     (4) two turns (write the script, then run and record).
    let s = shell_suite();
    s.write_fixture(
        "logs/app.log",
        "10:01 INFO 启动完成\n10:02 ERROR 连接超时\n10:05 INFO 请求处理\n10:07 ERROR 磁盘写失败\n10:09 WARN 内存偏高\n10:11 ERROR 鉴权失败\n",
    );
    let log_before = std::fs::read_to_string(s.ws_root.join("logs/app.log")).unwrap();
    let model = Arc::new(ScriptedModel::ok(
        "m",
        vec![
            ChatResponse::calls(vec![call("1", "fs_read", json!({"path": "logs/app.log"}))]),
            ChatResponse::calls(vec![call(
                "2",
                "fs_write",
                json!({"path": "scripts/parse_logs.py", "content": "import sys\nfrom collections import Counter\ncounts = Counter()\nfor line in open(sys.argv[1]):\n    parts = line.split()\n    if len(parts) >= 2:\n        counts[parts[1]] += 1\nfor level in (\"ERROR\", \"WARN\", \"INFO\"):\n    print(f\"{level}: {counts[level]}\")\n"}),
            )]),
            ChatResponse::text("脚本已写好，待运行。"),
            ChatResponse::text("脚本已写好，待运行。"),
        ],
    ));
    let router = Router::single(model);
    let (_r, _sess) = s.run(
        &router,
        AgentConfig::default(),
        "写个脚本统计 logs/app.log 各级别行数，跑一遍，把结果写进汇总",
    );
    // Turn 2 runs the script as its first action in a FRESH session
    // (taint is per-session; see the check task above), and the
    // model then records the real output.
    let model = Arc::new(ScriptedModel::ok(
        "m",
        vec![
            ChatResponse::calls(vec![call(
                "3",
                "shell_run",
                json!({"command": "python3 scripts/parse_logs.py logs/app.log"}),
            )]),
            ChatResponse::calls(vec![call(
                "4",
                "fs_write",
                json!({"path": "reports/log-summary.md", "content": "日志汇总\n- ERROR: 3\n- WARN: 1\n- INFO: 2\n"}),
            )]),
            ChatResponse::text("已写好解析脚本并跑过：ERROR: 3，WARN: 1，INFO: 2，结果记在 reports/log-summary.md。"),
            ChatResponse::text("已写好解析脚本并跑过：ERROR: 3，WARN: 1，INFO: 2，结果记在 reports/log-summary.md。"),
        ],
    ));
    let (r, sess) = s.run(
        &Router::single(model),
        AgentConfig::default(),
        "运行脚本，把结果写进汇总",
    );
    assert!(s.files().contains_key("scripts/parse_logs.py"));
    let output = s.tool_result(&sess, "3");
    assert!(output.contains("ERROR: 3"), "{output}");
    assert!(output.contains("WARN: 1"), "{output}");
    let report = &s.files()["reports/log-summary.md"];
    assert!(report.contains("ERROR: 3"), "{report}");
    assert_eq!(
        std::fs::read_to_string(s.ws_root.join("logs/app.log")).unwrap(),
        log_before,
        "app log untouched"
    );
    assert_eq!(s.sent_count(), 0);
    assert_eq!(s.events().len(), 0);
    let a = answer(&r);
    assert!(a.contains("ERROR: 3"), "{a}");
    assert!(a.contains("log-summary.md"), "{a}");
}
