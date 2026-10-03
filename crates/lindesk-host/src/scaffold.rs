//! `lindeskctl plugin new`: write a working plugin directory (manifest, source, README and
//! a test) from the templates in `crates/lindesk-host/templates/`. Every generated Python
//! and shell plugin passes [`crate::run_conformance`] as-is.

use std::fmt;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::str::FromStr;

use lindesk_plugin_api::PluginKind;
use serde::{Deserialize, Serialize};

/// Template language.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum Lang {
    /// Single-file stdlib SDK, vendored next to the plugin.
    Python,
    /// The `lindesk-plugin` crate.
    Rust,
    /// Plain bash.
    Shell,
}

impl Lang {
    pub const ALL: [Lang; 3] = [Lang::Python, Lang::Rust, Lang::Shell];

    pub fn as_str(self) -> &'static str {
        match self {
            Lang::Python => "python",
            Lang::Rust => "rust",
            Lang::Shell => "shell",
        }
    }

    /// Kinds this language has a template for.
    pub fn kinds(self) -> &'static [PluginKind] {
        use PluginKind::*;
        match self {
            Lang::Rust => &[Sensor, Adapter, Classifier, Policy, Actuator, Sink],
            Lang::Python | Lang::Shell => &[Sensor, Adapter, Classifier, Policy, Sink],
        }
    }
}

impl fmt::Display for Lang {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for Lang {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_ascii_lowercase().as_str() {
            "python" | "py" => Ok(Lang::Python),
            "rust" | "rs" => Ok(Lang::Rust),
            "shell" | "sh" | "bash" => Ok(Lang::Shell),
            _ => Err(format!("unknown language '{s}' (expected python, rust or shell)")),
        }
    }
}

/// Why scaffolding failed.
#[derive(Debug, thiserror::Error)]
pub enum ScaffoldError {
    #[error("invalid plugin name '{0}': use lowercase letters, digits and '-', starting with a letter")]
    InvalidName(String),
    #[error("there is no {lang} template for {kind} plugins (available: {available})")]
    Unsupported { kind: PluginKind, lang: Lang, available: String },
    #[error("{0} already exists and is not empty")]
    Exists(PathBuf),
    #[error("{path}: {source}")]
    Io { path: PathBuf, source: std::io::Error },
}

mod tpl {
    pub const MANIFEST: &str = include_str!("../templates/plugin.toml");

    pub const PY_MAIN: &str = include_str!("../templates/python/plugin.py");
    pub const PY_TEST: &str = include_str!("../templates/python/test_plugin.py");
    pub const PY_README: &str = include_str!("../templates/python/README.md");
    pub const PY_SDK: &str = include_str!("../templates/python/lindesk_plugin.py");
    pub const PY_CLASSIFIER: &str = include_str!("../templates/python/kinds/classifier.py");
    pub const PY_POLICY: &str = include_str!("../templates/python/kinds/policy.py");
    pub const PY_SENSOR: &str = include_str!("../templates/python/kinds/sensor.py");
    pub const PY_SINK: &str = include_str!("../templates/python/kinds/sink.py");
    pub const PY_ADAPTER: &str = include_str!("../templates/python/kinds/adapter.py");
    pub const PY_T_CLASSIFIER: &str = include_str!("../templates/python/tests/classifier.py");
    pub const PY_T_POLICY: &str = include_str!("../templates/python/tests/policy.py");
    pub const PY_T_SENSOR: &str = include_str!("../templates/python/tests/sensor.py");
    pub const PY_T_SINK: &str = include_str!("../templates/python/tests/sink.py");
    pub const PY_T_ADAPTER: &str = include_str!("../templates/python/tests/adapter.py");

    pub const SH_MAIN: &str = include_str!("../templates/shell/plugin.sh");
    pub const SH_TEST: &str = include_str!("../templates/shell/test.sh");
    pub const SH_README: &str = include_str!("../templates/shell/README.md");
    pub const SH_CLASSIFIER: &str = include_str!("../templates/shell/kinds/classifier.sh");
    pub const SH_POLICY: &str = include_str!("../templates/shell/kinds/policy.sh");
    pub const SH_SENSOR: &str = include_str!("../templates/shell/kinds/sensor.sh");
    pub const SH_SINK: &str = include_str!("../templates/shell/kinds/sink.sh");
    pub const SH_ADAPTER: &str = include_str!("../templates/shell/kinds/adapter.sh");

