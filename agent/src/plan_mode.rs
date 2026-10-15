//! A read-only "plan" profile, like Claude Code's plan mode. The user picks
//! it in the profile selector, or the model enters it with `EnterPlanMode`.
//! In plan mode the model cannot write or edit files and the sandbox is
//! read-only. `PresentPlan` shows a plan to the user in any profile, with a
//! text field for feedback; an approved plan leaves plan mode.

use std::sync::Arc;

use anyhow::{Result, bail};
use rho::extension::{
    Choice, ChoiceValue, Context, Extension, Field, FieldKey, FieldKind, Form, FormReply, Profile,
    ProfileId, SessionSetup, ToolOutput,
};
use rho::provider::{ToolName, ToolSpec};
use rho::tools::{Tool, input_schema, parse};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::Value;

pub const PLAN: ProfileId = ProfileId::from_static("plan");
const ENTER: ToolName = ToolName::from_static("EnterPlanMode");
const PRESENT: ToolName = ToolName::from_static("PresentPlan");

const DECISION: FieldKey = FieldKey::from_static("decision");
const FEEDBACK: FieldKey = FieldKey::from_static("feedback");
const APPROVE: &str = "approve";

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
            hidden_tools: vec![rho::tools::WRITE, rho::edit::EDIT, ENTER],
            read_only: true,
            instructions: Some(INSTRUCTIONS.into()),
        });
        session.tools.push(Arc::new(EnterPlanMode));
        session.tools.push(Arc::new(PresentPlan));
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
    fn kind(&self) -> rho::tools::ToolKind {
        rho::tools::ToolKind::SwitchMode
    }

    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: PRESENT,
            description: "Present an implementation plan to the user for approval, with room for their feedback. \
                In plan mode, an approved plan switches back to making changes. \
                Only for plans of changes to make; not for research questions."
                .into(),
            input_schema: input_schema::<PresentInput>(),
        }
    }

    fn describe(&self, input: &Value) -> Option<String> {
        Some(input["plan"].as_str()?.to_string())
    }

    async fn run(&self, input: Value, ctx: &Context) -> Result<ToolOutput> {
        let i: PresentInput = parse(input)?;
        if i.plan.trim().is_empty() {
            bail!("the plan is empty");
        }
        let answers = match ctx.ask(&form(&i.plan)).await {
            FormReply::Answers(answers) => answers,
            FormReply::Declined => Default::default(),
            FormReply::Unsupported => {
                bail!("the user interface cannot show the plan; present it as plain chat text")
            }
        };
        let approved = answers
            .get(&DECISION)
            .is_some_and(|v| v.iter().any(|d| d == APPROVE));
        let feedback = answers
            .get(&FEEDBACK)
            .and_then(|v| v.first())
            .map(|t| t.trim())
            .filter(|t| !t.is_empty());

        if approved {
            if ctx.profile().id == PLAN {
                ctx.set_profile(&Profile::WRITE)?;
            }
            let mut out = "The user approved the plan. Start implementing it.".to_string();
            if let Some(f) = feedback {
                out.push_str(&format!(" User notes: \"{f}\""));
            }
            return Ok(out.into());
        }
        let Some(f) = feedback else {
            ctx.stop("Plan not approved; tell the agent what to change.");
            return Ok(
                "The user did not approve the plan. Wait for their feedback; do not change anything yet."
                    .into(),
            );
        };
        Ok(format!(
            "The user did not approve the plan yet. Their feedback: \"{f}\". \
             Revise the plan and present it again; do not change anything yet."
        )
        .into())
    }
}

