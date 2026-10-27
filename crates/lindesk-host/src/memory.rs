//! Allocate a bounded memory.low budget through the whole cgroup hierarchy.
use std::collections::BTreeMap;

use lindesk_core::resolve::{Decided, Resolution};
use lindesk_plugin_api::{Knob, Provenance, Setting, Target};

pub const SOURCE: &str = "memory-hierarchy";

pub fn generated(p: &Provenance) -> bool {
    p.plugin == SOURCE || (p.plugin == "broker" && p.reason.starts_with("memory-hierarchy:"))
}

/// Children compete for a shared budget rather than independently claiming all RAM.
/// User hosts stop at their privilege boundary; the broker supplies the system ancestors.
pub fn protect(
    resolution: &mut Resolution,
    paths: &BTreeMap<Target, String>,
    cap: u64,
    allowed: impl Fn(&Target) -> bool,
) {
    let mut requested = BTreeMap::<String, u64>::new();
    for ((target, knob), d) in &resolution.desired {
        if *knob != Knob::MemoryLow {
            continue;
        }
        let Setting::MemoryLow { bytes } = d.setting else { continue };
        let Some(path) = paths.get(target) else { continue };
        if !path.starts_with('/') || path.split('/').any(|p| p == ".." || p == ".") {
            continue;
        }
        requested.insert(path.trim_end_matches('/').to_string(), bytes.min(cap));
    }
    // Materialize every ancestor, including ones not watched as application workloads.
    for path in requested.keys().cloned().collect::<Vec<_>>() {
        let mut parent = path.as_str();
        while let Some((p, _)) = parent.rsplit_once('/') {
            if p.is_empty() {
                break;
            }
            requested.entry(p.to_string()).or_default();
            parent = p;
        }
    }
    let mut children = BTreeMap::<String, Vec<String>>::new();
    for path in requested.keys() {
        let parent = path.rsplit_once('/').unwrap().0;
        children.entry(parent.to_string()).or_default().push(path.clone());
    }
    let mut need = requested.clone();
    let mut order: Vec<_> = requested.keys().cloned().collect();
    order.sort_by_key(|p| std::cmp::Reverse(p.matches('/').count()));
    for path in &order {
        let sum = children.get(path).into_iter().flatten().fold(0u64, |n, c| n.saturating_add(need[c]));
        need.insert(path.clone(), requested[path].max(sum).min(cap));
    }
    fn allocate(
        parent: &str,
        budget: u64,
        children: &BTreeMap<String, Vec<String>>,
        need: &BTreeMap<String, u64>,
        out: &mut BTreeMap<String, u64>,
    ) {
        let Some(cs) = children.get(parent) else { return };
        let sum: u128 = cs.iter().map(|c| need[c] as u128).sum();
        for child in cs {
            let b =
                if sum > budget as u128 { ((need[child] as u128 * budget as u128) / sum) as u64 } else { need[child] };
            out.insert(child.clone(), b);
            allocate(child, b, children, need, out);
        }
    }
    let mut budgets = BTreeMap::new();
    allocate("", cap, &children, &need, &mut budgets);
    for (path, bytes) in budgets {
        let target = crate::env::target_for_cgroup(&path);
        if !allowed(&target) {
            continue;
        }
        let key = (target, Knob::MemoryLow);
        if let Some(d) = resolution.desired.get_mut(&key) {
            d.setting = Setting::MemoryLow { bytes };
            d.provenance.reason.push_str("; shared memory protection budget");
        } else if bytes > 0 {
            resolution.desired.insert(
                key,
                Decided {
                    setting: Setting::MemoryLow { bytes },
                    provenance: Provenance {
                        plugin: SOURCE.into(),
                        reason: "ancestor budget for protected applications".into(),
                        ..Default::default()
                    },
                },
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn nested_apps_get_ancestor_budgets_without_double_counting_or_overcommit() {
        let mut r = Resolution::default();
        let mut paths = BTreeMap::new();
        for (name, bytes) in [("a.scope", 80), ("b.scope", 80)] {
            let t = Target::user_unit(1000, name);
            paths.insert(t.clone(), format!("/user.slice/user-1000.slice/user@1000.service/app.slice/{name}"));
            r.desired.insert(
                (t, Knob::MemoryLow),
                Decided { setting: Setting::MemoryLow { bytes }, provenance: Provenance::default() },
            );
        }
        protect(&mut r, &paths, 100, |_| true);
        for name in ["a.scope", "b.scope"] {
            assert_eq!(
                r.desired[&(Target::user_unit(1000, name), Knob::MemoryLow)].setting,
                Setting::MemoryLow { bytes: 50 }
            );
        }
        for t in [
            Target::system_unit("user.slice"),
            Target::system_unit("user-1000.slice"),
            Target::system_unit("user@1000.service"),
            Target::user_unit(1000, "app.slice"),
        ] {
            assert_eq!(r.desired[&(t, Knob::MemoryLow)].setting, Setting::MemoryLow { bytes: 100 });
        }
    }
    #[test]
    fn user_host_cannot_allocate_system_ancestors() {
        let t = Target::user_unit(1000, "a.scope");
        let paths =
            BTreeMap::from([(t.clone(), "/user.slice/user-1000.slice/user@1000.service/app.slice/a.scope".into())]);
        let mut r = Resolution::default();
        r.desired.insert(
            (t, Knob::MemoryLow),
            Decided { setting: Setting::MemoryLow { bytes: 50 }, provenance: Provenance::default() },
        );
        protect(&mut r, &paths, 100, |t| t.user() == Some(1000));
        assert!(r.desired.keys().all(|(t, _)| t.user() == Some(1000)));
    }
}