    pub const RS_CARGO: &str = include_str!("../templates/rust/Cargo.toml");
    pub const RS_MAIN: &str = include_str!("../templates/rust/main.rs");
    pub const RS_README: &str = include_str!("../templates/rust/README.md");
    pub const RS_CLASSIFIER: &str = include_str!("../templates/rust/kinds/classifier.rs");
    pub const RS_POLICY: &str = include_str!("../templates/rust/kinds/policy.rs");
    pub const RS_SENSOR: &str = include_str!("../templates/rust/kinds/sensor.rs");
    pub const RS_SINK: &str = include_str!("../templates/rust/kinds/sink.rs");
    pub const RS_ADAPTER: &str = include_str!("../templates/rust/kinds/adapter.rs");
    pub const RS_ACTUATOR: &str = include_str!("../templates/rust/kinds/actuator.rs");
}

/// The vendored Python SDK shipped with scaffolds (identical to `sdk/python/lindesk_plugin.py`).
pub const PYTHON_SDK: &str = tpl::PY_SDK;

fn valid_name(n: &str) -> bool {
    n.len() <= 64
        && n.starts_with(|c: char| c.is_ascii_lowercase())
        && n.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
        && !n.ends_with('-')
}

fn camel(name: &str) -> String {
    name.split('-')
        .filter(|p| !p.is_empty())
        .map(|p| {
            let mut c = p.chars();
            c.next().map(|f| f.to_ascii_uppercase().to_string() + c.as_str()).unwrap_or_default()
        })
        .collect()
}

fn manifest_section(kind: PluginKind) -> &'static str {
    match kind {
        PluginKind::Classifier => {
            "\n[classifier]\npriority = 50          # higher runs first; the first confident answer wins\nmin_confidence = 0.5\n# Host-side filter: only matching processes are sent to the plugin.\n# prefilter = { comm = [\"rustc\", \"cc1*\"] }\n"
        }
        PluginKind::Policy => "\n[policy]\npriority = 10          # intents with priority 0 get this priority\n",
        PluginKind::Sensor => "\n[sensor]\ninterval_ms = 1000\n",
        PluginKind::Sink => "\n[sink]\ntables = [\"events\"]    # pressure, units, events, metrics; empty = all\n",
        PluginKind::Adapter | PluginKind::Actuator => "",
    }
}

struct Vars<'a> {
    pairs: Vec<(&'static str, String)>,
    _p: std::marker::PhantomData<&'a ()>,
}

impl Vars<'_> {
    fn render(&self, template: &str) -> String {
        let mut s = template.to_string();
        // Bodies first: they may contain the simple placeholders.
        for (k, v) in self.pairs.iter().filter(|(k, _)| matches!(*k, "body" | "tests" | "cases" | "section")) {
            s = s.replace(&format!("{{{{{k}}}}}"), v);
        }
        for (k, v) in &self.pairs {
            s = s.replace(&format!("{{{{{k}}}}}"), v);
        }
        s
    }
}

/// A generated file.
struct File {
    rel: &'static str,
    contents: String,
    executable: bool,
}

