use a3s_box_core::{resolve_execution, BoxConfig, ResolvedExecutionPlan};

const LEGACY_PLAN_FIXTURE: &str = include_str!("fixtures/execution/no-policy-plan.json");

#[test]
fn omitted_policy_preserves_the_legacy_resolved_plan_fixture() {
    let expected: serde_json::Value = serde_json::from_str(LEGACY_PLAN_FIXTURE).unwrap();
    let plan = resolve_execution(&BoxConfig::default()).unwrap();

    assert_eq!(serde_json::to_value(&plan).unwrap(), expected);
    assert!(serde_json::to_value(BoxConfig::default())
        .unwrap()
        .get("security_policy")
        .is_none());

    let decoded: ResolvedExecutionPlan = serde_json::from_value(expected).unwrap();
    assert_eq!(decoded, plan);
}
