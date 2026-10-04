//! Find external plugins: scan plugin directories for `<name>/plugin.toml`, validate each
//! manifest, enforce privilege rules and apply `[plugins.<name>]` config.
//!
//! Rules:
//! * the manifest's `api` must be [`API_VERSION`], and `exec` must exist and be executable;
//! * a **user** host (`lindesk-session`) refuses `privilege = "system"` and the `actuator`
//!   kind, and skips nothing else;
//! * a **system** host (`lindeskd`) loads only `privilege = "system"` plugins, and only when
//!   the plugin root, the plugin directory, the manifest and the executable are owned by
//!   root and not group/world-writable ([`OwnershipCheck`], injectable for tests);
//! * the first directory that provides a name wins; later ones are reported as shadowed.

use std::collections::BTreeSet;
use std::fmt;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};

use lindesk_core::config::{Config, merge_json};
use lindesk_plugin_api::{API_VERSION, Manifest, PluginKind, Privilege};
use serde::Serialize;

/// The manifest file name inside a plugin directory.
pub const MANIFEST: &str = "plugin.toml";

/// Default priority of external plugins for kinds without a manifest section.
pub const DEFAULT_EXTERNAL_PRIORITY: i32 = 10;

/// Decides whether a path is trustworthy enough for a system plugin.
pub trait OwnershipCheck {
    /// `Ok` when `path` is owned by root and not writable by group or others.
    fn check(&self, path: &Path) -> Result<(), String>;
}

impl<F: Fn(&Path) -> Result<(), String>> OwnershipCheck for F {
    fn check(&self, path: &Path) -> Result<(), String> {
        self(path)
    }
}

/// The real check: `uid == 0` and `mode & 0o022 == 0`, following symlinks *and* checking
/// the link itself (a root-owned target behind a user-owned symlink is rejected).
#[derive(Clone, Copy, Debug, Default)]
pub struct RootOwned;

impl OwnershipCheck for RootOwned {
    fn check(&self, path: &Path) -> Result<(), String> {
        let check = |m: &std::fs::Metadata, what: &str| {
            if m.uid() != 0 {
                return Err(format!("{} {what}is owned by uid {}, not root", path.display(), m.uid()));
            }
            if !m.file_type().is_symlink() && m.permissions().mode() & 0o022 != 0 {
                return Err(format!(
                    "{} {what}is group/world-writable (mode {:o})",
                    path.display(),
                    m.permissions().mode() & 0o7777
                ));
            }
            Ok(())
        };
        let link = std::fs::symlink_metadata(path).map_err(|e| format!("{}: {e}", path.display()))?;
        if link.file_type().is_symlink() {
            check(&link, "(symlink) ")?;
        }
        let m = std::fs::metadata(path).map_err(|e| format!("{}: {e}", path.display()))?;
        check(&m, "")
    }
}

/// A plugin that passed validation (it may still be disabled by config).
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct DiscoveredPlugin {
    pub name: String,
    pub manifest: Manifest,
    pub manifest_path: PathBuf,
    /// The plugin directory (the process's working directory).
    pub dir: PathBuf,
    /// Resolved absolute path of the executable.
    pub exec: PathBuf,
    /// False when the manifest or `[plugins.<name>] enabled = false` disables it.
    pub enabled: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub disabled_reason: Option<String>,
    /// `[plugins.<name>] priority`, which overrides the manifest for every kind.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub priority_override: Option<i32>,
    /// Manifest `[config]` merged with `[plugins.<name>] config`.
    pub config: serde_json::Value,
}

impl DiscoveredPlugin {
    /// Effective priority for one kind: config override, else the manifest section, else 10.
    pub fn priority(&self, kind: PluginKind) -> i32 {
        if let Some(p) = self.priority_override {
            return p;
        }
        match kind {
            PluginKind::Classifier => self.manifest.classifier.as_ref().map(|c| c.priority),
            PluginKind::Policy => self.manifest.policy.as_ref().map(|p| p.priority),
            _ => None,
        }
        .unwrap_or(DEFAULT_EXTERNAL_PRIORITY)
    }
}

/// A plugin directory that was not loaded, and why.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Rejected {
    pub path: PathBuf,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    pub reason: String,
    /// True for "not meant for this host" (e.g. a user plugin seen by lindeskd), which is
    /// informational rather than an error.
    pub skipped: bool,
}

