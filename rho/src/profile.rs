//! Profiles: ways of working the session can switch between.

use crate::extension::Context;
use crate::feature::FeatureId;
use crate::provider::{ToolName, ToolSpec};

crate::id_type! {
    /// A `Profile`'s id, e.g. `write` or `plan`; stored with the session.
    pub ProfileId
}

/// `Profile::WRITE`, the profile every session has.
impl Default for ProfileId {
    fn default() -> Self {
        Profile::WRITE
    }
}

/// A way of working the user (or an extension, `Context::set_profile`) can
/// switch to during a session, e.g. a read-only "plan" profile. Switching
/// tells the model which tools appeared or disappeared, and the profile's
/// instructions.
#[derive(Clone, Debug, PartialEq)]
pub struct Profile {
    pub id: ProfileId,
    pub name: String,
    pub description: String,
    /// Tools the model does not get in this profile; calls to them fail.
    pub hidden_tools: Vec<ToolName>,
    /// Sandboxed tools can only read: read-write rules apply as read-only.
    pub read_only: bool,
    /// Told to the model when the session switches to this profile.
    pub instructions: Option<String>,
}

impl Profile {
    pub const WRITE: ProfileId = ProfileId::from_static("write");

    /// The default: every tool, the full sandbox.
    pub fn write() -> Self {
        Self {
            id: Self::WRITE,
            name: "Write".into(),
            description: "Read and change files".into(),
            hidden_tools: vec![],
            read_only: false,
            instructions: None,
        }
    }

    pub fn shows(&self, tool: &ToolName) -> bool {
        !self.hidden_tools.contains(tool)
    }
}

/// What the model needs to know about the profile and features, once per
/// change: on the first request if anything is restricted or on, then
/// after each switch. The tools it must not call, the ones it may call
/// now, the sandbox, and the instructions.
pub(crate) fn note(
    ctx: &Context,
    seen: &mut Option<(ProfileId, Vec<FeatureId>)>,
    specs: &[ToolSpec],
) -> Option<String> {
    let profile = ctx.profile();
    let features = ctx.features();
    let now = (profile.id.clone(), ctx.active_features());
    let seen = seen.replace(now.clone());
    if seen.as_ref() == Some(&now) {
        return None;
    }
    let active = &now.1;
    let profiles = ctx.profiles();
    let before = seen
        .as_ref()
        .and_then(|(id, _)| profiles.iter().find(|p| &p.id == id));
    let was_active = seen.as_ref().map(|(_, a)| a.as_slice());
    // Before the first request, every tool counts as callable.
    let callable = |p: Option<&Profile>, on: Option<&[FeatureId]>, n: &ToolName| {
        p.is_none_or(|p| p.shows(n))
            && on.is_none_or(|on| crate::feature::unlocks(&features, on, n))
    };
    let changed = |gone: bool| -> Vec<&str> {
        specs
            .iter()
            .map(|s| &s.name)
            .filter(|n| {
                let was = callable(before, was_active, n);
                let is = callable(Some(&profile), Some(active), n);
                if gone { was && !is } else { !was && is }
            })
            .map(ToolName::as_str)
            .collect()
    };
    let (removed, added) = (changed(true), changed(false));
    let old = was_active.unwrap_or_default();
    let turned_on: Vec<_> = features
        .iter()
        .filter(|f| active.contains(&f.id) && !old.contains(&f.id))
        .collect();
    let turned_off = features
        .iter()
        .filter(|f| !active.contains(&f.id) && old.contains(&f.id));
    let switched = seen.as_ref().is_none_or(|(id, _)| *id != profile.id);
    let sandboxed = ctx.sandbox().is_some();
    let mut parts = vec![];
    if seen.is_none() {
        // Nothing to say about an unrestricted profile without features.
        if removed.is_empty()
            && !(profile.read_only && sandboxed)
            && profile.instructions.is_none()
            && turned_on.is_empty()
        {
            return None;
        }
        parts.push(format!(
            "The session is in the {} profile ({}).",
            profile.name, profile.description
        ));
    } else if switched {
        parts.push(format!(
            "The session switched to the {} profile ({}).",
            profile.name, profile.description
        ));
    }
    parts.extend(turned_on.iter().map(|f| f.turned_on(sandboxed)));
    parts.extend(turned_off.map(|f| format!("The {} feature is off now.", f.id)));
    if !removed.is_empty() {
        parts.push(format!(
            "Do not call these tools now, calls fail: {}.",
            removed.join(", ")
        ));
    }
    if !added.is_empty() {
        parts.push(format!(
            "You can call these tools now: {}.",
            added.join(", ")
        ));
    }
    if switched && sandboxed && (profile.read_only || before.is_some_and(|b| b.read_only)) {
        parts.push(if profile.read_only {
            "The sandbox is read-only now: commands cannot change any files.".into()
        } else {
            "The sandbox allows writing to the workspace again.".into()
        });
    }
    let mut note = format!("<system-reminder>\n{}", parts.join(" "));
    let instructions = turned_on.iter().map(|f| &f.instructions);
    let profile_text = switched.then_some(&profile.instructions);
    for text in profile_text.into_iter().chain(instructions).flatten() {
        note += &format!("\n\n{text}");
    }
    Some(note + "\n</system-reminder>")
}
