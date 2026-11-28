//! Small development worlds for the command and image input paths.

use crate::checks::{self, Csv, CsvMatch, Evidence, TextFiles, Verdict};
use crate::report::{Check, Usage};
use hatch_agent::{Agent, AgentConfig, RunOptions, SessionStore};
use hatch_core::{time, Error, Result};
use hatch_llm::scripted::FnModel;
use hatch_llm::{ChatResponse, LlmError, Role, Router, ToolCall, Workload};
use hatch_policy::{
    ActionClass, ApprovalQueue, ApprovalStatus, Autonomy, Gatekeeper, PolicyConfig,
};
use hatch_sandbox::{RunOutput, Sandbox, SandboxMode};
use hatch_store::Db;
use hatch_tools::builtin::shell::ShellRun;
use hatch_tools::builtin::web::ApprovalStatusTool;
use hatch_tools::executors::ExecContext;
use hatch_tools::ws_exec::ExecPayload;
use hatch_tools::{
    execute_approved, ActionExecutor, ExecCommandExecutor, ExecOutcome, ExecutorRegistry, Registry,
    Workspace,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, Mutex};

const SPEC_FILE: &str = "probe.json";
const IMAGE_TYPE: &str = "image/png";
const MAX_HANDOFFS: usize = 3;

#[derive(Deserialize)]
pub struct ProbeWorld {
    name: String,
    tasks: Vec<Task>,
    #[serde(skip)]
    dir: PathBuf,
}

#[derive(Deserialize)]
struct Task {
    id: String,
    prompt: String,
    #[serde(flatten)]
    case: Case,
}

#[derive(Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum Case {
    Compute {
        command: String,
        input: String,
        output: String,
        columns: Vec<String>,
        rows: Vec<Vec<String>>,
    },
    Vision {
        image: String,
        fields: BTreeMap<String, Value>,
    },
}

impl ProbeWorld {
    pub fn is_probe_world(dir: &Path) -> bool {
        dir.join(SPEC_FILE).is_file()
    }

    pub fn load(dir: &Path) -> Result<Self> {
        let mut world: Self = serde_json::from_slice(&std::fs::read(dir.join(SPEC_FILE))?)?;
        world.dir = dir.canonicalize()?;
        let [task] = world.tasks.as_slice() else {
            return Err(Error::Invalid(
                "probe world must have exactly one task".into(),
            ));
        };
        let valid = match &task.case {
            Case::Compute {
                command,
                input,
                output,
                columns,
                rows,
            } => {
                command_script(command).is_some()
                    && relative_file(input)
                    && relative_file(output)
                    && input != output
                    && !columns.is_empty()
                    && rows.iter().all(|row| row.len() == columns.len())
            }
            Case::Vision { image, fields } => {
                relative_file(image)
                    && !fields.is_empty()
                    && fields
                        .keys()
                        .all(|key| !key.is_empty() && !key.contains(['/', '~']))
            }
        };
        if !valid || task.prompt.trim().is_empty() || task.id.is_empty() {
            return Err(Error::Invalid(format!("invalid probe task {}", task.id)));
        }
        Ok(world)
    }

    pub fn needs_sandbox(&self) -> bool {
        self.tasks
            .iter()
            .any(|task| matches!(task.case, Case::Compute { .. }))
    }

    fn load_image(&self, file: &str) -> Result<String> {
        let path = self.dir.join(file).canonicalize()?;
        if !path.starts_with(&self.dir) {
            return Err(Error::Invalid("probe image is outside its fixture".into()));
        }
        let encoded: String = std::fs::read_to_string(path)?
            .chars()
            .filter(|ch| !ch.is_ascii_whitespace())
            .collect();
        hatch_core::image::validate_image(&encoded, IMAGE_TYPE).map_err(Error::Invalid)?;
        Ok(encoded)
    }
}

fn relative_file(path: &str) -> bool {
    !path.is_empty()
        && Path::new(path)
            .components()
            .all(|part| matches!(part, Component::Normal(_)))
}

