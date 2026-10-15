//! Masks secrets before they reach the model, with the gitleaks rules
//! (`gitleaks.toml`, copied from gitleaks' `config/gitleaks.toml`).
//!
//! Stage 1 redacts at the source, so the history and the session database
//! only hold `REDACTED`: prompts, the system prompt and tool output. Stage 2
//! scans the JSON body of every model request (`before_send`) and catches
//! what stage 1 does not see (steering messages, `Context::complete`, older
//! sessions); a hit there is shown to the user. Write and Edit calls that
//! would put the marker into a file in place of the real secret are blocked.

use std::collections::{BTreeSet, HashSet};
use std::hash::{BuildHasher, RandomState};
use std::sync::{Arc, LazyLock, Mutex};

use anyhow::Result;
use gitleaks_rs::{Config, Scanner};
use rho::extension::{
    BoxFuture, Context, Decision, Extension, PromptSetup, SessionSetup, ToolCall, ToolOutput,
};
use rho::provider::{Content, ToolSpec};
use rho::tools::DynTool;
use serde_json::{Value, json};

const RULES: &str = include_str!("gitleaks.toml");

/// JSON blocks sent back unchanged: thinking is signed, reasoning encrypted,
/// images are base64.
const OPAQUE: [&str; 5] = [
    "thinking",
    "redacted_thinking",
    "reasoning",
    "image",
    "input_image",
];

/// Compiling the rules takes seconds; `Secrets::new` starts it in the
/// background. The rules are tested to compile (`rules_compile`).
static SCANNER: LazyLock<Scanner> = LazyLock::new(|| {
    Scanner::new(Config::from_toml(RULES).expect("gitleaks.toml")).expect("gitleaks rules")
});

pub struct Secrets {
    /// Hashes of the request body strings without secrets, so each request
    /// only scans what is new.
    clean: Mutex<HashSet<u64>>,
    /// Hashes of the strings with secrets the user was told about.
    reported: Mutex<HashSet<u64>>,
    hasher: RandomState,
}

impl Secrets {
    /// Starts compiling the rules; the first scan waits for it.
    pub fn new() -> Self {
        std::thread::spawn(|| LazyLock::force(&SCANNER));
        Self {
            clean: Mutex::default(),
            reported: Mutex::default(),
            hasher: RandomState::new(),
        }
    }

    /// Replaces the secrets in `text` with `REDACTED`; returns their rule ids.
    fn redact(&self, text: &mut String) -> Vec<String> {
        // As one line, like gitleaks: some rules span lines (private keys).
        let result = SCANNER.redact_line(text, None);
        if result.redaction_count > 0 {
            *text = result.content;
        }
        result.findings.into_iter().map(|f| f.rule_id).collect()
    }

    /// `redact` on every string in `value`, except in `OPAQUE` blocks. Adds
    /// the rule ids of secrets not reported before to `rules`.
    fn redact_json(&self, value: &mut Value, rules: &mut BTreeSet<String>) {
        match value {
            Value::String(s) => {
                let hash = self.hasher.hash_one(&*s);
                if self.clean.lock().unwrap().contains(&hash) {
                    return;
                }
                let found = self.redact(s);
                if found.is_empty() {
                    self.clean.lock().unwrap().insert(hash);
                } else if self.reported.lock().unwrap().insert(hash) {
                    rules.extend(found);
                }
            }
            Value::Array(items) => items.iter_mut().for_each(|v| self.redact_json(v, rules)),
            Value::Object(map) => {
                let kind = map.get("type").and_then(Value::as_str);
                if kind.is_some_and(|k| OPAQUE.contains(&k)) {
                    return;
                }
                map.values_mut().for_each(|v| self.redact_json(v, rules));
            }
            _ => {}
        }
    }
}

fn markers(text: &str) -> usize {
    text.matches(rho::store::MASK).count()
}

/// Why a Write or Edit is blocked: it adds `REDACTED` to the file.
const BLOCKED: &str = "this would write REDACTED into the file, where a secret was masked. You never see the real secret; leave the masked part out of your change (for Edit, choose oldText around it). If you really mean to write the marker itself (e.g. in a test or docs), repeat the call with allowMasked: true";

/// Write or Edit with an extra `allowMasked` input, for calls that mean to
/// write the marker itself; stripped before the tool runs.
struct AllowMasked(Arc<dyn DynTool>);

