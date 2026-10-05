//! `lindeskctl setup conflicts` and `setup snapshot`: other priority daemons
//! (system76-scheduler, ananicy(-cpp), uresourced), handled through the versioned change
//! system so every change is recorded and revertible, plus the install-time snapshot.
//!
//! * `--disable` applies one `service_state` changeset (stop + disable) to the system change
//!   store; `lindeskctl changes list` shows it and uninstall's `changes revert-all` undoes it.
//! * `--restore` reverts those changesets.
//! * `--reconcile` compares every conflicting unit with `/var/lib/lindesk/install-state.json`
//!   (the state before lindesk) and offers to restore differences made outside lindesk.
//!
//! systemd is reached through `systemctl` (`$LINDESK_SYSTEMCTL` overrides it, for fixtures).

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result, bail};
use lindesk_changes::install_state::{self, InstallState};
use lindesk_changes::{
    Author, ChangeOp, ChangeSet, ChangeStore, ConfigPaths, GuardContext, Origin, RealApplier, Roots, Scope,
    ServiceRunner, Services, Status, Systemctl,
};
use serde_json::json;

use crate::cli::{SetupCmd, SetupOrigin};
use crate::cmd::changes::{applied_message, render_plan};
use crate::ui::{self, Exit, Ui, Usage, out};

/// Default persistent state directory.
pub const LIB_DIR: &str = "/var/lib/lindesk";

pub fn run(ui: &Ui, cmd: &SetupCmd) -> Result<()> {
    match cmd {
        SetupCmd::Conflicts { disable, restore, reconcile, restore_conflicts, keep_current, yes, origin, lib_dir } => {
            let env = Env::new(lib_dir.as_deref(), *origin)?;
            if *disable {
                env.require_root("disabling conflicting daemons")?;
                disable_conflicts(ui, &env, *yes)
            } else if *restore {
                env.require_root("restoring conflicting daemons")?;
                restore_conflicts_cmd(ui, &env, *yes)
            } else if *reconcile {
                env.require_root("reconciling conflicting daemons")?;
                let choice = if *restore_conflicts {
                    Choice::Restore
                } else if *keep_current {
                    Choice::Keep
                } else {
                    Choice::Ask
                };
                reconcile_conflicts(ui, &env, choice)
            } else {
                show(ui, &env)
            }
        }
        SetupCmd::Snapshot { method, lib_dir } => {
            let env = Env::new(lib_dir.as_deref(), SetupOrigin::Install)?;
            env.require_root("recording the install-time snapshot")?;
            let (s, created) = install_state::record_install(
                &env.state_path(),
                method,
                env!("CARGO_PKG_VERSION"),
                &*env.services,
                lindesk_core::clock::now_ms(),
            )?;
            if ui.json {
                return ui.print_json(&json!({"created": created, "state": s}));
            }
            ui.info(format!(
                "{} {} {}",
                ui.ok_mark(),
                if created { "recorded the install-time state in" } else { "kept the original snapshot in" },
                env.state_path().display()
            ));
            Ok(())
        }
    }
}

/// Where and as whom this runs.
pub struct Env {
    lib: PathBuf,
    explicit: bool,
    origin: Origin,
    services: Services,
}

impl Env {
    fn new(lib: Option<&Path>, origin: SetupOrigin) -> Result<Env> {
        Ok(Env {
            lib: lib.map_or_else(|| PathBuf::from(LIB_DIR), Path::to_path_buf),
            explicit: lib.is_some(),
            origin: match origin {
                SetupOrigin::Cli => Origin::Cli,
                SetupOrigin::Install => Origin::Install,
            },
            services: Services::new(Systemctl::from_env()),
        })
    }

    fn require_root(&self, what: &str) -> Result<()> {
        if self.explicit { Ok(()) } else { crate::paths::require_root(what) }
    }

    fn state_path(&self) -> PathBuf {
        self.lib.join("install-state.json")
    }

    fn store(&self) -> Result<ChangeStore> {
        let dir = self.lib.join("changes");
        let applier =
            RealApplier::new(Roots::host(), ConfigPaths::host(Scope::System)).with_services(self.services.clone());
        let store = ChangeStore::open(&dir, Scope::System)
            .with_context(|| format!("opening the change store {}", dir.display()))?
            .with_applier(Arc::new(applier));
        store.recover()?;
        Ok(store)
    }

