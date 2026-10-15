//! Lets the model ask the user structured questions (pi's
//! ask-user-question). All questions go out as one form, so the user answers
//! them in a single dialog: each question is a pick list (one or several
//! options, with descriptions) next to a text field for an answer of their
//! own. In ACP clients that is an elicitation form.

use std::sync::Arc;

use anyhow::{Result, bail};
use rho::extension::{
    Choice, ChoiceValue, Context, Extension, Field, FieldKey, FieldKind, Form, FormReply,
    SessionSetup, ToolOutput,
};
use rho::provider::{ToolName, ToolSpec};
use rho::tools::{Tool, input_schema, parse};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::Value;

const NAME: ToolName = ToolName::from_static("AskUserQuestion");
const MAX_PREVIEW: usize = 600;

pub struct AskUser;

impl Extension for AskUser {
    async fn session_start(&self, session: &mut SessionSetup) {
        session.tools.push(Arc::new(AskUserQuestion));
    }
}

#[derive(Deserialize, JsonSchema)]
struct Input {
    /// Questions to ask (1-4).
    #[schemars(length(min = 1, max = 4))]
    questions: Vec<Question>,
    /// what this call is for, at most 10 words; shown to the user as its title
    // Required for the model; calls without it (older sessions) still run.
    #[schemars(required, with = "String")]
    #[expect(dead_code, reason = "only for the schema, read by the frontend")]
    description: Option<String>,
}

#[derive(Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
struct Question {
    /// The complete question, ending with a question mark.
    question: String,
    /// Very short tag, e.g. "Auth method". Max 16 characters.
    #[schemars(length(max = 16))]
    header: String,
    /// 2-4 distinct choices. The user can always type their own answer
    /// instead, do not add an "Other" option.
    #[schemars(length(min = 2, max = 4))]
    options: Vec<QuestionOption>,
    /// Allow selecting multiple options.
    #[serde(default)]
    multi_select: bool,
}

#[derive(Deserialize, JsonSchema)]
struct QuestionOption {
    /// Concise choice text (1-5 words). Max 60 characters.
    #[schemars(length(max = 60))]
    label: String,
    /// What this option means or its trade-offs.
    description: String,
    /// Optional snippet or mockup shown with the question.
    #[serde(default)]
    preview: Option<String>,
}

struct AskUserQuestion;

impl Tool for AskUserQuestion {
    fn titled(&self) -> bool {
        true
    }

    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: NAME,
            description: "Ask the user 1-4 structured multiple-choice questions when requirements are ambiguous or a decision is needed. \
                Each question has 2-4 options; the user can always type their own answer or add a note to their pick. \
                Use multiSelect when several answers are valid. \
                Group all questions into one call instead of calling it back-to-back. \
                If you recommend an option, put it first and append \"(Recommended)\" to its label.".into(),
            input_schema: input_schema::<Input>(),
        }
    }

    fn describe(&self, input: &Value) -> Option<String> {
        let input: Input = serde_json::from_value(input.clone()).ok()?;
        Some(markdown(&input.questions))
    }

    async fn run(&self, input: Value, ctx: &Context) -> Result<ToolOutput> {
        let input: Input = parse(input)?;
        validate(&input.questions)?;
        let form = form(&input.questions);
        let answers = match ctx.ask(&form).await {
            FormReply::Answers(answers) => answers,
            FormReply::Declined => return Ok("User declined to answer the questions".into()),
            FormReply::Unsupported => {
                bail!("the user interface cannot show questions; ask them as plain chat text")
            }
        };

        let lines: Vec<String> = input
            .questions
            .iter()
            .enumerate()
            .map(|(i, q)| answer(q, i, &answers))
            .collect();
        Ok(format!(
            "User has answered your questions: {} You can now continue with the user's answers in mind.",
            lines.join(" ")
        )
        .into())
    }
}

fn validate(questions: &[Question]) -> Result<()> {
    if !(1..=4).contains(&questions.len()) {
        bail!("ask 1-4 questions per call");
    }
    for (i, q) in questions.iter().enumerate() {
        if q.question.trim().is_empty() {
            bail!("question {} is empty", i + 1);
        }
        if questions[..i].iter().any(|p| p.question == q.question) {
            bail!("question text must be unique");
        }
        if !(2..=4).contains(&q.options.len()) {
            bail!("each question needs 2-4 options");
        }
        for (j, o) in q.options.iter().enumerate() {
            if o.label.trim().is_empty() {
                bail!("option labels must not be empty");
            }
            if q.options[..j].iter().any(|p| p.label == o.label) {
                bail!("option labels must be unique within a question");
            }
        }
    }
    Ok(())
}

fn pick_key(i: usize) -> FieldKey {
    FieldKey::new(format!("q{i}"))
}