impl DynTool for AllowMasked {
    fn spec(&self) -> ToolSpec {
        let mut spec = self.0.spec();
        spec.input_schema["properties"]["allowMasked"] = json!({
            "type": "boolean",
            "description": "Only when a call was blocked for writing the masked-secret marker and you really mean to write the marker itself",
        });
        spec
    }
    fn run<'a>(&'a self, mut input: Value, ctx: &'a Context) -> BoxFuture<'a, Result<ToolOutput>> {
        if let Some(o) = input.as_object_mut() {
            o.remove("allowMasked");
        }
        self.0.run(input, ctx)
    }
    fn describe(&self, input: &Value) -> Option<String> {
        self.0.describe(input)
    }
    fn rules(&self) -> &'static [&'static str] {
        self.0.rules()
    }
    fn kind(&self) -> rho::tools::ToolKind {
        self.0.kind()
    }
    fn titled(&self) -> bool {
        self.0.titled()
    }
}

impl Extension for Secrets {
    async fn session_start(&self, session: &mut SessionSetup) {
        for tool in &mut session.tools {
            let name = tool.spec().name;
            if name == rho::edit::EDIT || name == rho::tools::WRITE {
                *tool = Arc::new(AllowMasked(tool.clone()));
            }
        }
    }

    async fn before_prompt(&self, _ctx: &Context, prompt: &mut PromptSetup) {
        self.redact(&mut prompt.system);
        for content in &mut prompt.content {
            if let Content::Text { text } = content {
                self.redact(text);
            }
        }
    }

    async fn tool_result(&self, _ctx: &Context, _call: &ToolCall, result: &mut ToolOutput) {
        self.redact(&mut result.output);
    }

    fn before_send(&self, ctx: &Context, body: &mut Value) {
        let mut rules = BTreeSet::new();
        self.redact_json(body, &mut rules);
        if !rules.is_empty() {
            let rules: Vec<_> = rules.into_iter().collect();
            ctx.notify(
                "Secret redacted in request",
                &format!("Rules: {}", rules.join(", ")),
                false,
            );
        }
    }

    async fn tool_call(&self, ctx: &Context, call: &mut ToolCall) -> Decision {
        let input = &call.input;
        if input["allowMasked"] == true {
            return Decision::Allow;
        }
        let text = |v: &Value| v.as_str().unwrap_or_default().to_string();
        let adds = if call.name == rho::edit::EDIT {
            let edits = input["edits"].as_array().into_iter().flatten();
            edits
                .into_iter()
                .any(|e| markers(&text(&e["newText"])) > markers(&text(&e["oldText"])))
        } else if call.name == rho::tools::WRITE {
            let content = text(&input["content"]);
            if markers(&content) == 0 {
                return Decision::Allow;
            }
            let path = ctx.cwd().join(text(&input["path"]));
            let old = ctx
                .run_sandboxed(move || std::fs::read_to_string(path).unwrap_or_default())
                .await
                .unwrap_or_default();
            markers(&content) > markers(&old)
        } else {
            false
        };
        if adds {
            Decision::Block(BLOCKED.into())
        } else {
            Decision::Allow
        }
    }
}

#[cfg(test)]
mod tests {

    use rho::extension::NoFrontend;
    use rho::provider::{Registry, ToolCallId, ToolName};
    use rho::store::SessionId;
    use serde_json::json;

    use super::*;

    const TOKEN: &str = "ghp_xK4mN8pQ2rT6vW0yB3dF5hJ7lO9sU1wE3a5b";

    fn ctx(cwd: &std::path::Path) -> Context {
        let ctx = Context::new(
            SessionId::new("t"),
            cwd,
            Arc::new(NoFrontend),
            Registry::default(),
        );
        ctx.set_sandbox(None);
        ctx
    }

    fn call(name: ToolName, input: Value) -> ToolCall {
        ToolCall {
            id: ToolCallId::new("c"),
            name,
            input,
        }
    }

