//! Tests for the `rules` classifier.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};

use super::*;

fn proc(comm: &str) -> ProcInfo {
    ProcInfo { pid: 42, uid: 1000, comm: comm.into(), cgroup: "/user.slice".into(), ..Default::default() }
}

fn slice(name: &str) -> ProcInfo {
    ProcInfo { pid: 0, uid: 0, comm: String::new(), unit: Some(name.into()), ..Default::default() }
}

fn class_of(c: &RulesClassifier, p: &ProcInfo) -> Option<QosClass> {
    c.lookup(p).map(|c| c.class)
}

/// A fresh scratch directory under the system temp dir, removed on drop.
struct Scratch(PathBuf);

impl Scratch {
    fn new() -> Self {
        static N: AtomicU32 = AtomicU32::new(0);
        let p = std::env::temp_dir().join(format!(
            "lindesk-rules-test-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).unwrap();
        Scratch(p)
    }

    fn dir(&self, name: &str) -> PathBuf {
        let d = self.0.join(name);
        std::fs::create_dir_all(&d).unwrap();
        d
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn write(dir: &Path, file: &str, text: &str) {
    std::fs::write(dir.join(file), text).unwrap();
}

fn rule(name: &str, class: &str, matchers: &str) -> String {
    format!("[[rule]]\nname = \"{name}\"\nclass = \"{class}\"\n{matchers}\n")
}

#[test]
fn and_across_fields_or_within_a_list() {
    let c = RulesClassifier::from_str(
        "t.toml",
        &rule("rsync-backup", "batch", "comm = [\"rsync\", \"rclone\"]\nunit = [\"*backup*\"]"),
    )
    .unwrap();
    let mut p = proc("rsync");
    assert_eq!(class_of(&c, &p), None, "unit is required but missing");
    p.unit = Some("home-backup.service".into());
    assert_eq!(class_of(&c, &p), Some(QosClass::Batch));
    p.comm = "rclone".into();
    assert_eq!(class_of(&c, &p), Some(QosClass::Batch), "OR within a list");
    p.unit = Some("sync.service".into());
    assert_eq!(class_of(&c, &p), None, "AND across fields");
}

#[test]
fn globs_uid_app_id_exe_and_reason() {
    let text = [
        rule("by-uid", "background", "uid = [2000]\ncomm = [\"*\"]"),
        rule("steam", "interactive", "app_id = [\"com.valvesoftware.*\"]"),
        rule("portal", "critical", "exe = [\"/usr/libexec/xdg-desktop-portal*\"]\nconfidence = 0.7"),
    ]
    .concat();
    let c = RulesClassifier::from_str("50-mine.toml", &text).unwrap();
    let mut p = proc("anything");
    assert_eq!(class_of(&c, &p), None);
    p.uid = 2000;
    let got = c.lookup(&p).unwrap();
    assert_eq!(got.class, QosClass::Background);
    assert_eq!(got.reason, "rules: 50-mine.toml:by-uid");
    assert_eq!(got.confidence, 1.0);

    let mut p = proc("steam");
    p.app_id = Some("com.valvesoftware.Steam".into());
    assert_eq!(class_of(&c, &p), Some(QosClass::Interactive));

    let mut p = proc("xdg-desktop-por");
    p.exe = Some("/usr/libexec/xdg-desktop-portal-gnome".into());
    let got = c.lookup(&p).unwrap();
    assert_eq!((got.class, got.confidence), (QosClass::Critical, 0.7));
}

#[test]
fn first_match_wins_within_a_file_and_later_files_win() {
    let a = [rule("a1", "background", "comm = [\"foo*\"]"), rule("a2", "batch", "comm = [\"foobar\"]")].concat();
    let b = rule("b1", "critical", "comm = [\"foobar\"]");
    let c = RulesClassifier::from_files([("10-a.toml", a.as_str())]).unwrap();
    assert_eq!(c.lookup(&proc("foobar")).unwrap().reason, "rules: 10-a.toml:a1");
    let c = RulesClassifier::from_files([("10-a.toml", a.as_str()), ("20-b.toml", b.as_str())]).unwrap();
    assert_eq!(c.lookup(&proc("foobar")).unwrap().reason, "rules: 20-b.toml:b1");
    assert_eq!(c.lookup(&proc("foox")).unwrap().reason, "rules: 10-a.toml:a1", "falls through to earlier files");
}

#[test]
fn slices_match_on_unit() {
    let c = RulesClassifier::from_str("s.toml", &rule("bg", "background", "unit = [\"background.slice\"]")).unwrap();
    assert_eq!(class_of(&c, &slice("background.slice")), Some(QosClass::Background));
    assert_eq!(class_of(&c, &slice("app.slice")), None);
    // A comm rule never matches a slice (empty comm), unless it is a catch-all glob.
    let c = RulesClassifier::from_str("s.toml", &rule("x", "batch", "comm = [\"x*\"]")).unwrap();
    assert_eq!(class_of(&c, &slice("x.slice")), None);
}

#[test]
fn invalid_rules_are_rejected() {
    assert!(RulesClassifier::from_str("t.toml", "[[rule]]\nname = \"empty\"\nclass = \"batch\"\n").is_err());
    assert!(RulesClassifier::from_str("t.toml", &rule("c", "nonsense", "comm = [\"a\"]")).is_err());
    assert!(RulesClassifier::from_str("t.toml", &rule("g", "batch", "comm = [\"[a\"]")).is_err());
    assert!(RulesClassifier::from_str("t.toml", &rule("t", "batch", "comm = [\"a\"]\ncmom = [\"b\"]")).is_err());
    assert!(RulesClassifier::from_str("t.toml", &rule("k", "batch", "comm = [\"a\"]\nconfidence = 1.5")).is_err());
    assert!(RulesClassifier::from_str("t.toml", "").unwrap().lookup(&proc("a")).is_none());
}

#[test]
fn precedence_across_directories_and_drop_in_replacement() {
    let s = Scratch::new();
    let vendor = s.dir("usr");
    let etc = s.dir("etc");
    let user = s.dir("user");
    write(&vendor, "10-critical.toml", &rule("compositors", "critical", "comm = [\"gnome-shell\", \"kwin*\"]"));
    write(&vendor, "50-background.toml", &rule("indexers", "background", "comm = [\"tracker-miner-*\", \"baloo*\"]"));
    write(&vendor, "60-batch.toml", &rule("pkgs", "batch", "comm = [\"apt\", \"dpkg\"]"));
    // /etc: a low-numbered file still beats every vendor file.
    write(&etc, "05-local.toml", &rule("kwin-normal", "normal", "comm = [\"kwin_x11\"]"));
    // /etc replaces the vendor batch file entirely: dpkg is no longer classified.
    write(&etc, "60-batch.toml", &rule("only-apt", "batch", "comm = [\"apt\"]"));
    // user: overrides /etc and vendor.
    write(&user, "01-me.toml", &rule("baloo-batch", "batch", "comm = [\"baloo*\"]"));
    // An empty same-named file disables the vendor file.
    write(&user, "50-background.toml", "");
    // Non-TOML files are ignored; broken files are skipped and reported.
    write(&user, "README", "not toml");
    write(&user, "99-broken.toml", "[[rule]\n");

    let c = RulesClassifier::from_dirs([&vendor, &etc, &user]);
    assert_eq!(
        c.files(),
        vec![
            ("10-critical.toml", 1),
            ("05-local.toml", 1),
            ("60-batch.toml", 1),
            ("01-me.toml", 1),
            ("50-background.toml", 0)
        ]
    );
    assert_eq!(c.problems().len(), 1);
    assert!(c.problems()[0].path.ends_with("99-broken.toml"));

    assert_eq!(c.lookup(&proc("gnome-shell")).unwrap().reason, "rules: 10-critical.toml:compositors");
    assert_eq!(c.lookup(&proc("kwin_x11")).unwrap().reason, "rules: 05-local.toml:kwin-normal");
    assert_eq!(c.lookup(&proc("kwin_wayland")).unwrap().class, QosClass::Critical);
    assert_eq!(c.lookup(&proc("apt")).unwrap().reason, "rules: 60-batch.toml:only-apt");
    assert_eq!(class_of(&c, &proc("dpkg")), None, "vendor 60-batch.toml was replaced");
    assert_eq!(class_of(&c, &proc("tracker-miner-f")), None, "vendor 50-background.toml was masked");
    assert_eq!(c.lookup(&proc("baloo_file")).unwrap().reason, "rules: 01-me.toml:baloo-batch");

    // reload() picks up changes.
    let mut c = c;
    std::fs::remove_file(user.join("99-broken.toml")).unwrap();
    write(&user, "50-background.toml", &rule("tracker", "background", "comm = [\"tracker-*\"]"));
    assert!(c.reload().is_empty());
    assert_eq!(class_of(&c, &proc("tracker-miner-f")), Some(QosClass::Background));
    // Missing directories are fine.
    let c = RulesClassifier::from_dirs([s.0.join("nope")]);
    assert!(c.problems().is_empty() && c.files().is_empty());
}

#[test]
fn plugin_init_reads_config_dirs() {
    let s = Scratch::new();
    let d = s.dir("rules.d");
    write(&d, "10-x.toml", &rule("x", "batch", "comm = [\"x\"]"));
    let ctx = |privilege, config| InitContext {
        api_version: lindesk_plugin_api::API_VERSION,
        host: "simulate".into(),
        privilege,
        uid: None,
        caps: Default::default(),
        config,
    };
    let mut c = RulesClassifier::default();
    assert_eq!(c.info().name, "rules");
    c.init(&ctx(Privilege::User, serde_json::json!({ "dirs": [d.to_str().unwrap()] }))).unwrap();
    assert_eq!(c.classify(&proc("x")).unwrap().map(|c| c.class), Some(QosClass::Batch));
    assert_eq!(c.classify(&proc("y")).unwrap(), None);
    assert!(c.init(&ctx(Privilege::User, serde_json::json!({ "dir": [] }))).is_err());

    // The system broker never reads user-relative directories.
    c.init(&ctx(Privilege::System, serde_json::json!({ "dirs": ["/usr/share/x", "$XDG_CONFIG_HOME/l", "~/l"] })))
        .unwrap();
    assert_eq!(c.dirs(), &[PathBuf::from("/usr/share/x")]);
    assert_eq!(default_dirs(Privilege::System), vec![VENDOR_DIR, ADMIN_DIR]);
    assert_eq!(default_dirs(Privilege::User).len(), 3);
}

#[test]
fn packaged_default_rules() {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../packaging/config/rules.d");
    let c = RulesClassifier::from_dirs([&dir]);
    assert!(c.problems().is_empty(), "{:?}", c.problems());
    assert_eq!(
        c.files().iter().map(|f| f.0).collect::<Vec<_>>(),
        vec!["00-slices.toml", "10-critical.toml", "50-background.toml", "60-batch.toml"]
    );
    let cases: &[(&str, Option<QosClass>)] = &[
        ("gnome-shell", Some(QosClass::Critical)),
        ("kwin_wayland", Some(QosClass::Critical)),
        ("Xwayland", Some(QosClass::Critical)),
        ("pipewire-pulse", Some(QosClass::Critical)),
        ("xdg-desktop-por", Some(QosClass::Critical)),
        ("gnome-session-b", Some(QosClass::Critical)),
        ("at-spi2-registr", Some(QosClass::Critical)),
        ("tracker-miner-fs-3", Some(QosClass::Background)),
        ("tracker-miner-f", Some(QosClass::Background)),
        ("baloo_file_extr", Some(QosClass::Background)),
        ("evolution-alarm", Some(QosClass::Background)),
        ("syncthing", Some(QosClass::Background)),
        ("packagekitd", Some(QosClass::Batch)),
        ("unattended-upgr", Some(QosClass::Batch)),
        ("updatedb.mlocat", Some(QosClass::Batch)),
        ("restic", Some(QosClass::Batch)),
        ("firefox", None),
        ("rsync", None),
        ("bash", None),
    ];
    for (comm, want) in cases {
        assert_eq!(class_of(&c, &proc(comm)), *want, "{comm}");
    }

    let slices = [
        ("session.slice", Some(QosClass::Critical)),
        ("background.slice", Some(QosClass::Background)),
        ("lindesk-batch.slice", Some(QosClass::Batch)),
        ("app.slice", Some(QosClass::Normal)),
        ("system.slice", None),
    ];
    for (name, want) in slices {
        assert_eq!(class_of(&c, &slice(name)), want, "{name}");
    }
    // Processes *inside* app.slice are not classified by the slice rule.
    let mut ff = proc("firefox");
    ff.slice = Some("app.slice".into());
    ff.unit = Some("app-gnome-firefox-1234.scope".into());
    assert_eq!(class_of(&c, &ff), None);

    let unit = |comm: &str, unit: &str| ProcInfo { unit: Some(unit.into()), ..proc(comm) };
    assert_eq!(class_of(&c, &unit("rsync", "home-backup.service")), Some(QosClass::Batch));
    assert_eq!(class_of(&c, &unit("fwupd", "fwupd.service")), Some(QosClass::Batch));
    assert_eq!(class_of(&c, &unit("flatpak-system-", "flatpak-system-helper.service")), Some(QosClass::Batch));
    assert_eq!(class_of(&c, &unit("sh", "docker-0123abcd.scope")), Some(QosClass::Background));
    assert_eq!(class_of(&c, &unit("btrfs", "btrfs-scrub@-.service")), Some(QosClass::Batch));
    assert_eq!(c.lookup(&proc("gnome-shell")).unwrap().reason, "rules: 10-critical.toml:compositors");
}