impl fmt::Display for Rejected {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let what = self.name.as_deref().map_or_else(|| self.path.display().to_string(), str::to_string);
        write!(f, "{}{what}: {}", if self.skipped { "skipped " } else { "rejected " }, self.reason)
    }
}

/// Result of a scan.
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct Discovery {
    pub plugins: Vec<DiscoveredPlugin>,
    pub rejected: Vec<Rejected>,
}

/// Errors loading a single manifest.
#[derive(Debug, thiserror::Error)]
pub enum ManifestError {
    #[error("reading {path}: {source}")]
    Read { path: PathBuf, source: std::io::Error },
    #[error("parsing {path}: {source}")]
    Parse { path: PathBuf, source: Box<toml::de::Error> },
    #[error("{path}: {message}")]
    Invalid { path: PathBuf, message: String },
}

/// Parse a `plugin.toml`.
pub fn load_manifest(path: &Path) -> Result<Manifest, ManifestError> {
    let text = std::fs::read_to_string(path).map_err(|source| ManifestError::Read { path: path.into(), source })?;
    toml::from_str(&text).map_err(|source| ManifestError::Parse { path: path.into(), source: Box::new(source) })
}

/// Default directories for a host, highest precedence first.
///
/// * system: `/etc/lindesk/plugins`, `/usr/lib/lindesk/plugins`
/// * user: `$XDG_DATA_HOME/lindesk/plugins` (default `~/.local/share`), then the system
///   directories, which may also ship `privilege = "user"` plugins.
pub fn default_plugin_dirs(host: Privilege) -> Vec<PathBuf> {
    let system = [PathBuf::from("/etc/lindesk/plugins"), PathBuf::from("/usr/lib/lindesk/plugins")];
    match host {
        Privilege::System => system.to_vec(),
        Privilege::User => {
            let data = std::env::var_os("XDG_DATA_HOME")
                .filter(|v| !v.is_empty())
                .map(PathBuf::from)
                .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/share")));
            data.map(|d| d.join("lindesk/plugins")).into_iter().chain(system).collect()
        }
    }
}

/// Inputs to [`discover`].
pub struct DiscoveryOptions<'a> {
    /// The privilege of the host doing the scan.
    pub host: Privilege,
    /// Directories in precedence order (see [`default_plugin_dirs`]).
    pub dirs: Vec<PathBuf>,
    pub config: &'a Config,
    /// Used for system plugins only.
    pub ownership: &'a dyn OwnershipCheck,
}

/// Scan `dirs` for `<name>/plugin.toml`. Missing directories are ignored.
pub fn discover(opts: &DiscoveryOptions<'_>) -> Discovery {
    let mut out = Discovery::default();
    let mut seen = BTreeSet::new();
    for root in &opts.dirs {
        let Ok(rd) = std::fs::read_dir(root) else { continue };
        let mut dirs: Vec<PathBuf> =
            rd.filter_map(Result::ok).map(|e| e.path()).filter(|p| p.join(MANIFEST).is_file()).collect();
        dirs.sort();
        for dir in dirs {
            match inspect(&dir, opts.host, Some(root), opts.config, opts.ownership) {
                Ok(p) => {
                    if seen.insert(p.name.clone()) {
                        out.plugins.push(p);
                    } else {
                        out.rejected.push(Rejected {
                            path: dir,
                            name: Some(p.name),
                            reason: "shadowed by a plugin with the same name in an earlier directory".into(),
                            skipped: true,
                        });
                    }
                }
                Err(r) => out.rejected.push(r),
            }
        }
    }
    out
}

/// Validate one plugin directory without privilege or ownership rules (used by
/// `lindeskctl plugin test` and conformance). `path` may be the directory or the manifest.
pub fn inspect_dev(path: &Path) -> Result<DiscoveredPlugin, Rejected> {
    let (dir, manifest_path) = plugin_paths(path);
    let reject = |name: Option<String>, reason: String| Rejected { path: dir.clone(), name, reason, skipped: false };
    let manifest = load_manifest(&manifest_path).map_err(|e| reject(None, e.to_string()))?;
    validate(&manifest, &dir).map_err(|e| reject(Some(manifest.plugin.name.clone()), e))?;
    let exec = resolve_exec(&manifest, &dir);
    Ok(DiscoveredPlugin {
        name: manifest.plugin.name.clone(),
        enabled: manifest.plugin.enabled,
        disabled_reason: (!manifest.plugin.enabled).then(|| "disabled in plugin.toml".to_string()),
        priority_override: None,
        config: manifest.config.clone(),
        manifest,
        manifest_path,
        dir,
        exec,
    })
}

