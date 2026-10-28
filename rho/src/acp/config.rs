//! The session's config options (profile, model, thinking) for ACP.

use agent_client_protocol::schema::v1::{
    SessionConfigOption, SessionConfigOptionCategory, SessionConfigSelectOption,
    SetSessionConfigOptionRequest,
};

use crate::agent::{Agent, Session};
use crate::provider::Thinking;

pub(super) const THINKING_OPTION: &str = "thought_level";
pub(super) const PROFILE_OPTION: &str = "mode";

/// The session's profile (if there is a choice), model and thinking level
/// as ACP config options.
pub(super) fn config_options(session: &Session) -> Vec<SessionConfigOption> {
    let mut options = vec![];
    let profiles = session.ctx.profiles();
    if profiles.len() > 1 {
        let choices: Vec<_> = profiles
            .iter()
            .map(|p| {
                SessionConfigSelectOption::new(p.id.to_string(), p.name.clone())
                    .description(p.description.clone())
            })
            .collect();
        options.push(
            SessionConfigOption::select(
                PROFILE_OPTION,
                "Profile",
                session.ctx.profile().id.to_string(),
                choices,
            )
            .category(SessionConfigOptionCategory::Mode)
            .description("What the agent may do in this session"),
        );
    }
    options.extend(model_options(session));
    options
}

fn model_options(session: &Session) -> Vec<SessionConfigOption> {
    let mut models: Vec<_> = session
        .ctx
        .registry()
        .models()
        .into_iter()
        .map(|(spec, m)| {
            let mut description = format!("{spec}, {} context", tokens(m.context_window));
            if let Some(p) = m.price {
                description += &format!(", ${}/${} per MTok", p.input, p.output);
            }
            // The same model can come from several providers.
            let name = format!("{} ({})", m.name, spec.provider().as_str());
            SessionConfigSelectOption::new(spec.to_string(), name).description(description)
        })
        .collect();
    let mut options = vec![];
    // Without any known model there is nothing to pick from.
    if !models.is_empty() {
        // A model from `-m` that the provider does not list.
        let current = session.model.to_string();
        if models.iter().all(|m| *m.value.0 != *current) {
            models.insert(0, SessionConfigSelectOption::new(current.clone(), &current));
        }
        options.push(
            // Per `models` config rule, so clients remember a pick per rule.
            SessionConfigOption::select(session.model_key.clone(), "Model", current, models)
                .category(SessionConfigOptionCategory::Model)
                .description("Model for this session"),
        );
    }
    let levels: Vec<_> = Thinking::ALL
        .iter()
        .map(|t| SessionConfigSelectOption::new(t.as_str(), t.as_str()))
        .collect();
    options.push(
        SessionConfigOption::select(
            THINKING_OPTION,
            "Thinking",
            session.thinking.as_str(),
            levels,
        )
        .category(SessionConfigOptionCategory::ThoughtLevel)
        .description("Reasoning effort for this session"),
    );
    options
}

/// 1_000_000 -> "1M", 200_000 -> "200k".
fn tokens(n: u64) -> String {
    match n {
        n if n >= 1_000_000 && n % 1_000_000 == 0 => format!("{}M", n / 1_000_000),
        n if n >= 1_000 => format!("{}k", n / 1_000),
        n => n.to_string(),
    }
}

pub(super) fn set_config(
    agent: &Agent,
    session: &mut Session,
    req: &SetSessionConfigOptionRequest,
) -> anyhow::Result<()> {
    let Some(value) = req.value.as_value_id().map(|v| &*v.0) else {
        anyhow::bail!("expected a string value");
    };
    match &*req.config_id.0 {
        id if id == session.model_key => {
            let spec = value.parse()?;
            let listed = session
                .ctx
                .registry()
                .models()
                .iter()
                .any(|(m, _)| *m == spec);
            if !listed && spec != session.model {
                anyhow::bail!("unknown model {value:?}");
            }
            agent.set_model(session, &spec)
        }
        THINKING_OPTION => {
            session.thinking = value.parse()?;
            session.publish_info();
            Ok(())
        }
        other => anyhow::bail!("unknown config option {other:?}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokens_are_short() {
        assert_eq!(tokens(1_000_000), "1M");
        assert_eq!(tokens(200_000), "200k");
        assert_eq!(tokens(512), "512");
    }
}