fn text_key(i: usize) -> FieldKey {
    FieldKey::new(format!("q{i}_text"))
}

fn form(questions: &[Question]) -> Form {
    let mut fields = vec![];
    for (i, q) in questions.iter().enumerate() {
        let previews: Vec<String> = q
            .options
            .iter()
            .filter_map(|o| {
                let preview = o.preview.as_deref()?;
                Some(format!(
                    "Preview of {}:\n{}",
                    o.label,
                    preview.chars().take(MAX_PREVIEW).collect::<String>()
                ))
            })
            .collect();
        let description = std::iter::once(q.header.clone())
            .chain(previews)
            .collect::<Vec<_>>()
            .join("\n\n");
        fields.push(Field {
            key: pick_key(i),
            title: q.question.clone(),
            description: Some(description),
            kind: FieldKind::Choice {
                options: q
                    .options
                    .iter()
                    .map(|o| Choice {
                        value: ChoiceValue::new(&o.label),
                        title: o.label.clone(),
                        description: Some(o.description.clone()).filter(|d| !d.is_empty()),
                    })
                    .collect(),
                multiple: q.multi_select,
            },
            // The text field can stand alone.
            required: false,
        });
        fields.push(Field {
            key: text_key(i),
            title: format!("{}: your own answer or a note", q.header),
            description: Some(
                "Optional. Your own answer, or a note sent along with the selection above.".into(),
            ),
            kind: FieldKind::Text,
            required: false,
        });
    }
    let n = questions.len();
    Form {
        message: if n == 1 {
            "Question from the assistant".into()
        } else {
            format!("{n} questions from the assistant")
        },
        fields,
    }
}

/// The user's answer to question `i` as a line for the model: the picked
/// options (with the preview of a single pick) and any typed text. Text alone
/// is the answer; text next to a pick is a note on it.
fn answer(q: &Question, i: usize, answers: &rho::extension::Answers) -> String {
    let text = answers
        .get(&text_key(i))
        .and_then(|v| v.first())
        .map(|t| t.trim())
        .filter(|t| !t.is_empty());
    let picked = answers.get(&pick_key(i)).cloned().unwrap_or_default();
    if picked.is_empty() {
        return format!("\"{}\"=\"{}\".", q.question, text.unwrap_or("(no answer)"));
    }
    // Only a single pick has a preview worth repeating.
    let preview = match picked.as_slice() {
        [one] => q
            .options
            .iter()
            .find(|o| o.label == *one)
            .and_then(|o| o.preview.clone()),
        _ => None,
    };
    let mut line = format!("\"{}\"=\"{}\".", q.question, picked.join(", "));
    if let Some(p) = preview {
        line.push_str(&format!(" selected preview: {p}."));
    }
    if let Some(t) = text {
        line.push_str(&format!(" user notes: \"{t}\"."));
    }
    line
}

