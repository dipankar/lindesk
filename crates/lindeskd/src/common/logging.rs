//! Logging: journald when started by systemd (`JOURNAL_STREAM` set), stderr otherwise.
//! The journal is written through [`JournalLayer`], which drops records rather than wait for
//! a journald that has stopped reading.
//!
//! The filter is `--log-level` when given, else `RUST_LOG`, else `info`. Either may be a
//! full `tracing_subscriber::EnvFilter` directive such as `info,lindesk_host=debug`. Plugin
//! log lines carry a `plugin` field, which journald stores as `F_PLUGIN` ([`FIELD_PREFIX`]
//! `_` + the upper-cased name; `lindeskctl plugin logs` filters on it).

use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::{EnvFilter, Layer};

use super::journal::JournalLayer;

/// Prefix of every structured field in the journal except `message` (`lindeskctl plugin logs`
/// depends on `F_PLUGIN`).
pub const FIELD_PREFIX: &str = "F";

/// The filter directive to use.
pub fn directive(cli: Option<&str>, rust_log: Option<&str>) -> String {
    cli.or(rust_log).filter(|s| !s.trim().is_empty()).unwrap_or("info").to_string()
}

/// Install the global subscriber. `ident` is the syslog identifier in the journal.
pub fn init(ident: &str, cli_level: Option<&str>) {
    let rust_log = std::env::var("RUST_LOG").ok();
    let d = directive(cli_level, rust_log.as_deref());
    let filter = EnvFilter::try_new(&d).unwrap_or_else(|e| {
        eprintln!("{ident}: invalid log filter '{d}' ({e}); using 'info'");
        EnvFilter::new("info")
    });
    if std::env::var_os("JOURNAL_STREAM").is_some() {
        match JournalLayer::new(ident, FIELD_PREFIX) {
            Ok(j) => {
                let _ = tracing_subscriber::registry().with(j.with_filter(filter)).try_init();
                return;
            }
            Err(e) => eprintln!("{ident}: journald unavailable ({e}); logging to stderr"),
        }
    }
    let fmt = tracing_subscriber::fmt::layer().with_writer(std::io::stderr).with_target(false);
    let _ = tracing_subscriber::registry().with(fmt.with_filter(filter)).try_init();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn precedence() {
        assert_eq!(directive(Some("debug"), Some("warn")), "debug");
        assert_eq!(directive(None, Some("warn")), "warn");
        assert_eq!(directive(None, None), "info");
        assert_eq!(directive(Some(" "), None), "info");
    }
}