// The shell still executes the original approved payload. Accept only two
// literal arguments with horizontal whitespace and an optional "./".
fn command_script(command: &str) -> Option<&str> {
    let mut words = command.split([' ', '\t']).filter(|word| !word.is_empty());
    if words.next()? != "python3" {
        return None;
    }
    let script = words.next()?;
    let script = script.strip_prefix("./").unwrap_or(script);
    (words.next().is_none()
        && relative_file(script)
        && script
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"/._-".contains(&byte))
        && !script.starts_with('-'))
    .then_some(script)
}

#[derive(Serialize)]
struct CommandApproval {
    command: Option<String>,
    status: &'static str,
    reason: String,
}

#[derive(Serialize)]
struct CommandResult {
    exit_code: Option<i32>,
    isolated: bool,
    timed_out: bool,
    cancelled: bool,
}

struct ObservedCommand {
    inner: ExecCommandExecutor,
    results: Mutex<Vec<CommandResult>>,
}

impl ActionExecutor for ObservedCommand {
    fn kind(&self) -> &'static str {
        self.inner.kind()
    }

    fn execute(&self, payload: &Value, ctx: &ExecContext) -> Result<String> {
        let (receipt, output) = self.inner.execute_with_output(payload, ctx)?;
        let RunOutput {
            exit_code,
            isolated,
            timed_out,
            cancelled,
            ..
        } = output;
        self.results
            .lock()
            .expect("command observation lock")
            .push(CommandResult {
                exit_code,
                isolated,
                timed_out,
                cancelled,
            });
        Ok(receipt)
    }

    fn verify(
        &self,
        payload: &Value,
        receipt: &str,
        ctx: &ExecContext,
    ) -> Result<hatch_tools::verification::EvidenceCheck> {
        self.inner.verify(payload, receipt, ctx)
    }
}

#[derive(Serialize)]
pub struct ProbeReport {
    world: String,
    mode: &'static str,
    model: String,
    tasks: Vec<Outcome>,
    pub checks: Vec<Check>,
    usage: Usage,
}

impl ProbeReport {
    pub fn render_markdown(&self) -> String {
        let mut text = format!("# {}\n\n{} · {}\n\n", self.world, self.mode, self.model);
        for check in &self.checks {
            text.push_str(&format!(
                "- {} {}（{}）\n",
                check.mark(),
                check.name,
                check.detail
            ));
        }
        text
    }
}

#[derive(Serialize)]
struct Outcome {
    id: String,
    stop: String,
    answer: String,
    tool_calls: Vec<String>,
    executed_commands: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    command_approvals: Vec<CommandApproval>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    command_results: Vec<CommandResult>,
    evidence: Value,
}

pub fn run_probe(
    world: &ProbeWorld,
    live: Option<&Router>,
    sandbox: Option<Sandbox>,
) -> Result<ProbeReport> {
    if world.needs_sandbox()
        && sandbox
            .as_ref()
            .is_none_or(|sandbox| sandbox.mode != SandboxMode::Namespaces)
    {
        return Err(Error::Invalid(
            "compute probe requires the namespace sandbox".into(),
        ));
    }
    let task = &world.tasks[0];
    let image = match &task.case {
        Case::Vision { image, .. } => Some(world.load_image(image)?),
        Case::Compute { .. } => None,
    };
    let oracle;
    let router = match live {
        Some(router) => router,
        None => {
            oracle = crate::recording::single(scripted_model(task, image.as_deref()));
            &oracle
        }
    };
    let (outcome, checks) = run_task(world, task, router, sandbox.as_ref(), image)?;
    let totals = router.totals();
    Ok(ProbeReport {
        world: world.name.clone(),
        mode: if live.is_some() { "live" } else { "pipeline" },
        model: router
            .chain(Workload::Main)
            .iter()
            .map(|model| model.name())
            .collect::<Vec<_>>()
            .join(" → "),
        tasks: vec![outcome],
        checks,
        usage: Usage {
            estimated: live.is_none(),
            max_calls: router.call_limit(),
            calls: totals.calls,
            prompt_tokens: totals.prompt_tokens,
            completion_tokens: totals.completion_tokens,
            cached_tokens: totals.cached_tokens,
            reasoning_tokens: totals.reasoning_tokens,
        },
    })
}

