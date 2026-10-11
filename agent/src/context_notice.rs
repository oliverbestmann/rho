//! `ContextNotice`: tells the user when a run takes the context past a
//! quarter of the window. Once per crossing, so a resumed session that is
//! already above it stays quiet.

use rho::extension::{Context, Extension, RunEnd, SessionInfo};

const PERCENT: u64 = 25;

pub struct ContextNotice;

impl Extension for ContextNotice {
    async fn run_end(&self, ctx: &Context, run: &RunEnd) {
        let now = ctx.session_info();
        if below(&run.before) && !below(&now) {
            ctx.notify(
                &format!(
                    "Context over {PERCENT}%: {}k of {}k tokens.",
                    now.context_used() / 1000,
                    now.context_window / 1000
                ),
                "",
                false,
            );
        }
    }
}

fn below(info: &SessionInfo) -> bool {
    info.context_used() * 100 < info.context_window * PERCENT
}

#[cfg(test)]
mod tests {
    use super::*;
    use rho::provider::Usage;

    #[test]
    fn below_a_quarter() {
        let info = |input| SessionInfo {
            context_window: 200_000,
            last_usage: Usage {
                input,
                ..Default::default()
            },
            ..Default::default()
        };
        assert!(below(&info(49_999)));
        assert!(!below(&info(50_000)));
        // No window known: never below, so no notice.
        assert!(!below(&SessionInfo::default()));
    }
}