    #[tokio::test]
    async fn write_and_edit_get_allow_masked() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = ctx(dir.path());
        let mut setup = SessionSetup {
            ctx: ctx.clone(),
            tools: rho::tools::builtin(),
            system: String::new(),
            commands: vec![],
            resumed: false,
            profiles: vec![],
            features: vec![],
        };
        Secrets::new().session_start(&mut setup).await;
        let tool = |name| {
            let t = setup.tools.iter().find(|t| t.spec().name == name).unwrap();
            (
                t.clone(),
                t.spec().input_schema["properties"]["allowMasked"].is_object(),
            )
        };
        assert!(tool(rho::edit::EDIT).1);
        assert!(!tool(rho::tools::BASH).1);
        let (write, has) = tool(rho::tools::WRITE);
        assert!(has);
        let input = json!({"path": "f", "content": "x", "allowMasked": true});
        write.run(input, &ctx).await.unwrap();
        assert_eq!(std::fs::read_to_string(dir.path().join("f")).unwrap(), "x");
    }

    #[test]
    fn rules_compile() {
        assert!(SCANNER.rule_count() > 200);
    }

    #[tokio::test]
    async fn redacts_prompts_and_tool_output() {
        let s = Secrets::new();
        let ctx = ctx(std::path::Path::new("/"));
        let mut prompt = PromptSetup {
            content: vec![Content::text(format!("use GITHUB_TOKEN={TOKEN} please"))],
            system: "export AWS_ACCESS_KEY_ID=AKIAQWERTYUIO2QBKPXN".into(),
        };
        s.before_prompt(&ctx, &mut prompt).await;
        assert_eq!(
            prompt.content[0],
            Content::text("use GITHUB_TOKEN=REDACTED please")
        );
        assert!(!prompt.system.contains("AKIA"), "{}", prompt.system);

        let mut out = ToolOutput::from(format!("line\ntoken: {TOKEN}\nend"));
        s.tool_result(&ctx, &call(rho::tools::BASH, json!({})), &mut out)
            .await;
        assert_eq!(out.output, "line\ntoken: REDACTED\nend");
    }

    #[test]
    fn redacts_private_keys() {
        let s = Secrets::new();
        let key = "-----BEGIN RSA PRIVATE KEY-----\nMIIEpAIBAAKCAQEAu1SU1LfVLPHCozMxH2Mo4lgOEePzNm0tRgeLezV6ffAt0gunVTLw7onLRnrq0/IzW7yWR7QkrmBL7jTKEn5u+qKhbwKfBstIs+bMY2Zkp18gnTxKLxoS2tFczGkPLPgizskuemMghRniWaoLcyehkd3qqGElvW/VDL5AaWTg0nLVkjRo9z+40RQzuVaE8AkAFmxZzow3x+VJYKdjykkJ0iT9wCS0DRTXu269V264Vf/3jvredZiKRkgwlL9xNAwxXFg0x/XFw005UWVRIkdgcKWTjpBP2dPwVZ4WWC+9aGVd+Gyn1o0CLelf4rEjGoXbAAEgAqeGUxrcIlbjXfbcmwIDAQABAoIBAQCZE6m+4f1OT1EZAc8+UEPS0SeOjDrjYHZt2QjcE+EDIvMbzrj1ORQk8HbpfUcj8k5yGlGd2kDEBk10gEPmmWj+0IaRtyKzWSgaJWHtOfcd5IA5RknLM8HJ0AJsVXHHsh0bVfQlm6hl2r7Ng6S+MlexCB2N0XAdBlI7SV9k1i2wQLeRbHnvYwuAZB+jH4GxhOFv8uc1RzD5mbXKbpHGJ2t08b9WNrAH7l7c6rpo+wSwnF1jB6lFJ1N0n0vR0n7dS02hmpk8z8Y0cYjS9bPprmRfeGDaVhJ0djb3LgHb+UZ5fu3qX9vszo/8wkGnS6iHJcHmMxjxaeTYGLlb9b6JqbcBAoGBAO/MBx2Wq0yOO2O/Y3Kc0mUxiwOzc+4L+DzSXT1ZhXkrRQUm5Ln6Ss/Vq0Ik/LS4ZAxcGeCbEkFR2XPIZTCZUGm/GRmEKxp/wv0Aj3jIhBqzRtyRvxBzHMlK3PMuKTHi4b9bk+JXb1iIymXJcjOmgD4qJn2dKMfvH8ilG3YmqJBAoGBAMgr6b8JyuHrRjMDvRQnpbg0iq4uq0D18nbTIu4D8ydzJjrfNdzTr4u0eQgD0SwI9LXKJMmEbNQmDMaE0XHBnS6UhHzLgL07RO41vRSQbXsI4rUr6pOD8ws8wb0CfN/dWLvnUgZbLW7JtG4WyNNEpkvd4H/ULqhd+t7Hgb1RpqK7AoGAOcBXlCuHvbOfkpgQPhaX4BwxyN8BvMHkb3vXqQ8MI4qTO6wQOvBF0YSl0q4yy1M9JYfYcz8oAubBRdq2ySoSBA5fHfpb4WknZK9PNZkpzaXw9Ckwn6FRL1fC2Ex1FBMtLWcZEkMRWRnhJ1uJqBjGXZHDySX8rhpcFKMqh9rvwF0CgYBHeu5ld/QWm9XZz0E+7SFF1j5Bc4Rv7vW8VRGPeK1b+CckEiJqzfFUXJjaqjS6FxPP7qrRSgkPz/9cQGsAMoGmvDVlBUXl8Tj6ezyOT6FC+lwTvPhJFPGxFbcd0mIYwy1bVtOgzm48D3RU7oH1jC1DGNm6YP+Hd0f5qzfFq6pf+wKBgQDMWwiyVrchvqv4EqdEnOEb/BnrNXdM2yqLAPdpXkMxTHJGKsCYV3cfFuD5ZgZt5ztJLW5j6jyjpnS2dKt8NlDyvDoX7I4PCGUmDvSxjQqNHk+eDy0z+gnxRQGRzBUBrrj1xxV28YHvnsGEE0mgJWdNoAjaC85TjbB4EQIYaEuCMQ==\n-----END RSA PRIVATE KEY-----";
        let mut text = format!("id_rsa:\n{key}\ndone");
        let rules = s.redact(&mut text);
        assert!(rules.contains(&"private-key".to_string()), "{rules:?}");
        assert!(!text.contains("MIIEpAIBAAKCAQEA"), "{text}");
        assert!(text.ends_with("done"), "{text}");
    }

    #[test]
    fn request_bodies_are_redacted_except_opaque_blocks() {
        let s = Secrets::new();
        let mut body = json!({"messages": [
            {"role": "user", "content": [{"type": "text", "text": format!("t={TOKEN}")}]},
            {"role": "assistant", "content": [
                {"type": "thinking", "thinking": format!("t={TOKEN}"), "signature": "s"},
                {"type": "tool_use", "id": "1", "name": "Bash", "input": {"command": format!("echo {TOKEN}")}},
            ]},
        ]});
        let original = body.clone();
        let mut rules = BTreeSet::new();
        s.redact_json(&mut body, &mut rules);
        assert!(rules.contains("github-pat"), "{rules:?}");
        // The next request is redacted again, but not reported again.
        let mut again = original.clone();
        let mut rules_again = BTreeSet::new();
        s.redact_json(&mut again, &mut rules_again);
        assert_eq!(again, body);
        assert!(rules_again.is_empty());
        assert_eq!(body["messages"][0]["content"][0]["text"], "t=REDACTED");
        assert_eq!(
            body["messages"][1]["content"][1]["input"]["command"],
            "echo REDACTED"
        );
        assert_eq!(
            body["messages"][1]["content"][0]["thinking"],
            format!("t={TOKEN}")
        );
    }

    #[tokio::test]
    async fn blocks_writing_the_marker_into_files() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("old"), "a=REDACTED\n").unwrap();
        let (s, ctx) = (Secrets::new(), ctx(dir.path()));
        let decide = |name, input| {
            let (s, ctx) = (&s, &ctx);
            async move { s.tool_call(ctx, &mut call(name, input)).await }
        };
        let write = |path: &str, content: &str| json!({"path": path, "content": content});
        let edit = |old: &str, new: &str| json!({"path": "f", "edits": [{"oldText": "x", "newText": "y"}, {"oldText": old, "newText": new}]});
        use rho::edit::EDIT;
        use rho::tools::WRITE;
        assert!(matches!(
            decide(WRITE, write("new", "k=REDACTED")).await,
            Decision::Block(_)
        ));
        assert_eq!(decide(WRITE, write("new", "k=1")).await, Decision::Allow);
        // The file already had the marker.
        assert_eq!(
            decide(WRITE, write("old", "a=REDACTED\nb=2")).await,
            Decision::Allow
        );
        assert!(matches!(
            decide(EDIT, edit("k=1", "k=REDACTED")).await,
            Decision::Block(_)
        ));
        let mut allowed = write("new", &format!("k={}", rho::store::MASK));
        allowed["allowMasked"] = true.into();
        assert_eq!(decide(WRITE, allowed).await, Decision::Allow);
        assert_eq!(
            decide(EDIT, edit("k=REDACTED", "k=REDACTED\nl=2")).await,
            Decision::Allow
        );
    }
}
