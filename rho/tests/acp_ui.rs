//! What ACP clients see beyond text: model and thinking config options,
//! usage with cache details, Bash as a terminal, and question forms.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use agent_client_protocol::schema::ProtocolVersion;
use agent_client_protocol::schema::v1::{
    ClientCapabilities, ContentBlock, CreateElicitationRequest, CreateElicitationResponse,
    ElicitationAcceptAction, ElicitationCapabilities, ElicitationContentValue,
    ElicitationFormCapabilities, ImageContent, InitializeRequest, Meta, NewSessionRequest,
    PromptRequest, SessionConfigKind, SessionConfigOption, SessionConfigOptionValue, SessionId,
    SessionNotification, SessionUpdate, SetSessionConfigOptionRequest, TextContent,
};
use agent_client_protocol::{Client, ConnectionTo, Responder};
use anyhow::Result;
use futures::stream::BoxStream;
use rho::agent::Agent;
use rho::extension::{Choice, Context, Field, FieldKind, Form, FormReply, ToolOutput};
use rho::provider::{
    Content, Event, ModelInfo, Price, Provider, Request, StopReason, ToolSpec, Usage,
};
use rho::tools::Tool;
use serde_json::{Value, json};

mod common;
use common::{Scripted, text};

/// The scripted provider, plus two models to pick from.
struct Models(Arc<Scripted>);

impl Provider for Models {
    fn stream(&self, request: Request) -> BoxStream<'static, Result<Event>> {
        self.0.stream(request)
    }

    fn models(&self) -> Vec<ModelInfo> {
        let model = |id: &str, name: &str, window, input| ModelInfo {
            id: id.into(),
            name: name.into(),
            context_window: window,
            price: Some(Price {
                input,
                output: 5.0,
                cache_read: 0.1,
                cache_write: 1.25,
            }),
            price_above: None,
        };
        vec![
            model("big", "Big", 1_000_000, 2.0),
            model("small", "Small", 200_000, 1.0),
        ]
    }
}

fn agent_with(scripted: Arc<Scripted>, tools: Vec<Arc<dyn rho::tools::DynTool>>) -> Arc<Agent> {
    let provider = Arc::new(Models(scripted));
    let mut agent = Agent::new(provider.clone(), "big", tools);
    agent.model_spec = Some("fake/big".into());
    agent.registry.register("fake", provider);
    Arc::new(agent)
}

fn prompt(session_id: &SessionId, text: &str) -> PromptRequest {
    PromptRequest::new(
        session_id.clone(),
        vec![ContentBlock::Text(TextContent::new(text))],
    )
}

fn select_value(options: &[SessionConfigOption], id: &str) -> (String, Vec<String>) {
    let option = options.iter().find(|o| &*o.id.0 == id).expect(id);
    let SessionConfigKind::Select(select) = &option.kind else {
        panic!("{id} is not a select");
    };
    let values = match &select.options {
        agent_client_protocol::schema::v1::SessionConfigSelectOptions::Ungrouped(v) => {
            v.iter().map(|o| o.value.0.to_string()).collect()
        }
        _ => panic!("grouped"),
    };
    (select.current_value.0.to_string(), values)
}

type Updates = Arc<Mutex<Vec<SessionUpdate>>>;

fn recording(updates: &Updates) -> impl Fn(SessionNotification) + Clone + Send + 'static {
    let updates = updates.clone();
    move |n| updates.lock().unwrap().push(n.update)
}

async fn timed<T>(f: impl Future<Output = Result<T, agent_client_protocol::Error>>) -> T {
    tokio::time::timeout(Duration::from_secs(10), f)
        .await
        .expect("timed out")
        .expect("acp failed")
}