/// Validate one plugin directory for a host (see the module docs for the rules).
pub fn inspect(
    dir: &Path,
    host: Privilege,
    root: Option<&Path>,
    config: &Config,
    ownership: &dyn OwnershipCheck,
) -> Result<DiscoveredPlugin, Rejected> {
    let mut p = inspect_dev(dir)?;
    let reject = |reason: String, skipped: bool| Rejected {
        path: dir.to_path_buf(),
        name: Some(p.name.clone()),
        reason,
        skipped,
    };
    let m = &p.manifest.plugin;
    match host {
        Privilege::User => {
            if m.privilege == Privilege::System {
                return Err(reject(
                    "privilege = \"system\" plugins run in lindeskd, not in a user session".into(),
                    true,
                ));
            }
            if m.kinds.contains(&PluginKind::Actuator) {
                return Err(reject("actuator plugins need privilege = \"system\"".into(), false));
            }
        }
        Privilege::System => {
            if m.privilege == Privilege::User {
                return Err(reject("privilege = \"user\" plugins run in lindesk-session".into(), true));
            }
            let mut paths: Vec<&Path> = Vec::new();
            if let Some(root) = root {
                paths.push(root);
            }
            paths.extend([dir, p.manifest_path.as_path(), p.exec.as_path()]);
            for path in paths {
                if let Err(e) = ownership.check(path) {
                    return Err(reject(
                        format!("system plugins must be root-owned and not group/world-writable: {e}"),
                        false,
                    ));
                }
            }
        }
    }
    let pc = config.plugins.get(&p.name);
    if let Some(pc) = pc {
        if !pc.enabled {
            p.enabled = false;
            p.disabled_reason = Some(format!("disabled by [plugins.{}] in the config", p.name));
        }
        p.priority_override = pc.priority;
        merge_json(&mut p.config, &pc.config);
    }
    Ok(p)
}

/// The plugin directory and manifest path for `path` (a plugin directory or its
/// `plugin.toml`), both absolute: the directory is canonicalised (made absolute against the
/// current directory when that fails), so a relative `exec` is resolved exactly once and
/// the plugin process, which runs with the directory as its working directory, gets an
/// absolute program path.
pub fn plugin_paths(path: &Path) -> (PathBuf, PathBuf) {
    let (dir, file) = if path.is_dir() {
        (path.to_path_buf(), None)
    } else {
        let parent = path.parent().filter(|p| !p.as_os_str().is_empty()).unwrap_or(Path::new("."));
        (parent.to_path_buf(), path.file_name().map(PathBuf::from))
    };
    let dir = std::fs::canonicalize(&dir).or_else(|_| std::path::absolute(&dir)).unwrap_or(dir);
    let manifest = dir.join(file.unwrap_or_else(|| PathBuf::from(MANIFEST)));
    (dir, manifest)
}

fn resolve_exec(m: &Manifest, dir: &Path) -> PathBuf {
    let exec = Path::new(&m.plugin.exec);
    if exec.is_absolute() { exec.to_path_buf() } else { dir.join(exec) }
}

fn valid_name(n: &str) -> bool {
    !n.is_empty()
        && n.len() <= 64
        && n.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
        && !n.starts_with('.')
}

