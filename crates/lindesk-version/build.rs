//! Records the git commit and the build date for `lindesk-version`. No dependencies.
//!
//! * commit: `$LINDESK_GIT_HASH`, else `git rev-parse --short=7 HEAD` in this source tree,
//!   else none (a source tarball without `.git`; packaging passes `LINDESK_GIT_HASH`).
//! * date (`YYYY-MM-DD`, UTC): `$LINDESK_BUILD_DATE`, else `$SOURCE_DATE_EPOCH`, else the
//!   commit date of `HEAD`, else today. The commit date keeps builds of one commit
//!   reproducible and avoids a rebuild every day.
//!
//! Re-runs when `HEAD` moves (the files under `.git` that change on commit/checkout) or one
//! of the variables changes, so only this tiny crate and the binaries relink.

use std::path::{Path, PathBuf};
use std::process::Command;

fn git(dir: &Path, args: &[&str]) -> Option<String> {
    let out = Command::new("git").arg("-C").arg(dir).args(args).output().ok()?;
    let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
    (out.status.success() && !s.is_empty()).then_some(s)
}

fn env(name: &str) -> Option<String> {
    println!("cargo:rerun-if-env-changed={name}");
    std::env::var(name).ok().map(|v| v.trim().to_string()).filter(|v| !v.is_empty())
}

/// Days since 1970-01-01 → (year, month, day). Howard Hinnant's `civil_from_days`.
fn civil(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = (if mp < 10 { mp + 3 } else { mp - 9 }) as u32;
    let y = yoe + era * 400 + i64::from(m <= 2);
    (y, m, d)
}

fn date_of_epoch(secs: i64) -> String {
    let (y, m, d) = civil(secs.div_euclid(86_400));
    format!("{y:04}-{m:02}-{d:02}")
}

fn watch_head(src: &Path) {
    let Some(git_dir) = git(src, &["rev-parse", "--absolute-git-dir"]).map(PathBuf::from) else { return };
    let common = git(src, &["rev-parse", "--path-format=absolute", "--git-common-dir"]).map(PathBuf::from);
    let head = git_dir.join("HEAD");
    println!("cargo:rerun-if-changed={}", head.display());
    if let Ok(text) = std::fs::read_to_string(&head)
        && let Some(r) = text.trim().strip_prefix("ref: ")
    {
        let base = common.unwrap_or_else(|| git_dir.clone());
        // A loose ref, or packed-refs once git packs it; both may not exist yet.
        println!("cargo:rerun-if-changed={}", base.join(r).display());
        println!("cargo:rerun-if-changed={}", base.join("packed-refs").display());
    }
}

fn main() {
    let src = PathBuf::from(std::env::var_os("CARGO_MANIFEST_DIR").expect("cargo sets CARGO_MANIFEST_DIR"));
    println!("cargo:rerun-if-changed=build.rs");
    watch_head(&src);

    let hash = env("LINDESK_GIT_HASH").or_else(|| git(&src, &["rev-parse", "--short=7", "HEAD"])).unwrap_or_default();
    let date = env("LINDESK_BUILD_DATE")
        .or_else(|| env("SOURCE_DATE_EPOCH").and_then(|s| s.parse::<i64>().ok()).map(date_of_epoch))
        .or_else(|| git(&src, &["log", "-1", "--format=%ct", "HEAD"]).and_then(|s| s.parse().ok()).map(date_of_epoch))
        .unwrap_or_else(|| {
            let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs());
            date_of_epoch(now as i64)
        });
    let version = std::env::var("CARGO_PKG_VERSION").unwrap_or_default();
    let long = if hash.is_empty() { format!("{version} ({date})") } else { format!("{version} ({hash}, {date})") };
    println!("cargo:rustc-env=LINDESK_GIT_HASH={hash}");
    println!("cargo:rustc-env=LINDESK_BUILD_DATE={date}");
    println!("cargo:rustc-env=LINDESK_VERSION_LONG={long}");
}