#[tokio::test]
async fn model_and_thinking_are_session_config_options() {
    let scripted = Scripted::new(vec![vec![
        text("hello"),
        Event::Usage(Usage {
            input: 10,
            output: 5,
            cache_read: 100,
            cache_write: 20,
        }),
        Event::Done(StopReason::EndTurn),
    ]]);
    let agent = agent_with(scripted.clone(), vec![]);
    let updates = Updates::default();
    let record = recording(&updates);

    let client = Client
        .builder()
        .on_receive_notification(
            async move |n: SessionNotification, _cx| {
                record(n);
                Ok(())
            },
            agent_client_protocol::on_receive_notification!(),
        )
        .connect_with(rho::acp::agent(agent), async |cx: ConnectionTo<_>| {
            cx.send_request(InitializeRequest::new(ProtocolVersion::V1))
                .block_task()
                .await?;
            let session = cx
                .send_request(NewSessionRequest::new(std::env::current_dir().unwrap()))
                .block_task()
                .await?;
            let sid = session.session_id.clone();

            let options = session.config_options.expect("config options");
            assert_eq!(
                select_value(&options, "model"),
                (
                    "fake/big".into(),
                    vec!["fake/big".into(), "fake/small".into()]
                )
            );
            let (thinking, levels) = select_value(&options, "thought_level");
            assert_eq!(thinking, "high");
            assert_eq!(levels, ["off", "low", "medium", "high", "xhigh", "max"]);

            let set = |id: &str, value: &str| {
                cx.send_request(SetSessionConfigOptionRequest::new(
                    sid.clone(),
                    id.to_string(),
                    SessionConfigOptionValue::from(value),
                ))
                .block_task()
            };
            let options = set("model", "fake/small").await?.config_options;
            assert_eq!(select_value(&options, "model").0, "fake/small");
            let options = set("thought_level", "low").await?.config_options;
            assert_eq!(select_value(&options, "thought_level").0, "low");
            assert!(set("model", "fake/nope").await.is_err());
            assert!(set("thought_level", "loud").await.is_err());
            assert!(set("colour", "red").await.is_err());

            let response = cx.send_request(prompt(&sid, "hi")).block_task().await?;
            let usage = response.usage.expect("turn usage");
            assert_eq!(
                (
                    usage.input_tokens,
                    usage.output_tokens,
                    usage.cached_read_tokens,
                    usage.cached_write_tokens,
                    usage.total_tokens
                ),
                (10, 5, Some(100), Some(20), 135)
            );
            Ok(())
        });
    timed(client).await;

    // The run used the picked model and thinking level.
    let requests = scripted.requests.lock().unwrap();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].model, "small");
    assert_eq!(requests[0].thinking, rho::provider::Thinking::Low);

    let updates = updates.lock().unwrap();
    let usage: Vec<_> = updates
        .iter()
        .filter_map(|u| match u {
            SessionUpdate::UsageUpdate(u) => Some(u),
            _ => None,
        })
        .collect();
    // Switching the model republishes the context with the new window.
    assert_eq!((usage[0].used, usage[0].size), (0, 200_000));
    // Then the request: context in use, session cost and the cache breakdown.
    let request = usage.last().unwrap();
    assert_eq!((request.used, request.size), (135, 200_000));
    let cost = request.cost.as_ref().expect("cost");
    // (10 * 1.0 + 5 * 5.0 + 100 * 0.1 + 20 * 1.25) / 1e6
    assert!((cost.amount - 70e-6).abs() < 1e-12, "{}", cost.amount);
    assert_eq!(cost.currency, "USD");
    let rho = &Value::Object(request.meta.clone().unwrap())["rho"];
    assert_eq!(rho["model"], "fake/small");
    assert_eq!(rho["request"]["cacheRead"], 100);
    assert_eq!(rho["session"]["cacheWrite"], 20);
}

fn bash_run() -> Vec<Vec<Event>> {
    vec![
        vec![
            Event::Block(Content::ToolUse {
                id: "t1".into(),
                name: "Bash".into(),
                input: json!({"command": "echo hi"}),
            }),
            Event::Done(StopReason::ToolUse),
        ],
        vec![text("done"), Event::Done(StopReason::EndTurn)],
    ]
}

