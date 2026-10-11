//! A read-only "plan" profile, like Claude Code's plan mode. The user picks
//! it in the profile selector, or the model enters it with `EnterPlanMode`.
//! In plan mode the model cannot write or edit files and the sandbox is
//! read-only. It leaves with `ExitPlanMode`, which shows the plan and only
//! switches back to `write` if the user approves it.

use std::sync::Arc;

use anyhow::{Result, bail};
use rho::extension::{Context, Decision, Extension, Profile, SessionSetup, ToolCall, ToolOutput};
use rho::provider::ToolSpec;
use rho::tools::{Tool, input_schema, parse};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::Value;

pub const PLAN: &str = "plan";
const ENTER: &str = "EnterPlanMode";
const EXIT: &str = "ExitPlanMode";

const INSTRUCTIONS: &str = "You are in plan mode. Explore the code (Read, Bash, Grep, ...) and ask the user questions, \
but do not change anything: no file edits, no commits, no commands with side effects. \
Work out a concrete implementation plan. When it is ready, call ExitPlanMode with the plan; \
the user has to approve it before you can make changes.";

pub struct PlanMode;

impl Extension for PlanMode {
    async fn session_start(&self, session: &mut SessionSetup) {
        for p in &mut session.profiles {
            if p.id == Profile::WRITE {
                p.hidden_tools.push(EXIT.into());
            }
        }
        session.profiles.push(Profile {
            id: PLAN.into(),
            name: "Plan".into(),
            description: "Read-only: explore and plan, no changes".into(),
            hidden_tools: vec!["Write".into(), "Edit".into(), ENTER.into()],
            read_only: true,
            instructions: Some(INSTRUCTIONS.into()),
        });
        session.tools.push(Arc::new(EnterPlanMode));
        session.tools.push(Arc::new(ExitPlanMode));
    }

    /// The user approves the plan here, where the call id is known, so the
    /// question appears on the tool call that shows the plan.
    async fn tool_call(&self, ctx: &Context, call: &mut ToolCall) -> Decision {
        if call.name != EXIT || ctx.profile().id != PLAN {
            return Decision::Allow;
        }
        if ctx
            .confirm(Some(&call.id), "Approve this plan and start implementing?")
            .await
        {
            return Decision::Allow;
        }
        ctx.stop("Plan not approved; tell the agent what to change.");
        Decision::Block(
            "The user did not approve the plan. Stay in plan mode and wait for their feedback."
                .into(),
        )
    }
}

#[derive(Deserialize, JsonSchema)]
struct EnterInput {}

struct EnterPlanMode;

impl Tool for EnterPlanMode {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: ENTER.into(),
            description: "Switch to plan mode before a non-trivial change: when there are several ways to do it, \
                it touches many files, or the requirements are unclear. In plan mode you explore read-only and \
                design the approach; the user approves the plan before you change anything."
                .into(),
            input_schema: input_schema::<EnterInput>(),
        }
    }

    async fn run(&self, _input: Value, ctx: &Context) -> Result<ToolOutput> {
        ctx.set_profile(PLAN)?;
        Ok("Entered plan mode.".into())
    }
}

#[derive(Deserialize, JsonSchema)]
struct ExitInput {
    /// The implementation plan as markdown, shown to the user for approval.
    plan: String,
}

struct ExitPlanMode;

