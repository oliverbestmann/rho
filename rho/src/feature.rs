//! Features: named bundles of sandbox access and tools that the user turns
//! on and off during a session (`/enable ssh git`, `/disable ssh`).
//!
//! A tool listed by any feature is hidden until one of its features is on.
//! Features come from `features.json` in the config dir and from extensions
//! (`SessionSetup::features`).

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context as _, Result, bail};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::Value;

use crate::extension::{CommandName, Context, Extension, SessionSetup, SlashCommand, ToolOutput};
use crate::provider::{ToolName, ToolSpec};
use crate::sandbox::{Access, Policy};
use crate::tools::{Tool, input_schema, parse};

pub const REQUEST_FEATURE: ToolName = ToolName::from_static("RequestFeature");

crate::id_type! {
    /// A `Feature`'s id, e.g. `ssh`; what the user types after `/enable`.
    pub FeatureId
}

/// `features.json` holds a list of these. Paths are absolute or start with `~/`.
///
/// ```json
/// [{ "id": "ssh", "description": "SSH keys", "read": ["~/.ssh"] }]
/// ```
#[derive(Clone, Debug, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Feature {
    pub id: FeatureId,
    #[serde(default)]
    pub description: String,
    /// On when a session starts.
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub read: Vec<PathBuf>,
    #[serde(default)]
    pub read_write: Vec<PathBuf>,
    /// Hidden from the model unless a feature listing them is on.
    #[serde(default)]
    pub tools: Vec<ToolName>,
    /// Told to the model when the feature is turned on.
    #[serde(default)]
    pub instructions: Option<String>,
}

pub const FILE: &str = "features.json";

/// The features in `features.json`. A missing file means none; an invalid one is an error.
pub fn load(config_dir: &Path) -> Result<Vec<Feature>> {
    let path = config_dir.join(FILE);
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(vec![]),
        Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
    };
    let mut features: Vec<Feature> =
        serde_json::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;
    for f in &mut features {
        if f.id.as_str().is_empty() || f.id.as_str().contains(char::is_whitespace) {
            bail!("{}: invalid feature id {:?}", path.display(), f.id.as_str());
        }
        crate::sandbox::expand(f.read.iter_mut().chain(&mut f.read_write), &path)?;
    }
    for (i, f) in features.iter().enumerate() {
        if features[..i].iter().any(|g| g.id == f.id) {
            bail!("{}: feature {} is defined twice", path.display(), f.id);
        }
    }
    Ok(features)
}

impl Feature {
    pub(crate) fn allow(&self, policy: &mut Policy) {
        for p in &self.read {
            policy.allow(p, Access::Read);
        }
        for p in &self.read_write {
            policy.allow(p, Access::ReadWrite);
        }
    }

    /// One sentence for the model about the feature being turned on.
    pub(crate) fn turned_on(&self, sandboxed: bool) -> String {
        let mut text = format!("The {} feature is on", self.id);
        if !self.description.is_empty() {
            text += &format!(" ({})", self.description);
        }
        let list = |paths: &[PathBuf]| {
            let paths: Vec<_> = paths.iter().map(|p| p.display().to_string()).collect();
            paths.join(", ")
        };
        if sandboxed && !self.read.is_empty() {
            text += &format!("; the sandbox allows reading {}", list(&self.read));
        }
        if sandboxed && !self.read_write.is_empty() {
            text += &format!("; the sandbox allows writing {}", list(&self.read_write));
        }
        text + "."
    }

    /// Markdown for the confirmation: the description, then a list per
    /// access kind and the tools.
    fn details(&self) -> String {
        let mut text = self.description.clone();
        let mut list = |title: &str, items: Vec<String>| {
            if !items.is_empty() {
                text += &format!("\n\n{title}:");
                for item in items {
                    text += &format!("\n- `{item}`");
                }
            }
        };
        let paths = |paths: &[PathBuf]| paths.iter().map(|p| p.display().to_string()).collect();
        list("Read", paths(&self.read));
        list("Read-write", paths(&self.read_write));
        list(
            "Tools",
            self.tools.iter().map(ToString::to_string).collect(),
        );
        text.trim_start().to_string()
    }

    /// One line for the system prompt's list.
    fn listed(&self) -> String {
        let mut line = format!("- {}", self.id);
        if !self.description.is_empty() {
            line += &format!(": {}", self.description);
        }
        let mut parts = vec![];
        let mut paths = |what: &str, paths: &[PathBuf]| {
            if !paths.is_empty() {
                let paths: Vec<_> = paths.iter().map(|p| p.display().to_string()).collect();
                parts.push(format!("{what} {}", paths.join(", ")));
            }
        };
        paths("read", &self.read);
        paths("read-write", &self.read_write);
        if !self.tools.is_empty() {
            let tools: Vec<_> = self.tools.iter().map(ToolName::as_str).collect();
            parts.push(format!("tools {}", tools.join(", ")));
        }
        if !parts.is_empty() {
            line += &format!(" ({})", parts.join("; "));
        }
        line
    }
}

