//! Cross-module install smoke test. Drives `Module::install` for every module
//! in the crate against a single `MockModuleContext`, then asserts the
//! aggregate set of registered hooks matches the plan.

use ironclaw_modules::{
    AgentToAgentModule, ApprovalsModule, InteractiveModule, Module, MountSecurityModule,
    PermissionsModule, SchedulingModule, SelfModModule, TypingConfig, TypingModule,
    context::MockModuleContext,
};
use std::sync::Arc;

#[tokio::test]
async fn every_module_installs_and_registers_expected_hooks() {
    let ctx = MockModuleContext::new();
    let modules: Vec<Box<dyn Module>> = vec![
        Box::new(TypingModule::new(TypingConfig::default())),
        Box::new(MountSecurityModule::default()),
        Box::new(PermissionsModule::deny_all()),
        Box::new(ApprovalsModule::new()),
        Box::new(InteractiveModule::default()),
        Box::new(SchedulingModule::default()),
        Box::new(AgentToAgentModule),
        Box::new(SelfModModule),
    ];
    for m in &modules {
        let ctx: Arc<dyn ironclaw_modules::ModuleContext> = ctx.clone();
        m.install(ctx).await.unwrap();
    }
    let regs = ctx.registered();
    // Each of the following hooks must be wired by at least one module.
    for hook in [
        "delivery_ready",       // typing
        "access_gate",          // permissions
        "sender_scope_gate",    // permissions + approvals
        "delivery_action",      // approvals + interactive
        "message_interceptor",  // agent_to_agent
    ] {
        assert!(
            regs.contains(&hook),
            "expected `{hook}` to be registered; got {regs:?}"
        );
    }
    // Approvals registers a delivery action named approval_card; interactive
    // registers ask_user_question + send_card.
    let mut actions = ctx.delivery_actions();
    actions.sort();
    assert_eq!(
        actions,
        vec!["approval_card", "ask_user_question", "schedule", "send_card"]
    );
    // Names are all unique and stable.
    let names: Vec<&'static str> = modules.iter().map(|m| m.name()).collect();
    let mut sorted = names.clone();
    sorted.sort_unstable();
    sorted.dedup();
    assert_eq!(sorted.len(), names.len());
}