/// Write a new `kind` plugin called `name` in `lang` into the directory `dest` (created;
/// must not exist or be empty). Returns the files written.
///
/// * Python: `plugin.toml`, `<module>.py`, `lindesk_plugin.py` (vendored SDK),
///   `test_<module>.py` (stdlib unittest), `README.md`
/// * Shell: `plugin.toml`, `plugin.sh`, `test.sh`, `README.md`
/// * Rust: `plugin.toml`, `Cargo.toml` (depends on `lindesk-plugin` by version, with a
///   commented path alternative), `src/main.rs` with `cargo test` tests, `README.md`
pub fn scaffold(kind: PluginKind, lang: Lang, name: &str, dest: &Path) -> Result<Vec<PathBuf>, ScaffoldError> {
    if !valid_name(name) {
        return Err(ScaffoldError::InvalidName(name.into()));
    }
    if !lang.kinds().contains(&kind) {
        let available: Vec<&str> = lang.kinds().iter().map(|k| k.as_str()).collect();
        return Err(ScaffoldError::Unsupported { kind, lang, available: available.join(", ") });
    }
    if dest.exists() && std::fs::read_dir(dest).map(|mut d| d.next().is_some()).unwrap_or(true) {
        return Err(ScaffoldError::Exists(dest.into()));
    }
    let module = name.replace('-', "_");
    let files = match lang {
        Lang::Python => python(kind, name, &module),
        Lang::Shell => shell(kind, name),
        Lang::Rust => rust(kind, name),
    };
    let exec = match lang {
        Lang::Python => format!("{module}.py"),
        Lang::Shell => "plugin.sh".into(),
        Lang::Rust => format!("target/release/{name}"),
    };
    let vars = Vars {
        pairs: vec![
            ("section", manifest_section(kind).to_string()),
            ("name", name.to_string()),
            ("module", module.clone()),
            ("Struct", camel(name)),
            ("kind", kind.as_str().to_string()),
            ("exec", exec),
            ("privilege", if kind == PluginKind::Actuator { "system" } else { "user" }.to_string()),
        ],
        _p: std::marker::PhantomData,
    };
    let mut all = vec![File { rel: "plugin.toml", contents: tpl::MANIFEST.to_string(), executable: false }];
    all.extend(files);
    let io = |path: &Path| {
        let path = path.to_path_buf();
        move |source| ScaffoldError::Io { path, source }
    };
    std::fs::create_dir_all(dest).map_err(io(dest))?;
    let mut written = Vec::new();
    for f in all {
        let rel = f.rel.replace("{{module}}", &module);
        let path = dest.join(rel);
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).map_err(io(dir))?;
        }
        std::fs::write(&path, vars.render(&f.contents)).map_err(io(&path))?;
        if f.executable {
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).map_err(io(&path))?;
        }
        written.push(path);
    }
    Ok(written)
}

fn python(kind: PluginKind, _name: &str, _module: &str) -> Vec<File> {
    let (body, tests) = match kind {
        PluginKind::Classifier => (tpl::PY_CLASSIFIER, tpl::PY_T_CLASSIFIER),
        PluginKind::Policy => (tpl::PY_POLICY, tpl::PY_T_POLICY),
        PluginKind::Sensor => (tpl::PY_SENSOR, tpl::PY_T_SENSOR),
        PluginKind::Sink => (tpl::PY_SINK, tpl::PY_T_SINK),
        PluginKind::Adapter => (tpl::PY_ADAPTER, tpl::PY_T_ADAPTER),
        PluginKind::Actuator => unreachable!("checked by Lang::kinds"),
    };
    let main = tpl::PY_MAIN.replace("{{body}}", body.trim_end());
    let test = tpl::PY_TEST.replace("{{tests}}", tests.trim_end());
    vec![
        File { rel: "{{module}}.py", contents: main, executable: true },
        File { rel: "lindesk_plugin.py", contents: tpl::PY_SDK.to_string(), executable: false },
        File { rel: "test_{{module}}.py", contents: test, executable: true },
        File { rel: "README.md", contents: tpl::PY_README.to_string(), executable: false },
    ]
}

