//! The `rules` classifier: TOML rule files with glob matchers.
//!
//! # Rule files
//!
//! ```toml
//! # /usr/share/lindesk/rules.d/10-critical.toml
//! [[rule]]
//! name = "compositors"
//! class = "critical"
//! comm = ["gnome-shell", "kwin_wayland", "Xwayland"]
//! # optional, globs: exe, unit, slice, app_id; exact: uid = [1000]
//! confidence = 1.0   # optional, 0..=1, default 1.0
//! ```
//!
//! **Matching.** Every *non-empty* matcher list must match (AND across fields); within one
//! list any entry may match (OR). String matchers are globs ([`globset`] syntax: `*`, `?`,
//! `[a-z]`, `{a,b}`); `/` is an ordinary character, so `*` also matches across it. A matcher
//! on a field the process doesn't have (e.g. `exe` for a slice) never matches. A rule needs
//! at least one matcher.
//!
//! Slices with no processes are classified as `pid = 0`, `comm = ""`, `unit = "x.slice"`, so
//! rules meant for slices should match on `unit`. Remember that the kernel truncates `comm`
//! to 15 bytes (`gnome-session-binary` → `gnome-session-b`).
//!
//! # Precedence
//!
//! Directories are read in the order given (by default `/usr/share/lindesk/rules.d`, then
//! `/etc/lindesk/rules.d`, then `$XDG_CONFIG_HOME/lindesk/rules.d`), and `*.toml` files within
//! a directory in byte order of their names. That yields a list of files ordered by
//! (directory, name):
//!
//! * A file with the **same name** as a file in an earlier directory **replaces** it, like a
//!   systemd drop-in; the earlier file is not read at all. An empty file therefore disables a
//!   vendor file.
//! * Files are **evaluated last-first**: the last file in the list is consulted first. So
//!   `/etc` rules override vendor rules, user rules override both, and within one directory
//!   `90-local.toml` overrides `10-critical.toml`.
//! * Within a file, the **first** matching rule wins.
//!
//! Unreadable or invalid files are skipped (and reported by [`RulesClassifier::problems`]) so
//! that one bad user file cannot disable classification altogether.

#[cfg(test)]
mod tests;

use std::collections::BTreeMap;
use std::fmt;
use std::path::PathBuf;

use globset::{Glob, GlobSet, GlobSetBuilder};
use lindesk_plugin_api::{
    Classification, Classifier, InitContext, Plugin, PluginError, PluginInfo, PluginKind, PluginResult, Privilege,
    ProcInfo, QosClass,
};
use serde::Deserialize;

/// Plugin name.
pub const NAME: &str = "rules";

/// Vendor rules shipped with the package.
pub const VENDOR_DIR: &str = "/usr/share/lindesk/rules.d";
/// Administrator rules.
pub const ADMIN_DIR: &str = "/etc/lindesk/rules.d";
/// Per-user rules, relative to `$XDG_CONFIG_HOME` (default `~/.config`).
pub const USER_DIR: &str = "$XDG_CONFIG_HOME/lindesk/rules.d";

/// A problem loading rules.
#[derive(Debug, Clone, PartialEq)]
pub struct RulesError {
    /// File (or directory) concerned.
    pub path: PathBuf,
    /// What went wrong.
    pub message: String,
}

impl fmt::Display for RulesError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.path.display(), self.message)
    }
}

impl std::error::Error for RulesError {}

/// One `[[rule]]` as written in a file.
#[derive(Deserialize, Debug, Clone)]
#[serde(deny_unknown_fields)]
struct RuleSpec {
    name: String,
    class: QosClass,
    #[serde(default)]
    comm: Vec<String>,
    #[serde(default)]
    exe: Vec<String>,
    #[serde(default)]
    unit: Vec<String>,
    #[serde(default)]
    slice: Vec<String>,
    #[serde(default)]
    app_id: Vec<String>,
    #[serde(default)]
    uid: Vec<u32>,
    #[serde(default)]
    confidence: Option<f32>,
}

