//! `PresentDiff`: the model writes a diff to a file and asks the user to
//! review it (ADR 0076). The web client shows the file, which it gets as a
//! share link on the tool call, on a review page with line comments.
//! Approval is a permission request on the call, as for `PresentPlan`.

use std::sync::Arc;

use anyhow::Result;
use rho::extension::{Context, Decision, Extension, SessionSetup, ToolCall, ToolOutput};
use rho::provider::ToolSpec;
use rho::tools::share_file::{self, PRESENT_DIFF};
use rho::tools::{Tool, ToolKind, input_schema};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::Value;

pub struct Review;

impl Extension for Review {
    async fn session_start(&self, session: &mut SessionSetup) {
        session.tools.push(Arc::new(PresentDiff));
    }

    /// Asks here, where the call id is known, so the question appears on
    /// the call that links the diff.
    async fn tool_call(&self, ctx: &Context, call: &mut ToolCall) -> Decision {
        if call.name != PRESENT_DIFF {
            return Decision::Allow;
        }
        let Some(path) = call.input["path"].as_str() else {
            return Decision::Block("`path` is missing".into());
        };
        let path = share_file::shared_path(ctx.cwd(), path);
        if let Err(e) = share_file::open(ctx, path).await {
            return Decision::Block(format!("{e:#}"));
        }
        if ctx
            .confirm_with(
                Some(&call.id),
                "Approve these changes?",
                "Approve",
                "Request changes",
            )
            .await
        {
            return Decision::Allow;
        }
        ctx.stop("Changes not approved; the review comments follow.");
        Decision::Block(
            "The user requested changes. Wait for their review comments; do not change anything yet."
                .into(),
        )
    }
}

#[derive(Deserialize, JsonSchema)]
struct PresentInput {
    /// The file with the diff to review, in unified diff format.
    #[expect(dead_code, reason = "only for the schema, read in `Review::tool_call`")]
    path: String,
    /// what this review covers, at most 10 words; shown to the user as its title
    // Required for the model; calls without it still run.
    #[schemars(required, with = "String")]
    #[expect(dead_code, reason = "only for the schema, read by the frontend")]
    description: Option<String>,
}

struct PresentDiff;

impl Tool for PresentDiff {
    /// It only asks; the answer takes effect after it.
    fn repeatable(&self) -> bool {
        true
    }

    fn kind(&self) -> ToolKind {
        ToolKind::Other
    }

    fn titled(&self) -> bool {
        true
    }

    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: PRESENT_DIFF,
            description: "Ask the user to review changes, like a pull request review. \
                Write the diff to a file first (unified format, e.g. `git diff origin/main > /tmp/review.patch`) \
                and pass its path; the user sees it with syntax highlighting and can comment on lines. \
                Sandboxed like Read. The call returns when the user approved; \
                if they request changes, their comments come as the next message."
                .into(),
            input_schema: input_schema::<PresentInput>(),
        }
    }

    /// Runs only after `Review::tool_call` got the approval.
    async fn run(&self, _input: Value, _ctx: &Context) -> Result<ToolOutput> {
        Ok("The user approved the changes.".into())
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

    fn setup(approve: bool, dir: &std::path::Path) -> (Arc<Confirms>, Context) {
        let frontend = Arc::new(Confirms {
            approve,
            asked: Mutex::default(),
        });
        let ctx = Context::new(
            SessionId::new("s"),
            dir,
            frontend.clone(),
            Default::default(),
        );
        (frontend, ctx)
    }

    fn call(path: &str) -> ToolCall {
        ToolCall {
            id: ToolCallId::new("t1"),
            name: PRESENT_DIFF,
            input: json!({ "path": path }),
        }
    }

    fn patch() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("x.patch"), "--- a/x\n+++ b/x\n").unwrap();
        dir
    }

    #[tokio::test]
    async fn approve() {
        let dir = patch();
        let (frontend, ctx) = setup(true, dir.path());
        let mut call = call("x.patch");
        assert_eq!(Review.tool_call(&ctx, &mut call).await, Decision::Allow);
        assert_eq!(*frontend.asked.lock().unwrap(), [Some("t1".to_string())]);
        let out = PresentDiff.run(call.input, &ctx).await.unwrap();
        assert!(out.output.contains("approved"));
        assert!(!ctx.is_stopped());
    }

    #[tokio::test]
    async fn request_changes_stops() {
        let dir = patch();
        let (_, ctx) = setup(false, dir.path());
        let decision = Review.tool_call(&ctx, &mut call("x.patch")).await;
        assert!(matches!(decision, Decision::Block(r) if r.contains("requested changes")));
        assert!(ctx.is_stopped());
    }

    #[tokio::test]
    async fn missing_file_is_not_asked() {
        let dir = patch();
        let (frontend, ctx) = setup(true, dir.path());
        let decision = Review.tool_call(&ctx, &mut call("nope.patch")).await;
        assert!(matches!(decision, Decision::Block(r) if r.contains("nope.patch")));
        assert!(frontend.asked.lock().unwrap().is_empty());
        assert!(!ctx.is_stopped());
    }
}