/// The questions as markdown for the tool call, instead of the raw JSON.
fn markdown(questions: &[Question]) -> String {
    questions
        .iter()
        .map(|q| {
            let mut lines = vec![format!(
                "**{}**: {}{}",
                q.header,
                q.question,
                if q.multi_select {
                    " *(select several)*"
                } else {
                    ""
                }
            )];
            for (i, o) in q.options.iter().enumerate() {
                lines.push(if o.description.is_empty() {
                    format!("{}. **{}**", i + 1, o.label)
                } else {
                    format!("{}. **{}**: {}", i + 1, o.label, o.description)
                });
            }
            lines.join("\n")
        })
        .collect::<Vec<_>>()
        .join("\n\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use rho::agent::AgentEvent;
    use rho::extension::{Answers, BoxFuture, Frontend};
    use rho::store::SessionId;
    use serde_json::json;
    use std::sync::Mutex;

    /// Answers every form with `reply` and keeps the form it was shown.
    struct Scripted {
        reply: FormReply,
        asked: Mutex<Vec<Form>>,
    }

    impl Frontend for Scripted {
        fn event(&self, _event: AgentEvent<'_>) {}

        fn ask<'a>(&'a self, form: &'a Form) -> BoxFuture<'a, FormReply> {
            self.asked.lock().unwrap().push(form.clone());
            let reply = self.reply.clone();
            Box::pin(async { reply })
        }
    }

    fn ctx(reply: FormReply) -> (Arc<Scripted>, Context) {
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
        (frontend, ctx)
    }

    fn input() -> Value {
        json!({"questions": [
            {
                "question": "Which database?",
                "header": "Database",
                "options": [
                    {"label": "Postgres", "description": "SQL server", "preview": "CREATE TABLE t ();"},
                    {"label": "SQLite", "description": "Embedded"},
                ],
            },
            {
                "question": "Which features?",
                "header": "Features",
                "multiSelect": true,
                "options": [
                    {"label": "Auth", "description": ""},
                    {"label": "Cache", "description": "Redis"},
                ],
            },
        ]})
    }

    fn answers(pairs: &[(&str, &[&str])]) -> FormReply {
        FormReply::Answers(Answers::from_iter(pairs.iter().map(|(k, v)| {
            (FieldKey::new(*k), v.iter().map(|s| s.to_string()).collect())
        })))
    }

    #[tokio::test]
    async fn asks_one_form_and_reports_the_answers() {
        let (frontend, ctx) = ctx(answers(&[
            ("q0", &["Postgres"]),
            ("q1", &["Auth", "Cache"]),
        ]));
        let out = AskUserQuestion.run(input(), &ctx).await.unwrap();
        assert!(!out.is_error);
        assert_eq!(
            out.output,
            "User has answered your questions: \"Which database?\"=\"Postgres\". selected preview: CREATE TABLE t ();. \
             \"Which features?\"=\"Auth, Cache\". You can now continue with the user's answers in mind."
        );

        let asked = frontend.asked.lock().unwrap();
        assert_eq!(asked.len(), 1);
        let keys: Vec<_> = asked[0].fields.iter().map(|f| f.key.as_str()).collect();
        assert_eq!(keys, ["q0", "q0_text", "q1", "q1_text"]);
        let FieldKind::Choice { options, multiple } = &asked[0].fields[2].kind else {
            panic!("not a choice");
        };
        assert!(*multiple);
        assert_eq!(options[0].description, None);
        assert_eq!(options[1].description.as_deref(), Some("Redis"));
        let preview = asked[0].fields[0].description.as_deref().unwrap();
        assert!(
            preview.contains("Preview of Postgres:\nCREATE TABLE t ();"),
            "{preview}"
        );
    }

    #[tokio::test]
    async fn typed_text_goes_along_with_the_selection() {
        let (_, ctx) = ctx(answers(&[
            ("q0", &["Postgres"]),
            ("q0_text", &["  but maybe DuckDB  "]),
            ("q1_text", &["none"]),
        ]));
        let out = AskUserQuestion.run(input(), &ctx).await.unwrap();
        assert_eq!(
            out.output,
            "User has answered your questions: \"Which database?\"=\"Postgres\". selected preview: CREATE TABLE t ();. \
             user notes: \"but maybe DuckDB\". \"Which features?\"=\"none\". \
             You can now continue with the user's answers in mind."
        );
    }

    #[tokio::test]
    async fn unanswered_questions_are_marked() {
        let (_, ctx) = ctx(answers(&[("q0", &["SQLite"])]));
        let out = AskUserQuestion.run(input(), &ctx).await.unwrap();
        assert!(
            out.output
                .contains("\"Which database?\"=\"SQLite\". \"Which features?\"=\"(no answer)\"."),
            "{}",
            out.output
        );
    }

    #[tokio::test]
    async fn declined_and_unsupported() {
        let (_, ctx) = ctx(FormReply::Declined);
        let out = AskUserQuestion.run(input(), &ctx).await.unwrap();
        assert_eq!(out.output, "User declined to answer the questions");

        let (_, ctx) = ctx_unsupported();
        let err = AskUserQuestion.run(input(), &ctx).await.unwrap_err();
        assert!(format!("{err}").contains("plain chat text"), "{err}");
    }

    fn ctx_unsupported() -> (Arc<Scripted>, Context) {
        ctx(FormReply::Unsupported)
    }

    #[tokio::test]
    async fn rejects_bad_questions_without_asking() {
        let (frontend, ctx) = ctx(FormReply::Declined);
        for bad in [
            json!({"questions": []}),
            json!({"questions": [{"question": "q?", "header": "h", "options": [
                {"label": "a", "description": ""}]}]}),
            json!({"questions": [{"question": "q?", "header": "h", "options": [
                {"label": "a", "description": ""}, {"label": "a", "description": ""}]}]}),
        ] {
            assert!(AskUserQuestion.run(bad, &ctx).await.is_err());
        }
        assert!(frontend.asked.lock().unwrap().is_empty());
    }

    #[test]
    fn describes_the_questions_for_the_user() {
        assert_eq!(
            AskUserQuestion.describe(&input()).unwrap(),
            "**Database**: Which database?\n1. **Postgres**: SQL server\n2. **SQLite**: Embedded\n\n\
             **Features**: Which features? *(select several)*\n1. **Auth**\n2. **Cache**: Redis"
        );
        assert_eq!(AskUserQuestion.describe(&json!({"nope": 1})), None);
    }

    #[test]
    fn schema_limits_the_question_count() {
        let schema = AskUserQuestion.spec().input_schema;
        assert_eq!(schema["properties"]["questions"]["minItems"], 1);
        assert_eq!(schema["properties"]["questions"]["maxItems"], 4);
    }
}