impl Tool for ExitPlanMode {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: EXIT.into(),
            description: "Present your implementation plan to the user and leave plan mode once they approve it. \
                Only for plans of changes to make; not for research questions."
                .into(),
            input_schema: input_schema::<ExitInput>(),
        }
    }

    fn describe(&self, input: &Value) -> Option<String> {
        Some(input["plan"].as_str()?.to_string())
    }

    async fn run(&self, input: Value, ctx: &Context) -> Result<ToolOutput> {
        let i: ExitInput = parse(input)?;
        if i.plan.trim().is_empty() {
            bail!("the plan is empty");
        }
        ctx.set_profile(Profile::WRITE)?;
        Ok("The user approved the plan. Start implementing it.".into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rho::agent::AgentEvent;
    use rho::extension::{BoxFuture, Frontend};
    use serde_json::json;
    use std::sync::Mutex;

    /// Answers confirmations with `approve` and records them.
    struct Confirms {
        approve: bool,
        asked: Mutex<Vec<Option<String>>>,
    }

    impl Frontend for Confirms {
        fn event(&self, _event: AgentEvent<'_>) {}

        fn confirm<'a>(&'a self, id: Option<&'a str>, _title: &'a str) -> BoxFuture<'a, bool> {
            self.asked.lock().unwrap().push(id.map(String::from));
            let approve = self.approve;
            Box::pin(async move { approve })
        }
    }

    async fn setup(approve: bool) -> (Arc<Confirms>, SessionSetup) {
        let frontend = Arc::new(Confirms {
            approve,
            asked: Mutex::default(),
        });
        let ctx = Context::new("s", "/tmp", frontend.clone(), Default::default());
        let mut setup = SessionSetup {
            ctx,
            tools: vec![],
            system: String::new(),
            commands: vec![],
            resumed: false,
            profiles: vec![Profile::write()],
        };
        PlanMode.session_start(&mut setup).await;
        (frontend, setup)
    }

    fn exit_call() -> ToolCall {
        ToolCall {
            id: "t1".into(),
            name: EXIT.into(),
            input: json!({"plan": "1. do it"}),
        }
    }

    #[tokio::test]
    async fn adds_the_plan_profile() {
        let (_, setup) = setup(true).await;
        let [write, plan] = &setup.profiles[..] else {
            panic!("two profiles");
        };
        assert!(!write.shows(EXIT) && write.shows(ENTER) && write.shows("Edit"));
        assert!(plan.read_only);
        assert!(!plan.shows("Write") && !plan.shows("Edit") && !plan.shows(ENTER));
        assert!(plan.shows(EXIT) && plan.shows("Read") && plan.shows("Bash"));
    }

    /// The context with the profiles installed, as the agent does after
    /// `session_start`.
    async fn ctx(approve: bool) -> (Arc<Confirms>, Context) {
        let (frontend, setup) = setup(approve).await;
        setup.ctx.set_profiles(setup.profiles);
        (frontend, setup.ctx)
    }

    #[tokio::test]
    async fn enter_and_approved_exit() {
        let (frontend, ctx) = ctx(true).await;
        let out = EnterPlanMode.run(json!({}), &ctx).await.unwrap();
        assert_eq!(out.output, "Entered plan mode.");
        assert_eq!(ctx.profile().id, PLAN);

        let mut call = exit_call();
        assert_eq!(PlanMode.tool_call(&ctx, &mut call).await, Decision::Allow);
        assert_eq!(*frontend.asked.lock().unwrap(), [Some("t1".to_string())]);
        ExitPlanMode.run(call.input, &ctx).await.unwrap();
        assert_eq!(ctx.profile().id, Profile::WRITE);
        assert!(!ctx.is_stopped());
    }

    #[tokio::test]
    async fn rejected_exit_stays_and_stops() {
        let (_, ctx) = ctx(false).await;
        ctx.set_profile(PLAN).unwrap();
        let decision = PlanMode.tool_call(&ctx, &mut exit_call()).await;
        assert!(matches!(decision, Decision::Block(r) if r.contains("did not approve")));
        assert_eq!(ctx.profile().id, PLAN);
        assert!(ctx.is_stopped());
    }

    #[tokio::test]
    async fn other_calls_are_not_asked() {
        let (frontend, ctx) = ctx(false).await;
        // ExitPlanMode outside plan mode is hidden by the agent; no question.
        assert_eq!(
            PlanMode.tool_call(&ctx, &mut exit_call()).await,
            Decision::Allow
        );
        ctx.set_profile(PLAN).unwrap();
        let mut read = ToolCall {
            name: "Read".into(),
            ..exit_call()
        };
        assert_eq!(PlanMode.tool_call(&ctx, &mut read).await, Decision::Allow);
        assert!(frontend.asked.lock().unwrap().is_empty());
    }
}