#[derive(Deserialize, Debug, Default)]
#[serde(deny_unknown_fields)]
struct FileSpec {
    #[serde(default)]
    rule: Vec<RuleSpec>,
}

/// A compiled glob list; `None` means "no constraint".
#[derive(Debug, Clone)]
struct Matcher(Option<GlobSet>);

impl Matcher {
    fn new(field: &str, globs: &[String]) -> Result<Self, String> {
        if globs.is_empty() {
            return Ok(Matcher(None));
        }
        let mut b = GlobSetBuilder::new();
        for g in globs {
            b.add(Glob::new(g).map_err(|e| format!("{field}: {e}"))?);
        }
        Ok(Matcher(Some(b.build().map_err(|e| format!("{field}: {e}"))?)))
    }

    fn is_set(&self) -> bool {
        self.0.is_some()
    }

    fn matches(&self, value: Option<&str>) -> bool {
        match &self.0 {
            None => true,
            Some(set) => value.is_some_and(|v| set.is_match(v)),
        }
    }
}

/// A compiled rule.
#[derive(Debug, Clone)]
struct Rule {
    name: String,
    class: QosClass,
    confidence: f32,
    comm: Matcher,
    exe: Matcher,
    unit: Matcher,
    slice: Matcher,
    app_id: Matcher,
    uid: Vec<u32>,
}

impl Rule {
    fn compile(spec: RuleSpec) -> Result<Rule, String> {
        let ctx = |e: String| format!("rule '{}': {e}", spec.name);
        let confidence = spec.confidence.unwrap_or(1.0);
        if !(0.0..=1.0).contains(&confidence) {
            return Err(ctx(format!("confidence {confidence} is not in 0..=1")));
        }
        let rule = Rule {
            comm: Matcher::new("comm", &spec.comm).map_err(ctx)?,
            exe: Matcher::new("exe", &spec.exe).map_err(ctx)?,
            unit: Matcher::new("unit", &spec.unit).map_err(ctx)?,
            slice: Matcher::new("slice", &spec.slice).map_err(ctx)?,
            app_id: Matcher::new("app_id", &spec.app_id).map_err(ctx)?,
            uid: spec.uid,
            name: spec.name,
            class: spec.class,
            confidence,
        };
        let any = [&rule.comm, &rule.exe, &rule.unit, &rule.slice, &rule.app_id].iter().any(|m| m.is_set())
            || !rule.uid.is_empty();
        if !any {
            return Err(format!("rule '{}' has no matchers (it would match everything)", rule.name));
        }
        Ok(rule)
    }

    fn matches(&self, p: &ProcInfo) -> bool {
        self.comm.matches(Some(p.comm.as_str()))
            && self.exe.matches(p.exe.as_deref())
            && self.unit.matches(p.unit.as_deref())
            && self.slice.matches(p.slice.as_deref())
            && self.app_id.matches(p.app_id.as_deref())
            && (self.uid.is_empty() || self.uid.contains(&p.uid))
    }
}

/// The rules of one file.
#[derive(Debug, Clone)]
struct RuleFile {
    /// File name (e.g. `10-critical.toml`), used in reasons.
    name: String,
    rules: Vec<Rule>,
}

impl RuleFile {
    fn parse(name: &str, text: &str) -> Result<RuleFile, String> {
        let spec: FileSpec = toml::from_str(text).map_err(|e| e.to_string())?;
        let rules = spec.rule.into_iter().map(Rule::compile).collect::<Result<Vec<_>, _>>()?;
        Ok(RuleFile { name: name.to_string(), rules })
    }
}

/// Where the rules came from, for [`RulesClassifier::reload`].
#[derive(Debug, Clone)]
enum Source {
    Dirs(Vec<PathBuf>),
    Inline,
}

/// Configuration of the plugin (`[plugins.rules.config]`).
#[derive(Deserialize, Debug, Clone, Default)]
#[serde(default, deny_unknown_fields)]
pub struct RulesConfig {
    /// Rule directories, lowest precedence first. `$XDG_CONFIG_HOME`, `$HOME` and a leading
    /// `~/` are expanded. Unset = [`default_dirs`].
    pub dirs: Option<Vec<String>>,
}

