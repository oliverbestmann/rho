//! A read-only "plan" profile, like Claude Code's plan mode. The user picks
//! it in the profile selector, or the model enters it with `EnterPlanMode`.
//! In plan mode the model cannot write or edit files and the sandbox is
//! read-only. `PresentPlan` shows a plan to the user in any profile and asks
//! for approval on the tool call; an approved plan leaves plan mode.

use std::sync::Arc;

use anyhow::{Result, bail};
use rho::extension::{
    Context, Decision, Extension, Profile, ProfileId, SessionSetup, ToolCall, ToolOutput,
};
use rho::provider::{ToolName, ToolSpec};
use rho::tools::{Tool, input_schema, parse};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::Value;

pub const PLAN: ProfileId = ProfileId::from_static("plan");
const ENTER: ToolName = ToolName::from_static("EnterPlanMode");
const PRESENT: ToolName = ToolName::from_static("PresentPlan");

const INSTRUCTIONS: &str = "You are in plan mode. Explore the code (Read, Bash, Glob, Grep, ...) and ask the user questions, \
but do not change anything: no file edits, no commits, no commands with side effects. \
Work out a concrete implementation plan. When it is ready, call PresentPlan with the plan; \
the user has to approve it before you can make changes.";

pub struct PlanMode;

impl Extension for PlanMode {
    async fn session_start(&self, session: &mut SessionSetup) {
        session.profiles.push(Profile {
            id: PLAN,
            name: "Plan".into(),
            description: "Read-only: explore and plan, no changes".into(),
            hidden_tools: vec![rho::tools::WRITE, rho::tools::EDIT, ENTER],
            read_only: true,
            instructions: Some(INSTRUCTIONS.into()),
        });
        session.tools.push(Arc::new(EnterPlanMode));
        session.tools.push(Arc::new(PresentPlan));
    }

    /// The user approves the plan here, where the call id is known, so the
    /// question appears on the tool call that shows the plan.
    async fn tool_call(&self, ctx: &Context, call: &mut ToolCall) -> Decision {
        if call.name != PRESENT {
            return Decision::Allow;
        }
        if ctx
            .confirm_with(
                Some(&call.id),
                "Approve this plan and start implementing?",
                "Approve",
                "Skip, let me add something",
            )
            .await
        {
            return Decision::Allow;
        }
        ctx.stop("Plan not approved; tell the agent what to change.");
        Decision::Block(
            "The user did not approve the plan. Wait for their feedback; do not change anything yet."
                .into(),
        )
    }
}

#[derive(Deserialize, JsonSchema)]
struct EnterInput {}

struct EnterPlanMode;

impl Tool for EnterPlanMode {
    fn kind(&self) -> rho::tools::ToolKind {
        rho::tools::ToolKind::SwitchMode
    }

    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: ENTER,
            description: "Switch to plan mode before a non-trivial change: when there are several ways to do it, \
                it touches many files, or the requirements are unclear. In plan mode you explore read-only and \
                design the approach; the user approves the plan before you change anything."
                .into(),
            input_schema: input_schema::<EnterInput>(),
        }
    }

    async fn run(&self, _input: Value, ctx: &Context) -> Result<ToolOutput> {
        ctx.set_profile(&PLAN)?;
        Ok("Entered plan mode.".into())
    }
}

#[derive(Deserialize, JsonSchema)]
struct PresentInput {
    /// The implementation plan as markdown, shown to the user for approval.
    plan: String,
}

struct PresentPlan;

impl Tool for PresentPlan {
    /// It only asks; the answer takes effect after it.
    fn repeatable(&self) -> bool {
        true
    }