async fn run_bash(client_meta: Option<Meta>) -> Vec<SessionUpdate> {
    let agent = agent_with(Scripted::new(bash_run()), rho::tools::builtin());
    let updates = Updates::default();
    let record = recording(&updates);
    let mut capabilities = ClientCapabilities::new();
    capabilities.meta = client_meta;

    let client = Client
        .builder()
        .on_receive_notification(
            async move |n: SessionNotification, _cx| {
                record(n);
                Ok(())
            },
            agent_client_protocol::on_receive_notification!(),
        )
        .connect_with(rho::acp::agent(agent), async |cx: ConnectionTo<_>| {
            cx.send_request(
                InitializeRequest::new(ProtocolVersion::V1).client_capabilities(capabilities),
            )
            .block_task()
            .await?;
            let session = cx
                .send_request(NewSessionRequest::new(std::env::current_dir().unwrap()))
                .block_task()
                .await?;
            cx.send_request(prompt(&session.session_id, "go"))
                .block_task()
                .await?;
            Ok(())
        });
    timed(client).await;
    // The guard borrows `updates`, so it cannot be the tail expression.
    #[allow(clippy::let_and_return)]
    let updates = updates.lock().unwrap().clone();
    updates
}

#[tokio::test]
async fn bash_is_a_terminal_for_clients_that_ask_for_it() {
    let meta = json!({"terminal_output": true}).as_object().cloned();
    let wire: Vec<Value> = run_bash(meta)
        .await
        .into_iter()
        .filter(|u| {
            matches!(
                u,
                SessionUpdate::ToolCall(_) | SessionUpdate::ToolCallUpdate(_)
            )
        })
        .map(|u| serde_json::to_value(u).unwrap())
        .collect();
    assert_eq!(wire.len(), 2, "{wire:?}");
    assert_eq!(wire[0]["title"], "echo hi");
    assert_eq!(wire[0]["content"][0]["type"], "terminal");
    assert_eq!(wire[0]["_meta"]["terminal_info"]["terminal_id"], "t1");
    assert_eq!(wire[1]["_meta"]["terminal_output"]["data"], "hi\n");
    assert_eq!(wire[1]["_meta"]["terminal_exit"]["exit_code"], 0);
}

#[tokio::test]
async fn bash_is_a_highlighted_command_block_otherwise() {
    let wire: Vec<Value> = run_bash(None)
        .await
        .into_iter()
        .filter(|u| {
            matches!(
                u,
                SessionUpdate::ToolCall(_) | SessionUpdate::ToolCallUpdate(_)
            )
        })
        .map(|u| serde_json::to_value(u).unwrap())
        .collect();
    assert_eq!(
        wire[0]["content"][0]["content"]["text"],
        "```sh\necho hi\n```"
    );
    assert_eq!(
        wire[1]["content"][0]["content"]["text"],
        "```sh\necho hi\n```"
    );
    assert_eq!(wire[1]["content"][1]["content"]["text"], "hi\n");
}

/// Asks one form and returns the answers as JSON.
struct Ask;

impl Tool for Ask {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "Ask".into(),
            description: "ask".into(),
            input_schema: json!({"type": "object"}),
        }
    }

    async fn run(&self, _input: Value, ctx: &Context) -> Result<ToolOutput> {
        let form = Form {
            message: "Pick".into(),
            fields: vec![
                Field {
                    key: "db".into(),
                    title: "Which db?".into(),
                    description: None,
                    kind: FieldKind::Choice {
                        options: vec![Choice {
                            value: "pg".into(),
                            title: "Postgres".into(),
                            description: Some("SQL".into()),
                        }],
                        multiple: false,
                    },
                    required: true,
                },
                Field {
                    key: "note".into(),
                    title: "Note".into(),
                    description: None,
                    kind: FieldKind::Text,
                    required: false,
                },
            ],
        };
        Ok(match ctx.ask(&form).await {
            FormReply::Answers(a) => serde_json::to_string(&a)?,
            FormReply::Declined => "declined".into(),
            FormReply::Unsupported => "unsupported".into(),
        }
        .into())
    }
}