/// The built-in `rules` classifier. See the [module docs](self) for the file format and the
/// precedence rules.
#[derive(Debug, Clone)]
pub struct RulesClassifier {
    source: Source,
    /// Files in load order; evaluated in reverse.
    files: Vec<RuleFile>,
    problems: Vec<RulesError>,
}

impl Default for RulesClassifier {
    /// A classifier with no rules (classifies nothing) until [`Plugin::init`] loads them.
    fn default() -> Self {
        RulesClassifier { source: Source::Dirs(Vec::new()), files: Vec::new(), problems: Vec::new() }
    }
}

/// The default rule directories for a host of the given privilege: vendor and admin dirs,
/// plus the user's `$XDG_CONFIG_HOME/lindesk/rules.d` for the session agent.
pub fn default_dirs(privilege: Privilege) -> Vec<String> {
    let mut v = vec![VENDOR_DIR.to_string(), ADMIN_DIR.to_string()];
    if privilege == Privilege::User {
        v.push(USER_DIR.to_string());
    }
    v
}

/// Expand `$XDG_CONFIG_HOME`, `$HOME` and a leading `~/`. Returns `None` when a needed
/// variable is unset.
fn expand(dir: &str) -> Option<PathBuf> {
    let home = std::env::var_os("HOME").map(PathBuf::from);
    let xdg = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .or_else(|| home.as_ref().map(|h| h.join(".config")));
    let sub = |s: &str, var: &str, val: &Option<PathBuf>| -> Option<String> {
        if s.contains(var) { Some(s.replace(var, val.as_ref()?.to_str()?)) } else { Some(s.to_string()) }
    };
    let mut s = dir.to_string();
    if let Some(rest) = s.strip_prefix("~/") {
        s = format!("{}/{rest}", home.as_ref()?.to_str()?);
    }
    s = sub(&s, "$XDG_CONFIG_HOME", &xdg)?;
    s = sub(&s, "$HOME", &home)?;
    Some(PathBuf::from(s))
}

fn is_user_dir(dir: &str) -> bool {
    dir.starts_with('~') || dir.contains("$XDG_CONFIG_HOME") || dir.contains("$HOME")
}

impl RulesClassifier {
    /// Load every `*.toml` from `dirs` (lowest precedence first). Missing directories are
    /// fine; invalid files are skipped and listed in [`Self::problems`].
    pub fn from_dirs<I, P>(dirs: I) -> Self
    where
        I: IntoIterator<Item = P>,
        P: Into<PathBuf>,
    {
        let dirs: Vec<PathBuf> = dirs.into_iter().map(Into::into).collect();
        let (files, problems) = load_dirs(&dirs);
        RulesClassifier { source: Source::Dirs(dirs), files, problems }
    }

    /// Build from one in-memory rule file named `file_name` (used in reasons). Strict: any
    /// error is returned.
    pub fn from_str(file_name: &str, toml: &str) -> Result<Self, RulesError> {
        Self::from_files([(file_name, toml)])
    }

    /// Build from several in-memory files, given in load order (the last is consulted first).
    pub fn from_files<'a>(files: impl IntoIterator<Item = (&'a str, &'a str)>) -> Result<Self, RulesError> {
        let files = files
            .into_iter()
            .map(|(name, text)| {
                RuleFile::parse(name, text).map_err(|message| RulesError { path: name.into(), message })
            })
            .collect::<Result<Vec<_>, _>>()?;
        Ok(RulesClassifier { source: Source::Inline, files, problems: Vec::new() })
    }

    /// Re-read the directories this classifier was loaded from (no-op for in-memory rules).
    /// Returns the problems found; valid files are used regardless.
    pub fn reload(&mut self) -> &[RulesError] {
        if let Source::Dirs(dirs) = &self.source {
            let (files, problems) = load_dirs(dirs);
            self.files = files;
            self.problems = problems;
            for p in &self.problems {
                tracing::warn!(plugin = NAME, "skipping rules: {p}");
            }
        }
        &self.problems
    }

    /// Problems found by the last load (skipped files).
    pub fn problems(&self) -> &[RulesError] {
        &self.problems
    }

    /// Effective rule files in load order, as `(file name, number of rules)`.
    pub fn files(&self) -> Vec<(&str, usize)> {
        self.files.iter().map(|f| (f.name.as_str(), f.rules.len())).collect()
    }

    /// The directories rules are loaded from (empty for in-memory rules).
    pub fn dirs(&self) -> &[PathBuf] {
        match &self.source {
            Source::Dirs(d) => d,
            Source::Inline => &[],
        }
    }

    /// Classify one process: later files first, first matching rule within a file.
    pub fn lookup(&self, p: &ProcInfo) -> Option<Classification> {
        self.files.iter().rev().find_map(|f| {
            f.rules.iter().find(|r| r.matches(p)).map(|r| Classification {
                class: r.class,
                confidence: r.confidence,
                reason: format!("rules: {}:{}", f.name, r.name),
            })
        })
    }
}