    fn author(&self) -> Author {
        let client = if self.origin == Origin::Install { "installer" } else { "lindeskctl setup conflicts" };
        Author::new(crate::paths::uid(), self.origin).with_client(client)
    }

    fn ctx(&self) -> GuardContext {
        GuardContext::new(
            Scope::System,
            self.origin,
            Roots::host(),
            ConfigPaths::host(Scope::System),
            Default::default(),
        )
    }
}

/// Installed conflicting units with their (enabled, active) state.
fn installed(services: &dyn ServiceRunner) -> Vec<(String, bool, bool)> {
    install_state::observe(services)
        .into_iter()
        .filter(|u| u.installed)
        .map(|u| (u.unit, u.enabled, u.active))
        .collect()
}

/// Effective changesets that changed a conflicting service, newest first.
fn service_changes(store: &ChangeStore) -> Result<Vec<ChangeSet>> {
    Ok(store
        .history(usize::MAX)?
        .into_iter()
        .filter(|c| c.is_effective() && c.ops.iter().any(|o| matches!(o.op, ChangeOp::ServiceState { .. })))
        .collect())
}

fn state_word(enabled: bool, active: bool) -> String {
    format!("{}, {}", if enabled { "enabled" } else { "disabled" }, if active { "running" } else { "stopped" })
}

fn show(ui: &Ui, env: &Env) -> Result<()> {
    let units = installed(&*env.services);
    let snapshot = install_state::load(&env.state_path()).ok().flatten();
    let changes =
        if env.lib.join("changes").is_dir() { service_changes(&env.store()?).unwrap_or_default() } else { vec![] };
    if ui.json {
        return ui.print_json(&json!({
            "units": units.iter().map(|(u, e, a)| json!({"unit": u, "enabled": e, "active": a})).collect::<Vec<_>>(),
            "snapshot": snapshot,
            "changes": changes.iter().map(|c| c.id).collect::<Vec<_>>(),
        }));
    }
    if units.is_empty() {
        out("no conflicting priority daemons installed (system76-scheduler, ananicy, ananicy-cpp, uresourced)");
        return Ok(());
    }
    for (u, e, a) in &units {
        let by = changes
            .iter()
            .find(|c| c.ops.iter().any(|o| matches!(&o.op, ChangeOp::ServiceState { unit, .. } if unit == u)))
            .map(|c| format!("  (disabled by lindesk, change #{})", c.id))
            .unwrap_or_default();
        let line = format!("{u:<34} {}{by}", state_word(*e, *a));
        out(&if *e || *a { ui.yellow(line) } else { line });
    }
    if let Some(s) = &snapshot {
        out(&ui.dim(format!("install-time snapshot: {} ({} install)", env.state_path().display(), s.method)));
    }
    if units.iter().any(|(_, e, a)| *e || *a) {
        out(&ui.dim("disable them (revertible): sudo lindeskctl setup conflicts --disable"));
    }
    Ok(())
}

fn disable_conflicts(ui: &Ui, env: &Env, yes: bool) -> Result<()> {
    let ops: Vec<ChangeOp> = installed(&*env.services)
        .into_iter()
        .filter(|(_, e, a)| *e || *a)
        .map(|(unit, ..)| ChangeOp::ServiceState { unit, enabled: Some(false), active: Some(false) })
        .collect();
    if ops.is_empty() {
        if ui.json {
            return ui.print_json(&json!({"changed": false}));
        }
        out(&ui.dim("no conflicting daemon is enabled or running; nothing to do"));
        return Ok(());
    }
    let store = env.store()?;
    let plan = store.plan(&ops, &env.ctx());
    if !ui.json {
        out(render_plan(ui, &plan).trim_end());
    }
    if !plan.is_ok() {
        if ui.json {
            ui.print_json(&plan)?;
        }
        return Err(Exit(1).into());
    }
    if !yes {
        if !ui.stdin_tty {
            return Err(Usage("not a terminal: pass --yes to disable them without confirmation".into()).into());
        }
        if !ui::confirm("Stop and disable these (revert with: lindeskctl setup conflicts --restore)?") {
            ui.info("nothing changed");
            return Err(Exit(1).into());
        }
    }
    let names: Vec<String> = ops.iter().map(|o| crate::cmd::changes::op_target(o).1).collect();
    let cs = store.apply(&plan, env.author(), &format!("disable conflicting {}", names.join(", ")), None)?;
    if ui.json {
        ui.print_json(&cs)?;
    } else {
        out(&applied_message(ui, &cs, lindesk_core::clock::now_ms()));
    }
    if cs.status == Status::Failed { Err(Exit(1).into()) } else { Ok(()) }
}