/// The plan is the form's message: clients collapse the tool call card.
fn form(plan: &str) -> Form {
    Form {
        message: plan.into(),
        fields: vec![
            Field {
                key: DECISION,
                title: "Approve this plan and start implementing?".into(),
                description: None,
                kind: FieldKind::Choice {
                    options: vec![
                        Choice {
                            value: ChoiceValue::from_static(APPROVE),
                            title: "Approve".into(),
                            description: Some("Start implementing".into()),
                        },
                        Choice {
                            value: ChoiceValue::from_static("revise"),
                            title: "Revise".into(),
                            description: Some("Keep planning with the feedback below".into()),
                        },
                    ],
                    multiple: false,
                },
                // The feedback field can stand alone.
                required: false,
            },
            Field {
                key: FEEDBACK,
                title: "Feedback".into(),
                description: Some(
                    "Optional. What to change, or a note sent along with the approval.".into(),
                ),
                kind: FieldKind::Text,
                required: false,
            },
        ],
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rho::agent::AgentEvent;
    use rho::extension::{Answers, BoxFuture, Frontend};
    use rho::store::SessionId;
    use serde_json::json;
    use std::sync::Mutex;

    /// Answers every form with `reply` and counts the forms.
    struct Scripted {
        reply: FormReply,
        asked: Mutex<usize>,
    }

    impl Frontend for Scripted {
        fn event(&self, _event: AgentEvent<'_>) {}

        fn ask<'a>(&'a self, _form: &'a Form) -> BoxFuture<'a, FormReply> {
            *self.asked.lock().unwrap() += 1;
            let reply = self.reply.clone();
            Box::pin(async { reply })
        }
    }

    async fn setup(reply: FormReply) -> (Arc<Scripted>, SessionSetup) {
        let frontend = Arc::new(Scripted {
            reply,
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
    async fn ctx(reply: FormReply) -> (Arc<Scripted>, Context) {
        let (frontend, setup) = setup(reply).await;
        setup.ctx.set_profiles(setup.profiles);
        setup.ctx.set_profile(&PLAN).unwrap();
        (frontend, setup.ctx)
    }

    fn answers(pairs: &[(&FieldKey, &str)]) -> FormReply {
        FormReply::Answers(Answers::from_iter(
            pairs
                .iter()
                .map(|(k, v)| ((*k).clone(), vec![v.to_string()])),
        ))
    }

    async fn present(ctx: &Context) -> String {
        PresentPlan
            .run(json!({"plan": "1. do it"}), ctx)
            .await
            .unwrap()
            .output
    }

    #[tokio::test]
    async fn adds_the_plan_profile() {
        let (_, setup) = setup(FormReply::Declined).await;
        let [write, plan] = &setup.profiles[..] else {
            panic!("two profiles");
        };
        assert!(write.shows(&PRESENT) && write.shows(&ENTER) && write.shows(&rho::edit::EDIT));
        assert!(plan.read_only);
        assert!(
            !plan.shows(&rho::tools::WRITE) && !plan.shows(&rho::edit::EDIT) && !plan.shows(&ENTER)
        );
        assert!(
            plan.shows(&PRESENT) && plan.shows(&rho::tools::READ) && plan.shows(&rho::tools::BASH)
        );
    }

    #[tokio::test]
    async fn enter_and_approve_with_notes() {
        let (frontend, ctx) =
            ctx(answers(&[(&DECISION, APPROVE), (&FEEDBACK, " tests too ")])).await;
        ctx.set_profile(&Profile::WRITE).unwrap();
        let out = EnterPlanMode.run(json!({}), &ctx).await.unwrap();
        assert_eq!(out.output, "Entered plan mode.");
        assert_eq!(ctx.profile().id, PLAN);

        assert_eq!(
            present(&ctx).await,
            "The user approved the plan. Start implementing it. User notes: \"tests too\""
        );
        assert_eq!(*frontend.asked.lock().unwrap(), 1);
        assert_eq!(ctx.profile().id, Profile::WRITE);
        assert!(!ctx.is_stopped());
    }

    #[tokio::test]
    async fn feedback_without_approval_continues_in_plan_mode() {
        let (_, ctx) = ctx(answers(&[(&FEEDBACK, "use sqlite")])).await;
        let out = present(&ctx).await;
        assert!(
            out.contains("\"use sqlite\"") && out.contains("Revise"),
            "{out}"
        );
        assert_eq!(ctx.profile().id, PLAN);
        assert!(!ctx.is_stopped());
    }

    #[tokio::test]
    async fn dismissed_stays_and_stops() {
        let (_, ctx) = ctx(FormReply::Declined).await;
        assert!(present(&ctx).await.contains("did not approve"));
        assert_eq!(ctx.profile().id, PLAN);
        assert!(ctx.is_stopped());
    }

    #[tokio::test]
    async fn approval_outside_plan_mode_keeps_the_profile() {
        let (_, ctx) = ctx(answers(&[(&DECISION, APPROVE)])).await;
        ctx.set_profile(&Profile::WRITE).unwrap();
        assert!(present(&ctx).await.starts_with("The user approved"));
        assert_eq!(ctx.profile().id, Profile::WRITE);
    }

    #[tokio::test]
    async fn fails_closed_without_forms() {
        let (_, ctx) = ctx(FormReply::Unsupported).await;
        assert!(PresentPlan.run(json!({"plan": "x"}), &ctx).await.is_err());
        assert!(PresentPlan.run(json!({"plan": " "}), &ctx).await.is_err());
        assert_eq!(ctx.profile().id, PLAN);
    }
}