    fn kind(&self) -> rho::tools::ToolKind {
        rho::tools::ToolKind::SwitchMode
    }

    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: PRESENT,
            description: "Present an implementation plan to the user for approval. \
                In plan mode, an approved plan switches back to making changes. \
                Only for plans of changes to make; not for research questions."
                .into(),
            input_schema: input_schema::<PresentInput>(),
        }
    }

    fn describe(&self, input: &Value) -> Option<String> {
        Some(input["plan"].as_str()?.to_string())
    }

    /// Runs only after `PlanMode::tool_call` got the approval.
    async fn run(&self, input: Value, ctx: &Context) -> Result<ToolOutput> {
        let i: PresentInput = parse(input)?;
        if i.plan.trim().is_empty() {
            bail!("the plan is empty");
        }
        if ctx.profile().id == PLAN {
            ctx.set_profile(&Profile::WRITE)?;
        }
        Ok("The user approved the plan. Start implementing it.".into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rho::agent::AgentEvent;
    use rho::extension::{BoxFuture, Frontend};
    use rho::provider::ToolCallId;
    use rho::store::SessionId;
    use serde_json::json;
    use std::sync::Mutex;

    /// Answers confirmations with `approve` and records the call ids.
    struct Confirms {
        approve: bool,
        asked: Mutex<Vec<Option<String>>>,
    }

    impl Frontend for Confirms {
        fn event(&self, _event: AgentEvent<'_>) {}

        fn confirm<'a>(
            &'a self,
            id: Option<&'a ToolCallId>,
            _title: &'a str,
        ) -> BoxFuture<'a, bool> {
            self.asked
                .lock()
                .unwrap()
                .push(id.map(ToolCallId::to_string));
            let approve = self.approve;
            Box::pin(async move { approve })
        }
    }

    async fn setup(approve: bool) -> (Arc<Confirms>, SessionSetup) {
        let frontend = Arc::new(Confirms {
            approve,
            asked: Mutex::default(),
        });
        let ctx = Context::new(
            SessionId::new("s"),
            "/tmp",
            frontend.clone(),
            Default::default(),
        );
        let mut setup = SessionSetup {
            ctx,
            tools: vec![],
            system: String::new(),
            commands: vec![],
            resumed: false,
            profiles: vec![Profile::write()],
            features: vec![],
        };
        PlanMode.session_start(&mut setup).await;
        (frontend, setup)
    }

    /// The context in plan mode with the profiles installed, as the agent
    /// does after `session_start`.
    async fn ctx(approve: bool) -> (Arc<Confirms>, Context) {
        let (frontend, setup) = setup(approve).await;
        setup.ctx.set_profiles(setup.profiles);
        setup.ctx.set_profile(&PLAN).unwrap();
        (frontend, setup.ctx)
    }

    fn present_call() -> ToolCall {
        ToolCall {
            id: ToolCallId::new("t1"),
            name: PRESENT,
            input: json!({"plan": "1. do it"}),
        }
    }

    #[tokio::test]
    async fn adds_the_plan_profile() {
        let (_, setup) = setup(true).await;
        let [write, plan] = &setup.profiles[..] else {
            panic!("two profiles");
        };
        assert!(write.shows(&PRESENT) && write.shows(&ENTER) && write.shows(&rho::tools::EDIT));
        assert!(plan.read_only);
        assert!(
            !plan.shows(&rho::tools::WRITE)
                && !plan.shows(&rho::tools::EDIT)
                && !plan.shows(&ENTER)
        );
        assert!(
            plan.shows(&PRESENT) && plan.shows(&rho::tools::READ) && plan.shows(&rho::tools::BASH)
        );
    }

    #[tokio::test]
    async fn enter_and_approve() {
        let (frontend, ctx) = ctx(true).await;
        ctx.set_profile(&Profile::WRITE).unwrap();
        let out = EnterPlanMode.run(json!({}), &ctx).await.unwrap();
        assert_eq!(out.output, "Entered plan mode.");
        assert_eq!(ctx.profile().id, PLAN);

        let mut call = present_call();
        assert_eq!(PlanMode.tool_call(&ctx, &mut call).await, Decision::Allow);
        assert_eq!(*frontend.asked.lock().unwrap(), [Some("t1".to_string())]);
        PresentPlan.run(call.input, &ctx).await.unwrap();
        assert_eq!(ctx.profile().id, Profile::WRITE);
        assert!(!ctx.is_stopped());
    }

    #[tokio::test]
    async fn skipped_stays_and_stops() {
        let (_, ctx) = ctx(false).await;
        let decision = PlanMode.tool_call(&ctx, &mut present_call()).await;
        assert!(matches!(decision, Decision::Block(r) if r.contains("did not approve")));
        assert_eq!(ctx.profile().id, PLAN);
        assert!(ctx.is_stopped());
    }

    #[tokio::test]
    async fn approval_outside_plan_mode_keeps_the_profile() {
        let (frontend, ctx) = ctx(true).await;
        ctx.set_profile(&Profile::WRITE).unwrap();
        let mut call = present_call();
        assert_eq!(PlanMode.tool_call(&ctx, &mut call).await, Decision::Allow);
        PresentPlan.run(call.input, &ctx).await.unwrap();
        assert_eq!(ctx.profile().id, Profile::WRITE);
        assert_eq!(frontend.asked.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn other_calls_are_not_asked() {
        let (frontend, ctx) = ctx(false).await;
        let mut read = ToolCall {
            name: ToolName::new("Read"),
            ..present_call()
        };
        assert_eq!(PlanMode.tool_call(&ctx, &mut read).await, Decision::Allow);
        assert!(frontend.asked.lock().unwrap().is_empty());
        assert!(PresentPlan.run(json!({"plan": " "}), &ctx).await.is_err());
    }
}