fn restore_conflicts_cmd(ui: &Ui, env: &Env, yes: bool) -> Result<()> {
    let store = env.store()?;
    let changes = service_changes(&store)?;
    if changes.is_empty() {
        if ui.json {
            return ui.print_json(&json!({"reverted": []}));
        }
        out(&ui.dim("lindesk hasn't disabled any conflicting daemon; nothing to restore"));
        return Ok(());
    }
    if !ui.json {
        for c in &changes {
            out(&format!("  #{:<4} {}", c.id, c.description));
        }
    }
    if !yes && ui.stdin_tty && !ui::confirm("Revert these changes?") {
        ui.info("nothing changed");
        return Err(Exit(1).into());
    }
    let mut done = Vec::new();
    for c in &changes {
        let r = store.revert(c.id, env.author(), false)?;
        if !ui.json {
            out(&format!("{} #{} reverted {}", ui.ok_mark(), c.id, ui.dim(format!("(as #{})", r.id))));
        }
        done.push(json!({"id": c.id, "by": r.id}));
    }
    if ui.json {
        return ui.print_json(&json!({"reverted": done}));
    }
    Ok(())
}

/// What `--reconcile` does with a difference.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Choice {
    Ask,
    Restore,
    Keep,
}

fn reconcile_conflicts(ui: &Ui, env: &Env, choice: Choice) -> Result<()> {
    let path = env.state_path();
    let Some(state): Option<InstallState> = install_state::load(&path)? else {
        if ui.json {
            return ui.print_json(&json!({"snapshot": null, "differences": []}));
        }
        out(&ui.dim(format!("no install-time snapshot at {}; nothing to compare", path.display())));
        return Ok(());
    };
    let diffs = install_state::differences(&state, &*env.services);
    if diffs.is_empty() {
        if ui.json {
            return ui.print_json(&json!({"differences": [], "restored": [], "kept": []}));
        }
        out(&ui.dim("every conflicting daemon is as it was before lindesk was installed"));
        return Ok(());
    }
    let mut restored = Vec::new();
    let mut kept = Vec::new();
    for d in &diffs {
        let line = format!(
            "{}: before lindesk {}, now {}",
            d.unit,
            state_word(d.snapshot.enabled, d.snapshot.active),
            state_word(d.current.enabled, d.current.active)
        );
        if !ui.json {
            out(&line);
        }
        let restore = match choice {
            Choice::Restore => true,
            Choice::Keep => false,
            Choice::Ask if ui.stdin_tty => ui::confirm(&format!("Restore {} to how it was?", d.unit)),
            Choice::Ask => false,
        };
        if restore {
            restored.push(d.unit.clone());
        } else {
            kept.push(d.unit.clone());
        }
    }
    if !restored.is_empty() {
        let ops: Vec<ChangeOp> = diffs
            .iter()
            .filter(|d| restored.contains(&d.unit))
            .map(|d| ChangeOp::ServiceState {
                unit: d.unit.clone(),
                enabled: Some(d.snapshot.enabled),
                active: Some(d.snapshot.active),
            })
            .collect();
        let store = env.store()?;
        let plan = store.plan(&ops, &env.ctx());
        if !plan.is_ok() {
            bail!("restoring was refused: {:?}", plan.rejections);
        }
        let cs = store.apply(&plan, env.author(), "restore conflicting daemons to their pre-lindesk state", None)?;
        if cs.status == Status::Failed {
            bail!("restoring failed: {}", cs.error.unwrap_or_default());
        }
    }
    if ui.json {
        return ui.print_json(&json!({"differences": diffs, "restored": restored, "kept": kept}));
    }
    for u in &restored {
        out(&format!("{} {u} restored", ui.ok_mark()));
    }
    for u in &kept {
        let d = diffs.iter().find(|d| &d.unit == u).expect("kept units come from diffs");
        out(&ui.dim(format!(
            "kept {u} as it is now; to restore it: sudo systemctl {} {u}{}",
            if d.snapshot.enabled { "enable" } else { "disable" },
            if d.snapshot.active { " && sudo systemctl start ".to_string() + u } else { String::new() }
        )));
    }
    if choice == Choice::Ask && !ui.stdin_tty && !kept.is_empty() {
        out(&ui.dim("(not a terminal: kept; pass --restore-conflicts to restore them)"));
    }
    Ok(())
}
