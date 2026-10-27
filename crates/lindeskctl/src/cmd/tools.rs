//! `completions`, `man`, `bench` and git-style external subcommands.

use std::ffi::OsString;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, anyhow};
use clap::CommandFactory;
use clap_complete::Shell;

use crate::cli::{Cli, ShellArg};
use crate::paths;
use crate::ui::{Ui, Usage, closest};

pub fn completions(shell: ShellArg, w: &mut dyn std::io::Write) {
    let shell = match shell {
        ShellArg::Bash => Shell::Bash,
        ShellArg::Zsh => Shell::Zsh,
        ShellArg::Fish => Shell::Fish,
        ShellArg::Elvish => Shell::Elvish,
        ShellArg::Powershell => Shell::PowerShell,
    };
    let mut cmd = Cli::command();
    clap_complete::generate(shell, &mut cmd, "lindeskctl", w);
}

/// Hand-written pages for the other binaries.
pub const DAEMON_PAGES: &[(&str, &str)] = &[
    ("lindeskd.8", include_str!("../../man/lindeskd.8")),
    ("lindesk-session.1", include_str!("../../man/lindesk-session.1")),
    ("lindesk-mcp.1", include_str!("../../man/lindesk-mcp.1")),
];

fn render(cmd: &clap::Command) -> Result<Vec<u8>> {
    let mut buf = Vec::new();
    clap_mangen::Man::new(cmd.clone()).render(&mut buf)?;
    Ok(buf)
}

/// Write every page into `dir`; returns the files written.
pub fn man(dir: &Path) -> Result<Vec<PathBuf>> {
    std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    let mut cmd = Cli::command();
    cmd.build();
    let mut written = Vec::new();
    let mut write = |name: String, bytes: &[u8]| -> Result<()> {
        let p = dir.join(name);
        std::fs::write(&p, bytes).with_context(|| format!("writing {}", p.display()))?;
        written.push(p);
        Ok(())
    };
    write("lindeskctl.1".into(), &render(&cmd)?)?;
    fn walk(prefix: &str, cmd: &clap::Command, write: &mut dyn FnMut(String, &[u8]) -> Result<()>) -> Result<()> {
        for sub in cmd.get_subcommands() {
            if sub.is_hide_set() || sub.get_name() == "help" {
                continue;
            }
            let name = format!("{prefix}-{}", sub.get_name());
            // clap wants 'static names; this runs once per page in a one-shot command.
            let leaked: &'static str = Box::leak(name.clone().into_boxed_str());
            let page = sub.clone().name(leaked).display_name(leaked);
            write(format!("{name}.1"), &render(&page)?)?;
            walk(&name, sub, write)?;
        }
        Ok(())
    }
    walk("lindeskctl", &cmd, &mut write)?;
    for (name, text) in DAEMON_PAGES {
        write(name.to_string(), text.as_bytes())?;
    }
    Ok(written)
}

pub fn man_cmd(ui: &Ui, dir: &Path) -> Result<()> {
    let files = man(dir)?;
    if ui.json {
        return ui.print_json(&files);
    }
    ui.info(format!("{} wrote {} man pages to {}", ui.ok_mark(), files.len(), dir.display()));
    Ok(())
}

/// `lindeskctl bench …` → `lindesk-bench …`.
pub fn bench(args: &[OsString]) -> Result<()> {
    let bin = paths::which("lindesk-bench").ok_or_else(|| {
        anyhow!("lindesk-bench isn't installed (it ships with the lindesk package; from a checkout: cargo build --release -p lindesk-bench, then put it on PATH)")
    })?;
    let err = Command::new(&bin).args(args).exec();
    Err(anyhow!("running {}: {err}", bin.display()))
}

/// Top-level subcommand names, for suggestions.
pub fn command_names() -> Vec<String> {
    Cli::command().get_subcommands().filter(|c| !c.is_hide_set()).map(|c| c.get_name().to_string()).collect()
}

