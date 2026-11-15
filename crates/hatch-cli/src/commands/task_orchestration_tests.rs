//! Unit tests for `task_orchestration`: goal features drive the
//! router to exactly one mode per goal shape.

#[cfg(test)]
mod tests {
    use crate::commands::task_orchestration::{features_for, parse_plan_draft, SubmitRequest, SubtaskRequest};
use serde_json::json;
use crate::commands::task_routing::{route_goal, ExecutionMode};

    #[test]
    fn features_come_from_request_structure_not_free_text() {
        let req = SubmitRequest {
            goal: "给我准备明天和李婷的会".into(),
            kind: Some("meeting-brief".into()),
            ..Default::default()
        };
        let f = features_for(&req);
        assert!(f.steps_known && !f.multi_stage && !f.single_observation_driven);
        assert_eq!(route_goal(&f), ExecutionMode::FixedFlow);
        // A URL in the goal marks it observation-driven.
        let req = SubmitRequest {
            goal: "打开 https://example.com/page 看看规则".into(),
            observe: false,
            ..Default::default()
        };
        assert!(features_for(&req).single_observation_driven);
        // Subtasks mean parallelizable + multi-stage.
        let req = SubmitRequest {
            goal: "研究三个候选".into(),
            subtasks: vec![SubtaskRequest {
                title: "候选一".into(),
                kind: "meeting-brief".into(),
                budget: 10,
            }],
            ..Default::default()
        };
        let f = features_for(&req);
        assert!(f.independently_parallelizable && f.multi_stage && f.has_subtasks);
        assert_eq!(route_goal(&f), ExecutionMode::DurableSubtasks);
    }

    #[test]
    fn plan_draft_validation_is_structural() {
        let ok = json!([{"id": "s1", "title": "查日程", "tool": "calendar_list", "arguments": {}, "expected": "有会"}]);
        assert_eq!(parse_plan_draft(&ok).unwrap().len(), 1);
        assert!(parse_plan_draft(&json!([])).is_err());
        assert!(parse_plan_draft(&json!([{"id": "s1", "title": "x", "tool": "", "expected": "y"}])).is_err());
        let dup = json!([
            {"id": "s1", "title": "a", "tool": "calendar_list", "expected": "x"},
            {"id": "s1", "title": "b", "tool": "mail_search", "expected": "y"}
        ]);
        assert!(parse_plan_draft(&dup).is_err());
    }
}