/// Whether `tool` may be called with the `active` features on: either no
/// feature gates it, or an active one lists it.
pub(crate) fn unlocks(features: &[Feature], active: &[FeatureId], tool: &ToolName) -> bool {
    let mut gating = features
        .iter()
        .filter(|f| f.tools.contains(tool))
        .peekable();
    gating.peek().is_none() || gating.any(|f| active.contains(&f.id))
}

/// `/features`, `/enable <id>...` and `/disable <id>...` for the user; the
/// list of features in the system prompt and the `RequestFeature` tool for
/// the model. Register it after extensions that add features, so they are
/// listed.
pub struct FeatureCommands;

const FEATURES: CommandName = CommandName::from_static("features");
const ENABLE: CommandName = CommandName::from_static("enable");
const DISABLE: CommandName = CommandName::from_static("disable");

impl Extension for FeatureCommands {
    async fn session_start(&self, session: &mut SessionSetup) {
        session.commands.push(SlashCommand {
            name: FEATURES,
            description: "List the features and whether they are on".into(),
            hint: None,
        });
        for (name, what) in [(ENABLE, "Turn on"), (DISABLE, "Turn off")] {
            session.commands.push(SlashCommand {
                name,
                description: format!("{what} features (sandbox access and tools)"),
                hint: Some("feature ...".into()),
            });
        }
        if session.features.is_empty() {
            return;
        }
        // Only the definitions: they do not change, so the prompt cache
        // holds. Which are on is told in the history.
        let lines: Vec<_> = session.features.iter().map(Feature::listed).collect();
        session.system += &format!(
            "\n\n<features>\nFeatures add sandbox access and tools. The user turns them on and off; \
             you are told when that happens. If the task needs one that is off, ask for it with \
             {REQUEST_FEATURE}.\n{}\n</features>",
            lines.join("\n")
        );
        session.tools.push(Arc::new(RequestFeature));
    }

    async fn command(&self, ctx: &Context, name: &CommandName, args: &str) -> Option<String> {
        if *name == FEATURES {
            return Some(list(ctx));
        }
        let on = if *name == ENABLE {
            true
        } else if *name == DISABLE {
            false
        } else {
            return None;
        };
        let ids: Vec<_> = args.split_whitespace().map(FeatureId::new).collect();
        if ids.is_empty() {
            return Some(list(ctx));
        }
        let mut lines = vec![];
        for id in &ids {
            lines.push(match ctx.set_feature(id, on) {
                Ok(true) => format!("{id} is {}.", if on { "on" } else { "off" }),
                Ok(false) => format!("{id} was already {}.", if on { "on" } else { "off" }),
                Err(e) => format!("{e:#}"),
            });
        }
        Some(lines.join("\n"))
    }
}

fn list(ctx: &Context) -> String {
    let features = ctx.features();
    if features.is_empty() {
        return format!("No features defined; add them to {FILE} in the config dir.");
    }
    let active = ctx.active_features();
    let lines: Vec<_> = features
        .iter()
        .map(|f| {
            let state = if active.contains(&f.id) { "on" } else { "off" };
            format!("- `{}` ({state}) {}", f.id, f.description)
        })
        .collect();
    lines.join("\n")
}

#[derive(Deserialize, JsonSchema)]
struct RequestFeatureInput {
    /// the feature's id
    feature: FeatureId,
    /// why it is needed, shown to the user
    reason: String,
}

/// Lets the model ask the user to turn on a feature.
pub struct RequestFeature;