/// `lindeskctl foo …` → `lindeskctl-foo …` from PATH, or a "did you mean" usage error.
pub fn external(args: &[OsString]) -> Result<()> {
    let name = args.first().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
    let bin = format!("lindeskctl-{name}");
    if !name.is_empty()
        && !name.contains('/')
        && let Some(path) = paths::which(&bin)
    {
        let err = Command::new(&path).args(&args[1..]).exec();
        return Err(anyhow!("running {}: {err}", path.display()));
    }
    let names = command_names();
    let mut msg = format!("unknown command '{name}'");
    if let Some(c) = closest(&name, names.iter().map(String::as_str)) {
        msg.push_str(&format!(". Did you mean '{c}'?"));
    } else {
        msg.push_str(&format!(" (and no {bin} on PATH). Commands: {}", names.join(", ")));
    }
    msg.push_str("\nSee: lindeskctl --help");
    Err(Usage(msg).into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn completions_mention_subcommands() {
        for shell in [ShellArg::Bash, ShellArg::Zsh, ShellArg::Fish, ShellArg::Elvish, ShellArg::Powershell] {
            let mut buf = Vec::new();
            completions(shell, &mut buf);
            let s = String::from_utf8(buf).unwrap();
            assert!(s.contains("lindeskctl"), "{shell:?}");
            assert!(s.contains("simulate") && s.contains("plugin"), "{shell:?}");
            for sub in [
                "profile",
                "changes",
                "calibrate",
                "hardware",
                "swap",
                "mcp",
                "revert-to",
                "sysctl",
                "latency",
                "depend",
                "batch",
                "gpu",
            ] {
                assert!(s.contains(sub), "{shell:?} completion lacks {sub}");
            }
        }
    }

    #[test]
    fn man_pages_for_every_subcommand() {
        let d = tempfile::tempdir().unwrap();
        let files = man(d.path()).unwrap();
        let names: Vec<String> = files.iter().map(|p| p.file_name().unwrap().to_string_lossy().into_owned()).collect();
        assert!(!names.iter().any(|n| n == "lindesk-data.1"), "lindesk-data ships its own page");
        for n in [
            "lindeskctl.1",
            "lindeskctl-latency-show.1",
            "lindeskctl-depend.1",
            "lindeskctl-batch.1",
            "lindeskctl-gpu.1",
            "lindeskctl-doctor.1",
            "lindeskctl-plugin.1",
            "lindeskctl-plugin-new.1",
            "lindeskctl-data-delete.1",
            "lindeskd.8",
            "lindesk-session.1",
            "lindesk-mcp.1",
            "lindeskctl-profile.1",
            "lindeskctl-profile-use.1",
            "lindeskctl-changes.1",
            "lindeskctl-changes-apply.1",
            "lindeskctl-changes-revert-to.1",
            "lindeskctl-calibrate.1",
            "lindeskctl-hardware.1",
            "lindeskctl-swap.1",
            "lindeskctl-mcp.1",
            "lindeskctl-mcp-config.1",
        ] {
            assert!(names.contains(&n.to_string()), "missing {n}: {names:?}");
        }
        let root = std::fs::read_to_string(d.path().join("lindeskctl.1")).unwrap();
        assert!(root.starts_with(".ie") || root.contains(".TH"), "roff");
        assert!(root.contains("simulate"));
        let plugin = std::fs::read_to_string(d.path().join("lindeskctl-plugin-new.1")).unwrap();
        assert!(plugin.contains("lindeskctl\\-plugin\\-new") || plugin.contains("lindeskctl-plugin-new"), "{plugin}");
        for (_, text) in DAEMON_PAGES {
            assert!(text.starts_with(".TH "), "hand-written pages start with .TH");
        }
    }

    #[test]
    fn unknown_command_suggests() {
        let e = external(&["stauts".into()]).unwrap_err();
        assert!(e.downcast_ref::<Usage>().is_some());
        assert!(e.to_string().contains("Did you mean 'status'?"), "{e}");
    }
}
