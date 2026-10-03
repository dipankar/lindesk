//! Turn competing intents into exactly one value per (target, knob).
//!
//! Precedence: admin overrides, then the higher plugin priority, then plugin name (so the
//! result is deterministic). The winning value then passes through [`SafetyCaps`].

use std::collections::BTreeMap;

use lindesk_plugin_api::{Intent, Knob, Provenance, Setting, Target};

use crate::safety::{ClampContext, SafetyCaps, Verdict};

/// Plugin name used for admin overrides from the config file.
pub const ADMIN: &str = "admin";

#[derive(Clone, Debug, PartialEq)]
pub struct Proposal {
    pub plugin: String,
    pub intent: Intent,
}

impl Proposal {
    pub fn new(plugin: impl Into<String>, intent: Intent) -> Self {
        Proposal { plugin: plugin.into(), intent }
    }

    fn rank(&self) -> (bool, i32) {
        (self.plugin == ADMIN, self.intent.priority)
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Decided {
    pub setting: Setting,
    pub provenance: Provenance,
}

#[derive(Clone, Debug, PartialEq)]
pub enum NoteKind {
    Clamped,
    Rejected,
    /// Lost to a higher-precedence intent (informational).
    Overruled,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Note {
    pub kind: NoteKind,
    pub plugin: String,
    pub target: Target,
    pub knob: Knob,
    pub message: String,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Resolution {
    pub desired: BTreeMap<(Target, Knob), Decided>,
    pub notes: Vec<Note>,
}

pub fn resolve(mut proposals: Vec<Proposal>, caps: &SafetyCaps, ctx: impl Fn(&Target) -> ClampContext) -> Resolution {
    // Best first.
    proposals.sort_by(|a, b| b.rank().cmp(&a.rank()).then_with(|| a.plugin.cmp(&b.plugin)));

    let mut out = Resolution::default();
    for p in proposals {
        let key = (p.intent.target.clone(), p.intent.setting.knob());
        if let Some(winner) = out.desired.get(&key) {
            if winner.setting != p.intent.setting {
                out.notes.push(Note {
                    kind: NoteKind::Overruled,
                    plugin: p.plugin.clone(),
                    target: key.0.clone(),
                    knob: key.1.clone(),
                    message: format!("{} overruled by {}", p.intent.setting, winner.provenance.plugin),
                });
            }
            continue;
        }
        let mut provenance = Provenance {
            plugin: p.plugin.clone(),
            reason: p.intent.reason.clone(),
            priority: p.intent.priority,
            clamped_from: None,
        };
        let setting = match caps.check(&p.intent.setting, &ctx(&p.intent.target)) {
            Verdict::Ok(s) => s,
            Verdict::Clamped { setting, from } => {
                out.notes.push(Note {
                    kind: NoteKind::Clamped,
                    plugin: p.plugin.clone(),
                    target: key.0.clone(),
                    knob: key.1.clone(),
                    message: format!("{from} clamped to {setting}"),
                });
                provenance.clamped_from = Some(from.to_string());
                setting
            }
            Verdict::Rejected(why) => {
                out.notes.push(Note {
                    kind: NoteKind::Rejected,
                    plugin: p.plugin.clone(),
                    target: key.0.clone(),
                    knob: key.1.clone(),
                    message: why,
                });
                // A rejected intent doesn't block lower-ranked ones for the same knob.
                continue;
            }
        };
        out.desired.insert(key, Decided { setting, provenance });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use lindesk_plugin_api::QosClass;

    fn intent(t: &Target, s: Setting, prio: i32) -> Intent {
        Intent { target: t.clone(), setting: s, priority: prio, reason: "test".into() }
    }

    #[test]
    fn precedence_admin_then_priority_then_name() {
        let t = Target::system_unit("docker.service");
        let props = vec![
            Proposal::new("threshold", intent(&t, Setting::CpuWeight { value: 50 }, 0)),
            Proposal::new("zeta", intent(&t, Setting::CpuWeight { value: 30 }, 10)),
            Proposal::new("alpha", intent(&t, Setting::CpuWeight { value: 40 }, 10)),
        ];
        let r = resolve(props.clone(), &SafetyCaps::default(), |_| ClampContext::default());
        let d = &r.desired[&(t.clone(), Knob::CpuWeight)];
        assert_eq!(d.setting, Setting::CpuWeight { value: 40 });
        assert_eq!(d.provenance.plugin, "alpha");
        assert_eq!(r.notes.iter().filter(|n| n.kind == NoteKind::Overruled).count(), 2);

        let mut with_admin = props;
        with_admin.push(Proposal::new(ADMIN, intent(&t, Setting::CpuWeight { value: 77 }, -100)));
        let r = resolve(with_admin, &SafetyCaps::default(), |_| ClampContext::default());
        assert_eq!(r.desired[&(t, Knob::CpuWeight)].setting, Setting::CpuWeight { value: 77 });
    }

    #[test]
    fn rejected_falls_through_to_next() {
        let t = Target::user_unit(1000, "gnome-shell-wayland.service");
        let props = vec![
            Proposal::new("evil", intent(&t, Setting::CpuIdle { value: true }, 100)),
            Proposal::new("threshold", intent(&t, Setting::CpuIdle { value: false }, 0)),
        ];
        let r = resolve(props, &SafetyCaps::default(), |_| ClampContext { class: Some(QosClass::Critical), ..Default::default() });
        assert_eq!(r.desired[&(t, Knob::CpuIdle)].setting, Setting::CpuIdle { value: false });
        assert!(r.notes.iter().any(|n| n.kind == NoteKind::Rejected && n.plugin == "evil"));
    }

    #[test]
    fn clamps_record_provenance() {
        let t = Target::user_unit(1000, "app-firefox.scope");
        let props = vec![Proposal::new("greedy", intent(&t, Setting::UclampMin { percent: 100.0 }, 10))];
        let r = resolve(props, &SafetyCaps::default(), |_| ClampContext::default());
        let d = &r.desired[&(t, Knob::UclampMin)];
        assert_eq!(d.setting, Setting::UclampMin { percent: 50.0 });
        assert!(d.provenance.clamped_from.is_some());
    }
}
