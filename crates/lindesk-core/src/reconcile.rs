//! Track what lindesk changed so it can be undone. Persisted to `/run/lindesk/state.json`
//! (and `$XDG_RUNTIME_DIR/lindesk/state.json` for the session agent) so a crash or restart
//! never leaves the system modified.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use lindesk_plugin_api::{Knob, Provenance, Setting, Target, TimestampMs};

use crate::resolve::Decided;

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Entry {
    pub target: Target,
    pub knob: Knob,
    /// Value before lindesk touched it (`None` = unknown → reset to default).
    pub original: Option<Setting>,
    pub current: Setting,
    pub provenance: Provenance,
    pub since: TimestampMs,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Action {
    /// Apply `setting`. When `first` is true the actuator should read the original first.
    Apply { target: Target, setting: Setting, provenance: Provenance, first: bool },
    /// Restore the original value.
    Reset { target: Target, knob: Knob, original: Option<Setting> },
}

impl Action {
    pub fn target(&self) -> &Target {
        match self {
            Action::Apply { target, .. } | Action::Reset { target, .. } => target,
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct AppliedState {
    entries: BTreeMap<(Target, Knob), Entry>,
}

impl AppliedState {
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn entries(&self) -> impl Iterator<Item = &Entry> {
        self.entries.values()
    }

    pub fn get(&self, target: &Target, knob: &Knob) -> Option<&Entry> {
        self.entries.get(&(target.clone(), knob.clone()))
    }

    pub fn for_target<'a>(&'a self, target: &'a Target) -> impl Iterator<Item = &'a Entry> + 'a {
        self.entries.values().filter(move |e| &e.target == target)
    }

    /// The minimal set of actions that moves the system from this state to `desired`.
    /// Resets come first so that a knob moving between plugins never double-applies.
    pub fn plan(&self, desired: &BTreeMap<(Target, Knob), Decided>) -> Vec<Action> {
        let mut resets = Vec::new();
        let mut applies = Vec::new();
        for (key, e) in &self.entries {
            if !desired.contains_key(key) {
                resets.push(Action::Reset {
                    target: e.target.clone(),
                    knob: e.knob.clone(),
                    original: e.original.clone(),
                });
            }
        }
        for ((target, knob), d) in desired {
            match self.entries.get(&(target.clone(), knob.clone())) {
                Some(e) if e.current == d.setting => {}
                Some(_) => applies.push(Action::Apply {
                    target: target.clone(),
                    setting: d.setting.clone(),
                    provenance: d.provenance.clone(),
                    first: false,
                }),
                None => applies.push(Action::Apply {
                    target: target.clone(),
                    setting: d.setting.clone(),
                    provenance: d.provenance.clone(),
                    first: true,
                }),
            }
        }
        resets.extend(applies);
        resets
    }

    /// Update provenance for unchanged values (so `explain` shows the latest reason).
    pub fn refresh_provenance(&mut self, desired: &BTreeMap<(Target, Knob), Decided>) {
        for (key, d) in desired {
            if let Some(e) = self.entries.get_mut(key)
                && e.current == d.setting
            {
                e.provenance = d.provenance.clone();
            }
        }
    }

    /// Record a successful apply. `original` is only used for the first apply of a knob.
    pub fn record_apply(
        &mut self,
        target: &Target,
        setting: &Setting,
        original: Option<Setting>,
        provenance: &Provenance,
        now: TimestampMs,
    ) {
        let key = (target.clone(), setting.knob());
        match self.entries.get_mut(&key) {
            Some(e) => {
                e.current = setting.clone();
                e.provenance = provenance.clone();
            }
            None => {
                self.entries.insert(
                    key,
                    Entry {
                        target: target.clone(),
                        knob: setting.knob(),
                        original,
                        current: setting.clone(),
                        provenance: provenance.clone(),
                        since: now,
                    },
                );
            }
        }
    }

    pub fn record_reset(&mut self, target: &Target, knob: &Knob) {
        self.entries.remove(&(target.clone(), knob.clone()));
    }

    /// Forget a target that no longer exists (unit stopped); nothing to restore.
    pub fn forget(&mut self, target: &Target) {
        self.entries.retain(|(t, _), _| t != target);
    }

    /// Actions that undo everything.
    pub fn restore_all(&self) -> Vec<Action> {
        self.plan(&BTreeMap::new())
    }

    pub fn to_json(&self) -> String {
        serde_json::to_string_pretty(&self.entries.values().collect::<Vec<_>>()).unwrap_or_else(|_| "[]".into())
    }

    pub fn from_json(s: &str) -> Result<Self, serde_json::Error> {
        let v: Vec<Entry> = serde_json::from_str(s)?;
        Ok(AppliedState { entries: v.into_iter().map(|e| ((e.target.clone(), e.knob.clone()), e)).collect() })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn decided(s: Setting) -> Decided {
        Decided { setting: s, provenance: Provenance { plugin: "t".into(), ..Default::default() } }
    }

    #[test]
    fn plan_apply_change_reset_cycle() {
        let t = Target::system_unit("packagekit.service");
        let mut st = AppliedState::default();
        let mut want = BTreeMap::new();
        want.insert((t.clone(), Knob::CpuWeight), decided(Setting::CpuWeight { value: 20 }));

        let a = st.plan(&want);
        assert!(matches!(&a[..], [Action::Apply { first: true, .. }]));
        st.record_apply(
            &t,
            &Setting::CpuWeight { value: 20 },
            Some(Setting::CpuWeight { value: 100 }),
            &Provenance::default(),
            1,
        );
        assert!(st.plan(&want).is_empty(), "idempotent");

        want.insert((t.clone(), Knob::CpuWeight), decided(Setting::CpuWeight { value: 10 }));
        let a = st.plan(&want);
        assert!(matches!(&a[..], [Action::Apply { first: false, .. }]));
        st.record_apply(&t, &Setting::CpuWeight { value: 10 }, None, &Provenance::default(), 2);
        assert_eq!(
            st.get(&t, &Knob::CpuWeight).unwrap().original,
            Some(Setting::CpuWeight { value: 100 }),
            "original kept"
        );

        let a = st.plan(&BTreeMap::new());
        assert_eq!(
            a,
            vec![Action::Reset {
                target: t.clone(),
                knob: Knob::CpuWeight,
                original: Some(Setting::CpuWeight { value: 100 })
            }]
        );
    }

    #[test]
    fn persists() {
        let mut st = AppliedState::default();
        let t = Target::user_unit(1000, "app-foo.scope");
        st.record_apply(&t, &Setting::IoWeight { value: 300 }, None, &Provenance::default(), 5);
        let back = AppliedState::from_json(&st.to_json()).unwrap();
        assert_eq!(back, st);
        assert_eq!(back.restore_all().len(), 1);
    }
}