impl Tool for RequestFeature {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: REQUEST_FEATURE,
            description:
                "Ask the user to turn on a feature from <features> for the rest of the session."
                    .into(),
            input_schema: input_schema::<RequestFeatureInput>(),
        }
    }

    async fn run(&self, input: Value, ctx: &Context) -> Result<ToolOutput> {
        let i: RequestFeatureInput = parse(input)?;
        let Some(feature) = ctx.features().into_iter().find(|f| f.id == i.feature) else {
            bail!("unknown feature {}", i.feature);
        };
        if ctx.active_features().contains(&feature.id) {
            return Ok(format!("The {} feature is already on.", feature.id).into());
        }
        let title = format!(
            "Turn on the {} feature?\n{}\n\n{}",
            feature.id,
            feature.details(),
            i.reason
        );
        if !ctx.confirm(None, &title).await {
            bail!("the user did not turn on the {} feature", feature.id);
        }
        ctx.set_feature(&feature.id, true)?;
        Ok(format!("The {} feature is on.", feature.id).into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn load_validates_and_expands() {
        let dir = tempfile::tempdir().unwrap();
        assert!(load(dir.path()).unwrap().is_empty());
        let file = dir.path().join(FILE);
        std::fs::write(
            &file,
            r#"[{"id": "ssh", "read": ["~/.ssh"], "tools": ["X"]}]"#,
        )
        .unwrap();
        let features = load(dir.path()).unwrap();
        let home = PathBuf::from(std::env::var_os("HOME").unwrap());
        assert_eq!(features[0].read, [home.join(".ssh")]);
        assert!(!features[0].enabled);

        for bad in [
            r#"[{"id": "a b"}]"#,
            r#"[{"id": "a"}, {"id": "a"}]"#,
            r#"[{"id": "a", "read": ["rel"]}]"#,
            r#"[{"id": "a", "nope": 1}]"#,
        ] {
            std::fs::write(&file, bad).unwrap();
            assert!(load(dir.path()).is_err(), "{bad}");
        }
    }

    fn feature(id: &str, tools: &[&str]) -> Feature {
        Feature {
            id: FeatureId::new(id),
            description: "desc".into(),
            enabled: false,
            read: vec!["/r".into()],
            read_write: vec![],
            tools: tools.iter().map(|t| ToolName::new(*t)).collect(),
            instructions: None,
        }
    }

    /// Approves every confirmation.
    struct Yes;

    impl crate::extension::Frontend for Yes {
        fn event(&self, _event: crate::agent::AgentEvent<'_>) {}
        fn confirm<'a>(
            &'a self,
            _id: Option<&'a crate::provider::ToolCallId>,
            _title: &'a str,
        ) -> crate::extension::BoxFuture<'a, bool> {
            Box::pin(async { true })
        }
    }

    fn ctx(frontend: Arc<dyn crate::extension::Frontend>) -> Context {
        let id = crate::store::SessionId::new("s");
        let ctx = Context::new(id, "/tmp", frontend, Default::default());
        ctx.set_features(vec![feature("ssh", &["T"])]);
        ctx
    }

    #[tokio::test]
    async fn request_feature_needs_approval() {
        let ask = |id: &str| serde_json::json!({"feature": id, "reason": "push"});
        let denied = ctx(Arc::new(crate::extension::NoFrontend));
        assert!(RequestFeature.run(ask("ssh"), &denied).await.is_err());
        assert!(denied.active_features().is_empty());

        let ctx = ctx(Arc::new(Yes));
        let err = RequestFeature.run(ask("nope"), &ctx).await.unwrap_err();
        assert!(err.to_string().contains("unknown feature nope"));
        RequestFeature.run(ask("ssh"), &ctx).await.unwrap();
        assert_eq!(ctx.active_features(), [FeatureId::new("ssh")]);
        let again = RequestFeature.run(ask("ssh"), &ctx).await.unwrap();
        assert!(again.output.contains("already on"));
    }

    #[tokio::test]
    async fn features_are_listed_in_the_prompt_with_the_tool() {
        let mut setup = SessionSetup {
            ctx: ctx(Arc::new(crate::extension::NoFrontend)),
            tools: vec![],
            system: String::new(),
            commands: vec![],
            resumed: false,
            profiles: vec![],
            features: vec![],
        };
        FeatureCommands.session_start(&mut setup).await;
        assert!(setup.system.is_empty() && setup.tools.is_empty());
        assert_eq!(setup.commands.len(), 3);

        setup.features = vec![feature("ssh", &["T", "U"])];
        FeatureCommands.session_start(&mut setup).await;
        assert!(
            setup
                .system
                .contains("- ssh: desc (read /r; tools T, U)\n</features>"),
            "{}",
            setup.system
        );
        assert_eq!(setup.tools[0].spec().name, REQUEST_FEATURE);
    }

    #[test]
    fn details_list_paths_and_tools() {
        let mut f = feature("ssh", &["T"]);
        f.read_write = vec!["/w".into(), "/x".into()];
        assert_eq!(
            f.details(),
            "desc\n\nRead:\n- `/r`\n\nRead-write:\n- `/w`\n- `/x`\n\nTools:\n- `T`"
        );
        f.description.clear();
        assert!(f.details().starts_with("Read:"));
    }

    #[test]
    fn gated_tools_need_an_active_feature() {
        let f = |id: &str, tools: &[&str]| Feature {
            id: FeatureId::new(id),
            description: String::new(),
            enabled: false,
            read: vec![],
            read_write: vec![],
            tools: tools.iter().map(|t| ToolName::new(*t)).collect(),
            instructions: None,
        };
        let features = [f("a", &["T"]), f("b", &["T", "U"])];
        let (t, u, free) = (ToolName::new("T"), ToolName::new("U"), ToolName::new("V"));
        assert!(unlocks(&features, &[], &free));
        assert!(!unlocks(&features, &[], &t));
        assert!(unlocks(&features, &[FeatureId::new("a")], &t));
        assert!(!unlocks(&features, &[FeatureId::new("a")], &u));
        assert!(unlocks(&features, &[FeatureId::new("b")], &u));
    }
}
