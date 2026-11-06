//! BFCL 题目函数的桩工具：不做真事，只记录调用、返回固定成功。
//!
//! 调用本身由 agent 记进会话 transcript（runner 从那里取参数评分），
//! 所以桩本身是无状态的。审批走 workspace harness 的 decide() 模式：
//! 桩实现 `to_approval`，策略把它的类设为 Draft，调用先进审批队列，
//! harness 自动批准并由 [`BfclStubExecutor`] 执行（返回 `{"ok": true}`）。

use super::dataset::{to_underscored, BfclFunction};
use chrono::Duration;
use hatch_core::ids;
use hatch_core::Result;
use hatch_llm::ToolSpec;
use hatch_policy::{ActionClass, NewApproval};
use hatch_tools::{ActionExecutor, ExecContext, Tool, ToolContext, ToolOutput};
use serde_json::Value;

/// 审批 kind：桩调用与桩执行器共用。
pub const BFCL_STUB_KIND: &str = "bfcl_stub";

/// 桩的固定返回值（单轮评分只看调用本身，返回值不影响分）。
pub const STUB_OK: &str = "{\"ok\": true}";

/// 一道 BFCL 函数对应的桩工具。spec 的名字是下划线化的
/// （OpenAI 兼容接口不允许函数名带点），原始点名保留在 `dotted_name`。
pub struct BfclStub {
    spec: ToolSpec,
    dotted_name: String,
}

impl BfclStub {
    pub fn new(func: &BfclFunction) -> Self {
        Self {
            spec: ToolSpec {
                name: to_underscored(&func.name),
                description: func.description.clone(),
                parameters: func.parameters.clone(),
            },
            dotted_name: func.name.clone(),
        }
    }

    pub fn dotted_name(&self) -> &str {
        &self.dotted_name
    }
}

impl Tool for BfclStub {
    fn spec(&self) -> ToolSpec {
        self.spec.clone()
    }

    /// 桩没有副作用：不读用户数据、不写任何东西、不发任何请求。
    /// 用 ReadPersonal 是因为它的自主度上限是 Silent（可逆类），
    /// 策略可以把它压到 Draft 走审批队列；这里并不真的读数据。
    fn class(&self) -> ActionClass {
        ActionClass::ReadPersonal
    }

    fn call(&self, _args: &Value, _ctx: &ToolContext) -> Result<ToolOutput> {
        Ok(ToolOutput::system(STUB_OK))
    }

    fn to_approval(&self, args: &Value, ctx: &ToolContext) -> Result<Option<NewApproval>> {
        let payload = args.clone();
        Ok(Some(NewApproval {
            idempotency_key: ids::stable_id(
                "bfcl_stub",
                &[&ctx.origin, &ctx.call_id, &payload.to_string()],
            ),
            kind: BFCL_STUB_KIND.into(),
            class: ActionClass::ReadPersonal,
            title: format!("BFCL 桩调用：{}", self.spec.name),
            preview: payload.to_string(),
            payload,
            reasons: vec![],
            origin: ctx.origin.clone(),
            ttl: Duration::hours(72),
        }))
    }
}

/// 批准后的桩执行器：什么都不做，返回固定成功。
pub struct BfclStubExecutor;

impl ActionExecutor for BfclStubExecutor {
    fn kind(&self) -> &'static str {
        BFCL_STUB_KIND
    }

    fn execute(&self, _payload: &Value, _ctx: &ExecContext) -> Result<String> {
        Ok(STUB_OK.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hatch_core::time;
    use hatch_core::CancelToken;
    use hatch_tools::Tool;
    use serde_json::json;

    fn func() -> BfclFunction {
        BfclFunction {
            name: "triangle_properties.get".into(),
            description: "Get props.".into(),
            parameters: json!({"type": "object", "properties": {"a": {"type": "number"}}}),
        }
    }

    fn ctx() -> ToolContext<'static> {
        // Db 借用：用泄漏的内存库给一个 'static 引用，测试内只读 origin/call_id。
        let db: &'static hatch_store::Db =
            Box::leak(Box::new(hatch_store::Db::open_in_memory().unwrap()));
        ToolContext {
            db,
            now: time::now(),
            tz: chrono_tz::Asia::Shanghai,
            user_email: "bfcl-eval@localhost".into(),
            origin: "chat:s1".into(),
            tainted: false,
            call_id: "c1".into(),
            cancel: CancelToken::new(),
        }
    }

    #[test]
    fn spec_underscores_the_name_and_keeps_the_schema() {
        let stub = BfclStub::new(&func());
        let spec = stub.spec();
        assert_eq!(spec.name, "triangle_properties_get");
        assert_eq!(stub.dotted_name(), "triangle_properties.get");
        assert_eq!(spec.description, "Get props.");
        assert_eq!(spec.parameters["type"], "object");
        assert_eq!(stub.class(), ActionClass::ReadPersonal);
    }

    #[test]
    fn call_returns_fixed_success() {
        let stub = BfclStub::new(&func());
        let out = stub.call(&json!({"a": 5}), &ctx()).unwrap();
        assert_eq!(out.content, STUB_OK);
    }

    #[test]
    fn to_approval_queues_a_stub_approval() {
        let stub = BfclStub::new(&func());
        let approval = stub.to_approval(&json!({"a": 5}), &ctx()).unwrap().unwrap();
        assert_eq!(approval.kind, BFCL_STUB_KIND);
        assert_eq!(approval.class, ActionClass::ReadPersonal);
        assert_eq!(approval.origin, "chat:s1");
        assert_eq!(approval.payload, json!({"a": 5}));
    }

    #[test]
    fn executor_returns_fixed_success() {
        let exec = BfclStubExecutor;
        assert_eq!(exec.kind(), BFCL_STUB_KIND);
        let db = hatch_store::Db::open_in_memory().unwrap();
        let out = exec
            .execute(
                &json!({"a": 5}),
                &ExecContext {
                    db: &db,
                    now: time::now(),
                    approval_id: "ap1",
                    origin: "chat:s1",
                    cancel: None,
                },
            )
            .unwrap();
        assert_eq!(out, STUB_OK);
    }
}