fn run_task(
    world: &ProbeWorld,
    task: &Task,
    router: &Router,
    sandbox: Option<&Sandbox>,
    image: Option<String>,
) -> Result<(Outcome, Vec<Check>)> {
    let scratch = tempfile::tempdir()?;
    let mut tools = Registry::new();
    let mut executors = ExecutorRegistry::new();
    let compute = match &task.case {
        Case::Compute {
            command,
            input,
            output,
            ..
        } => {
            let project = scratch.path().join("project");
            crate::faults::runner::copy_dir(&world.dir.join("project"), &project)?;
            let workspace = Workspace::new(&project)?;
            let before = TextFiles::capture(workspace.root());
            if !matches!(before.read(input), Evidence::Observed(_))
                || before.read(output) != Evidence::Absent
            {
                return Err(Error::Invalid(
                    "compute fixture requires a readable input and an absent output".into(),
                ));
            }
            tools = tools
                .with(Arc::new(ShellRun {
                    ws: workspace.clone(),
                    max_timeout_secs: 15,
                }))
                .with(Arc::new(ApprovalStatusTool));
            let observer = Arc::new(ObservedCommand {
                inner: ExecCommandExecutor {
                    sandbox: Arc::new(
                        sandbox
                            .expect("compute sandbox is checked before starting")
                            .clone(),
                    ),
                },
                results: Mutex::new(Vec::new()),
            });
            executors = executors.with(observer.clone());
            Some((command.as_str(), workspace, before, observer))
        }
        Case::Vision { .. } => None,
    };

    let db = Db::open_in_memory()?;
    let mut policy = PolicyConfig::default();
    policy.set(ActionClass::ExecCommand, Autonomy::Draft);
    let gate = Gatekeeper::new(policy);
    let agent = Agent {
        db: &db,
        router,
        tools: &tools,
        executors: &executors,
        gate: &gate,
        config: AgentConfig::default(),
    };
    let session = SessionStore::new(&db).create(&task.id, &time::now())?;
    let origin = format!("chat:{session}");
    let mut turn = agent.run(
        &session,
        &task.prompt,
        &time::now(),
        RunOptions {
            images: image
                .map(|data| vec![(data, IMAGE_TYPE.into())])
                .unwrap_or_default(),
            ..RunOptions::default()
        },
    )?;
    let mut tool_calls = turn.tool_calls.clone();
    let mut executed = Vec::new();
    let mut command_approvals = Vec::new();
    if let Some((allowed, workspace, _, _)) = &compute {
        let queue = ApprovalQueue::new(&db);
        let mut submitted = false;
        for _ in 0..MAX_HANDOFFS {
            let mut feedback = Vec::new();
            for approval in queue
                .list(Some(ApprovalStatus::Pending))?
                .into_iter()
                .filter(|approval| approval.origin == origin)
            {
                let payload = serde_json::from_value::<ExecPayload>(approval.payload.clone()).ok();
                let allowed_script = command_script(allowed).expect("validated probe command");
                let rejection = if submitted {
                    Some("本任务只执行一次转换命令，已有一次批准的执行尝试。".to_owned())
                } else if approval.kind != "exec_command"
                    || !payload.as_ref().is_some_and(|payload| {
                        payload.root == workspace.root()
                            && payload.cwd.is_empty()
                            && command_script(&payload.command) == Some(allowed_script)
                    })
                {
                    Some(format!(
                        "请在工作区根目录单独运行 `python3 {allowed_script}`，脚本路径可加 ./。"
                    ))
                } else {
                    None
                };
                let now = time::now();
                queue.decide(
                    &approval.id,
                    rejection.is_none(),
                    rejection.as_deref().unwrap_or("probe task approval"),
                    &now,
                )?;
                let (decision, status) = if let Some(reason) = rejection {
                    ("rejected", format!("已拒绝。{reason}"))
                } else {
                    submitted = true;
                    match execute_approved(&db, &executors, &approval.id, &now)? {
                        ExecOutcome::Executed(receipt) => {
                            executed.push(receipt);
                            ("executed", "已执行，请检查回执".into())
                        }
                        ExecOutcome::Failed(reason) | ExecOutcome::Ambiguous(reason) => (
                            "unconfirmed",
                            format!("执行没有确认成功，请如实记录：{reason}"),
                        ),
                        ExecOutcome::AlreadyDone(_) => {
                            ("duplicate", "此操作已执行，未重复执行。".into())
                        }
                    }
                };
                command_approvals.push(CommandApproval {
                    command: payload.map(|payload| hatch_core::secrets::redact(&payload.command)),
                    status: decision,
                    reason: status.clone(),
                });
                feedback.push(format!("- {}（{}）：{status}", approval.id, approval.title));
            }
            if feedback.is_empty() {
                break;
            }
            let input = format!("审批结果：\n{}\n请核对回执后继续。", feedback.join("\n"));
            turn = agent.run_turn(&session, &input, &time::now())?;
            tool_calls.extend(turn.tool_calls.iter().cloned());
        }
    }
    let answer = turn.answer.unwrap_or_default();
    let mut command_results = Vec::new();
    let (checks, evidence) = match (&task.case, compute) {
        (
            Case::Compute {
                input,
                output,
                columns,
                rows,
                ..
            },
            Some((_, workspace, before, observer)),
        ) => {
            command_results =
                std::mem::take(&mut *observer.results.lock().expect("command observation lock"));
            score_compute(
                &before,
                workspace.root(),
                input,
                output,
                columns,
                rows,
                &command_results,
            )
        }
        (Case::Vision { fields, .. }, _) => score_vision(&answer, fields),
        _ => unreachable!("compute task is prepared before the agent runs"),
    };
    Ok((
        Outcome {
            id: task.id.clone(),
            stop: format!("{:?}", turn.stop),
            answer,
            tool_calls,
            executed_commands: executed,
            command_approvals,
            command_results,
            evidence,
        },
        checks,
    ))
}