/// Collect the effective files (later directories replace same-named files) and parse them.
fn load_dirs(dirs: &[PathBuf]) -> (Vec<RuleFile>, Vec<RulesError>) {
    let mut problems = Vec::new();
    // file name → (dir index, path); a later dir overwrites an earlier entry.
    let mut chosen: BTreeMap<String, (usize, PathBuf)> = BTreeMap::new();
    for (i, dir) in dirs.iter().enumerate() {
        let entries = match std::fs::read_dir(dir) {
            Ok(e) => e,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => {
                problems.push(RulesError { path: dir.clone(), message: e.to_string() });
                continue;
            }
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let Some(name) = path.file_name().and_then(|n| n.to_str()).map(str::to_owned) else { continue };
            if !name.ends_with(".toml") || name.starts_with('.') || path.is_dir() {
                continue;
            }
            chosen.insert(name, (i, path));
        }
    }
    let mut ordered: Vec<(usize, String, PathBuf)> = chosen.into_iter().map(|(n, (i, p))| (i, n, p)).collect();
    ordered.sort();
    let mut files = Vec::new();
    for (_, name, path) in ordered {
        match std::fs::read_to_string(&path) {
            Ok(text) => match RuleFile::parse(&name, &text) {
                Ok(f) => files.push(f),
                Err(message) => problems.push(RulesError { path, message }),
            },
            Err(e) => problems.push(RulesError { path, message: e.to_string() }),
        }
    }
    (files, problems)
}

impl Plugin for RulesClassifier {
    fn info(&self) -> PluginInfo {
        PluginInfo {
            name: NAME.into(),
            version: env!("CARGO_PKG_VERSION").into(),
            kinds: vec![PluginKind::Classifier],
            description: "Classifies processes and units with TOML glob rules from rules.d directories".into(),
        }
    }

    /// Reads `{"dirs": [...]}` (default [`default_dirs`]) and loads the rules. With system
    /// privilege, user-relative entries (`~`, `$HOME`, `$XDG_CONFIG_HOME`) are skipped.
    fn init(&mut self, ctx: &InitContext) -> PluginResult<()> {
        let cfg: RulesConfig = if ctx.config.is_null() {
            RulesConfig::default()
        } else {
            serde_json::from_value(ctx.config.clone()).map_err(|e| PluginError::Failed(format!("{NAME}: {e}")))?
        };
        let dirs: Vec<PathBuf> = cfg
            .dirs
            .unwrap_or_else(|| default_dirs(ctx.privilege))
            .iter()
            .filter(|d| ctx.privilege == Privilege::User || !is_user_dir(d))
            .filter_map(|d| expand(d))
            .collect();
        *self = RulesClassifier::from_dirs(dirs);
        for p in &self.problems {
            tracing::warn!(plugin = NAME, "skipping rules: {p}");
        }
        Ok(())
    }
}

impl Classifier for RulesClassifier {
    fn classify(&mut self, proc_info: &ProcInfo) -> PluginResult<Option<Classification>> {
        Ok(self.lookup(proc_info))
    }
}