fn shell(kind: PluginKind, _name: &str) -> Vec<File> {
    const RECORD: &str = r#"'{"jsonrpc":"2.0","method":"sink.record","params":{"records":[{"table":"events","kind":"level","ts":0,"from":"green","to":"red","policy":"t"}]}}'"#;
    let (body, cases, init_extra, request, expect) = match kind {
        PluginKind::Classifier => (
            tpl::SH_CLASSIFIER,
            "    classifier.classify)\n        classify \"$id\" \"$line\"\n        ;;",
            "",
            r#"'{"jsonrpc":"2.0","id":2,"method":"classifier.classify","params":{"proc":{"pid":5,"uid":1000,"comm":"rsync","cgroup":"/x"}}}'"#.to_string(),
            r#""id":2,"result":{"class":"batch""#,
        ),
        PluginKind::Policy => (
            tpl::SH_POLICY,
            "    policy.evaluate)\n        evaluate \"$id\" \"$line\"\n        ;;",
            "",
            r#"'{"jsonrpc":"2.0","id":2,"method":"policy.evaluate","params":{"snapshot":{"now":0,"pressure":[],"units":[],"events":[],"caps":{}}}}'"#.to_string(),
            r#""id":2,"result":{"intents":[]"#,
        ),
        PluginKind::Sensor => (
            tpl::SH_SENSOR,
            "    sensor.poll)\n        poll \"$id\" \"$line\"\n        ;;",
            "",
            r#"'{"jsonrpc":"2.0","id":2,"method":"sensor.poll","params":{"now":5,"targets":[]}}'"#.to_string(),
            r#""id":2,"result":{"samples":["#,
        ),
        PluginKind::Sink => (
            tpl::SH_SINK,
            "    sink.record)\n        record \"$line\"\n        ;;\n    sink.flush)\n        reply \"$id\" null\n        ;;",
            r#","tables":["events"]"#,
            format!(r#"{RECORD} '{{"jsonrpc":"2.0","id":2,"method":"sink.flush"}}'"#),
            r#""id":2,"result":null"#,
        ),
        PluginKind::Adapter => (
            tpl::SH_ADAPTER,
            "    adapter.start)\n        start_adapter \"$id\"\n        ;;",
            "",
            r#"'{"jsonrpc":"2.0","id":2,"method":"adapter.start"}'"#.to_string(),
            r#""id":2,"result":null"#,
        ),
        PluginKind::Actuator => unreachable!("checked by Lang::kinds"),
    };
    let main = tpl::SH_MAIN
        .replace("{{body}}", body.trim_end())
        .replace("{{cases}}", cases)
        .replace("{{init_extra}}", init_extra);
    let test = tpl::SH_TEST.replace("{{test_request}}", &request).replace("{{test_expect}}", expect);
    vec![
        File { rel: "plugin.sh", contents: main, executable: true },
        File { rel: "test.sh", contents: test, executable: true },
        File { rel: "README.md", contents: tpl::SH_README.to_string(), executable: false },
    ]
}

fn rust(kind: PluginKind, _name: &str) -> Vec<File> {
    let body = match kind {
        PluginKind::Classifier => tpl::RS_CLASSIFIER,
        PluginKind::Policy => tpl::RS_POLICY,
        PluginKind::Sensor => tpl::RS_SENSOR,
        PluginKind::Sink => tpl::RS_SINK,
        PluginKind::Adapter => tpl::RS_ADAPTER,
        PluginKind::Actuator => tpl::RS_ACTUATOR,
    };
    vec![
        File { rel: "Cargo.toml", contents: tpl::RS_CARGO.to_string(), executable: false },
        File { rel: "src/main.rs", contents: tpl::RS_MAIN.replace("{{body}}", body.trim_end()), executable: false },
        File { rel: "README.md", contents: tpl::RS_README.to_string(), executable: false },
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_and_langs() {
        assert_eq!(camel("steam-games"), "SteamGames");
        assert!(valid_name("on-battery2"));
        for bad in ["", "Steam", "-x", "x-", "a_b", "a.b"] {
            assert!(!valid_name(bad), "{bad}");
        }
        assert_eq!("py".parse::<Lang>().unwrap(), Lang::Python);
        assert!("cobol".parse::<Lang>().is_err());
    }

    #[test]
    fn refuses_bad_inputs() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(matches!(
            scaffold(PluginKind::Actuator, Lang::Python, "x", &tmp.path().join("x")),
            Err(ScaffoldError::Unsupported { .. })
        ));
        assert!(matches!(
            scaffold(PluginKind::Policy, Lang::Python, "Bad Name", &tmp.path().join("x")),
            Err(ScaffoldError::InvalidName(_))
        ));
        std::fs::write(tmp.path().join("occupied"), "").unwrap();
        assert!(matches!(scaffold(PluginKind::Policy, Lang::Python, "x", tmp.path()), Err(ScaffoldError::Exists(_))));
    }

    #[test]
    fn vendored_sdk_matches_sdk_dir() {
        let sdk = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../sdk/python/lindesk_plugin.py");
        if let Ok(s) = std::fs::read_to_string(sdk) {
            assert!(
                s == PYTHON_SDK,
                "templates/python/lindesk_plugin.py is out of date: copy sdk/python/lindesk_plugin.py"
            );
        }
    }

    #[test]
    fn no_placeholders_left() {
        let tmp = tempfile::tempdir().unwrap();
        for lang in Lang::ALL {
            for kind in lang.kinds() {
                let dir = tmp.path().join(format!("{lang}-{kind}"));
                for f in scaffold(*kind, lang, "demo-plugin", &dir).unwrap() {
                    let text = std::fs::read_to_string(&f).unwrap();
                    assert!(!text.contains("{{"), "{} has a leftover placeholder", f.display());
                }
                let m = crate::discovery::load_manifest(&dir.join("plugin.toml")).unwrap();
                assert_eq!(m.plugin.kinds, vec![*kind]);
            }
        }
    }
}