fn score_compute(
    before: &TextFiles,
    root: &Path,
    input: &str,
    output: &str,
    columns: &[String],
    rows: &[Vec<String>],
    executions: &[CommandResult],
) -> (Vec<Check>, Value) {
    let after = TextFiles::capture(root);
    let content = after.read(output);
    let parsed = match content {
        Evidence::Observed(content) => {
            Csv::parse(content).map_or(Evidence::Invalid, Evidence::Observed)
        }
        Evidence::Absent => Evidence::Absent,
        Evidence::Invalid => Evidence::Invalid,
        Evidence::Unavailable => Evidence::Unavailable,
    };
    let data = match &parsed {
        Evidence::Observed(data) => Evidence::Observed(data),
        Evidence::Absent => Evidence::Absent,
        Evidence::Invalid => Evidence::Invalid,
        Evidence::Unavailable => Evidence::Unavailable,
    };
    let columns: Vec<_> = columns.iter().map(String::as_str).collect();
    let mut table = checks::csv(data, CsvMatch::Columns(&columns))
        .and(checks::csv(data, CsvMatch::Rows(rows.len())));
    for (row, cells) in rows.iter().enumerate() {
        for (column, value) in cells.iter().enumerate() {
            table = table.and(checks::csv(data, CsvMatch::Cell { row, column, value }));
        }
    }
    let preserved = match (before.read(input), after.read(input)) {
        (Evidence::Observed(left), Evidence::Observed(right)) => Verdict::from(left == right),
        (Evidence::Unavailable, _) | (_, Evidence::Unavailable) => Verdict::InsufficientEvidence,
        _ => Verdict::Fail,
    };
    let exited = Verdict::from(matches!(
        executions,
        [CommandResult {
            exit_code: Some(0),
            isolated: true,
            timed_out: false,
            cancelled: false
        }]
    ));
    (
        vec![
            exited.report(
                "approved transform executed once",
                format!("{} observed command result(s)", executions.len()),
            ),
            table.report("output table matches independent reference", output),
            preserved.report("original table unchanged", input),
        ],
        json!({"output_path": output, "output": match content { Evidence::Observed(s) => Some(s), _ => None }}),
    )
}

