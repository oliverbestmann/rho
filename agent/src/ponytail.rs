//! Adds the ponytail skill ("lazy senior dev": the smallest complete change)
//! to the system prompt, at its default "full" level.
//!
//! `ponytail.md` is SKILL.md from <https://github.com/DietrichGebert/ponytail>
//! (MIT), without its front matter, the level switching (rho has no
//! `/ponytail` command) and the levels table. Update it by hand.

use rho::extension::{Extension, SessionSetup};

const SKILL: &str = include_str!("ponytail.md");

pub struct Ponytail;

impl Extension for Ponytail {
    async fn session_start(&self, session: &mut SessionSetup) {
        session.system += &format!("\n\n<ponytail>\n{}\n</ponytail>", SKILL.trim());
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use rho::extension::{Context, NoFrontend, Profile};
    use rho::store::SessionId;

    #[tokio::test]
    async fn appends_the_skill_to_the_system_prompt() {
        let mut setup = SessionSetup {
            ctx: Context::new(
                SessionId::new("s"),
                "/tmp",
                Arc::new(NoFrontend),
                Default::default(),
            ),
            tools: vec![],
            system: "base".into(),
            commands: vec![],
            resumed: false,
            profiles: vec![Profile::write()],
        };
        Ponytail.session_start(&mut setup).await;
        assert!(setup.system.starts_with("base\n\n<ponytail>\n# Ponytail\n"));
        assert!(
            setup
                .system
                .ends_with("anything the user asked for.\n</ponytail>")
        );
        assert!(!setup.system.contains("/ponytail lite") && !setup.system.contains("ultra"));
    }
}