/// Runs a prompt that calls `Ask`; returns what the tool returned.
async fn ask_with(form_support: bool, reply: Option<(&'static str, &'static str)>) -> String {
    let scripted = Scripted::new(vec![
        vec![
            Event::Block(Content::ToolUse {
                id: "a1".into(),
                name: "Ask".into(),
                input: json!({}),
            }),
            Event::Done(StopReason::ToolUse),
        ],
        vec![text("ok"), Event::Done(StopReason::EndTurn)],
    ]);
    let agent = agent_with(scripted.clone(), vec![Arc::new(Ask)]);
    let schemas = Arc::new(Mutex::new(vec![]));
    let seen = schemas.clone();
    let mut capabilities = ClientCapabilities::new();
    if form_support {
        capabilities = capabilities
            .elicitation(ElicitationCapabilities::new().form(ElicitationFormCapabilities::new()));
    }

    let client = Client
        .builder()
        .on_receive_request(
            async move |req: CreateElicitationRequest,
                        responder: Responder<CreateElicitationResponse>,
                        _cx| {
                seen.lock()
                    .unwrap()
                    .push(serde_json::to_value(&req).unwrap());
                responder.respond(match reply {
                    Some((db, note)) => CreateElicitationResponse::new(
                        ElicitationAcceptAction::new().content(
                            [
                                ("db".to_string(), ElicitationContentValue::from(db)),
                                ("note".to_string(), ElicitationContentValue::from(note)),
                            ]
                            .into_iter()
                            .collect::<std::collections::BTreeMap<_, _>>(),
                        ),
                    ),
                    None => CreateElicitationResponse::new(
                        agent_client_protocol::schema::v1::OtherElicitationAction::new(
                            "_never",
                            Default::default(),
                        ),
                    ),
                })
            },
            agent_client_protocol::on_receive_request!(),
        )
        .connect_with(rho::acp::agent(agent), async |cx: ConnectionTo<_>| {
            cx.send_request(
                InitializeRequest::new(ProtocolVersion::V1).client_capabilities(capabilities),
            )
            .block_task()
            .await?;
            let session = cx
                .send_request(NewSessionRequest::new(std::env::current_dir().unwrap()))
                .block_task()
                .await?;
            cx.send_request(prompt(&session.session_id, "ask"))
                .block_task()
                .await?;
            Ok(())
        });
    timed(client).await;

    if form_support {
        let schemas = schemas.lock().unwrap();
        assert_eq!(schemas.len(), 1);
        assert_eq!(schemas[0]["message"], "Pick");
        assert_eq!(schemas[0]["mode"], "form");
        assert_eq!(schemas[0]["requestedSchema"]["required"], json!(["db"]));
        assert_eq!(
            schemas[0]["requestedSchema"]["properties"]["db"]["oneOf"][0]["title"],
            "Postgres"
        );
    }

    let requests = scripted.requests.lock().unwrap();
    let Content::ToolResult { content, .. } = &requests[1].messages.last().unwrap().content[0]
    else {
        panic!("no tool result");
    };
    content.clone()
}