fn score_vision(answer: &str, fields: &BTreeMap<String, Value>) -> (Vec<Check>, Value) {
    let parsed = hatch_llm::json_extract::extract_json(answer).filter(Value::is_object);
    let evidence = parsed
        .as_ref()
        .map_or(Evidence::Invalid, Evidence::Observed);
    let checks = fields
        .iter()
        .map(|(name, expected)| {
            evidence
                .evaluate(|answer| {
                    answer.get(name).is_some_and(|actual| {
                        match (actual.as_str(), expected.as_str()) {
                            (Some(actual), Some(expected)) if name == "source_region" => {
                                let actual = actual.trim().to_ascii_lowercase();
                                let value = match actual.as_str() {
                                    "top-left" => "upper-left",
                                    "top-right" => "upper-right",
                                    "bottom-left" => "lower-left",
                                    "bottom-right" => "lower-right",
                                    other => other,
                                };
                                value.eq_ignore_ascii_case(expected.trim())
                            }
                            (Some(actual), Some(expected))
                                if matches!(name.as_str(), "station_code" | "calibration_code") =>
                            {
                                actual.trim().eq_ignore_ascii_case(expected.trim())
                            }
                            _ => actual == expected,
                        }
                    })
                })
                .report(
                    &format!("image field {name}"),
                    "structured answer checked against the image reference",
                )
        })
        .collect();
    (checks, json!({"parsed_answer": parsed}))
}