/// Structural checks that don't depend on the host.
pub fn validate(m: &Manifest, dir: &Path) -> Result<(), String> {
    let p = &m.plugin;
    if !valid_name(&p.name) {
        return Err(format!("invalid plugin name '{}': use letters, digits, '-', '_' and '.'", p.name));
    }
    if p.api != API_VERSION {
        return Err(format!("targets plugin API v{}, but this host speaks v{API_VERSION}", p.api));
    }
    if p.kinds.is_empty() {
        return Err("`kinds` is empty: list at least one of sensor, adapter, classifier, policy, actuator, sink".into());
    }
    let unique: BTreeSet<_> = p.kinds.iter().collect();
    if unique.len() != p.kinds.len() {
        return Err("`kinds` lists a kind twice".into());
    }
    for (section, kind) in [
        (m.classifier.is_some(), PluginKind::Classifier),
        (m.policy.is_some(), PluginKind::Policy),
        (m.sensor.is_some(), PluginKind::Sensor),
        (m.sink.is_some(), PluginKind::Sink),
    ] {
        if section && !p.kinds.contains(&kind) {
            return Err(format!("has a [{kind}] section but `kinds` does not include \"{kind}\""));
        }
    }
    if let Some(c) = &m.classifier {
        if !(0.0..=1.0).contains(&c.min_confidence) {
            return Err(format!("[classifier] min_confidence {} is outside 0..=1", c.min_confidence));
        }
        crate::registry::CompiledPrefilter::new(&c.prefilter).map_err(|e| format!("[classifier] prefilter: {e}"))?;
    }
    if m.sensor.as_ref().is_some_and(|s| s.interval_ms == 0) {
        return Err("[sensor] interval_ms must be > 0".into());
    }
    let exec = resolve_exec(m, dir);
    let meta = std::fs::metadata(&exec).map_err(|e| format!("exec {}: {e}", exec.display()))?;
    if !meta.is_file() {
        return Err(format!("exec {} is not a file", exec.display()));
    }
    if meta.permissions().mode() & 0o111 == 0 {
        return Err(format!("exec {} is not executable (try: chmod +x {})", exec.display(), exec.display()));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn write_plugin(root: &Path, name: &str, extra: &str, privilege: &str, kinds: &str) -> PathBuf {
        let dir = root.join(name);
        fs::create_dir_all(&dir).unwrap();
        fs::write(
            dir.join(MANIFEST),
            format!(
                "[plugin]\nname = \"{name}\"\nversion = \"0.1.0\"\napi = 1\nkinds = [{kinds}]\nexec = \"run.sh\"\nprivilege = \"{privilege}\"\n{extra}"
            ),
        )
        .unwrap();
        let exec = dir.join("run.sh");
        fs::write(&exec, "#!/bin/sh\n").unwrap();
        fs::set_permissions(&exec, fs::Permissions::from_mode(0o755)).unwrap();
        dir
    }

    fn trust_all(_: &Path) -> Result<(), String> {
        Ok(())
    }

    fn opts<'a>(
        host: Privilege,
        dirs: Vec<PathBuf>,
        config: &'a Config,
        own: &'a dyn OwnershipCheck,
    ) -> DiscoveryOptions<'a> {
        DiscoveryOptions { host, dirs, config, ownership: own }
    }

    #[test]
    fn user_host_loads_user_plugins_and_applies_config() {
        let tmp = tempfile::tempdir().unwrap();
        write_plugin(
            tmp.path(),
            "steam",
            "[classifier]\npriority = 50\n[config]\nproton = true\nlevel = 1\n",
            "user",
            "\"classifier\"",
        );
        write_plugin(tmp.path(), "off", "", "user", "\"policy\"");
        let config = Config::from_toml(
            "[plugins.steam]\npriority = 70\nconfig = { level = 2 }\n[plugins.off]\nenabled = false\n",
        )
        .unwrap();
        let d = discover(&opts(Privilege::User, vec![tmp.path().into()], &config, &trust_all));
        assert!(d.rejected.is_empty(), "{:?}", d.rejected);
        let steam = d.plugins.iter().find(|p| p.name == "steam").unwrap();
        assert!(steam.enabled);
        assert_eq!(steam.priority(PluginKind::Classifier), 70);
        assert_eq!(steam.config, serde_json::json!({"proton": true, "level": 2}));
        assert!(steam.exec.ends_with("steam/run.sh"));
        let off = d.plugins.iter().find(|p| p.name == "off").unwrap();
        assert!(!off.enabled);
        assert!(off.disabled_reason.as_deref().unwrap().contains("[plugins.off]"));
        assert_eq!(off.priority(PluginKind::Policy), DEFAULT_EXTERNAL_PRIORITY);
    }

    #[test]
    fn user_host_refuses_system_and_actuators() {
        let tmp = tempfile::tempdir().unwrap();
        write_plugin(tmp.path(), "sys", "", "system", "\"policy\"");
        write_plugin(tmp.path(), "act", "", "user", "\"actuator\"");
        let config = Config::default();
        let d = discover(&opts(Privilege::User, vec![tmp.path().into()], &config, &trust_all));
        assert!(d.plugins.is_empty());
        let r: Vec<_> = d.rejected.iter().map(|r| (r.name.clone().unwrap(), r.skipped)).collect();
        assert_eq!(r, vec![("act".to_string(), false), ("sys".to_string(), true)]);
    }

    #[test]
    fn system_host_requires_root_ownership() {
        let tmp = tempfile::tempdir().unwrap();
        let good = write_plugin(tmp.path(), "good", "", "system", "\"actuator\"");
        write_plugin(tmp.path(), "evil", "", "system", "\"policy\"");
        write_plugin(tmp.path(), "user", "", "user", "\"policy\"");
        let config = Config::default();
        // Pretend everything is root-owned except evil's executable.
        let fake = |p: &Path| {
            if p.ends_with("evil/run.sh") {
                Err(format!("{} is owned by uid 1000, not root", p.display()))
            } else {
                Ok(())
            }
        };
        let d = discover(&opts(Privilege::System, vec![tmp.path().into()], &config, &fake));
        assert_eq!(d.plugins.iter().map(|p| p.name.as_str()).collect::<Vec<_>>(), ["good"]);
        assert_eq!(d.plugins[0].dir, good);
        let evil = d.rejected.iter().find(|r| r.name.as_deref() == Some("evil")).unwrap();
        assert!(!evil.skipped);
        assert!(evil.reason.contains("root-owned"), "{}", evil.reason);
        assert!(d.rejected.iter().any(|r| r.name.as_deref() == Some("user") && r.skipped));

        // And the real check rejects files owned by the (non-root) test user.
        let d = discover(&opts(Privilege::System, vec![tmp.path().into()], &config, &RootOwned));
        if unsafe { libc::geteuid() } != 0 {
            assert!(d.plugins.is_empty());
        }
    }

    #[test]
    fn root_owned_rejects_writable_and_foreign_files() {
        let tmp = tempfile::tempdir().unwrap();
        let f = tmp.path().join("x");
        fs::write(&f, "").unwrap();
        fs::set_permissions(&f, fs::Permissions::from_mode(0o666)).unwrap();
        let err = RootOwned.check(&f).unwrap_err();
        if unsafe { libc::geteuid() } != 0 {
            assert!(err.contains("not root"), "{err}");
        }
        // /usr/bin/env is root-owned and 0755 on any sane system.
        assert!(RootOwned.check(Path::new("/usr/bin/env")).is_ok());
    }

    #[test]
    fn validation_errors_are_helpful() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = write_plugin(tmp.path(), "a", "", "user", "\"classifier\"");
        fs::set_permissions(dir.join("run.sh"), fs::Permissions::from_mode(0o644)).unwrap();
        let e = inspect_dev(&dir).unwrap_err();
        assert!(e.reason.contains("chmod +x"), "{}", e.reason);

        let dir = write_plugin(tmp.path(), "b", "[policy]\npriority = 1\n", "user", "\"classifier\"");
        assert!(inspect_dev(&dir).unwrap_err().reason.contains("[policy] section"));

        let dir = write_plugin(tmp.path(), "c", "", "user", "");
        assert!(inspect_dev(&dir).unwrap_err().reason.contains("`kinds` is empty"));

        let dir =
            write_plugin(tmp.path(), "d", "[classifier]\nprefilter = { comm = [\"[\"] }\n", "user", "\"classifier\"");
        assert!(inspect_dev(&dir).unwrap_err().reason.contains("prefilter"));

        // (Separate directories: rewriting a file in place forces an ext4 flush, which is
        // slow on a machine under heavy I/O.)
        let manifest = |name: &str, text: &str| {
            let dir = tmp.path().join(name);
            fs::create_dir_all(&dir).unwrap();
            fs::write(dir.join(MANIFEST), text).unwrap();
            dir
        };
        let dir = manifest("e", "[plugin]\nname = \"e\"\nversion = \"1\"\napi = 2\nkinds = [\"sink\"]\nexec = \"x\"\n");
        assert!(inspect_dev(&dir).unwrap_err().reason.contains("API v2"));
        let dir = manifest("f", "[plugin]\nname = \"f\"\nnope = 1\n");
        assert!(inspect_dev(&dir).unwrap_err().reason.contains("parsing"));
    }

    #[test]
    fn first_directory_wins() {
        let a = tempfile::tempdir().unwrap();
        let b = tempfile::tempdir().unwrap();
        write_plugin(a.path(), "dup", "", "user", "\"sink\"");
        write_plugin(b.path(), "dup", "", "user", "\"sink\"");
        let config = Config::default();
        let d = discover(&opts(Privilege::User, vec![a.path().into(), b.path().into()], &config, &trust_all));
        assert_eq!(d.plugins.len(), 1);
        assert!(d.plugins[0].dir.starts_with(a.path()));
        assert!(d.rejected[0].reason.contains("shadowed"));
    }
}