#[tokio::test]
async fn images_go_to_the_model_and_bad_ones_are_refused() {
    let scripted = Scripted::new(vec![vec![text("a cat"), Event::Done(StopReason::EndTurn)]]);
    let agent = agent_with(scripted.clone(), vec![]);
    // A 1x1 PNG.
    const PNG: &str = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mNk+M9QDwADhgGAWjR9awAAAABJRU5ErkJggg==";

    let client =
        Client
            .builder()
            .connect_with(rho::acp::agent(agent), async |cx: ConnectionTo<_>| {
                let init = cx
                    .send_request(InitializeRequest::new(ProtocolVersion::V1))
                    .block_task()
                    .await?;
                assert!(init.agent_capabilities.prompt_capabilities.image);
                let session = cx
                    .send_request(NewSessionRequest::new(std::env::current_dir().unwrap()))
                    .block_task()
                    .await?;
                let sid = session.session_id;

                // Refused before it reaches the history, so the session goes on.
                let tiff = PromptRequest::new(
                    sid.clone(),
                    vec![ContentBlock::Image(ImageContent::new(PNG, "image/tiff"))],
                );
                let err = cx.send_request(tiff).block_task().await.unwrap_err();
                assert!(
                    format!("{err:?}").contains("unsupported image type"),
                    "{err:?}"
                );

                let prompt = PromptRequest::new(
                    sid,
                    vec![
                        ContentBlock::Text(TextContent::new("what is this?")),
                        ContentBlock::Image(ImageContent::new(PNG, "image/png")),
                    ],
                );
                cx.send_request(prompt).block_task().await?;
                Ok(())
            });
    timed(client).await;

    let requests = scripted.requests.lock().unwrap();
    assert_eq!(requests.len(), 1);
    assert_eq!(
        requests[0].messages[0].content,
        vec![
            Content::text("what is this?"),
            Content::image("image/png", PNG)
        ]
    );
}

#[tokio::test]
async fn read_image_sends_the_image_to_model_and_client() {
    use base64::Engine;
    const PNG: &str = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mNk+M9QDwADhgGAWjR9awAAAABJRU5ErkJggg==";
    let dir = tempfile::tempdir().unwrap();
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(PNG)
        .unwrap();
    std::fs::write(dir.path().join("pic.png"), bytes).unwrap();
    let scripted = Scripted::new(vec![
        vec![
            Event::Block(Content::ToolUse {
                id: "i1".into(),
                name: "ReadImage".into(),
                input: json!({"path": "pic.png"}),
            }),
            Event::Done(StopReason::ToolUse),
        ],
        vec![text("a dot"), Event::Done(StopReason::EndTurn)],
    ]);
    let agent = agent_with(scripted.clone(), rho::tools::builtin());
    let updates = Updates::default();
    let record = recording(&updates);
    let cwd = dir.path().to_path_buf();

    let client = Client
        .builder()
        .on_receive_notification(
            async move |n: SessionNotification, _cx| {
                record(n);
                Ok(())
            },
            agent_client_protocol::on_receive_notification!(),
        )
        .connect_with(rho::acp::agent(agent), async |cx: ConnectionTo<_>| {
            cx.send_request(InitializeRequest::new(ProtocolVersion::V1))
                .block_task()
                .await?;
            let session = cx
                .send_request(NewSessionRequest::new(cwd))
                .block_task()
                .await?;
            cx.send_request(prompt(&session.session_id, "look"))
                .block_task()
                .await?;
            Ok(())
        });
    timed(client).await;

    let requests = scripted.requests.lock().unwrap();
    let Content::ToolResult { images, .. } = &requests[1].messages.last().unwrap().content[0]
    else {
        panic!("no tool result");
    };
    assert_eq!(
        images,
        &[rho::provider::ImageSource::Base64 {
            media_type: "image/png".into(),
            data: PNG.into()
        }]
    );

    let shown = updates
        .lock()
        .unwrap()
        .iter()
        .find_map(|u| match u {
            SessionUpdate::ToolCallUpdate(u) => Some(serde_json::to_value(u).unwrap()),
            _ => None,
        })
        .expect("tool call update");
    assert_eq!(shown["content"][1]["content"]["type"], "image");
    assert_eq!(shown["content"][1]["content"]["data"], PNG);
}

#[tokio::test]
async fn questions_are_elicitation_forms() {
    let out = ask_with(true, Some(("pg", "  "))).await;
    // Empty text is dropped.
    assert_eq!(out, r#"{"db":["pg"]}"#);
}

#[tokio::test]
async fn a_dismissed_form_is_declined() {
    assert_eq!(ask_with(true, None).await, "declined");
}

#[tokio::test]
async fn clients_without_forms_get_unsupported() {
    assert_eq!(ask_with(false, Some(("pg", ""))).await, "unsupported");
}