fn scripted_model(task: &Task, image: Option<&str>) -> Arc<FnModel> {
    let compute = match &task.case {
        Case::Compute { command, .. } => Some(command.clone()),
        Case::Vision { .. } => None,
    };
    let vision = match &task.case {
        Case::Vision { fields, .. } => {
            image.map(|image| (image.to_owned(), json!(fields).to_string()))
        }
        Case::Compute { .. } => None,
    };
    Arc::new(FnModel::new("probe-oracle", move |request| {
        let call = |name: &str, arguments| {
            ChatResponse::calls(vec![ToolCall {
                id: format!("probe-{name}"),
                name: name.into(),
                arguments,
            }])
        };
        if let Some(command) = &compute {
            let since_user: Vec<_> = request
                .messages
                .iter()
                .rev()
                .take_while(|m| !crate::oracle::is_user_turn(m))
                .collect();
            let has_tool_result = since_user.iter().any(|m| m.role == Role::Tool);
            if let Some(id) = crate::oracle::approval_id(&request.messages) {
                return Ok(if has_tool_result {
                    ChatResponse::text("已核对命令回执。")
                } else {
                    call("approval_status", json!({"id": id}))
                });
            }
            return Ok(if has_tool_result {
                ChatResponse::text("等待审批。")
            } else {
                call("shell_run", json!({"command": command}))
            });
        }
        if let Some((expected_image, answer)) = &vision {
            let present = request
                .messages
                .iter()
                .filter(|m| m.role == Role::User)
                .flat_map(|m| m.images())
                .any(|(data, media)| data == expected_image && media == IMAGE_TYPE);
            if present {
                return Ok(ChatResponse::text(answer));
            }
        }
        Err(LlmError::permanent("probe oracle expected a user image"))
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vision_oracle_requires_the_actual_image_and_checks_each_field() {
        let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../eval/worlds/vision-probe");
        let world = ProbeWorld::load(&fixture).unwrap();
        let report = run_probe(&world, None, None).unwrap();
        assert!(report.checks.iter().all(|check| check.passed));

        let task = &world.tasks[0];
        let Case::Vision { image, fields } = &task.case else {
            panic!("expected the vision fixture");
        };
        let expected_image = world.load_image(image).unwrap();
        let router = crate::recording::single(scripted_model(task, Some(&expected_image)));
        let (_, checks) = run_task(&world, task, &router, None, None).unwrap();
        assert!(checks.iter().all(|check| !check.passed));

        let mut incorrect = fields.clone();
        incorrect.insert("calibration_code".into(), json!("guessed"));
        let (checks, _) = score_vision(&json!(incorrect).to_string(), fields);
        let failed: Vec<_> = checks.iter().filter(|check| !check.passed).collect();
        assert_eq!(failed.len(), 1);
        assert_eq!(failed[0].name, "image field calibration_code");
    }

    #[test]
    fn text_only_compass_guesses_earn_no_credit() {
        let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../eval/worlds/vision-probe");
        let world = ProbeWorld::load(&fixture).unwrap();
        let task = &world.tasks[0];
        for side in ["left", "right"] {
            let router = Router::single(Arc::new(FnModel::new("text-only", move |request| {
                let prompt = request
                    .messages
                    .iter()
                    .find(|message| crate::oracle::is_user_turn(message))
                    .expect("user prompt");
                assert!(!prompt.has_images(), "control must receive text only");
                // A text-only model can infer a compass half from the label,
                // but has no clue which side of that half contains the panel.
                let text = prompt.text_content();
                assert!(text.contains("SOUTH 面板"), "expected a south-facing guess");
                let region = format!("lower-{side}");
                assert!(text.contains(&region), "expected a valid position choice");
                Ok(ChatResponse::text(
                    json!({
                        "station_code": null,
                        "source_region": region,
                        "calibration_code": null,
                    })
                    .to_string(),
                ))
            })));
            let (task_result, checks) = run_task(&world, task, &router, None, None).unwrap();
            assert_eq!(task_result.stop, "Answered");
            assert_eq!(
                task_result.evidence["parsed_answer"]["source_region"],
                format!("lower-{side}")
            );
            assert_eq!(checks.len(), 3);
            assert!(checks
                .iter()
                .all(|check| !check.passed && !check.insufficient_evidence));
        }
    }

    #[test]
    fn visual_code_and_region_spellings_are_normalized() {
        let fields = BTreeMap::from([
            ("station_code".into(), json!("VP-42")),
            ("calibration_code".into(), json!("CAL-15")),
            ("source_region".into(), json!("lower-right")),
        ]);
        let (checks, _) = score_vision(
            r#"{"station_code":" vp-42 ","calibration_code":"cal-15","source_region":"Bottom-Right"}"#,
            &fields,
        );
        assert!(checks.iter().all(|check| check.passed));
    }

    #[test]
    fn approval_accepts_literal_script_paths_and_rejects_compound_commands() {
        for command in [
            "python3 normalize.py",
            "python3 ./normalize.py",
            "python3\t./normalize.py",
        ] {
            assert_eq!(command_script(command), Some("normalize.py"));
        }
        for command in [
            "python normalize.py",
            "python3 normalize.py && cat totals.csv",
            "python3\nnormalize.py",
            "python3 ../normalize.py",
            "python3 -c",
        ] {
            assert_eq!(command_script(command), None, "{command:?}");
        }
    }

    #[test]
    fn command_receipt_cannot_pass_with_wrong_output_or_modified_input() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path();
        std::fs::write(root.join("input.csv"), "source").unwrap();
        let before = TextFiles::capture(root);
        std::fs::write(root.join("input.csv"), "modified").unwrap();
        std::fs::write(root.join("output.csv"), "value\nincorrect\n").unwrap();
        let (checks, _) = score_compute(
            &before,
            root,
            "input.csv",
            "output.csv",
            &["value".into()],
            &[vec!["expected".into()]],
            &[CommandResult {
                exit_code: Some(0),
                isolated: true,
                timed_out: false,
                cancelled: false,
            }],
        );
        assert_eq!(
            checks.iter().map(|check| check.passed).collect::<Vec<_>>(),
            vec![true, false, false]
        );
    }
}
